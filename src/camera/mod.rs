mod led;
pub(crate) use led::LedMode;
mod linux_uvc;
mod protocol;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::{
    config::{CameraAdapter, CameraConfig},
    model::{
        BuiltInGesture, CameraAttitudeSource, CameraImageControl, CameraImageControlState, unix_ms,
    },
    runtime::Runtime,
};
use linux_uvc::{
    LinuxUvcTransport, XuTransport, ZoomControl, image_control_kind, image_control_options,
    unavailable_image_control, validate_image_control_value,
};
use protocol::{
    AI_GET_GIM_STATE, AI_GET_QUICK_STATUS, AiGestureStatus, AiGimbalState, CameraStatus,
    FRAME_SIZE, TRACKING_SELECTOR, VENDOR_SELECTOR,
};

const COMMAND_QUEUE_CAPACITY: usize = 32;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const GESTURE_STATUS_MINIMUM_INTERVAL: Duration = Duration::from_secs(5);
const TELEMETRY_MAXIMUM_BACKOFF: Duration = Duration::from_secs(60);
const IMAGE_SETTINGS_MINIMUM_INTERVAL: Duration = Duration::from_secs(5);
const HDR_SWITCH_MINIMUM_INTERVAL: Duration = Duration::from_secs(3);
const NUDGE_SPEED_FRACTION: f64 = 0.25;
pub const PAN_TILT_LEASE: Duration = Duration::from_millis(350);

#[derive(Debug)]
enum Command {
    Shutdown,
    McpActivity,
    Led {
        mode: LedMode,
    },
    Power {
        enabled: bool,
    },
    Move {
        yaw: f32,
        pitch: f32,
        roll: f32,
    },
    Recenter,
    Tracking {
        enabled: bool,
    },
    Hdr {
        enabled: bool,
    },
    BuiltInGesture {
        feature: BuiltInGesture,
        enabled: bool,
    },
    Zoom {
        magnification: f32,
    },
    PanTiltSpeed {
        pan_direction: i8,
        tilt_direction: i8,
        speed_fraction: f64,
    },
    ImageControl {
        control: CameraImageControl,
        value: i32,
    },
}

#[derive(Debug)]
enum CommandOutcome {
    Applied,
    ImageControls(Vec<CameraImageControlState>),
}

struct Request {
    command: Command,
    response: oneshot::Sender<Result<CommandOutcome, String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TelemetryKind {
    Gimbal,
    Gestures,
    Status,
    ImageSettings,
}

#[derive(Debug)]
enum TelemetryUpdate {
    Gimbal(AiGimbalState),
    Gestures(AiGestureStatus),
    Status(CameraStatus),
    ImageSettings(Vec<CameraImageControlState>),
    Failure {
        kind: TelemetryKind,
        error: String,
        retry_after: Duration,
    },
}

struct PollSchedule {
    interval: Duration,
    next_due: Instant,
    consecutive_failures: u32,
}

impl PollSchedule {
    fn new(interval: Duration, now: Instant) -> Self {
        Self {
            interval,
            next_due: now + interval,
            consecutive_failures: 0,
        }
    }

    fn due(&self, now: Instant) -> bool {
        now >= self.next_due
    }

    fn succeeded(&mut self, now: Instant) {
        self.consecutive_failures = 0;
        self.next_due = now + self.interval;
    }

    fn failed(&mut self, now: Instant) -> Duration {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let multiplier = 1_u32 << self.consecutive_failures.min(6);
        let backoff = self
            .interval
            .saturating_mul(multiplier)
            .min(TELEMETRY_MAXIMUM_BACKOFF);
        self.next_due = now + backoff;
        backoff
    }
}

struct TelemetryPoller {
    tx: tokio_mpsc::UnboundedSender<TelemetryUpdate>,
    gimbal: PollSchedule,
    gestures: PollSchedule,
    status: PollSchedule,
    image_settings: PollSchedule,
}

impl TelemetryPoller {
    fn new(
        interval: Duration,
        tx: tokio_mpsc::UnboundedSender<TelemetryUpdate>,
        now: Instant,
    ) -> Self {
        let gesture_interval = interval
            .saturating_mul(5)
            .max(GESTURE_STATUS_MINIMUM_INTERVAL);
        let image_settings_interval = interval.max(IMAGE_SETTINGS_MINIMUM_INTERVAL);
        Self {
            tx,
            gimbal: PollSchedule::new(interval, now),
            gestures: PollSchedule::new(gesture_interval, now),
            status: PollSchedule::new(interval, now),
            image_settings: PollSchedule::new(image_settings_interval, now),
        }
    }

    fn next_kind(&self, now: Instant) -> Option<TelemetryKind> {
        if self.gimbal.due(now) {
            Some(TelemetryKind::Gimbal)
        } else if self.status.due(now) {
            Some(TelemetryKind::Status)
        } else if self.gestures.due(now) {
            Some(TelemetryKind::Gestures)
        } else if self.image_settings.due(now) {
            Some(TelemetryKind::ImageSettings)
        } else {
            None
        }
    }

    fn schedule_mut(&mut self, kind: TelemetryKind) -> &mut PollSchedule {
        match kind {
            TelemetryKind::Gimbal => &mut self.gimbal,
            TelemetryKind::Gestures => &mut self.gestures,
            TelemetryKind::Status => &mut self.status,
            TelemetryKind::ImageSettings => &mut self.image_settings,
        }
    }
}

#[derive(Clone)]
pub struct CameraHandle {
    tx: SyncSender<Request>,
    telemetry: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    powered_on: Arc<AtomicBool>,
    power_transition: Arc<AtomicBool>,
    max_yaw_degrees: f32,
    max_pitch_degrees: f32,
    controlled_zoom_bits: Arc<AtomicU32>,
}

impl CameraHandle {
    /// Best-effort feedback must never block or fail an MCP operation.
    pub(crate) fn notify_mcp_activity(&self) {
        if !self.is_powered_on() || self.power_transition.load(Ordering::Relaxed) {
            return;
        }
        let (response, _) = oneshot::channel();
        if let Err(error) = self.tx.try_send(Request {
            command: Command::McpActivity,
            response,
        }) {
            tracing::debug!(%error, "MCP LED feedback could not be queued");
        }
    }

    /// Accept a desired mode; USB application is asynchronous with bounded retries.
    /// Internal backend control; no API, MCP tool, or persisted user setting.
    #[allow(dead_code)] // Usage policy is deliberately deferred; production stays off.
    pub(crate) async fn set_led_mode(&self, mode: LedMode) -> Result<()> {
        self.request(Command::Led { mode }).await.map(|_| ())
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.request(Command::Shutdown).await?;
        if let Some(task) = self.telemetry.lock().await.take() {
            let _ = task.await;
        }
        Ok(())
    }

    pub async fn set_powered_on(&self, enabled: bool) -> Result<()> {
        self.begin_power_transition();
        let result = self.request(Command::Power { enabled }).await.map(|_| ());
        self.end_power_transition();
        result
    }

    pub fn is_powered_on(&self) -> bool {
        self.powered_on.load(Ordering::Relaxed)
    }

    pub fn begin_power_transition(&self) {
        self.power_transition.store(true, Ordering::Relaxed);
    }

    pub fn end_power_transition(&self) {
        self.power_transition.store(false, Ordering::Relaxed);
    }

    pub fn validate_orientation(&self, yaw: f32, pitch: f32, roll: f32) -> Result<()> {
        if !yaw.is_finite() || !pitch.is_finite() || !roll.is_finite() {
            bail!("camera angles must be finite");
        }
        if yaw.abs() > self.max_yaw_degrees {
            bail!("yaw exceeds the configured safe limit");
        }
        if pitch.abs() > self.max_pitch_degrees {
            bail!("pitch exceeds the configured safe limit");
        }
        if roll.abs() > 45.0 {
            bail!("roll exceeds the fixed safe limit");
        }
        Ok(())
    }

    pub async fn move_to(&self, yaw: f32, pitch: f32, roll: f32) -> Result<()> {
        self.validate_orientation(yaw, pitch, roll)?;
        self.request(Command::Move { yaw, pitch, roll })
            .await
            .map(|_| ())
    }

    pub async fn recenter(&self) -> Result<()> {
        self.request(Command::Recenter).await.map(|_| ())
    }

    pub async fn set_tracking(&self, enabled: bool) -> Result<()> {
        self.request(Command::Tracking { enabled })
            .await
            .map(|_| ())
    }

    pub async fn set_hdr(&self, enabled: bool) -> Result<()> {
        self.request(Command::Hdr { enabled }).await.map(|_| ())
    }

    pub async fn set_built_in_gesture(&self, feature: BuiltInGesture, enabled: bool) -> Result<()> {
        self.request(Command::BuiltInGesture { feature, enabled })
            .await
            .map(|_| ())
    }

    pub fn validate_zoom(magnification: f32) -> Result<()> {
        if !magnification.is_finite() || !(1.0..=4.0).contains(&magnification) {
            bail!("zoom magnification must be between 1.0 and 4.0");
        }
        Ok(())
    }

