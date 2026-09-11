use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeState {
    pub version: &'static str,
    pub started_at_ms: u64,
    pub camera: CameraState,
    pub pipeline: PipelineState,
    pub video_effects: VideoEffectsState,
    pub perception: PerceptionState,
    pub last_scenario: Option<ScenarioActivation>,
    #[serde(default)]
    pub audio_capture_sources: Vec<String>,
    #[serde(default)]
    pub audio_virtual: crate::audio::VirtualMicrophone,
    #[serde(default)]
    pub audio_voice: crate::voice::VoiceState,
    #[serde(default)]
    pub audio_gain: crate::audio_gain::GainStatus,
    #[serde(default)]
    pub audio_released_sources: Vec<String>,
    #[serde(default)]
    pub audio_busy_sources: Vec<String>,
    #[serde(default)]
    pub audio_output_applications: usize,
    #[serde(default)]
    pub audio_reservations: std::collections::BTreeMap<String, crate::audio::Reservation>,
    #[serde(default)]
    pub last_photo: Option<Value>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            started_at_ms: unix_ms(),
            camera: CameraState::default(),
            pipeline: PipelineState::default(),
            video_effects: VideoEffectsState::default(),
            perception: PerceptionState::default(),
            last_scenario: None,
            audio_capture_sources: Vec::new(),
            audio_virtual: Default::default(),
            audio_voice: Default::default(),
            audio_gain: Default::default(),
            audio_released_sources: Vec::new(),
            audio_busy_sources: Vec::new(),
            audio_output_applications: 0,
            audio_reservations: Default::default(),
            last_photo: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BackgroundEffect {
    #[default]
    GreenScreen,
    Blur,
    PixelParty,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VideoOutputMode {
    #[default]
    Camera,
    ComicAvatar,
    DepthMap,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AvatarEngine {
    Portrait3d,
    #[default]
    Liveportrait,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VideoIdentity {
    #[default]
    Camera,
    Portrait3d,
    Liveportrait,
    DepthMap,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct VideoEffectsState {
    #[serde(default)]
    pub transform: crate::video_transform::VideoTransform,
    pub output_mode: VideoOutputMode,
    pub avatar_engine: Option<AvatarEngine>,
    pub background_enabled: bool,
    pub background_effect: BackgroundEffect,
    // Compatibility state for clients using the original dedicated control.
    pub green_screen_enabled: bool,
    pub mask_available: bool,
    pub mask_frame_id: Option<u64>,
    pub mask_width: Option<u32>,
    pub mask_height: Option<u32>,
    pub mask_captured_at_ms: Option<u64>,
    pub mask_published_at_ms: Option<u64>,
    pub avatar_available: bool,
    pub avatar_frame_id: Option<u64>,
    pub avatar_width: Option<u32>,
    pub avatar_height: Option<u32>,
    pub avatar_captured_at_ms: Option<u64>,
    pub avatar_published_at_ms: Option<u64>,
    pub depth_available: bool,
    pub depth_frame_id: Option<u64>,
    pub depth_width: Option<u32>,
    pub depth_height: Option<u32>,
    pub depth_far: Option<f32>,
    pub depth_near: Option<f32>,
    pub depth_captured_at_ms: Option<u64>,
    pub depth_published_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CameraAttitudeSource {
    #[default]
    Unavailable,
    LastCommanded,
    Measured,
    Simulated,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CameraState {
    pub available: bool,
    pub device_id: Option<String>,
    pub name: Option<String>,
    pub powered_on: Option<bool>,
    pub power_error: Option<String>,
    pub adapter: String,
    #[serde(default)]
    pub capabilities: CameraCapabilities,
    pub serial: Option<String>,
    pub tracking: Option<bool>,
    pub tracking_sample_at_ms: Option<u64>,
    pub tracking_error: Option<String>,
    pub face_tracking: FaceTrackingState,
    pub hands_tracking: HandsTrackingState,
    pub zoom_magnification: Option<f32>,
    pub zoom_sample_at_ms: Option<u64>,
    pub zoom_error: Option<String>,
    pub hdr: Option<bool>,
    pub hdr_sample_at_ms: Option<u64>,
    pub hdr_error: Option<String>,
    pub image_settings: CameraImageSettingsState,
    pub built_in_gestures: BuiltInGestureState,
    pub yaw_degrees: Option<f32>,
    pub pitch_degrees: Option<f32>,
    pub roll_degrees: Option<f32>,
    pub euler_yaw_degrees: Option<f32>,
    pub euler_pitch_degrees: Option<f32>,
    pub euler_roll_degrees: Option<f32>,
    pub yaw_velocity_degrees_per_second: Option<f32>,
    pub pitch_velocity_degrees_per_second: Option<f32>,
    pub roll_velocity_degrees_per_second: Option<f32>,
    pub attitude_source: CameraAttitudeSource,
    pub sample_at_ms: Option<u64>,
    pub telemetry_error: Option<String>,
    pub last_command_at_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CameraCapabilities {
    pub power: bool,
    pub absolute_position: bool,
    pub pan_tilt: bool,
    pub motor_telemetry: bool,
    pub tracking: bool,
    pub hdr: bool,
    pub zoom: bool,
    pub image_settings: bool,
    pub built_in_gestures: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CameraImageControl {
    Brightness,
    Contrast,
    Saturation,
    Hue,
    Gamma,
    Gain,
    BacklightCompensation,
    PowerLineFrequency,
    WhiteBalanceAutomatic,
    WhiteBalanceTemperature,
    RedBalance,
    BlueBalance,
    Sharpness,
    AutoExposure,
    ExposureTimeAbsolute,
    ExposureDynamicFramerate,
    FocusAbsolute,
    FocusAutomaticContinuous,
    FacePriorityAutoExposure,
}

impl CameraImageControl {
    pub const STANDARD: [Self; 18] = [
        Self::Brightness,
        Self::Contrast,
        Self::Saturation,
        Self::Hue,
        Self::Gamma,
        Self::Gain,
        Self::BacklightCompensation,
        Self::PowerLineFrequency,
        Self::WhiteBalanceAutomatic,
        Self::WhiteBalanceTemperature,
        Self::RedBalance,
        Self::BlueBalance,
        Self::Sharpness,
        Self::AutoExposure,
        Self::ExposureTimeAbsolute,
        Self::ExposureDynamicFramerate,
        Self::FocusAbsolute,
        Self::FocusAutomaticContinuous,
    ];
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CameraImageControlKind {
    Integer,
    Boolean,
    Menu,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CameraImageControlOption {
    pub value: i32,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CameraImageControlState {
    pub control: CameraImageControl,
    pub kind: CameraImageControlKind,
    pub available: bool,
    pub active: bool,
    pub read_only: bool,
    pub value: Option<i32>,
    pub minimum: Option<i32>,
    pub maximum: Option<i32>,
    pub step: Option<i32>,
    pub default_value: Option<i32>,
    pub options: Vec<CameraImageControlOption>,
    pub sample_at_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CameraImageSettingsState {
    pub controls: Vec<CameraImageControlState>,
    pub error: Option<String>,
}

impl CameraImageSettingsState {
    pub fn upsert(&mut self, control: CameraImageControlState) {
        if let Some(existing) = self
            .controls
            .iter_mut()
            .find(|existing| existing.control == control.control)
        {
            *existing = control;
        } else {
            self.controls.push(control);
        }
    }

    pub fn value(&self, control: CameraImageControl) -> Option<i32> {
        self.controls
            .iter()
            .find(|state| state.control == control)
            .and_then(|state| state.value)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FaceTrackingState {
    pub enabled: bool,
    pub active: bool,
    pub target_visible: bool,
    #[serde(default)]
    pub target_source: Option<FaceTrackingTarget>,
    pub target_x: Option<f32>,
    pub target_y: Option<f32>,
    #[serde(default)]
    pub speed_fraction: f32,
    #[serde(default)]
    pub auto_zoom: AutoZoomState,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AutoZoomState {
    pub enabled: bool,
    pub calibrated: bool,
    pub zoom_magnification: Option<f32>,
    pub target_face_size: Option<f32>,
    pub face_size: Option<f32>,
    pub at_limit: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HandsTrackingState {
    pub enabled: bool,
    pub active: bool,
    pub hands_visible: u8,
    pub rapid_motion: bool,
    #[serde(default)]
    pub recovering_arms: bool,
    pub zoom_frozen: bool,
    pub target_x: Option<f32>,
    pub target_y: Option<f32>,
    #[serde(default)]
    pub speed_fraction: f32,
    pub calibrated: bool,
    pub zoom_magnification: Option<f32>,
    pub target_span: Option<f32>,
    pub hand_span: Option<f32>,
    pub at_limit: bool,
    pub error: Option<String>,
    pub zoom_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FaceTrackingTarget {
    Face,
    Shoulders,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BuiltInGesture {
    TargetSelection,
    Zoom,
    DynamicZoom,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BuiltInGestureState {
    pub target_selection: Option<bool>,
    pub zoom: Option<bool>,
    pub dynamic_zoom: Option<bool>,
    pub sample_at_ms: Option<u64>,
    pub error: Option<String>,
}

impl BuiltInGestureState {
    pub fn set(&mut self, feature: BuiltInGesture, enabled: bool) {
        match feature {
            BuiltInGesture::TargetSelection => self.target_selection = Some(enabled),
            BuiltInGesture::Zoom => self.zoom = Some(enabled),
            BuiltInGesture::DynamicZoom => self.dynamic_zoom = Some(enabled),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PipelineState {
    /// Buffers are held without streaming while capture is stopped.
    #[serde(default)]
    pub camera_reserved: bool,
    #[serde(default)]
    pub output_muted: bool,
    pub enabled: bool,
    pub running: bool,
    pub source: String,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub frame_count: u64,
    pub last_frame_at_ms: Option<u64>,
    pub restart_count: u32,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PerceptionState {
    pub active_models: crate::perception_demand::Models,
    pub worker_connected: bool,
    pub error: Option<String>,
    pub frame_id: Option<u64>,
    pub face_detected: bool,
    pub face_landmarks: Vec<Landmark>,
    pub hand_detected: bool,
    pub hand_landmarks: Vec<Landmark>,
    pub last_hand_at_ms: Option<u64>,
    pub pose_detected: bool,
    pub pose_landmarks: Vec<Landmark>,
    pub gesture: Option<String>,
    pub confidence: Option<f32>,
    pub peak_gesture: Option<String>,
    pub peak_gesture_confidence: Option<f32>,
    pub peak_gesture_at_ms: Option<u64>,
    pub phone_near_mouth: crate::phone_gesture::PhoneGestureState,
    pub sample_at_ms: Option<u64>,
    pub latency_ms: Option<f32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Landmark {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<f32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SemanticEvent {
    pub sequence: u64,
    pub kind: String,
    pub source: String,
    pub emitted_at_ms: u64,
    pub confidence: Option<f32>,
    #[serde(default)]
    pub data: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScenarioActivation {
    pub scenario_id: String,
    pub action: String,
    pub triggered_at_ms: u64,
    pub trigger_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PerceptionObservation {
    #[serde(default = "crate::perception_demand::Models::all")]
    pub active_models: crate::perception_demand::Models,
    pub frame_id: u64,
    pub captured_at_ms: u64,
    #[serde(default)]
    pub image_width: u32,
    #[serde(default)]
    pub image_height: u32,
    #[serde(default)]
    pub face_detected: bool,
    #[serde(default)]
    pub face_landmarks: Vec<Landmark>,
    #[serde(default)]
    pub hand_detected: bool,
    #[serde(default)]
    pub hand_landmarks: Vec<Landmark>,
    #[serde(default)]
    pub pose_detected: bool,
    #[serde(default)]
    pub pose_landmarks: Vec<Landmark>,
    pub gesture: Option<String>,
    #[serde(default)]
    pub confidence: f32,
    pub latency_ms: Option<f32>,
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
