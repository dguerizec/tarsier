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
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BackgroundEffect {
    #[default]
    GreenScreen,
    Blur,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VideoOutputMode {
    #[default]
    Camera,
    ComicAvatar,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AvatarEngine {
    #[default]
    #[serde(rename = "stylized-3d")]
    Stylized3d,
    Liveportrait,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct VideoEffectsState {
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
    pub adapter: String,
    pub serial: Option<String>,
    pub tracking: Option<bool>,
    pub tracking_sample_at_ms: Option<u64>,
    pub tracking_error: Option<String>,
    pub face_tracking: FaceTrackingState,
    pub zoom_magnification: Option<f32>,
    pub zoom_sample_at_ms: Option<u64>,
    pub zoom_error: Option<String>,
    pub hdr: Option<bool>,
    pub hdr_sample_at_ms: Option<u64>,
    pub hdr_error: Option<String>,
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FaceTrackingState {
    pub enabled: bool,
    pub active: bool,
    pub target_visible: bool,
    #[serde(default)]
    pub target_source: Option<FaceTrackingTarget>,
    pub target_x: Option<f32>,
    pub target_y: Option<f32>,
    pub error: Option<String>,
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
    pub frame_id: u64,
    pub captured_at_ms: u64,
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
