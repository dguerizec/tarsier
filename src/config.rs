use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub video: VideoConfig,
    pub camera: CameraConfig,
    pub perception: PerceptionConfig,
    pub presets: Vec<CameraPresetConfig>,
    pub scenarios: Vec<ScenarioConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            video: VideoConfig::default(),
            camera: CameraConfig::default(),
            perception: PerceptionConfig::default(),
            presets: Vec::new(),
            scenarios: vec![ScenarioConfig::default()],
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        let config: Self = toml::from_str(&source)
            .with_context(|| format!("invalid configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.video.width == 0 || self.video.height == 0 || self.video.fps == 0 {
            bail!("video width, height, and fps must be greater than zero");
        }
        if self.video.preview_quality == 0 || self.video.preview_quality > 100 {
            bail!("video preview_quality must be between 1 and 100");
        }
        if self.video.restart_delay_ms == 0 {
            bail!("video restart_delay_ms must be greater than zero");
        }
        if self.camera.poll_interval_ms != 0
            && self.camera.poll_interval_ms < self.camera.minimum_command_interval_ms
        {
            bail!("camera poll interval must not be shorter than the command interval");
        }
        if self.perception.width == 0 || self.perception.height == 0 || self.perception.fps == 0 {
            bail!("perception width, height, and fps must be greater than zero");
        }
        if self.perception.mask_fps == 0 {
            bail!("perception mask_fps must be greater than zero");
        }
        if self.perception.restart_delay_ms == 0 {
            bail!("perception restart_delay_ms must be greater than zero");
        }
        if !(0.0..=1.0).contains(&self.perception.minimum_confidence)
            || !(0.0..=1.0).contains(&self.perception.release_confidence)
            || !(0.0..=1.0).contains(&self.perception.detection_confidence)
        {
            bail!("perception confidence values must be between 0 and 1");
        }
        if self.perception.release_confidence >= self.perception.minimum_confidence {
            bail!("perception release_confidence must be below minimum_confidence");
        }
        let mut preset_ids = HashSet::new();
        for preset in &self.presets {
            if !valid_identifier(&preset.id) {
                bail!(
                    "camera preset IDs may contain only ASCII letters, digits, dots, underscores, and hyphens"
                );
            }
            if !preset_ids.insert(&preset.id) {
                bail!("camera preset IDs must be unique");
            }
            if !preset.yaw.is_finite()
                || !preset.pitch.is_finite()
                || !preset.roll.is_finite()
                || preset.yaw.abs() > self.camera.max_yaw_degrees
                || preset.pitch.abs() > self.camera.max_pitch_degrees
                || preset.roll.abs() > 45.0
            {
                bail!(
                    "camera preset angles must be finite and within the configured safety limits"
                );
            }
        }
        let mut scenario_ids = HashSet::new();
        for scenario in &self.scenarios {
            if !valid_identifier(&scenario.id) {
                bail!(
                    "scenario IDs may contain only ASCII letters, digits, dots, underscores, and hyphens"
                );
            }
            if !scenario_ids.insert(&scenario.id) {
                bail!("scenario IDs must be unique");
            }
            if scenario.event.trim().is_empty() || scenario.action.trim().is_empty() {
                bail!("scenario events and actions must not be empty");
            }
        }
        Ok(())
    }
}

