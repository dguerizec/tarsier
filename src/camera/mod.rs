mod linux_uvc;
mod protocol;

use std::{
    sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::oneshot;

use crate::{
    config::{CameraAdapter, CameraConfig},
    model::{BuiltInGesture, CameraAttitudeSource, unix_ms},
    runtime::Runtime,
};
use linux_uvc::{LinuxUvcTransport, XuTransport, ZoomControl};
use protocol::{FRAME_SIZE, GIM_GET_STATE, TRACKING_SELECTOR, VENDOR_SELECTOR};

const COMMAND_QUEUE_CAPACITY: usize = 32;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const NUDGE_DURATION: Duration = Duration::from_millis(120);
const NUDGE_SPEED_FRACTION: f64 = 0.25;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct GimbalAngles {
    pub yaw_degrees: f32,
    pub pitch_degrees: f32,
    pub roll_degrees: f32,
}

#[derive(Debug)]
enum Command {
    QueryState,
    Move {
        yaw: f32,
        pitch: f32,
        roll: f32,
    },
    Recenter,
    Tracking {
        enabled: bool,
    },
    BuiltInGesture {
        feature: BuiltInGesture,
        enabled: bool,
    },
    Zoom {
        magnification: f32,
    },
    Nudge {
        pan_direction: i8,
        tilt_direction: i8,
    },
}

struct Request {
    command: Command,
    response: oneshot::Sender<Result<Option<GimbalAngles>, String>>,
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

    pub async fn nudge(&self, pan_direction: i8, tilt_direction: i8) -> Result<()> {
        if !(-1..=1).contains(&pan_direction)
            || !(-1..=1).contains(&tilt_direction)
            || (pan_direction == 0) == (tilt_direction == 0)
        {
            bail!("camera nudge must select exactly one pan or tilt direction");
        }
        self.request(Command::Nudge {
            pan_direction,
            tilt_direction,
        })
        .await
        .map(|_| ())
    }

    async fn query_state(&self) -> Result<GimbalAngles> {
        self.request(Command::QueryState)
            .await?
            .ok_or_else(|| anyhow!("camera returned no gimbal state"))
    }

    async fn request(&self, command: Command) -> Result<Option<GimbalAngles>> {
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
            runtime
                .update(|state| {
                    state.camera.available = true;
                    state.camera.yaw_degrees = Some(0.0);
                    state.camera.pitch_degrees = Some(0.0);
                    state.camera.roll_degrees = Some(0.0);
                    state.camera.zoom_magnification = Some(1.0);
                    state.camera.attitude_source = CameraAttitudeSource::Simulated;
                    state.camera.sample_at_ms = Some(unix_ms());
                })
                .await;
            if config.poll_interval_ms > 0 {
                spawn_polling(handle.clone(), config.poll_interval_ms, runtime);
            }
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
            let handle = spawn_worker(transport, &config);
            runtime
                .update(|state| {
                    state.camera.available = true;
                    state.camera.zoom_magnification = initial_zoom;
                    state.camera.error = None;
                })
                .await;
            if config.poll_interval_ms > 0 {
                tracing::warn!(
                    interval_ms = config.poll_interval_ms,
                    "experimental vendor attitude polling is enabled and may reset the camera during streaming"
                );
                spawn_polling(handle.clone(), config.poll_interval_ms, runtime);
            }
            Ok(Some(handle))
        }
    }
}

fn spawn_worker<T: XuTransport + 'static>(transport: T, config: &CameraConfig) -> CameraHandle {
    let (tx, rx) = sync_channel(COMMAND_QUEUE_CAPACITY);
    let interval = Duration::from_millis(config.minimum_command_interval_ms);
    std::thread::Builder::new()
        .name("tarsier-camera-owner".into())
        .spawn(move || Worker::new(transport, rx, interval).run())
        .expect("failed to spawn camera owner thread");
    CameraHandle {
        tx,
        max_yaw_degrees: config.max_yaw_degrees,
        max_pitch_degrees: config.max_pitch_degrees,
    }
}

struct Worker<T> {
    transport: T,
    rx: Receiver<Request>,
    sequence: u16,
    minimum_interval: Duration,
    last_io: Option<Instant>,
}

impl<T: XuTransport> Worker<T> {
    fn new(transport: T, rx: Receiver<Request>, minimum_interval: Duration) -> Self {
        Self {
            transport,
            rx,
            sequence: 0,
            minimum_interval,
            last_io: None,
        }
    }