    pub async fn set_zoom(&self, magnification: f32) -> Result<()> {
        Self::validate_zoom(magnification)?;
        self.request(Command::Zoom { magnification }).await?;
        self.controlled_zoom_bits
            .store(magnification.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    pub fn controlled_zoom_magnification(&self) -> Option<f32> {
        let magnification = f32::from_bits(self.controlled_zoom_bits.load(Ordering::Relaxed));
        magnification.is_finite().then_some(magnification)
    }

    pub async fn set_image_control(
        &self,
        control: CameraImageControl,
        value: i32,
    ) -> Result<Vec<CameraImageControlState>> {
        match self
            .request(Command::ImageControl { control, value })
            .await?
        {
            CommandOutcome::ImageControls(controls) => Ok(controls),
            CommandOutcome::Applied => bail!("camera returned no image-control readback"),
        }
    }

    pub async fn set_pan_tilt_speed(&self, pan_direction: i8, tilt_direction: i8) -> Result<()> {
        if !(-1..=1).contains(&pan_direction)
            || !(-1..=1).contains(&tilt_direction)
            || (pan_direction != 0 && tilt_direction != 0)
        {
            bail!("camera movement must select at most one pan or tilt direction");
        }
        self.request(Command::PanTiltSpeed {
            pan_direction,
            tilt_direction,
            speed_fraction: NUDGE_SPEED_FRACTION,
        })
        .await
        .map(|_| ())
    }

    pub async fn set_face_tracking_speed(
        &self,
        pan_direction: i8,
        tilt_direction: i8,
        speed_fraction: f64,
    ) -> Result<()> {
        if !(-1..=1).contains(&pan_direction) || !(-1..=1).contains(&tilt_direction) {
            bail!("face tracking movement directions must be between -1 and 1");
        }
        if !speed_fraction.is_finite() || !(0.0..=1.0).contains(&speed_fraction) {
            bail!("face tracking speed fraction must be between 0 and 1");
        }
        self.request(Command::PanTiltSpeed {
            pan_direction,
            tilt_direction,
            speed_fraction,
        })
        .await
        .map(|_| ())
    }

    async fn request(&self, command: Command) -> Result<CommandOutcome> {
        if !matches!(command, Command::Power { .. } | Command::Shutdown) {
            if self.power_transition.load(Ordering::Relaxed) {
                bail!("camera power is changing");
            }
            if !self.is_powered_on() {
                bail!("camera is powered off");
            }
        }
        let (response, rx) = oneshot::channel();
        self.tx
            .try_send(Request { command, response })
            .map_err(|error| anyhow!("camera command queue is unavailable: {error}"))?;
        rx.await
            .map_err(|_| anyhow!("camera command worker stopped"))?
            .map_err(|error| anyhow!(error))
    }
}

pub async fn start(config: CameraConfig, runtime: Runtime) -> Result<Option<CameraHandle>> {
    let name = crate::devices::cameras()
        .unwrap_or_default()
        .into_iter()
        .find(|camera| camera.id == config.control_device)
        .map(|camera| camera.name);
    runtime
        .update(|state| {
            state.camera.adapter = adapter_name(config.adapter).into();
            state.camera.name = if config.adapter == CameraAdapter::Mock {
                Some("Synthetic video".into())
            } else {
                name
            };
            state.camera.device_id = matches!(
                config.adapter,
                CameraAdapter::V4l2 | CameraAdapter::ObsbotTiny2
            )
            .then(|| config.control_device.clone());
            state.camera.capabilities = if matches!(
                config.adapter,
                CameraAdapter::Mock | CameraAdapter::ObsbotTiny2
            ) {
                crate::model::CameraCapabilities {
                    power: true,
                    absolute_position: true,
                    pan_tilt: true,
                    motor_telemetry: true,
                    tracking: true,
                    hdr: true,
                    zoom: true,
                    image_settings: true,
                    built_in_gestures: true,
                }
            } else {
                Default::default()
            };
        })
        .await;
    match config.adapter {
        CameraAdapter::Disabled => Ok(None),
        CameraAdapter::Mock => {
            let handle = spawn_mock(&config);
            let now = unix_ms();
            runtime
                .update(|state| {
                    state.camera.available = true;
                    state.camera.powered_on = Some(true);
                    state.camera.yaw_degrees = Some(0.0);
                    state.camera.pitch_degrees = Some(0.0);
                    state.camera.roll_degrees = Some(0.0);
                    state.camera.tracking = Some(false);
                    state.camera.tracking_sample_at_ms = Some(now);
                    state.camera.zoom_magnification = Some(1.0);
                    state.camera.hdr = Some(false);
                    state.camera.hdr_sample_at_ms = Some(now);
                    state.camera.image_settings.controls = mock_image_controls();
                    state.camera.attitude_source = CameraAttitudeSource::Simulated;
                    state.camera.sample_at_ms = Some(now);
                })
                .await;
            Ok(Some(handle))
        }
        CameraAdapter::V4l2 => {
            let mut transport = LinuxUvcTransport::open(&config.control_device, config.xu_unit)?;
            let controls = transport.image_controls();
            let (handle, telemetry_rx) = spawn_worker(transport, &config, None);
            runtime
                .update(|state| {
                    state.camera.available = true;
                    state.camera.capabilities.image_settings = controls.iter().any(|c| c.available);
                    state.camera.image_settings.controls = controls;
                    state.camera.powered_on = None;
                })
                .await;
            *handle.telemetry.lock().await = Some(spawn_telemetry_updates(
                telemetry_rx,
                runtime,
                config.control_device.clone(),
            ));
            Ok(Some(handle))
        }
        CameraAdapter::ObsbotTiny2 => {
            let mut transport = LinuxUvcTransport::open(&config.control_device, config.xu_unit)?;
            let mut initial_image_controls = transport.image_controls();
            initial_image_controls.push(unavailable_image_control(
                CameraImageControl::FacePriorityAutoExposure,
                "awaiting selector-6 status readback",
            ));
            let initial_zoom = match transport.zoom_control() {
                Ok(control) => match magnification_from_zoom_units(control) {
                    Ok(magnification) => Some(magnification),
                    Err(error) => {
                        tracing::warn!(%error, "camera returned an invalid absolute zoom range");
                        None
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "camera absolute zoom state is unavailable");
                    None
                }
            };
            let (handle, telemetry_rx) = spawn_worker(transport, &config, initial_zoom);
            let zoom_sample_at_ms = initial_zoom.map(|_| unix_ms());
            runtime
                .update(|state| {
                    state.camera.available = true;
                    state.camera.powered_on = Some(true);
                    state.camera.zoom_magnification = initial_zoom;
                    state.camera.zoom_sample_at_ms = zoom_sample_at_ms;
                    state.camera.image_settings.controls = initial_image_controls;
                    state.camera.error = None;
                })
                .await;
            if config.poll_interval_ms > 0 {
                tracing::info!(
                    interval_ms = config.poll_interval_ms,
                    "low-priority AI telemetry polling is enabled"
                );
            }
            *handle.telemetry.lock().await = Some(spawn_telemetry_updates(
                telemetry_rx,
                runtime,
                config.control_device.clone(),
            ));
            Ok(Some(handle))
        }
    }
}

fn spawn_worker<T: XuTransport + 'static>(
    transport: T,
    config: &CameraConfig,
    initial_zoom: Option<f32>,
) -> (CameraHandle, tokio_mpsc::UnboundedReceiver<TelemetryUpdate>) {
    let (tx, rx) = sync_channel(COMMAND_QUEUE_CAPACITY);
    let (telemetry_tx, telemetry_rx) = tokio_mpsc::unbounded_channel();
    let powered_on = Arc::new(AtomicBool::new(true));
    let worker_powered_on = Arc::clone(&powered_on);
    let interval = Duration::from_millis(config.minimum_command_interval_ms);
    let standard_only = config.adapter == CameraAdapter::V4l2;
    let poll_interval =
        (config.poll_interval_ms > 0).then(|| Duration::from_millis(config.poll_interval_ms));
    std::thread::Builder::new()
        .name("tarsier-camera-owner".into())
        .spawn(move || {
            let mut worker = Worker::new(transport, rx, interval)
                .with_power_state(worker_powered_on)
                .with_telemetry(poll_interval, telemetry_tx);
            worker.standard_only = standard_only;
            if !standard_only {
                worker = worker.with_led_control();
            }
            worker.run()
        })
        .expect("failed to spawn camera owner thread");
    (
        CameraHandle {
            tx,
            telemetry: Default::default(),
            powered_on,
            power_transition: Arc::new(AtomicBool::new(false)),
            max_yaw_degrees: config.max_yaw_degrees,
            max_pitch_degrees: config.max_pitch_degrees,
            controlled_zoom_bits: Arc::new(AtomicU32::new(
                initial_zoom.unwrap_or(f32::NAN).to_bits(),
            )),
        },
        telemetry_rx,
    )
}

struct Worker<T> {
    standard_only: bool,
    led: Option<led::Led>,
    transport: T,
    rx: Receiver<Request>,
    sequence: u16,
    minimum_interval: Duration,
    last_io: Option<Instant>,
    pan_tilt_direction: (i8, i8),
    pan_tilt_speed_fraction: f64,
    pan_tilt_deadline: Option<Instant>,
    hdr_state: Option<bool>,
    last_hdr_switch: Option<Instant>,
    telemetry: Option<TelemetryPoller>,
    powered_on: Arc<AtomicBool>,
}

impl<T: XuTransport> Worker<T> {
    fn new(transport: T, rx: Receiver<Request>, minimum_interval: Duration) -> Self {
        Self {
            standard_only: false,
            led: None,
            transport,
            rx,
            sequence: 0,
            minimum_interval,
            last_io: None,
            pan_tilt_direction: (0, 0),
            pan_tilt_speed_fraction: 0.0,
            pan_tilt_deadline: None,
            hdr_state: None,
            last_hdr_switch: None,
            telemetry: None,
            powered_on: Arc::new(AtomicBool::new(true)),
        }
    }

