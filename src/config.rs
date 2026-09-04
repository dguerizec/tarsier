use std::{net::SocketAddr, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub video: VideoConfig,
    pub camera: CameraConfig,
    pub perception: PerceptionConfig,
    pub scenarios: Vec<ScenarioConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            video: VideoConfig::default(),
            camera: CameraConfig::default(),
            perception: PerceptionConfig::default(),
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
        if self.camera.poll_interval_ms < self.camera.minimum_command_interval_ms {
            bail!("camera poll interval must not be shorter than the command interval");
        }
        if !(0.0..=1.0).contains(&self.perception.minimum_confidence)
            || !(0.0..=1.0).contains(&self.perception.release_confidence)
        {
            bail!("perception confidence values must be between 0 and 1");
        }
        if self.perception.release_confidence >= self.perception.minimum_confidence {
            bail!("perception release_confidence must be below minimum_confidence");
        }
        Ok(())
    }
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
            poll_interval_ms: 500,
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
    ObsbotTiny2,
    Mock,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PerceptionConfig {
    pub enabled: bool,
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub minimum_confidence: f32,
    pub dwell_ms: u64,
    pub release_confidence: f32,
    pub cooldown_ms: u64,
}

impl Default for PerceptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            device: "/dev/video42".into(),
            width: 640,
            height: 360,
            fps: 10,
            minimum_confidence: 0.85,
            dwell_ms: 800,
            release_confidence: 0.65,
            cooldown_ms: 3000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScenarioConfig {
    pub id: String,
    pub enabled: bool,
    pub event: String,
    pub action: String,
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
    fn rejects_missing_hysteresis() {
        let mut config = Config::default();
        config.perception.release_confidence = config.perception.minimum_confidence;
        assert!(config.validate().is_err());
    }
}
