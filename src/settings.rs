use std::{
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    config::Config,
    model::{AvatarEngine, BackgroundEffect, VideoIdentity, VideoOutputMode},
};

const SETTINGS_VERSION: u32 = 1;
const MAX_SETTINGS_BYTES: usize = 16 * 1024;
const SETTINGS_PATH_ENVIRONMENT: &str = "TARSIER_USER_SETTINGS_PATH";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct VideoResolution {
    pub width: u32,
    pub height: u32,
}

impl VideoResolution {
    pub fn valid(self) -> bool {
        matches!(
            (self.width, self.height),
            (1280, 720) | (1920, 1080) | (3840, 2160)
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct AudioSettings {
    pub voice_show_controls: bool,
    pub voice_disabled_models: std::collections::BTreeSet<String>,
    pub voice_enabled: bool,
    pub voice_pitch: i32,
    pub voice_model: String,
    pub capture_sources: Vec<String>,
    pub output_source: Option<String>,
    pub output_enabled: bool,
    pub output_muted: bool,
    pub output_auto_gain: bool,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            voice_show_controls: true,
            voice_disabled_models: Default::default(),
            voice_enabled: false,
            voice_pitch: 0,
            voice_model: crate::voice::default_model(),
            capture_sources: Vec::new(),
            output_source: None,
            output_enabled: false,
            output_muted: false,
            output_auto_gain: true,
        }
    }
}

impl AudioSettings {
    pub fn from_state(state: &crate::model::RuntimeState) -> Self {
        Self {
            voice_show_controls: state.audio_voice.show_controls,
            voice_disabled_models: state.audio_voice.disabled_models.clone(),
            voice_enabled: state.audio_voice.enabled,
            voice_pitch: state.audio_voice.pitch,
            voice_model: state.audio_voice.model.clone(),
            capture_sources: state.audio_capture_sources.clone(),
            output_source: state.audio_virtual.source.clone(),
            output_enabled: state.audio_virtual.enabled,
            output_muted: state.audio_virtual.muted,
            output_auto_gain: state.audio_virtual.auto_gain,
        }
    }