    fn with_led_control(mut self) -> Self {
        self.led = Some(led::Led::new(Instant::now()));
        self
    }

    fn update_led(&mut self) {
        if !self.powered_on.load(Ordering::Relaxed) {
            return;
        }
        let Some(led) = &mut self.led else {
            return;
        };
        led.advance(Instant::now());
        if Instant::now() < led.retry_at {
            return;
        }
        let initialized = led.initialized;
        let result = (|| -> Result<()> {
            if !initialized {
                let mut frame = [0; FRAME_SIZE];
                frame[..3].copy_from_slice(&[0x18, 1, 0]);
                self.set(TRACKING_SELECTOR, &mut frame)?;
                self.led.as_mut().unwrap().initialized = true;
            }
            let led = self.led.as_ref().unwrap();
            let level = led.brightness(Instant::now());
            if led.applied != Some(level) {
                let mut frame = [0; FRAME_SIZE];
                frame[..3].copy_from_slice(&[0x1a, 1, level]);
                self.set(TRACKING_SELECTOR, &mut frame)?;
                self.led.as_mut().unwrap().applied = Some(level);
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.led.as_mut().unwrap().failed(Instant::now());
            tracing::warn!(%error, "camera LED update failed; retrying in five seconds");
        }
    }

    fn extinguish_led(&mut self) {
        if let Some(led) = &mut self.led {
            led.set_mode(LedMode::Off, Instant::now());
            led.invalidate(Instant::now());
            self.update_led();
        }
    }

    fn with_power_state(mut self, powered_on: Arc<AtomicBool>) -> Self {
        self.powered_on = powered_on;
        self
    }

    fn with_telemetry(
        mut self,
        interval: Option<Duration>,
        tx: tokio_mpsc::UnboundedSender<TelemetryUpdate>,
    ) -> Self {
        self.telemetry =
            interval.map(|interval| TelemetryPoller::new(interval, tx, Instant::now()));
        self
    }

    fn run(mut self) {
        loop {
            self.expire_pan_tilt_lease();
            match self.rx.try_recv() {
                Ok(request) => {
                    if matches!(request.command, Command::Shutdown) {
                        if self.pan_tilt_direction != (0, 0) {
                            let _ = self.transport.set_pan_tilt_speed_units(0, 0);
                        }
                        self.extinguish_led();
                        drop(self);
                        let _ = request.response.send(Ok(CommandOutcome::Applied));
                        return;
                    }
                    let result = self
                        .execute(request.command)
                        .map_err(|error| error.to_string());
                    let _ = request.response.send(result);
                }
                Err(TryRecvError::Empty) => {
                    if !self.poll_telemetry_if_due() {
                        std::thread::sleep(self.minimum_interval);
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    if self.pan_tilt_direction != (0, 0) {
                        let _ = self.transport.set_pan_tilt_speed_units(0, 0);
                    }
                    self.extinguish_led();
                    break;
                }
            }
            self.update_led();
        }
    }

    fn execute(&mut self, command: Command) -> Result<CommandOutcome> {
        let standard_command = match &command {
            Command::Shutdown | Command::McpActivity | Command::Led { .. } => true,
            Command::ImageControl { control, .. } => {
                *control != CameraImageControl::FacePriorityAutoExposure
            }
            _ => false,
        };
        if self.standard_only && !standard_command {
            bail!("this hardware command is not supported by the active camera");
        }
        if !matches!(command, Command::Power { .. } | Command::Shutdown)
            && !self.powered_on.load(Ordering::Relaxed)
        {
            bail!("camera is powered off");
        }
        match command {
            Command::Shutdown => unreachable!("shutdown is handled by the owner loop"),
            Command::McpActivity => {
                if let Some(led) = &mut self.led {
                    led.notify_mcp(Instant::now());
                }
                Ok(CommandOutcome::Applied)
            }
            Command::Led { mode } => {
                if let Some(led) = &mut self.led {
                    led.set_mode(mode, Instant::now());
                }
                Ok(CommandOutcome::Applied)
            }
            Command::Power { enabled } => {
                self.set_powered_on(enabled)?;
                Ok(CommandOutcome::Applied)
            }
            Command::Move { yaw, pitch, roll } => {
                self.wake("move")?;
                let mut frame = protocol::move_frame(self.next_sequence(), yaw, pitch, roll);
                self.set(VENDOR_SELECTOR, &mut frame)?;
                Ok(CommandOutcome::Applied)
            }
            Command::Recenter => {
                self.wake("recenter")?;
                let mut frame = protocol::recenter_frame(self.next_sequence());
                self.set(VENDOR_SELECTOR, &mut frame)?;
                Ok(CommandOutcome::Applied)
            }
            Command::Tracking { enabled } => {
                self.wake("tracking")?;
                let mut payload = protocol::tracking_payload(enabled);
                self.set(TRACKING_SELECTOR, &mut payload)?;
                Ok(CommandOutcome::Applied)
            }
            Command::Hdr { enabled } => {
                self.set_hdr(enabled)?;
                Ok(CommandOutcome::Applied)
            }
            Command::BuiltInGesture { feature, enabled } => {
                self.wake("built_in_gesture")?;
                let mut frame =
                    protocol::built_in_gesture_frame(self.next_sequence(), feature, enabled);
                self.set(VENDOR_SELECTOR, &mut frame)?;
                Ok(CommandOutcome::Applied)
            }
            Command::Zoom { magnification } => {
                self.pace();
                let control = self.transport.zoom_control()?;
                let units = zoom_units_from_magnification(magnification, control)?;
                self.transport.set_zoom_units(units)?;
                self.last_io = Some(Instant::now());
                Ok(CommandOutcome::Applied)
            }
            Command::PanTiltSpeed {
                pan_direction,
                tilt_direction,
                speed_fraction,
            } => {
                self.set_pan_tilt_speed(pan_direction, tilt_direction, speed_fraction)?;
                Ok(CommandOutcome::Applied)
            }
            Command::ImageControl { control, value } => Ok(CommandOutcome::ImageControls(
                self.set_image_control(control, value)?,
            )),
        }
    }

    fn set_image_control(
        &mut self,
        control: CameraImageControl,
        value: i32,
    ) -> Result<Vec<CameraImageControlState>> {
        if control == CameraImageControl::FacePriorityAutoExposure {
            let exposure = self
                .transport
                .image_control(CameraImageControl::AutoExposure);
            let active = exposure.available && exposure.value != Some(1);
            let before = face_priority_auto_exposure_control(Some(false), active);
            validate_image_control_value(&before, value)?;
            let mut payload = protocol::face_priority_auto_exposure_payload(value != 0);
            self.set(TRACKING_SELECTOR, &mut payload)?;
            let status = self.query_status()?;
            let readback = status
                .face_priority_auto_exposure
                .ok_or_else(|| anyhow!("camera returned invalid face-priority AE readback"))?;
            if readback != (value != 0) {
                bail!(
                    "camera face-priority AE readback mismatch: requested {}, got {}",
                    value != 0,
                    readback
                );
            }
            return Ok(vec![
                exposure,
                face_priority_auto_exposure_control(Some(readback), active),
            ]);
        }

        self.validate_image_control_mode(control)?;
        self.pace();
        let updated = self.transport.set_image_control(control, value)?;
        self.last_io = Some(Instant::now());
        let mut controls = vec![updated];
        for dependency in image_control_dependencies(control) {
            controls.push(self.transport.image_control(*dependency));
        }
        if !self.standard_only && control == CameraImageControl::AutoExposure {
            let status = self.query_status()?;
            controls.push(face_priority_auto_exposure_control(
                status.face_priority_auto_exposure,
                value != 1,
            ));
        }
        Ok(controls)
    }

    fn validate_image_control_mode(&mut self, control: CameraImageControl) -> Result<()> {
        let requirement = match control {
            CameraImageControl::ExposureTimeAbsolute | CameraImageControl::Gain => Some((
                CameraImageControl::AutoExposure,
                1,
                "manual exposure must be enabled first",
            )),
            CameraImageControl::WhiteBalanceTemperature
            | CameraImageControl::RedBalance
            | CameraImageControl::BlueBalance => Some((
                CameraImageControl::WhiteBalanceAutomatic,
                0,
                "automatic white balance must be disabled first",
            )),
            CameraImageControl::FocusAbsolute => Some((
                CameraImageControl::FocusAutomaticContinuous,
                0,
                "continuous autofocus must be disabled first",
            )),
            _ => None,
        };
        let Some((dependency, required_value, message)) = requirement else {
            return Ok(());
        };
        let state = self.transport.image_control(dependency);
        if (state.available && state.value != Some(required_value))
            || (!state.available && !self.standard_only)
        {
            bail!(message);
        }
        Ok(())
    }

    fn set_powered_on(&mut self, enabled: bool) -> Result<()> {
        if self.powered_on.load(Ordering::Relaxed) == enabled {
            return Ok(());
        }
        if !enabled && self.pan_tilt_direction != (0, 0) {
            self.set_pan_tilt_speed(0, 0, NUDGE_SPEED_FRACTION)?;
        }
        crate::audit::record(
            "camera.power.command",
            serde_json::json!({"enabled": enabled, "reason": "power_transition"}),
        );
        let mut frame = if enabled {
            protocol::wake_frame(self.next_sequence())
        } else {
            protocol::sleep_frame(self.next_sequence())
        };
        if let Err(error) = self.set(VENDOR_SELECTOR, &mut frame) {
            crate::audit::record("camera.power.command_failed", serde_json::json!({}));
            return Err(error);
        }
        crate::audit::record(
            "camera.power.command_sent",
            serde_json::json!({"enabled": enabled}),
        );
        self.powered_on.store(enabled, Ordering::Relaxed);
        if let Some(led) = &mut self.led {
            led.invalidate(Instant::now());
        }
        if enabled {
            std::thread::sleep(Duration::from_millis(100));
        }
        // Let the firmware settle before reconciling its status with this command.
        if let Some(poller) = &mut self.telemetry {
            poller.status.next_due =
                Instant::now() + poller.status.interval.max(Duration::from_secs(1));
        }
        Ok(())
    }

    fn set_pan_tilt_speed(
        &mut self,
        pan_direction: i8,
        tilt_direction: i8,
        speed_fraction: f64,
    ) -> Result<()> {
        let direction = (pan_direction, tilt_direction);
        if direction == self.pan_tilt_direction
            && (direction == (0, 0)
                || (speed_fraction - self.pan_tilt_speed_fraction).abs() < f64::EPSILON)
        {
            self.pan_tilt_deadline = (direction != (0, 0)).then(|| Instant::now() + PAN_TILT_LEASE);
            return Ok(());
        }

        self.pace();
        let (pan, tilt) = if pan_direction == 0 && tilt_direction == 0 {
            (0, 0)
        } else {
            let controls = self.transport.pan_tilt_speed_controls()?;
            (
                speed_units(controls.pan, pan_direction, speed_fraction)?,
                speed_units(controls.tilt, tilt_direction, speed_fraction)?,
            )
        };
        if let Err(error) = self.transport.set_pan_tilt_speed_units(pan, tilt) {
            let _ = self.transport.set_pan_tilt_speed_units(0, 0);
            self.pan_tilt_direction = (0, 0);
            self.pan_tilt_speed_fraction = 0.0;
            self.pan_tilt_deadline = None;
            return Err(error);
        }
        self.last_io = Some(Instant::now());
        self.pan_tilt_direction = direction;
        self.pan_tilt_speed_fraction = if direction == (0, 0) {
            0.0
        } else {
            speed_fraction
        };
        self.pan_tilt_deadline = (direction != (0, 0)).then(|| Instant::now() + PAN_TILT_LEASE);
        Ok(())
    }

    fn set_hdr(&mut self, enabled: bool) -> Result<()> {
        if self.hdr_state == Some(enabled) {
            return Ok(());
        }
        if let Some(remaining) = self
            .last_hdr_switch
            .and_then(|last_switch| HDR_SWITCH_MINIMUM_INTERVAL.checked_sub(last_switch.elapsed()))
        {
            bail!(
                "HDR must not be switched again for another {:.1} seconds",
                remaining.as_secs_f32()
            );
        }
        let mut payload = protocol::hdr_payload(enabled);
        self.set(TRACKING_SELECTOR, &mut payload)?;
        self.hdr_state = Some(enabled);
        self.last_hdr_switch = Some(Instant::now());
        Ok(())
    }

    fn expire_pan_tilt_lease(&mut self) {
        if self
            .pan_tilt_deadline
            .is_none_or(|deadline| Instant::now() < deadline)
        {
            return;
        }
        if let Err(error) = self.set_pan_tilt_speed(0, 0, NUDGE_SPEED_FRACTION) {
            tracing::error!(%error, "failed to stop pan/tilt after its movement lease expired");
        }
    }

    fn wake(&mut self, reason: &str) -> Result<()> {
        crate::audit::record("camera.wake.command", serde_json::json!({"reason": reason}));
        let mut frame = protocol::wake_frame(self.next_sequence());
        if let Err(error) = self.set(VENDOR_SELECTOR, &mut frame) {
            crate::audit::record("camera.wake.command_failed", serde_json::json!({}));
            return Err(error);
        }
        crate::audit::record("camera.wake.command_sent", serde_json::json!({}));
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    fn poll_telemetry_if_due(&mut self) -> bool {
        let now = Instant::now();
        let Some(kind) = self.telemetry.as_ref().and_then(|poller| {
            if self.standard_only {
                return poller
                    .image_settings
                    .due(now)
                    .then_some(TelemetryKind::ImageSettings);
            }
            if self.powered_on.load(Ordering::Relaxed) {
                poller.next_kind(now)
            } else {
                poller.status.due(now).then_some(TelemetryKind::Status)
            }
        }) else {
            return false;
        };

        let result = match kind {
            TelemetryKind::Gimbal => self.query_gimbal().map(TelemetryUpdate::Gimbal),
            TelemetryKind::Gestures => self.query_ai_status().map(TelemetryUpdate::Gestures),
            TelemetryKind::Status => self.query_status().map(TelemetryUpdate::Status),
            TelemetryKind::ImageSettings => {
                self.pace();
                let controls = self.transport.image_controls();
                self.last_io = Some(Instant::now());
                Ok(TelemetryUpdate::ImageSettings(controls))
            }
        };
        let completed_at = Instant::now();
        let poller = self.telemetry.as_mut().expect("telemetry poller exists");
        let update = match result {
            Ok(update) => {
                poller.schedule_mut(kind).succeeded(completed_at);
                update
            }
            Err(error) => TelemetryUpdate::Failure {
                kind,
                error: error.to_string(),
                retry_after: poller.schedule_mut(kind).failed(completed_at),
            },
        };
        if poller.tx.send(update).is_err() {
            self.telemetry = None;
        }
        true
    }

    fn query_gimbal(&mut self) -> Result<AiGimbalState> {
        let sequence = self.next_sequence();
        let request = protocol::ai_gimbal_query(sequence);
        let payload = self.query_vendor(request, sequence, AI_GET_GIM_STATE)?;
        Ok(protocol::decode_ai_gimbal_state(&payload)?)
    }

    fn query_ai_status(&mut self) -> Result<AiGestureStatus> {
        let sequence = self.next_sequence();
        let request = protocol::ai_status_query(sequence);
        let payload = self.query_vendor(request, sequence, AI_GET_QUICK_STATUS)?;
        Ok(protocol::decode_ai_gesture_status(&payload)?)
    }

    fn query_vendor(
        &mut self,
        mut request: [u8; FRAME_SIZE],
        sequence: u16,
        command: u16,
    ) -> Result<Vec<u8>> {
        self.set(VENDOR_SELECTOR, &mut request)?;
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        while Instant::now() < deadline {
            let mut reply = [0_u8; FRAME_SIZE];
            self.get(VENDOR_SELECTOR, &mut reply)?;
            let Ok(frame) = protocol::parse_frame(&reply) else {
                continue;
            };
            if frame.sequence != sequence || frame.command != command {
                continue;
            }
            return Ok(frame.payload);
        }
        bail!("timed out waiting for camera reply 0x{command:04x}")
    }

    fn query_status(&mut self) -> Result<CameraStatus> {
        let mut status = [0_u8; FRAME_SIZE];
        self.get(TRACKING_SELECTOR, &mut status)?;
        let status = protocol::decode_camera_status(&status)?;
        if status.hdr.is_some() {
            self.hdr_state = status.hdr;
        }
        if let Some(enabled) = status.powered_on {
            let previous = self.powered_on.swap(enabled, Ordering::Relaxed);
            if previous != enabled {
                // A physical sleep cancels the firmware movement. Do not send
                // another motor command to a sleeping device.
                self.pan_tilt_direction = (0, 0);
                self.pan_tilt_speed_fraction = 0.0;
                self.pan_tilt_deadline = None;
                if let Some(led) = &mut self.led {
                    led.invalidate(Instant::now());
                }
            }
        }
        Ok(status)
    }

    fn set(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
        self.pace();
        self.transport.set(selector, data)?;
        self.last_io = Some(Instant::now());
        Ok(())
    }

    fn get(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
        self.pace();
        self.transport.get(selector, data)?;
        self.last_io = Some(Instant::now());
        Ok(())
    }

    fn pace(&self) {
        if let Some(last) = self.last_io {
            let elapsed = last.elapsed();
            if elapsed < self.minimum_interval {
                std::thread::sleep(self.minimum_interval - elapsed);
            }
        }
    }

    fn next_sequence(&mut self) -> u16 {
        self.sequence = self.sequence.wrapping_add(1);
        self.sequence
    }
}

fn image_control_dependencies(control: CameraImageControl) -> &'static [CameraImageControl] {
    match control {
        CameraImageControl::AutoExposure => &[
            CameraImageControl::ExposureTimeAbsolute,
            CameraImageControl::Gain,
            CameraImageControl::ExposureDynamicFramerate,
        ],
        CameraImageControl::WhiteBalanceAutomatic => &[
            CameraImageControl::WhiteBalanceTemperature,
            CameraImageControl::RedBalance,
            CameraImageControl::BlueBalance,
        ],
        CameraImageControl::FocusAutomaticContinuous => &[CameraImageControl::FocusAbsolute],
        _ => &[],
    }
}

fn face_priority_auto_exposure_control(
    enabled: Option<bool>,
    active: bool,
) -> CameraImageControlState {
    CameraImageControlState {
        control: CameraImageControl::FacePriorityAutoExposure,
        kind: image_control_kind(CameraImageControl::FacePriorityAutoExposure),
        available: enabled.is_some(),
        active,
        read_only: false,
        value: enabled.map(i32::from),
        minimum: Some(0),
        maximum: Some(1),
        step: Some(1),
        default_value: Some(0),
        options: image_control_options(CameraImageControl::FacePriorityAutoExposure),
        sample_at_ms: Some(unix_ms()),
        error: enabled
            .is_none()
            .then(|| "camera returned invalid face-priority AE status".to_owned()),
    }
}

fn mock_image_control(
    control: CameraImageControl,
    value: i32,
    minimum: i32,
    maximum: i32,
    step: i32,
    default_value: i32,
) -> CameraImageControlState {
    CameraImageControlState {
        control,
        kind: image_control_kind(control),
        available: true,
        active: true,
        read_only: false,
        value: Some(value),
        minimum: Some(minimum),
        maximum: Some(maximum),
        step: Some(step),
        default_value: Some(default_value),
        options: image_control_options(control),
        sample_at_ms: Some(unix_ms()),
        error: None,
    }
}

fn mock_image_controls() -> Vec<CameraImageControlState> {
    let mut controls = vec![
        mock_image_control(CameraImageControl::Brightness, 50, 0, 100, 1, 50),
        mock_image_control(CameraImageControl::Contrast, 50, 0, 100, 1, 50),
        mock_image_control(CameraImageControl::Saturation, 50, 0, 100, 1, 50),
        mock_image_control(CameraImageControl::Hue, 50, 0, 100, 1, 50),
        mock_image_control(CameraImageControl::Gain, 1, 1, 32, 1, 1),
        mock_image_control(CameraImageControl::BacklightCompensation, 9, 0, 18, 1, 9),
        mock_image_control(CameraImageControl::PowerLineFrequency, 0, 0, 2, 1, 0),
        mock_image_control(CameraImageControl::WhiteBalanceAutomatic, 1, 0, 1, 1, 1),
        mock_image_control(
            CameraImageControl::WhiteBalanceTemperature,
            5000,
            2000,
            10000,
            100,
            5000,
        ),
        mock_image_control(CameraImageControl::RedBalance, 1024, 0, 2048, 1, 1024),
        mock_image_control(CameraImageControl::BlueBalance, 1024, 0, 2048, 1, 1024),
        mock_image_control(CameraImageControl::Sharpness, 50, 0, 100, 1, 50),
        mock_image_control(CameraImageControl::AutoExposure, 0, 0, 3, 1, 0),
        mock_image_control(
            CameraImageControl::ExposureTimeAbsolute,
            330,
            1,
            2500,
            1,
            330,
        ),
        mock_image_control(CameraImageControl::ExposureDynamicFramerate, 0, 0, 1, 1, 0),
        mock_image_control(CameraImageControl::FocusAbsolute, 25, 0, 100, 1, 25),
        mock_image_control(CameraImageControl::FocusAutomaticContinuous, 1, 0, 1, 1, 1),
        face_priority_auto_exposure_control(Some(false), true),
    ];
    update_mock_image_control_modes(&mut controls);
    controls
}

fn update_mock_image_control_modes(controls: &mut [CameraImageControlState]) {
    let value = |control| {
        controls
            .iter()
            .find(|state| state.control == control)
            .and_then(|state| state.value)
    };
    let exposure_manual = value(CameraImageControl::AutoExposure) == Some(1);
    let white_balance_automatic = value(CameraImageControl::WhiteBalanceAutomatic) == Some(1);
    let focus_automatic = value(CameraImageControl::FocusAutomaticContinuous) == Some(1);
    for state in controls {
        state.active = match state.control {
            CameraImageControl::ExposureTimeAbsolute | CameraImageControl::Gain => exposure_manual,
            CameraImageControl::FacePriorityAutoExposure => !exposure_manual,
            CameraImageControl::WhiteBalanceTemperature
            | CameraImageControl::RedBalance
            | CameraImageControl::BlueBalance => !white_balance_automatic,
            CameraImageControl::FocusAbsolute => !focus_automatic,
            _ => true,
        };
    }
}

fn spawn_mock(config: &CameraConfig) -> CameraHandle {
    let (tx, rx) = sync_channel::<Request>(COMMAND_QUEUE_CAPACITY);
    let powered_on = Arc::new(AtomicBool::new(true));
    let worker_powered_on = Arc::clone(&powered_on);
    std::thread::Builder::new()
        .name("tarsier-mock-camera".into())
        .spawn(move || {
            let mut image_controls = mock_image_controls();
            while let Ok(request) = rx.recv() {
                if matches!(request.command, Command::Shutdown) {
                    let _ = request.response.send(Ok(CommandOutcome::Applied));
                    break;
                }
                let result = match request.command {
                    Command::Shutdown => unreachable!(),
                    Command::Power { enabled } => {
                        worker_powered_on.store(enabled, Ordering::Relaxed);
                        Ok(CommandOutcome::Applied)
                    }
                    Command::McpActivity
                    | Command::Led { .. }
                    | Command::Move { .. }
                    | Command::Recenter
                    | Command::Tracking { .. }
                    | Command::Hdr { .. }
                    | Command::BuiltInGesture { .. }
                    | Command::Zoom { .. }
                    | Command::PanTiltSpeed { .. } => Ok(CommandOutcome::Applied),
                    Command::ImageControl { control, value } => {
                        let result = image_controls
                            .iter_mut()
                            .find(|state| state.control == control)
                            .ok_or_else(|| anyhow!("mock camera image control is unavailable"))
                            .and_then(|state| {
                                validate_image_control_value(state, value)?;
                                state.value = Some(value);
                                state.sample_at_ms = Some(unix_ms());
                                Ok(())
                            });
                        result.map(|()| {
                            update_mock_image_control_modes(&mut image_controls);
                            CommandOutcome::ImageControls(image_controls.clone())
                        })
                    }
                };
                let _ = request
                    .response
                    .send(result.map_err(|error| error.to_string()));
            }
        })
        .expect("failed to spawn mock camera thread");
    CameraHandle {
        tx,
        telemetry: Default::default(),
        powered_on,
        power_transition: Arc::new(AtomicBool::new(false)),
        max_yaw_degrees: config.max_yaw_degrees,
        max_pitch_degrees: config.max_pitch_degrees,
        controlled_zoom_bits: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
    }
}

fn zoom_units_from_magnification(magnification: f32, control: ZoomControl) -> Result<i32> {
    validate_zoom_control(control)?;
    let position = f64::from(magnification - 1.0) / 3.0;
    let raw = f64::from(control.minimum) + f64::from(control.maximum - control.minimum) * position;
    let step = f64::from(control.step);
    let snapped =
        f64::from(control.minimum) + ((raw - f64::from(control.minimum)) / step).round() * step;
    Ok((snapped as i32).clamp(control.minimum, control.maximum))
}

fn magnification_from_zoom_units(control: ZoomControl) -> Result<f32> {
    validate_zoom_control(control)?;
    let position =
        (control.value - control.minimum) as f32 / (control.maximum - control.minimum) as f32;
    Ok(1.0 + 3.0 * position)
}

fn magnification_from_zoom_percent(percent: u8) -> f32 {
    1.0 + 3.0 * f32::from(percent.min(100)) / 100.0
}

fn validate_zoom_control(control: ZoomControl) -> Result<()> {
    if control.maximum <= control.minimum || control.step <= 0 {
        bail!("camera absolute zoom range is invalid");
    }
    Ok(())
}

fn speed_units(control: ZoomControl, direction: i8, speed_fraction: f64) -> Result<i32> {
    if control.minimum >= 0
        || control.maximum <= 0
        || control.step <= 0
        || !(-1..=1).contains(&direction)
    {
        bail!("camera pan/tilt speed range is invalid");
    }
    if direction == 0 {
        return Ok(0);
    }

    let limit = if direction > 0 {
        control.maximum
    } else {
        control.minimum
    };
    if !speed_fraction.is_finite() || !(0.0..=1.0).contains(&speed_fraction) {
        bail!("camera pan/tilt speed fraction must be between 0 and 1");
    }
    let requested = f64::from(limit) * speed_fraction;
    let step = f64::from(control.step);
    let snapped = (requested / step).round() * step;
    let minimum_magnitude = control.step * i32::from(direction);
    let units = if snapped == 0.0 {
        minimum_magnitude
    } else {
        snapped as i32
    };
    Ok(units.clamp(control.minimum, control.maximum))
}

fn spawn_telemetry_updates(
    mut updates: tokio_mpsc::UnboundedReceiver<TelemetryUpdate>,
    runtime: Runtime,
    control_device: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(update) = updates.recv().await {
            // The adapter remains configured when its USB device disappears.
            let available = tokio::fs::try_exists(&control_device)
                .await
                .unwrap_or(false);
            runtime
                .update(|state| state.camera.available = available)
                .await;
            match update {
                TelemetryUpdate::Gimbal(sample) => {
                    runtime
                        .update(|state| {
                            state.camera.yaw_degrees = Some(sample.motor.yaw);
                            state.camera.pitch_degrees = Some(sample.motor.pitch);
                            state.camera.roll_degrees = Some(sample.motor.roll);
                            state.camera.euler_yaw_degrees = Some(sample.euler.yaw);
                            state.camera.euler_pitch_degrees = Some(sample.euler.pitch);
                            state.camera.euler_roll_degrees = Some(sample.euler.roll);
                            state.camera.yaw_velocity_degrees_per_second =
                                Some(sample.velocity.yaw);
                            state.camera.pitch_velocity_degrees_per_second =
                                Some(sample.velocity.pitch);
                            state.camera.roll_velocity_degrees_per_second =
                                Some(sample.velocity.roll);
                            state.camera.attitude_source = CameraAttitudeSource::Measured;
                            state.camera.sample_at_ms = Some(unix_ms());
                            state.camera.telemetry_error = None;
                        })
                        .await;
                }
                TelemetryUpdate::Gestures(status) => {
                    runtime
                        .update(|state| {
                            state.camera.built_in_gestures.target_selection =
                                Some(status.target_selection);
                            state.camera.built_in_gestures.zoom = Some(status.zoom);
                            state.camera.built_in_gestures.dynamic_zoom = Some(status.dynamic_zoom);
                            state.camera.built_in_gestures.sample_at_ms = Some(unix_ms());
                            state.camera.built_in_gestures.error = None;
                        })
                        .await;
                }
                TelemetryUpdate::Status(status) => {
                    runtime
                        .update(|state| {
                            state.camera.tracking_error = None;
                            state.camera.zoom_error = None;
                            state.camera.hdr_error = None;
                            if let Some(tracking) = status.tracking {
                                state.camera.tracking = Some(tracking);
                                state.camera.tracking_sample_at_ms = Some(unix_ms());
                            }
                            if let Some(zoom_percent) = status.zoom_percent {
                                state.camera.zoom_magnification =
                                    Some(magnification_from_zoom_percent(zoom_percent));
                                state.camera.zoom_sample_at_ms = Some(unix_ms());
                            }
                            if let Some(hdr) = status.hdr {
                                state.camera.hdr = Some(hdr);
                                state.camera.hdr_sample_at_ms = Some(unix_ms());
                            }
                            if let Some(enabled) = status.face_priority_auto_exposure {
                                let active = matches!(
                                    state
                                        .camera
                                        .image_settings
                                        .value(CameraImageControl::AutoExposure),
                                    Some(value) if value != 1
                                );
                                state.camera.image_settings.upsert(
                                    face_priority_auto_exposure_control(Some(enabled), active),
                                );
                            }
                        })
                        .await;
                }
                TelemetryUpdate::ImageSettings(controls) => {
                    runtime
                        .update(|state| {
                            let face_priority = state
                                .camera
                                .image_settings
                                .value(CameraImageControl::FacePriorityAutoExposure)
                                .map(|value| value != 0);
                            state.camera.image_settings.controls = controls;
                            let active = matches!(
                                state
                                    .camera
                                    .image_settings
                                    .value(CameraImageControl::AutoExposure),
                                Some(value) if value != 1
                            );
                            if state.camera.capabilities.tracking {
                                state.camera.image_settings.upsert(
                                    face_priority_auto_exposure_control(face_priority, active),
                                );
                            }
                            state.camera.image_settings.error = None;
                        })
                        .await;
                }
                TelemetryUpdate::Failure {
                    kind,
                    error,
                    retry_after,
                } => {
                    tracing::warn!(
                        telemetry = ?kind,
                        retry_after_ms = retry_after.as_millis(),
                        %error,
                        "camera telemetry read failed; backing off this signal"
                    );
                    runtime
                        .update(|state| match kind {
                            TelemetryKind::Gimbal => state.camera.telemetry_error = Some(error),
                            TelemetryKind::Gestures => {
                                state.camera.built_in_gestures.error = Some(error)
                            }
                            TelemetryKind::Status => {
                                state.camera.tracking_error = Some(error.clone());
                                state.camera.zoom_error = Some(error.clone());
                                state.camera.hdr_error = Some(error);
                            }
                            TelemetryKind::ImageSettings => {
                                state.camera.image_settings.error = Some(error)
                            }
                        })
                        .await;
                }
            }
        }
    })
}

fn adapter_name(adapter: CameraAdapter) -> &'static str {
    match adapter {
        CameraAdapter::ObsbotTiny2 => "obsbot-tiny-2",
        CameraAdapter::V4l2 => "v4l2",
        CameraAdapter::Mock => "mock",
        CameraAdapter::Disabled => "disabled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires TARSIER_TEST_CAMERA pointing to a local V4L2 webcam"]
    async fn standard_camera_hardware_readback() {
        let runtime = Runtime::new();
        let config = CameraConfig {
            adapter: CameraAdapter::V4l2,
            control_device: std::env::var("TARSIER_TEST_CAMERA").unwrap(),
            ..Default::default()
        };
        let camera = start(config, runtime.clone()).await.unwrap().unwrap();
        let state = runtime.state().await.camera;
        assert!(state.available);
        assert!(state.capabilities.image_settings);
        assert!(!state.capabilities.power);
        assert!(!state.capabilities.pan_tilt);
        let brightness = state.image_settings.value(CameraImageControl::Brightness).unwrap();
        // Write the already selected value, leaving the user's image unchanged.
        let result = camera.set_image_control(CameraImageControl::Brightness, brightness).await;
        camera.shutdown().await.unwrap();
        let controls = result.unwrap();
        assert!(controls.iter().any(|c| c.control == CameraImageControl::Brightness && c.value == Some(brightness)));
    }