    fn run(mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(request) => {
                    let result = self
                        .execute(request.command)
                        .map_err(|error| error.to_string());
                    let _ = request.response.send(result);
                }
                Err(TryRecvError::Empty) => std::thread::sleep(self.minimum_interval),
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn execute(&mut self, command: Command) -> Result<Option<GimbalAngles>> {
        match command {
            Command::QueryState => self.query_gimbal().map(Some),
            Command::Move { yaw, pitch, roll } => {
                self.wake()?;
                let mut frame = protocol::move_frame(self.next_sequence(), yaw, pitch, roll);
                self.set(VENDOR_SELECTOR, &mut frame)?;
                Ok(None)
            }
            Command::Recenter => {
                self.wake()?;
                let mut frame = protocol::recenter_frame(self.next_sequence());
                self.set(VENDOR_SELECTOR, &mut frame)?;
                Ok(None)
            }
            Command::Tracking { enabled } => {
                self.wake()?;
                let mut payload = protocol::tracking_payload(enabled);
                self.set(TRACKING_SELECTOR, &mut payload)?;
                Ok(None)
            }
            Command::BuiltInGesture { feature, enabled } => {
                self.wake()?;
                let mut frame =
                    protocol::built_in_gesture_frame(self.next_sequence(), feature, enabled);
                self.set(VENDOR_SELECTOR, &mut frame)?;
                Ok(None)
            }
            Command::Zoom { magnification } => {
                self.pace();
                let control = self.transport.zoom_control()?;
                let units = zoom_units_from_magnification(magnification, control)?;
                self.transport.set_zoom_units(units)?;
                self.last_io = Some(Instant::now());
                Ok(None)
            }
            Command::Nudge {
                pan_direction,
                tilt_direction,
            } => self.nudge(pan_direction, tilt_direction).map(|()| None),
        }
    }

    fn nudge(&mut self, pan_direction: i8, tilt_direction: i8) -> Result<()> {
        self.pace();
        let controls = self.transport.pan_tilt_speed_controls()?;
        let pan = speed_units(controls.pan, pan_direction)?;
        let tilt = speed_units(controls.tilt, tilt_direction)?;
        self.transport.set_pan_tilt_speed_units(pan, tilt)?;
        self.last_io = Some(Instant::now());

        std::thread::sleep(NUDGE_DURATION);
        self.pace();
        let stopped = self.transport.set_pan_tilt_speed_units(0, 0);
        self.last_io = Some(Instant::now());
        stopped.context("failed to stop pan/tilt after a camera nudge")
    }

    fn wake(&mut self) -> Result<()> {
        let mut frame = protocol::wake_frame(self.next_sequence());
        self.set(VENDOR_SELECTOR, &mut frame)?;
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    fn query_gimbal(&mut self) -> Result<GimbalAngles> {
        let sequence = self.next_sequence();
        let mut request = protocol::gimbal_query(sequence);
        self.set(VENDOR_SELECTOR, &mut request)?;
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        while Instant::now() < deadline {
            let mut reply = [0_u8; FRAME_SIZE];
            self.get(VENDOR_SELECTOR, &mut reply)?;
            let Ok(frame) = protocol::parse_frame(&reply) else {
                continue;
            };
            if frame.sequence != sequence || frame.command != GIM_GET_STATE {
                continue;
            }
            let (yaw, pitch, roll) = protocol::decode_gimbal_angles(&frame.payload)?;
            return Ok(GimbalAngles {
                yaw_degrees: yaw,
                pitch_degrees: pitch,
                roll_degrees: roll,
            });
        }
        bail!("timed out waiting for a matching gimbal state reply")
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
            let mut angles = GimbalAngles {
                yaw_degrees: 0.0,
                pitch_degrees: 0.0,
                roll_degrees: 0.0,
            };
            while let Ok(request) = rx.recv() {
                let result = match request.command {
                    Command::QueryState => Ok(Some(angles)),
                    Command::Move { yaw, pitch, roll } => {
                        angles = GimbalAngles {
                            yaw_degrees: yaw,
                            pitch_degrees: pitch,
                            roll_degrees: roll,
                        };
                        Ok(None)
                    }
                    Command::Recenter => {
                        angles = GimbalAngles {
                            yaw_degrees: 0.0,
                            pitch_degrees: 0.0,
                            roll_degrees: 0.0,
                        };
                        Ok(None)
                    }
                    Command::Tracking { .. } => Ok(None),
                    Command::BuiltInGesture { .. } => Ok(None),
                    Command::Zoom { .. } => Ok(None),
                    Command::Nudge { .. } => Ok(None),
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

fn validate_zoom_control(control: ZoomControl) -> Result<()> {
    if control.maximum <= control.minimum || control.step <= 0 {
        bail!("camera absolute zoom range is invalid");
    }
    Ok(())
}

fn speed_units(control: ZoomControl, direction: i8) -> Result<i32> {
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
    let requested = f64::from(limit) * NUDGE_SPEED_FRACTION;
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

fn spawn_polling(handle: CameraHandle, interval_ms: u64, runtime: Runtime) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
        let mut consecutive_failures = 0_u32;
        loop {
            interval.tick().await;
            match handle.query_state().await {
                Ok(angles) => {
                    consecutive_failures = 0;
                    runtime
                        .update(|state| {
                            state.camera.available = true;
                            state.camera.yaw_degrees = Some(angles.yaw_degrees);
                            state.camera.pitch_degrees = Some(angles.pitch_degrees);
                            state.camera.roll_degrees = Some(angles.roll_degrees);
                            state.camera.attitude_source = CameraAttitudeSource::Measured;
                            state.camera.sample_at_ms = Some(unix_ms());
                            state.camera.error = None;
                        })
                        .await;
                }
                Err(error) => {
                    consecutive_failures += 1;
                    runtime
                        .update(|state| {
                            state.camera.available = consecutive_failures < 3;
                            state.camera.error = Some(error.to_string());
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

        fn get(&mut self, _selector: u8, _data: &mut [u8]) -> Result<()> {
            unreachable!("recorded commands do not read from the camera")
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
            let payload = [0xd3, 0xfd, 0xff, 0xf3, 0x8f, 0xef];
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
        let angles = worker.query_gimbal().unwrap();
        assert_eq!(worker.transport.operations, ["set", "get"]);
        assert!((angles.yaw_degrees - -42.09).abs() < 0.01);
        assert!((angles.roll_degrees - -5.57).abs() < 0.01);
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
    fn nudge_uses_a_bounded_speed_pulse_and_stops() {
        let (_tx, rx) = sync_channel(1);
        let mut worker = Worker::new(RecordingTransport::default(), rx, Duration::ZERO);

        worker
            .execute(Command::Nudge {
                pan_direction: 1,
                tilt_direction: 0,
            })
            .unwrap();

        assert_eq!(worker.transport.pan_tilt_speed_units, [(40, 0), (0, 0)]);
    }
}
