mod linux_uvc;
mod protocol;

use std::{
    sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::{
    config::{CameraAdapter, CameraConfig},
    model::{BuiltInGesture, CameraAttitudeSource, unix_ms},
    runtime::Runtime,
};
use linux_uvc::{LinuxUvcTransport, XuTransport, ZoomControl};
use protocol::{
    AI_GET_GIM_STATE, AI_GET_QUICK_STATUS, AiGestureStatus, AiGimbalState, CameraStatus,
    FRAME_SIZE, TRACKING_SELECTOR, VENDOR_SELECTOR,
};

const COMMAND_QUEUE_CAPACITY: usize = 32;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const GESTURE_STATUS_MINIMUM_INTERVAL: Duration = Duration::from_secs(5);
const TELEMETRY_MAXIMUM_BACKOFF: Duration = Duration::from_secs(60);
const HDR_SWITCH_MINIMUM_INTERVAL: Duration = Duration::from_secs(3);
const NUDGE_SPEED_FRACTION: f64 = 0.25;
pub const PAN_TILT_LEASE: Duration = Duration::from_millis(350);

#[derive(Debug)]
enum Command {
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
}

struct Request {
    command: Command,
    response: oneshot::Sender<Result<(), String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TelemetryKind {
    Gimbal,
    Gestures,
    Status,
}

#[derive(Debug)]
enum TelemetryUpdate {
    Gimbal(AiGimbalState),
    Gestures(AiGestureStatus),
    Status(CameraStatus),
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
        Self {
            tx,
            gimbal: PollSchedule::new(interval, now),
            gestures: PollSchedule::new(gesture_interval, now),
            status: PollSchedule::new(interval, now),
        }
    }

    fn next_kind(&self, now: Instant) -> Option<TelemetryKind> {
        if self.gimbal.due(now) {
            Some(TelemetryKind::Gimbal)
        } else if self.status.due(now) {
            Some(TelemetryKind::Status)
        } else if self.gestures.due(now) {
            Some(TelemetryKind::Gestures)
        } else {
            None
        }
    }

    fn schedule_mut(&mut self, kind: TelemetryKind) -> &mut PollSchedule {
        match kind {
            TelemetryKind::Gimbal => &mut self.gimbal,
            TelemetryKind::Gestures => &mut self.gestures,
            TelemetryKind::Status => &mut self.status,
        }
    }
}

#[derive(Clone)]
pub struct CameraHandle {
    tx: SyncSender<Request>,
    max_yaw_degrees: f32,
    max_pitch_degrees: f32,
}

impl CameraHandle {
    pub async fn move_to(&self, yaw: f32, pitch: f32, roll: f32) -> Result<()> {
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
        self.request(Command::Hdr { enabled }).await
    }

    pub async fn set_built_in_gesture(&self, feature: BuiltInGesture, enabled: bool) -> Result<()> {
        self.request(Command::BuiltInGesture { feature, enabled })
            .await
            .map(|_| ())
    }

    pub async fn set_zoom(&self, magnification: f32) -> Result<()> {
        if !magnification.is_finite() || !(1.0..=4.0).contains(&magnification) {
            bail!("zoom magnification must be between 1.0 and 4.0");
        }
        self.request(Command::Zoom { magnification })
            .await
            .map(|_| ())
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

    async fn request(&self, command: Command) -> Result<()> {
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
    runtime
        .update(|state| state.camera.adapter = adapter_name(config.adapter).into())
        .await;
    match config.adapter {
        CameraAdapter::Disabled => Ok(None),
        CameraAdapter::Mock => {
            let handle = spawn_mock(&config);
            let now = unix_ms();
            runtime
                .update(|state| {
                    state.camera.available = true;
                    state.camera.yaw_degrees = Some(0.0);
                    state.camera.pitch_degrees = Some(0.0);
                    state.camera.roll_degrees = Some(0.0);
                    state.camera.tracking = Some(false);
                    state.camera.tracking_sample_at_ms = Some(now);
                    state.camera.zoom_magnification = Some(1.0);
                    state.camera.hdr = Some(false);
                    state.camera.hdr_sample_at_ms = Some(now);
                    state.camera.attitude_source = CameraAttitudeSource::Simulated;
                    state.camera.sample_at_ms = Some(now);
                })
                .await;
            Ok(Some(handle))
        }
        CameraAdapter::ObsbotTiny2 => {
            let mut transport = LinuxUvcTransport::open(&config.control_device, config.xu_unit)?;
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
            let (handle, telemetry_rx) = spawn_worker(transport, &config);
            let zoom_sample_at_ms = initial_zoom.map(|_| unix_ms());
            runtime
                .update(|state| {
                    state.camera.available = true;
                    state.camera.zoom_magnification = initial_zoom;
                    state.camera.zoom_sample_at_ms = zoom_sample_at_ms;
                    state.camera.error = None;
                })
                .await;
            if config.poll_interval_ms > 0 {
                tracing::info!(
                    interval_ms = config.poll_interval_ms,
                    "low-priority AI telemetry polling is enabled"
                );
            }
            spawn_telemetry_updates(telemetry_rx, runtime);
            Ok(Some(handle))
        }
    }
}

fn spawn_worker<T: XuTransport + 'static>(
    transport: T,
    config: &CameraConfig,
) -> (CameraHandle, tokio_mpsc::UnboundedReceiver<TelemetryUpdate>) {
    let (tx, rx) = sync_channel(COMMAND_QUEUE_CAPACITY);
    let (telemetry_tx, telemetry_rx) = tokio_mpsc::unbounded_channel();
    let interval = Duration::from_millis(config.minimum_command_interval_ms);
    let poll_interval =
        (config.poll_interval_ms > 0).then(|| Duration::from_millis(config.poll_interval_ms));
    std::thread::Builder::new()
        .name("tarsier-camera-owner".into())
        .spawn(move || {
            Worker::new(transport, rx, interval)
                .with_telemetry(poll_interval, telemetry_tx)
                .run()
        })
        .expect("failed to spawn camera owner thread");
    (
        CameraHandle {
            tx,
            max_yaw_degrees: config.max_yaw_degrees,
            max_pitch_degrees: config.max_pitch_degrees,
        },
        telemetry_rx,
    )
}

struct Worker<T> {
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
}

impl<T: XuTransport> Worker<T> {
    fn new(transport: T, rx: Receiver<Request>, minimum_interval: Duration) -> Self {
        Self {
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
        }
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
                    break;
                }
            }
        }
    }

    fn execute(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Move { yaw, pitch, roll } => {
                self.wake()?;
                let mut frame = protocol::move_frame(self.next_sequence(), yaw, pitch, roll);
                self.set(VENDOR_SELECTOR, &mut frame)
            }
            Command::Recenter => {
                self.wake()?;
                let mut frame = protocol::recenter_frame(self.next_sequence());
                self.set(VENDOR_SELECTOR, &mut frame)
            }
            Command::Tracking { enabled } => {
                self.wake()?;
                let mut payload = protocol::tracking_payload(enabled);
                self.set(TRACKING_SELECTOR, &mut payload)
            }
            Command::Hdr { enabled } => self.set_hdr(enabled),
            Command::BuiltInGesture { feature, enabled } => {
                self.wake()?;
                let mut frame =
                    protocol::built_in_gesture_frame(self.next_sequence(), feature, enabled);
                self.set(VENDOR_SELECTOR, &mut frame)
            }
            Command::Zoom { magnification } => {
                self.pace();
                let control = self.transport.zoom_control()?;
                let units = zoom_units_from_magnification(magnification, control)?;
                self.transport.set_zoom_units(units)?;
                self.last_io = Some(Instant::now());
                Ok(())
            }
            Command::PanTiltSpeed {
                pan_direction,
                tilt_direction,
                speed_fraction,
            } => self.set_pan_tilt_speed(pan_direction, tilt_direction, speed_fraction),
        }
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

    fn wake(&mut self) -> Result<()> {
        let mut frame = protocol::wake_frame(self.next_sequence());
        self.set(VENDOR_SELECTOR, &mut frame)?;
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    fn poll_telemetry_if_due(&mut self) -> bool {
        let now = Instant::now();
        let Some(kind) = self
            .telemetry
            .as_ref()
            .and_then(|poller| poller.next_kind(now))
        else {
            return false;
        };

        let result = match kind {
            TelemetryKind::Gimbal => self.query_gimbal().map(TelemetryUpdate::Gimbal),
            TelemetryKind::Gestures => self.query_ai_status().map(TelemetryUpdate::Gestures),
            TelemetryKind::Status => self.query_status().map(TelemetryUpdate::Status),
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

fn spawn_mock(config: &CameraConfig) -> CameraHandle {
    let (tx, rx) = sync_channel::<Request>(COMMAND_QUEUE_CAPACITY);
    std::thread::Builder::new()
        .name("tarsier-mock-camera".into())
        .spawn(move || {
            while let Ok(request) = rx.recv() {
                let result = match request.command {
                    Command::Move { .. }
                    | Command::Recenter
                    | Command::Tracking { .. }
                    | Command::Hdr { .. }
                    | Command::BuiltInGesture { .. }
                    | Command::Zoom { .. }
                    | Command::PanTiltSpeed { .. } => Ok(()),
                };
                let _ = request.response.send(result);
            }
        })
        .expect("failed to spawn mock camera thread");
    CameraHandle {
        tx,
        max_yaw_degrees: config.max_yaw_degrees,
        max_pitch_degrees: config.max_pitch_degrees,
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
) {
    tokio::spawn(async move {
        while let Some(update) = updates.recv().await {
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
                        })
                        .await;
                }
            }
        }
    });
}

fn adapter_name(adapter: CameraAdapter) -> &'static str {
    match adapter {
        CameraAdapter::ObsbotTiny2 => "obsbot-tiny-2",
        CameraAdapter::Mock => "mock",
        CameraAdapter::Disabled => "disabled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ReplyingTransport {
        request: Option<[u8; FRAME_SIZE]>,
        operations: Vec<&'static str>,
    }

    #[derive(Default)]
    struct RecordingTransport {
        writes: Vec<(u8, [u8; FRAME_SIZE])>,
        zoom_units: Vec<i32>,
        pan_tilt_speed_units: Vec<(i32, i32)>,
    }

    impl XuTransport for RecordingTransport {
        fn set(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
            self.writes.push((selector, data.try_into().unwrap()));
            Ok(())
        }

        fn get(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
            assert_eq!(selector, TRACKING_SELECTOR);
            data[0x04] = 23;
            data[0x06] = 1;
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
        assert!(handle.set_zoom(2.5).await.is_ok());
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