fn valid_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= 64
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8742".parse().expect("static address is valid"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct VideoConfig {
    pub source: VideoSource,
    pub input_device: String,
    pub output_device: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub preview_width: u32,
    pub preview_height: u32,
    pub preview_quality: u32,
    pub loopback_enabled: bool,
    pub green_screen_enabled: bool,
    pub restart_delay_ms: u64,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            source: VideoSource::Camera,
            input_device: "/dev/video0".into(),
            output_device: "/dev/video42".into(),
            width: 1280,
            height: 720,
            fps: 30,
            preview_width: 640,
            preview_height: 360,
            preview_quality: 75,
            loopback_enabled: true,
            green_screen_enabled: false,
            restart_delay_ms: 1000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VideoSource {
    #[default]
    Camera,
    Test,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct CameraConfig {
    pub adapter: CameraAdapter,
    pub control_device: String,
    pub xu_unit: u8,
    pub poll_interval_ms: u64,
    pub minimum_command_interval_ms: u64,
    pub max_yaw_degrees: f32,
    pub max_pitch_degrees: f32,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            adapter: CameraAdapter::ObsbotTiny2,
            control_device: "/dev/video0".into(),
            xu_unit: 2,
            poll_interval_ms: 0,
            minimum_command_interval_ms: 20,
            max_yaw_degrees: 130.0,
            max_pitch_degrees: 90.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CameraAdapter {
    #[default]
    #[serde(rename = "obsbot-tiny-2", alias = "obsbot-tiny2")]
    ObsbotTiny2,
    Mock,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PerceptionConfig {
    pub enabled: bool,
    pub supervise_worker: bool,
    pub worker_project: PathBuf,
    pub restart_delay_ms: u64,
    pub source: PerceptionSource,
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub mask_fps: u32,
    pub minimum_confidence: f32,
    pub detection_confidence: f32,
    pub dwell_ms: u64,
    pub release_confidence: f32,
    pub cooldown_ms: u64,
    pub face_dwell_ms: u64,
    pub face_release_ms: u64,
}

impl Default for PerceptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            supervise_worker: true,
            worker_project: "worker".into(),
            restart_delay_ms: 1000,
            source: PerceptionSource::Preview,
            device: "/dev/video42".into(),
            width: 640,
            height: 360,
            fps: 10,
            mask_fps: 30,
            minimum_confidence: 0.60,
            detection_confidence: 0.5,
            dwell_ms: 800,
            release_confidence: 0.40,
            cooldown_ms: 3000,
            face_dwell_ms: 300,
            face_release_ms: 500,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PerceptionSource {
    #[default]
    Preview,
    Device,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScenarioConfig {
    pub id: String,
    pub enabled: bool,
    pub event: String,
    pub action: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CameraPresetConfig {
    pub id: String,
    pub yaw: f32,
    pub pitch: f32,
    #[serde(default)]
    pub roll: f32,
}

impl Default for CameraPresetConfig {
    fn default() -> Self {
        Self {
            id: "center".into(),
            yaw: 0.0,
            pitch: 0.0,
            roll: 0.0,
        }
    }
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            id: "open-palm-demo".into(),
            enabled: true,
            event: "gesture.open_palm.held".into(),
            action: "demo.open_palm".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn zero_disables_camera_polling_but_short_nonzero_intervals_are_rejected() {
        let mut config = Config::default();
        assert_eq!(config.camera.poll_interval_ms, 0);
        config.validate().unwrap();

        config.camera.poll_interval_ms = config.camera.minimum_command_interval_ms - 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn example_configuration_is_loadable() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/tarsier.example.toml");
        Config::load(Some(&path)).unwrap();
    }

    #[test]
    fn rejects_missing_hysteresis() {
        let mut config = Config::default();
        config.perception.release_confidence = config.perception.minimum_confidence;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_invalid_detection_confidence() {
        let mut config = Config::default();
        config.perception.detection_confidence = 1.1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_perception_rate() {
        let mut config = Config::default();
        config.perception.fps = 0;
        assert!(config.validate().is_err());
        config.perception.fps = 10;
        config.perception.mask_fps = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_disabled_video_recovery() {
        let mut config = Config::default();
        config.video.restart_delay_ms = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_unsafe_or_duplicate_camera_presets() {
        let mut unsafe_config = Config::default();
        unsafe_config.presets.push(CameraPresetConfig::default());
        unsafe_config.presets[0].yaw = 140.0;
        assert!(unsafe_config.validate().is_err());

        let mut duplicate_config = Config::default();
        duplicate_config.presets.push(CameraPresetConfig::default());
        duplicate_config.presets.push(CameraPresetConfig::default());
        assert!(duplicate_config.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_scenarios() {
        let mut config = Config::default();
        config.scenarios.push(ScenarioConfig::default());
        assert!(config.validate().is_err());
    }
}
