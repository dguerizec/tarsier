use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeState {
    pub version: &'static str,
    pub started_at_ms: u64,
    pub camera: CameraState,
    pub pipeline: PipelineState,
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
            perception: PerceptionState::default(),
            last_scenario: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CameraState {
    pub available: bool,
    pub adapter: String,
    pub serial: Option<String>,
    pub tracking: Option<bool>,
    pub yaw_degrees: Option<f32>,
    pub pitch_degrees: Option<f32>,
    pub roll_degrees: Option<f32>,
    pub sample_at_ms: Option<u64>,
    pub last_command_at_ms: Option<u64>,
    pub error: Option<String>,
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
    pub frame_id: Option<u64>,
    pub face_detected: bool,
    pub gesture: Option<String>,
    pub confidence: Option<f32>,
    pub sample_at_ms: Option<u64>,
    pub latency_ms: Option<f32>,
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