    #[test]
    fn standard_camera_exposure_never_uses_vendor_commands() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);
        worker.standard_only = true;
        worker
            .execute(Command::ImageControl {
                control: CameraImageControl::AutoExposure,
                value: 1,
            })
            .unwrap();
        assert_eq!(
            worker.transport.image_control_values,
            vec![(CameraImageControl::AutoExposure, 1)]
        );
        for command in [
            Command::Recenter,
            Command::Power { enabled: false },
            Command::Hdr { enabled: true },
            Command::Tracking { enabled: true },
            Command::ImageControl {
                control: CameraImageControl::FacePriorityAutoExposure,
                value: 1,
            },
            Command::Zoom { magnification: 2.0 },
        ] {
            assert!(worker.execute(command).is_err());
        }
        assert!(worker.transport.writes.is_empty());
    }

    #[test]
    fn physical_power_changes_are_polled_passively_while_asleep() {
        let (_tx, rx) = sync_channel(1);
        let (updates, mut received) = tokio_mpsc::unbounded_channel();
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO)
            .with_telemetry(Some(Duration::from_secs(1)), updates);
        worker.pan_tilt_direction = (1, 0);
        worker.pan_tilt_deadline = Some(Instant::now() + PAN_TILT_LEASE);
        worker.transport.device_status = 3;
        worker.query_status().unwrap();
        assert!(!worker.powered_on.load(Ordering::Relaxed));
        assert_eq!(worker.pan_tilt_direction, (0, 0));
        assert!(worker.pan_tilt_deadline.is_none());
        assert!(worker.execute(Command::Recenter).is_err());