    pub fn apply(&self, state: &mut crate::model::RuntimeState) {
        if state.audio_voice.model != self.voice_model
            || (!state
                .audio_voice
                .disabled_models
                .contains(&self.voice_model)
                && self.voice_disabled_models.contains(&self.voice_model))
        {
            state.audio_voice.generation = state.audio_voice.generation.wrapping_add(1);
            state.audio_voice.ready = false;
            state.audio_voice.error = None;
            state.audio_voice.inference_ms = None;
            state.audio_voice.pipeline_ms = None;
            state.audio_voice.model = self.voice_model.clone();
        }
        state.audio_voice.show_controls = self.voice_show_controls;
        state.audio_voice.disabled_models = self.voice_disabled_models.clone();
        state.audio_voice.enabled = self.voice_enabled
            && self.voice_show_controls
            && !self.voice_disabled_models.contains(&self.voice_model);
        state.audio_voice.pitch = self.voice_pitch;
        state.audio_capture_sources = self.capture_sources.clone();
        state.audio_virtual.source = self.output_source.clone();
        state.audio_virtual.enabled = self.output_enabled;
        state.audio_virtual.muted = self.output_muted;
        state.audio_virtual.auto_gain = self.output_auto_gain;
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserSettings {
    #[serde(default)]
    pub video_output_muted: bool,
    #[serde(default = "crate::mute_media::default_selection")]
    pub video_mute_media: Option<crate::mute_media::Selection>,
    #[serde(default)]
    pub video_mute_library: Vec<crate::mute_media::Selection>,
    #[serde(default)]
    pub liveportrait_source: Option<PathBuf>,
    #[serde(default)]
    pub portrait3d_model: Option<PathBuf>,
    #[serde(default)]
    pub camera_device: Option<String>,
    #[serde(default)]
    pub video_input_library: Vec<crate::mute_media::Selection>,
    #[serde(default)]
    pub audio_reserve_inputs: Option<bool>,
    #[serde(default)]
    pub audio_input_reservations: std::collections::BTreeMap<String, bool>,
    #[serde(default)]
    pub audio: AudioSettings,
    #[serde(default)]
    pub network_lan_access: Option<bool>,
    #[serde(default)]
    pub perception_delegates: Option<crate::config::MediaPipeDelegates>,
    #[serde(default)]
    pub video_resolution: Option<VideoResolution>,
    #[serde(default)]
    pub video_transform: crate::video_transform::VideoTransform,
    version: u32,
    pub video_identity: VideoIdentity,
    pub background_enabled: bool,
    pub background_effect: BackgroundEffect,
    #[serde(default = "crate::background::default_plugin")]
    pub background_plugin: String,
    pub face_tracking_enabled: bool,
    #[serde(default)]
    pub auto_zoom_enabled: bool,
    #[serde(default)]
    pub hands_tracking_enabled: bool,
}

impl UserSettings {
    fn remember_mute_media(&mut self) {
        if let Some(selection) = &self.video_mute_media {
            let default = crate::mute_media::default_selection().unwrap();
            if selection.filename != default.filename
                && !self.video_mute_library.iter().any(|item| item.filename == selection.filename)
            {
                self.video_mute_library.push(selection.clone());
            }
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self {
            video_output_muted: false,
            video_mute_media: crate::mute_media::default_selection(),
            video_mute_library: Vec::new(),
            liveportrait_source: None,
            portrait3d_model: None,
            camera_device: None,
            video_input_library: Vec::new(),
            audio_reserve_inputs: None,
            audio_input_reservations: Default::default(),
            audio: AudioSettings::default(),
            version: SETTINGS_VERSION,
            network_lan_access: None,
            perception_delegates: None,
            video_resolution: None,
            video_transform: Default::default(),
            video_identity: identity_from_mode(config.video.output_mode, config.avatar.engine),
            background_enabled: config.video.background_enabled,
            background_effect: config.video.background_effect,
            background_plugin: crate::background::default_plugin(),
            face_tracking_enabled: false,
            auto_zoom_enabled: false,
            hands_tracking_enabled: false,
        }
    }

    pub fn output_mode(&self) -> VideoOutputMode {
        match self.video_identity {
            VideoIdentity::Camera => VideoOutputMode::Camera,
            VideoIdentity::DepthMap => VideoOutputMode::DepthMap,
            VideoIdentity::Portrait3d | VideoIdentity::Liveportrait => VideoOutputMode::ComicAvatar,
        }
    }

    pub fn avatar_engine(&self) -> Option<AvatarEngine> {
        match self.video_identity {
            VideoIdentity::Portrait3d => Some(AvatarEngine::Portrait3d),
            VideoIdentity::Liveportrait => Some(AvatarEngine::Liveportrait),
            VideoIdentity::Camera | VideoIdentity::DepthMap => None,
        }
    }

    fn validate(self) -> Result<Self> {
        if !crate::voice::valid_model_name(&self.audio.voice_model) {
            bail!("invalid voice model filename");
        }
        if !(-12..=12).contains(&self.audio.voice_pitch) {
            bail!("voice pitch must be between -12 and 12");
        }
        if self
            .video_resolution
            .is_some_and(|resolution| !resolution.valid())
        {
            bail!("unsupported video resolution");
        }
        if !self.video_transform.valid() {
            bail!("video rotation must be 0, 90, 180, or 270 degrees");
        }
        if self.version != SETTINGS_VERSION {
            bail!(
                "unsupported user settings version {}, expected {SETTINGS_VERSION}",
                self.version
            );
        }
        if self.auto_zoom_enabled && !self.face_tracking_enabled {
            bail!("auto zoom cannot be restored without face tracking");
        }
        if self.face_tracking_enabled && self.hands_tracking_enabled {
            bail!("face tracking and hands tracking cannot be restored together");
        }
        Ok(self)
    }
}

fn identity_from_mode(mode: VideoOutputMode, engine: AvatarEngine) -> VideoIdentity {
    match mode {
        VideoOutputMode::Camera => VideoIdentity::Camera,
        VideoOutputMode::DepthMap => VideoIdentity::DepthMap,
        VideoOutputMode::ComicAvatar => match engine {
            AvatarEngine::Portrait3d => VideoIdentity::Portrait3d,
            AvatarEngine::Liveportrait => VideoIdentity::Liveportrait,
        },
    }
}

#[derive(Clone, Debug)]
pub struct UserSettingsStore {
    path: Arc<PathBuf>,
    current: Arc<Mutex<UserSettings>>,
}

impl UserSettingsStore {
    pub async fn load(path: PathBuf, fallback: UserSettings) -> Result<(Self, UserSettings)> {
        let settings = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                if bytes.len() > MAX_SETTINGS_BYTES {
                    bail!(
                        "user settings {} exceed {MAX_SETTINGS_BYTES} bytes",
                        path.display()
                    );
                }
                serde_json::from_slice::<UserSettings>(&bytes)
                    .with_context(|| format!("invalid user settings {}", path.display()))?
                    .validate()?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fallback,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read user settings {}", path.display()));
            }
        };
        Ok((
            Self {
                path: Arc::new(path),
                current: Arc::new(Mutex::new(settings.clone())),
            },
            settings,
        ))
    }

    pub fn video_input_directory(&self) -> PathBuf {
        self.path.parent().unwrap_or(Path::new(".")).join("video-inputs")
    }

    pub async fn video_input_library(&self) -> Vec<crate::mute_media::Selection> {
        self.current.lock().await.video_input_library.clone()
    }

    pub async fn remember_video_input(&self, selection: crate::mute_media::Selection) -> Result<()> {
        self.replace(|settings| {
            if !settings.video_input_library.iter().any(|item| item.filename == selection.filename) {
                settings.video_input_library.push(selection);
            }
        }).await
    }

    pub fn mute_media_directory(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(Path::new("."))
            .join("mute-media")
    }

    pub async fn set_video_mute_media(
        &self,
        selection: Option<crate::mute_media::Selection>,
    ) -> Result<()> {
        self.replace(|settings| {
            settings.remember_mute_media();
            settings.video_mute_media = selection;
            settings.remember_mute_media();
        }).await
    }

    pub async fn video_mute_library(&self) -> Vec<crate::mute_media::Selection> {
        let mut settings = self.current.lock().await.clone();
        settings.remember_mute_media();
        settings.video_mute_library
    }

    pub async fn delete_video_mute_media(&self, filename: &str) -> Result<()> {
        self.replace(|settings| {
            settings.remember_mute_media();
            settings.video_mute_library.retain(|item| item.filename != filename);
            if settings.video_mute_media.as_ref().is_some_and(|item| item.filename == filename) {
                settings.video_mute_media = crate::mute_media::default_selection();
            }
        }).await
    }

    pub fn portrait_directory(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(Path::new("."))
            .join("portraits")
    }

    pub async fn set_portrait3d_model(&self, path: PathBuf) -> Result<()> {
        self.replace(|settings| settings.portrait3d_model = Some(path))
            .await
    }

    pub async fn set_liveportrait_source(&self, path: PathBuf) -> Result<()> {
        self.replace(|settings| settings.liveportrait_source = Some(path))
            .await
    }

    pub async fn set_audio(&self, audio: AudioSettings) -> Result<()> {
        self.replace(|settings| settings.audio = audio).await
    }

    pub async fn set_video_identity(&self, identity: VideoIdentity) -> Result<()> {
        self.replace(|settings| settings.video_identity = identity)
            .await
    }

    pub async fn set_video_output_muted(&self, muted: bool) -> Result<()> {
        self.replace(|settings| settings.video_output_muted = muted)
            .await
    }

    pub async fn set_video_transform(
        &self,
        transform: crate::video_transform::VideoTransform,
    ) -> Result<()> {
        self.replace(|settings| settings.video_transform = transform)
            .await
    }

    pub async fn set_background_selection(&self, enabled: bool, effect: BackgroundEffect, plugin: String) -> Result<()> {
        self.replace(|settings| {
            settings.background_enabled = enabled;
            settings.background_effect = effect;
            settings.background_plugin = plugin;
        }).await
    }

    pub async fn set_background(&self, enabled: bool, effect: BackgroundEffect) -> Result<()> {
        self.replace(|settings| {
            settings.background_enabled = enabled;
            settings.background_effect = effect;
        })
        .await
    }

    pub async fn set_devices(
        &self,
        camera: String,
        input_reservations: std::collections::BTreeMap<String, bool>,
    ) -> Result<()> {
        self.replace(|settings| {
            if settings.camera_device.as_ref() != Some(&camera) {
                settings.face_tracking_enabled = false;
                settings.auto_zoom_enabled = false;
                settings.hands_tracking_enabled = false;
            }
            settings.camera_device = Some(camera);
            // Migrate the former global switch to per-input preferences.
            settings.audio_reserve_inputs = Some(true);
            settings.audio_input_reservations = input_reservations;
        })
        .await
    }

    pub async fn set_perception_delegates(&self, delegates: crate::config::MediaPipeDelegates) -> Result<()> {
        self.replace(|settings| settings.perception_delegates = Some(delegates)).await
    }

    pub async fn set_network_lan_access(&self, enabled: bool) -> Result<()> {
        self.replace(|settings| settings.network_lan_access = Some(enabled))
            .await
    }

    pub async fn set_video_resolution(&self, resolution: VideoResolution) -> Result<()> {
        if !resolution.valid() {
            bail!("unsupported video resolution");
        }
        self.replace(|settings| {
            settings.video_resolution = Some(resolution);
            if resolution.width >= 3840 {
                settings.video_identity = VideoIdentity::Camera;
                settings.background_enabled = false;
                settings.video_transform = Default::default();
            }
        })
        .await
    }

    pub async fn set_face_tracking(&self, enabled: bool) -> Result<()> {
        self.replace(|settings| {
            settings.face_tracking_enabled = enabled;
            if enabled {
                settings.hands_tracking_enabled = false;
            }
            if !enabled {
                settings.auto_zoom_enabled = false;
            }
        })
        .await
    }

    pub async fn set_auto_zoom(&self, enabled: bool) -> Result<()> {
        self.replace(|settings| settings.auto_zoom_enabled = enabled)
            .await
    }

    pub async fn set_hands_tracking(&self, enabled: bool) -> Result<()> {
        self.replace(|settings| {
            settings.hands_tracking_enabled = enabled;
            if enabled {
                settings.face_tracking_enabled = false;
                settings.auto_zoom_enabled = false;
            }
        })
        .await
    }

    async fn replace(&self, update: impl FnOnce(&mut UserSettings)) -> Result<()> {
        let mut current = self.current.lock().await;
        let mut next = current.clone();
        update(&mut next);
        let path = Arc::clone(&self.path);
        let to_write = next.clone();
        tokio::task::spawn_blocking(move || persist(&path, to_write))
            .await
            .context("user settings writer stopped unexpectedly")??;
        *current = next;
        Ok(())
    }
}

pub fn default_path() -> Result<PathBuf> {
    if let Some(path) = non_empty_environment(SETTINGS_PATH_ENVIRONMENT) {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = non_empty_environment("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("tarsier/user-settings.json"));
    }
    let Some(home) = non_empty_environment("HOME") else {
        bail!(
            "cannot locate user settings: set {SETTINGS_PATH_ENVIRONMENT}, XDG_STATE_HOME, or HOME"
        );
    };
    Ok(PathBuf::from(home).join(".local/state/tarsier/user-settings.json"))
}

fn non_empty_environment(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn persist(path: &Path, settings: UserSettings) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create user settings directory {}",
                parent.display()
            )
        })?;
    }
    let mut bytes = serde_json::to_vec_pretty(&settings)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_SETTINGS_BYTES {
        bail!("settings storage is full; remove unused saved media before adding more");
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("user-settings.json");
    let temporary = path.with_file_name(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).with_context(|| {
            format!(
                "failed to create temporary user settings {}",
                temporary.display()
            )
        })?;
        file.write_all(&bytes)
            .with_context(|| format!("failed to write user settings {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync user settings {}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace user settings {}", path.display()))?;
        if let Some(parent) = parent {
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .with_context(|| {
                    format!(
                        "failed to sync user settings directory {}",
                        parent.display()
                    )
                })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn mute_library_preserves_existing_media_and_multiple_selections() {
        let path = std::env::temp_dir().join(format!("tarsier-library-{}-{}/settings.json", std::process::id(), crate::model::unix_ms()));
        let mut old = UserSettings::from_config(&Config::default());
        let first = crate::mute_media::Selection { filename: format!("{}.png", "a".repeat(64)), name: "Work".into(), kind: crate::mute_media::Kind::Image };
        let second = crate::mute_media::Selection { filename: format!("{}.mp4", "b".repeat(64)), name: "Friends".into(), kind: crate::mute_media::Kind::Video };
        old.video_mute_media = Some(first.clone());
        let (store, _) = UserSettingsStore::load(path.clone(), old).await.unwrap();
        assert_eq!(store.video_mute_library().await, vec![first.clone()]);
        store.set_video_mute_media(Some(second.clone())).await.unwrap();
        store.set_video_mute_media(crate::mute_media::default_selection()).await.unwrap();
        store.set_video_mute_media(None).await.unwrap();
        let (store, _) = UserSettingsStore::load(path.clone(), UserSettings::from_config(&Config::default())).await.unwrap();
        assert_eq!(store.video_mute_library().await, vec![first.clone(), second.clone()]);
        store.set_video_mute_media(Some(first.clone())).await.unwrap();
        store.delete_video_mute_media(&second.filename).await.unwrap();
        let (_, restored) = UserSettingsStore::load(path.clone(), UserSettings::from_config(&Config::default())).await.unwrap();
        assert_eq!(restored.video_mute_media, Some(first.clone()));
        assert_eq!(restored.video_mute_library, vec![first]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn legacy_audio_settings_enable_automatic_gain_without_reusing_calibration() {
        let settings: super::AudioSettings = serde_json::from_str(
            r#"{"output_enabled":true,"calibrations":{"old":{"gain_db":24}}}"#,
        )
        .unwrap();
        assert!(settings.output_auto_gain);
        assert!(super::AudioSettings::default().output_auto_gain);
    }

    use super::*;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tarsier-user-settings-{}-{}-{name}/settings.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn device_selection_restores_stable_camera_and_microphones_together() {
        let path = test_path("devices");
        let defaults = UserSettings::from_config(&Config::default());
        let (store, _) = UserSettingsStore::load(path.clone(), defaults.clone())
            .await
            .unwrap();
        let camera = "/dev/v4l/by-id/usb-camera-video-index0".to_owned();
        let audio = AudioSettings {
            capture_sources: vec!["usb-microphone".into()],
            output_source: Some("usb-microphone".into()),
            output_muted: true,
            ..Default::default()
        };
        store.set_audio(audio.clone()).await.unwrap();
        let reservations = std::collections::BTreeMap::from([("usb-microphone".into(), false)]);
        store
            .set_devices(camera.clone(), reservations.clone())
            .await
            .unwrap();
        let (_, restored) = UserSettingsStore::load(path.clone(), defaults)
            .await
            .unwrap();
        assert_eq!(restored.camera_device, Some(camera));
        assert_eq!(restored.audio_input_reservations, reservations);
        assert_eq!(restored.audio, audio);
        store.set_audio(audio).await.unwrap();
        let (_, restored) =
            UserSettingsStore::load(path.clone(), UserSettings::from_config(&Config::default()))
                .await
                .unwrap();
        assert_eq!(restored.audio_input_reservations, reservations);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn audio_preferences_restore_without_runtime_status_or_reservations() {
        let path = test_path("audio");
        let fallback = UserSettings::from_config(&Config::default());
        let mut legacy = serde_json::to_value(&fallback).unwrap();
        legacy.as_object_mut().unwrap().remove("audio");
        assert_eq!(
            serde_json::from_value::<UserSettings>(legacy)
                .unwrap()
                .audio,
            AudioSettings::default()
        );
        let (store, _) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        let audio = AudioSettings {
            voice_show_controls: true,
            voice_disabled_models: std::collections::BTreeSet::from(["Disabled.pth".into()]),
            capture_sources: vec!["disconnected-mic".into()],
            output_source: Some("disabled-mic".into()),
            output_enabled: true,
            output_muted: true,
            output_auto_gain: false,
            voice_enabled: true,
            voice_pitch: 3,
            voice_model: "Shigure.pth".into(),
        };
        store.set_audio(audio.clone()).await.unwrap();
        store.set_network_lan_access(true).await.unwrap();
        let (_, restored) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        assert_eq!(restored.audio, audio);
        let mut state = crate::model::RuntimeState::default();
        restored.audio.apply(&mut state);
        assert_eq!(AudioSettings::from_state(&state), audio);
        assert!(!state.audio_virtual.running);
        assert!(state.audio_released_sources.is_empty());
        store.set_audio(AudioSettings::default()).await.unwrap();
        let (_, restored) = UserSettingsStore::load(path.clone(), fallback)
            .await
            .unwrap();
        assert_eq!(restored.audio, AudioSettings::default());
        assert_eq!(restored.network_lan_access, Some(true));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn network_access_persists_both_choices() {
        let path = test_path("network");
        let fallback = UserSettings::from_config(&Config::default());
        assert_eq!(fallback.network_lan_access, None);
        let (store, _) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        for enabled in [true, false] {
            store.set_network_lan_access(enabled).await.unwrap();
            let (_, restored) = UserSettingsStore::load(path.clone(), fallback.clone())
                .await
                .unwrap();
            assert_eq!(restored.network_lan_access, Some(enabled));
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn resolution_persists_and_4k_resets_effects() {
        let path = test_path("resolution");
        let fallback = UserSettings::from_config(&Config::default());
        let (store, _) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        store
            .set_video_identity(VideoIdentity::Liveportrait)
            .await
            .unwrap();
        store
            .set_background(true, BackgroundEffect::Blur)
            .await
            .unwrap();
        store
            .set_video_resolution(VideoResolution {
                width: 3840,
                height: 2160,
            })
            .await
            .unwrap();
        let (_, restored) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        assert_eq!(
            restored.video_resolution,
            Some(VideoResolution {
                width: 3840,
                height: 2160
            })
        );
        assert_eq!(restored.video_identity, VideoIdentity::Camera);
        assert!(!restored.background_enabled);
        assert_eq!(restored.video_transform, Default::default());
        assert!(
            store
                .set_video_resolution(VideoResolution {
                    width: 9999,
                    height: 2160
                })
                .await
                .is_err()
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn settings_are_atomically_persisted_and_loaded() {
        let path = test_path("round-trip");
        let fallback = UserSettings::from_config(&Config::default());
        let (store, loaded) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        assert_eq!(loaded, fallback);

        store
            .set_video_identity(VideoIdentity::Liveportrait)
            .await
            .unwrap();
        store
            .set_background(true, BackgroundEffect::PixelParty)
            .await
            .unwrap();
        store.set_face_tracking(true).await.unwrap();
        store.set_auto_zoom(true).await.unwrap();

        let (_, restored) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        assert_eq!(restored.video_identity, VideoIdentity::Liveportrait);
        assert!(restored.background_enabled);
        assert_eq!(restored.background_effect, BackgroundEffect::PixelParty);
        assert!(restored.face_tracking_enabled);
        assert!(restored.auto_zoom_enabled);
        assert_eq!(restored.output_mode(), VideoOutputMode::ComicAvatar);
        assert_eq!(restored.avatar_engine(), Some(AvatarEngine::Liveportrait));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn malformed_settings_fail_closed_instead_of_using_camera_defaults() {
        let path = test_path("malformed");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"video_identity":"camera"}"#).unwrap();

        let error =
            UserSettingsStore::load(path.clone(), UserSettings::from_config(&Config::default()))
                .await
                .unwrap_err();
        assert!(error.to_string().contains("invalid user settings"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn settings_without_auto_zoom_remain_compatible() {
        let path = test_path("legacy-without-auto-zoom");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            br#"{
                "version": 1,
                "video_identity": "camera",
                "background_enabled": false,
                "background_effect": "green-screen",
                "face_tracking_enabled": true
            }"#,
        )
        .unwrap();

        let (_, restored) =
            UserSettingsStore::load(path.clone(), UserSettings::from_config(&Config::default()))
                .await
                .unwrap();
        assert!(restored.face_tracking_enabled);
        assert!(!restored.auto_zoom_enabled);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn disabling_face_tracking_also_disables_auto_zoom() {
        let path = test_path("tracking-dependency");
        let fallback = UserSettings::from_config(&Config::default());
        let (store, _) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        store.set_face_tracking(true).await.unwrap();
        store.set_auto_zoom(true).await.unwrap();
        store.set_face_tracking(false).await.unwrap();

        let (_, restored) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        assert!(!restored.face_tracking_enabled);
        assert!(!restored.auto_zoom_enabled);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn local_tracking_modes_are_persisted_exclusively() {
        let path = test_path("exclusive-local-tracking");
        let fallback = UserSettings::from_config(&Config::default());
        let (store, _) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        store.set_face_tracking(true).await.unwrap();
        store.set_auto_zoom(true).await.unwrap();

        store.set_hands_tracking(true).await.unwrap();
        let (_, hands_restored) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        assert!(hands_restored.hands_tracking_enabled);
        assert!(!hands_restored.face_tracking_enabled);
        assert!(!hands_restored.auto_zoom_enabled);

        store.set_face_tracking(true).await.unwrap();
        let (_, face_restored) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        assert!(face_restored.face_tracking_enabled);
        assert!(!face_restored.hands_tracking_enabled);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