        for status in [3, 0, 4, 1] {
            worker.transport.device_status = status;
            let poller = worker.telemetry.as_mut().unwrap();
            poller.gimbal.next_due = Instant::now();
            poller.status.next_due = Instant::now();
            assert!(worker.poll_telemetry_if_due());
            assert!(matches!(
                received.try_recv().unwrap(),
                TelemetryUpdate::Status(_)
            ));
            assert_eq!(worker.powered_on.load(Ordering::Relaxed), status == 1);
        }
        assert!(worker.transport.writes.is_empty());
        assert!(worker.transport.pan_tilt_speed_units.is_empty());
    }

    #[test]
    fn led_initializes_once_and_reapplies_after_sleep() {
        let (_tx, rx) = sync_channel(1);
        let mut worker =
            Worker::new(RecordingTransport::default(), rx, Duration::ZERO).with_led_control();
        worker.update_led();
        assert_eq!(worker.transport.writes.len(), 2);
        assert_eq!(worker.transport.writes[0].0, TRACKING_SELECTOR);
        assert_eq!(&worker.transport.writes[0].1[..3], &[0x18, 1, 0]);
        assert_eq!(&worker.transport.writes[1].1[..3], &[0x1a, 1, 0]);
        assert!(worker.transport.writes[1].1[3..].iter().all(|b| *b == 0));
        worker.update_led();
        assert_eq!(worker.transport.writes.len(), 2);
        worker
            .execute(Command::Led {
                mode: LedMode::Steady,
            })
            .unwrap();
        worker.update_led();
        assert_eq!(
            &worker.transport.writes.last().unwrap().1[..3],
            &[0x1a, 1, 3]
        );
        worker.set_powered_on(false).unwrap();
        let count = worker.transport.writes.len();
        worker.update_led();
        assert_eq!(worker.transport.writes.len(), count);
        worker.set_powered_on(true).unwrap();
        worker.update_led();
        assert_eq!(
            &worker.transport.writes.last().unwrap().1[..3],
            &[0x1a, 1, 3]
        );
        worker.extinguish_led();
        assert_eq!(
            &worker.transport.writes.last().unwrap().1[..3],
            &[0x1a, 1, 0]
        );
    }

    #[test]
    fn led_failure_backoff_suppresses_io_until_retry() {
        let (_tx, rx) = sync_channel(1);
        let mut worker =
            Worker::new(RecordingTransport::default(), rx, Duration::ZERO).with_led_control();
        worker.transport.fail_led_once = true;
        worker.update_led();
        assert!(worker.led.as_ref().unwrap().retry_at > Instant::now());
        worker.update_led();
        assert!(worker.transport.writes.is_empty());
        worker.led.as_mut().unwrap().retry_at = Instant::now();
        worker.update_led();
        assert_eq!(worker.transport.writes.len(), 2);
    }

    struct ReplyingTransport {
        request: Option<[u8; FRAME_SIZE]>,
        operations: Vec<&'static str>,
    }

    #[tokio::test]
    async fn retiring_a_camera_rejects_commands_from_old_handles() {
        let config = CameraConfig {
            adapter: CameraAdapter::Mock,
            ..CameraConfig::default()
        };
        let camera = start(config, Runtime::new()).await.unwrap().unwrap();
        let old = camera.clone();
        camera.shutdown().await.unwrap();
        assert!(old.recenter().await.is_err());
    }

    #[derive(Default)]
    struct RecordingTransport {
        fail_led_once: bool,
        device_status: u8,
        writes: Vec<(u8, [u8; FRAME_SIZE])>,
        zoom_units: Vec<i32>,
        pan_tilt_speed_units: Vec<(i32, i32)>,
        image_control_values: Vec<(CameraImageControl, i32)>,
        face_priority_auto_exposure: bool,
    }

    impl RecordingTransport {
        fn recorded_image_controls(&self) -> Vec<CameraImageControlState> {
            let mut controls = mock_image_controls();
            for (control, value) in &self.image_control_values {
                if let Some(state) = controls.iter_mut().find(|state| state.control == *control) {
                    state.value = Some(*value);
                }
            }
            if let Some(state) = controls
                .iter_mut()
                .find(|state| state.control == CameraImageControl::FacePriorityAutoExposure)
            {
                state.value = Some(i32::from(self.face_priority_auto_exposure));
            }
            update_mock_image_control_modes(&mut controls);
            controls
        }
    }

    impl XuTransport for RecordingTransport {
        fn set(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
            if selector == TRACKING_SELECTOR && self.fail_led_once {
                self.fail_led_once = false;
                bail!("injected LED transport failure");
            }
            if selector == TRACKING_SELECTOR && data[..2] == [0x03, 0x01] {
                self.face_priority_auto_exposure = data[2] != 0;
            }
            self.writes.push((selector, data.try_into().unwrap()));
            Ok(())
        }

        fn get(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
            assert_eq!(selector, TRACKING_SELECTOR);
            data[0x09] = self.device_status;
            data[0x04] = 23;
            data[0x06] = 1;
            data[0x07] = u8::from(self.face_priority_auto_exposure);
            data[0x18] = 2;
            data[0x1c] = 0;
            Ok(())
        }

        fn zoom_control(&mut self) -> Result<ZoomControl> {
            Ok(ZoomControl {
                minimum: 0,
                maximum: 100,
                step: 1,
                value: self.zoom_units.last().copied().unwrap_or(0),
            })
        }

        fn set_zoom_units(&mut self, units: i32) -> Result<()> {
            self.zoom_units.push(units);
            Ok(())
        }

        fn pan_tilt_speed_controls(&mut self) -> Result<linux_uvc::PanTiltSpeedControls> {
            Ok(linux_uvc::PanTiltSpeedControls {
                pan: ZoomControl {
                    minimum: -160,
                    maximum: 160,
                    step: 1,
                    value: 0,
                },
                tilt: ZoomControl {
                    minimum: -120,
                    maximum: 120,
                    step: 1,
                    value: 0,
                },
            })
        }

        fn set_pan_tilt_speed_units(&mut self, pan: i32, tilt: i32) -> Result<()> {
            self.pan_tilt_speed_units.push((pan, tilt));
            Ok(())
        }

        fn image_controls(&mut self) -> Vec<CameraImageControlState> {
            self.recorded_image_controls()
        }

        fn image_control(&mut self, control: CameraImageControl) -> CameraImageControlState {
            self.recorded_image_controls()
                .into_iter()
                .find(|state| state.control == control)
                .unwrap()
        }

        fn set_image_control(
            &mut self,
            control: CameraImageControl,
            value: i32,
        ) -> Result<CameraImageControlState> {
            let state = self.image_control(control);
            validate_image_control_value(&state, value)?;
            self.image_control_values.push((control, value));
            Ok(self.image_control(control))
        }
    }

    impl XuTransport for ReplyingTransport {
        fn set(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
            assert_eq!(selector, VENDOR_SELECTOR);
            self.request = Some(data.try_into().unwrap());
            self.operations.push("set");
            Ok(())
        }

        fn get(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
            assert_eq!(selector, VENDOR_SELECTOR);
            let request = protocol::parse_frame(self.request.as_ref().unwrap()).unwrap();
            let payload = match request.command {
                AI_GET_GIM_STATE => vec![
                    29, 0, 38, 0, 0xb3, 0xf9, 0, 0, 76, 0, 0x85, 0x05, 0xff, 0xff, 1, 0, 8, 0,
                ],
                AI_GET_QUICK_STATUS => vec![0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0],
                command => panic!("unexpected query command 0x{command:04x}"),
            };
            let reply =
                protocol::build_frame(request.sequence, request.command, 0x0a, 0x29, &payload)
                    .unwrap();
            data.copy_from_slice(&reply);
            self.operations.push("get");
            Ok(())
        }
    }

    #[tokio::test]
    async fn telemetry_tracks_device_removal_and_return() {
        let runtime = Runtime::new();
        runtime.update(|state| state.camera.available = true).await;
        let device = std::env::temp_dir().join(format!(
            "tarsier-device-presence-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        for present in [false, true, false] {
            if present {
                std::fs::write(&device, b"").unwrap();
            } else if device.exists() {
                std::fs::remove_file(&device).unwrap();
            }
            let (tx, rx) = tokio_mpsc::unbounded_channel();
            let task =
                spawn_telemetry_updates(rx, runtime.clone(), device.to_str().unwrap().to_owned());
            tx.send(TelemetryUpdate::Failure {
                kind: TelemetryKind::Status,
                error: "readback unavailable".into(),
                retry_after: Duration::from_secs(1),
            })
            .unwrap();
            drop(tx);
            task.await.unwrap();
            assert_eq!(runtime.state().await.camera.available, present);
            assert_eq!(
                runtime.state().await.camera.zoom_error.as_deref(),
                Some("readback unavailable")
            );
        }
    }

    #[tokio::test]
    async fn rejects_moves_outside_safe_limits() {
        let runtime = Runtime::new();
        let handle = start(
            CameraConfig {
                adapter: CameraAdapter::Mock,
                ..CameraConfig::default()
            },
            runtime,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(handle.move_to(131.0, 0.0, 0.0).await.is_err());
        assert!(handle.move_to(0.0, 91.0, 0.0).await.is_err());
        assert!(handle.move_to(20.0, -10.0, 0.0).await.is_ok());
        assert!(handle.set_zoom(0.9).await.is_err());
        assert!(handle.set_zoom(4.1).await.is_err());
        assert_eq!(handle.controlled_zoom_magnification(), Some(1.0));
        assert!(handle.set_zoom(2.5).await.is_ok());
        assert_eq!(handle.controlled_zoom_magnification(), Some(2.5));
    }

    #[test]
    fn query_uses_one_serialized_request_and_matching_reply() {
        let (_tx, rx) = sync_channel(1);
        let transport = ReplyingTransport {
            request: None,
            operations: Vec::new(),
        };
        let mut worker = Worker::new(transport, rx, Duration::ZERO);
        let sample = worker.query_gimbal().unwrap();
        assert_eq!(worker.transport.operations, ["set", "get"]);
        assert_eq!(sample.motor.yaw, 141.3);
        assert_eq!(sample.motor.pitch, 7.6);
        assert_eq!(sample.velocity.yaw, 0.8);
        let request = protocol::parse_frame(worker.transport.request.as_ref().unwrap()).unwrap();
        assert_eq!(request.command, AI_GET_GIM_STATE);
    }

    #[test]
    fn gesture_status_query_uses_the_same_serialized_mailbox() {
        let (_tx, rx) = sync_channel(1);
        let transport = ReplyingTransport {
            request: None,
            operations: Vec::new(),
        };
        let mut worker = Worker::new(transport, rx, Duration::ZERO);
        let status = worker.query_ai_status().unwrap();
        assert_eq!(worker.transport.operations, ["set", "get"]);
        assert_eq!(
            status,
            AiGestureStatus {
                target_selection: true,
                zoom: false,
                dynamic_zoom: true,
            }
        );
        let request = protocol::parse_frame(worker.transport.request.as_ref().unwrap()).unwrap();
        assert_eq!(request.command, AI_GET_QUICK_STATUS);
    }

    #[test]
    fn failed_telemetry_reads_back_off_independently() {
        let now = Instant::now();
        let mut schedule = PollSchedule::new(Duration::from_secs(1), now);
        assert!(!schedule.due(now));
        assert!(schedule.due(now + Duration::from_secs(1)));

        let retry = schedule.failed(now + Duration::from_secs(1));
        assert_eq!(retry, Duration::from_secs(2));
        assert!(!schedule.due(now + Duration::from_secs(2)));
        assert!(schedule.due(now + Duration::from_secs(3)));

        schedule.succeeded(now + Duration::from_secs(3));
        assert_eq!(schedule.consecutive_failures, 0);
        assert!(schedule.due(now + Duration::from_secs(4)));
    }

    #[test]
    fn selector_six_status_reports_tracking_and_ai_zoom() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);
        let status = worker.query_status().unwrap();
        assert_eq!(status.tracking, Some(true));
        assert_eq!(status.zoom_percent, Some(23));
        assert_eq!(status.hdr, Some(true));
        assert_eq!(status.face_priority_auto_exposure, Some(false));
        assert_eq!(
            magnification_from_zoom_percent(status.zoom_percent.unwrap()),
            1.69
        );
    }

    #[test]
    fn hdr_uses_selector_six_and_enforces_the_sdk_switch_interval() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);

        worker.set_hdr(true).unwrap();
        assert_eq!(worker.transport.writes.len(), 1);
        assert_eq!(worker.transport.writes[0].0, TRACKING_SELECTOR);
        assert_eq!(&worker.transport.writes[0].1[..3], &[0x01, 0x01, 0x01]);

        worker.set_hdr(true).unwrap();
        assert_eq!(worker.transport.writes.len(), 1);
        assert!(worker.set_hdr(false).is_err());

        worker.last_hdr_switch = Some(Instant::now() - HDR_SWITCH_MINIMUM_INTERVAL);
        worker.set_hdr(false).unwrap();
        assert_eq!(&worker.transport.writes[1].1[..3], &[0x01, 0x01, 0x00]);
    }

    #[test]
    fn image_control_writes_are_serialized_and_return_dependent_readback() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);

        let controls = worker
            .set_image_control(CameraImageControl::AutoExposure, 1)
            .unwrap();
        assert_eq!(
            worker.transport.image_control_values,
            [(CameraImageControl::AutoExposure, 1)]
        );
        assert_eq!(
            controls
                .iter()
                .find(|state| state.control == CameraImageControl::ExposureTimeAbsolute)
                .map(|state| state.active),
            Some(true)
        );
        assert_eq!(
            controls
                .iter()
                .find(|state| state.control == CameraImageControl::FacePriorityAutoExposure)
                .map(|state| state.active),
            Some(false)
        );
    }

    #[test]
    fn face_priority_auto_exposure_uses_selector_six_and_checks_readback() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);

        let controls = worker
            .set_image_control(CameraImageControl::FacePriorityAutoExposure, 1)
            .unwrap();
        assert_eq!(worker.transport.writes[0].0, TRACKING_SELECTOR);
        assert_eq!(&worker.transport.writes[0].1[..3], &[0x03, 0x01, 0x01]);
        assert_eq!(
            controls
                .iter()
                .find(|state| state.control == CameraImageControl::FacePriorityAutoExposure)
                .and_then(|state| state.value),
            Some(1)
        );
    }

    #[test]
    fn built_in_gesture_command_is_serialized_after_wake() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);

        worker
            .execute(Command::BuiltInGesture {
                feature: BuiltInGesture::Zoom,
                enabled: false,
            })
            .unwrap();

        assert_eq!(worker.transport.writes.len(), 2);
        assert_eq!(worker.transport.writes[0].0, VENDOR_SELECTOR);
        assert_eq!(worker.transport.writes[1].0, VENDOR_SELECTOR);
        let command = protocol::parse_frame(&worker.transport.writes[1].1).unwrap();
        assert_eq!(command.command, protocol::AI_SET_GESTURE_ZOOM);
        assert_eq!(command.payload, [0]);
    }

    #[test]
    fn power_command_sleeps_and_wakes_without_accepting_controls_while_off() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);

        worker.execute(Command::Power { enabled: false }).unwrap();
        assert!(!worker.powered_on.load(Ordering::Relaxed));
        let sleep = protocol::parse_frame(&worker.transport.writes[0].1).unwrap();
        assert_eq!(sleep.command, protocol::CAM_SET_DEV_STATUS);
        assert_eq!(sleep.payload, [1, 0, 0, 0]);
        assert!(worker.execute(Command::Recenter).is_err());

        worker.execute(Command::Power { enabled: true }).unwrap();
        assert!(worker.powered_on.load(Ordering::Relaxed));
        let wake = protocol::parse_frame(&worker.transport.writes[1].1).unwrap();
        assert_eq!(wake.payload, [0, 0, 0, 0]);
    }

    #[test]
    fn zoom_magnification_maps_to_the_standard_control_range() {
        let control = ZoomControl {
            minimum: 10,
            maximum: 110,
            step: 2,
            value: 10,
        };
        assert_eq!(zoom_units_from_magnification(1.0, control).unwrap(), 10);
        assert_eq!(zoom_units_from_magnification(2.5, control).unwrap(), 60);
        assert_eq!(zoom_units_from_magnification(4.0, control).unwrap(), 110);

        assert_eq!(magnification_from_zoom_units(control).unwrap(), 1.0);
        assert_eq!(
            magnification_from_zoom_units(ZoomControl {
                value: 60,
                ..control
            })
            .unwrap(),
            2.5
        );
        assert_eq!(magnification_from_zoom_percent(0), 1.0);
        assert_eq!(magnification_from_zoom_percent(23), 1.69);
        assert_eq!(magnification_from_zoom_percent(100), 4.0);
    }

    #[test]
    fn zoom_command_uses_the_serialized_standard_control() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);

        worker
            .execute(Command::Zoom { magnification: 2.5 })
            .unwrap();

        assert!(worker.transport.writes.is_empty());
        assert_eq!(worker.transport.zoom_units, [50]);
    }

    #[test]
    fn pan_tilt_speed_starts_and_stops_without_pulsing() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);

        worker
            .execute(Command::PanTiltSpeed {
                pan_direction: 1,
                tilt_direction: 0,
                speed_fraction: NUDGE_SPEED_FRACTION,
            })
            .unwrap();
        assert_eq!(worker.transport.pan_tilt_speed_units, [(40, 0)]);

        worker
            .execute(Command::PanTiltSpeed {
                pan_direction: 0,
                tilt_direction: 0,
                speed_fraction: NUDGE_SPEED_FRACTION,
            })
            .unwrap();

        assert_eq!(worker.transport.pan_tilt_speed_units, [(40, 0), (0, 0)]);
    }

    #[test]
    fn face_tracking_applies_and_updates_a_proportional_diagonal_speed() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);

        worker
            .execute(Command::PanTiltSpeed {
                pan_direction: 1,
                tilt_direction: -1,
                speed_fraction: 0.025,
            })
            .unwrap();
        worker
            .execute(Command::PanTiltSpeed {
                pan_direction: 1,
                tilt_direction: -1,
                speed_fraction: 0.05,
            })
            .unwrap();

        assert_eq!(worker.transport.pan_tilt_speed_units, [(4, -3), (8, -6)]);
        assert_eq!(worker.pan_tilt_direction, (1, -1));
        assert_eq!(worker.pan_tilt_speed_fraction, 0.05);
    }

    #[test]
    fn pan_tilt_lease_renews_without_rewriting_and_expires_to_stop() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);

        worker
            .execute(Command::PanTiltSpeed {
                pan_direction: -1,
                tilt_direction: 0,
                speed_fraction: NUDGE_SPEED_FRACTION,
            })
            .unwrap();
        let initial_deadline = worker.pan_tilt_deadline.unwrap();

        worker
            .execute(Command::PanTiltSpeed {
                pan_direction: -1,
                tilt_direction: 0,
                speed_fraction: NUDGE_SPEED_FRACTION,
            })
            .unwrap();
        assert_eq!(worker.transport.pan_tilt_speed_units, [(-40, 0)]);
        assert!(worker.pan_tilt_deadline.unwrap() >= initial_deadline);

        worker.pan_tilt_deadline = Some(Instant::now());
        worker.expire_pan_tilt_lease();

        assert_eq!(worker.transport.pan_tilt_speed_units, [(-40, 0), (0, 0)]);
        assert_eq!(worker.pan_tilt_direction, (0, 0));
        assert_eq!(worker.pan_tilt_deadline, None);
    }
}
