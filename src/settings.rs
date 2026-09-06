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

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct AudioSettings {
    pub capture_sources: Vec<String>,
    pub output_source: Option<String>,
    pub output_enabled: bool,
    pub output_muted: bool,
}

impl AudioSettings {
    pub fn from_state(state: &crate::model::RuntimeState) -> Self {
        Self {
            capture_sources: state.audio_capture_sources.clone(),
            output_source: state.audio_virtual.source.clone(),
            output_enabled: state.audio_virtual.enabled,
            output_muted: state.audio_virtual.muted,
        }
    }

    pub fn apply(&self, state: &mut crate::model::RuntimeState) {
        state.audio_capture_sources = self.capture_sources.clone();
        state.audio_virtual.source = self.output_source.clone();
        state.audio_virtual.enabled = self.output_enabled;
        state.audio_virtual.muted = self.output_muted;
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserSettings {
    #[serde(default)]
    pub audio: AudioSettings,
    #[serde(default)]
    pub network_lan_access: Option<bool>,
    #[serde(default)]
    pub video_resolution: Option<VideoResolution>,
    #[serde(default)]
    pub video_transform: crate::video_transform::VideoTransform,
    version: u32,
    pub video_identity: VideoIdentity,
    pub background_enabled: bool,
    pub background_effect: BackgroundEffect,
    pub face_tracking_enabled: bool,
    #[serde(default)]
    pub auto_zoom_enabled: bool,
    #[serde(default)]
    pub hands_tracking_enabled: bool,
}

impl UserSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            audio: AudioSettings::default(),
            version: SETTINGS_VERSION,
            network_lan_access: None,
            video_resolution: None,
            video_transform: Default::default(),
            video_identity: identity_from_mode(config.video.output_mode, config.avatar.engine),
            background_enabled: config.video.background_enabled,
            background_effect: config.video.background_effect,
            face_tracking_enabled: false,
            auto_zoom_enabled: false,
            hands_tracking_enabled: false,
        }
    }

    pub fn output_mode(&self) -> VideoOutputMode {
        match self.video_identity {
            VideoIdentity::Camera => VideoOutputMode::Camera,
            VideoIdentity::DepthMap => VideoOutputMode::DepthMap,
            VideoIdentity::Stylized3d | VideoIdentity::Liveportrait => VideoOutputMode::ComicAvatar,
        }
    }

    pub fn avatar_engine(&self) -> Option<AvatarEngine> {
        match self.video_identity {
            VideoIdentity::Stylized3d => Some(AvatarEngine::Stylized3d),
            VideoIdentity::Liveportrait => Some(AvatarEngine::Liveportrait),
            VideoIdentity::Camera | VideoIdentity::DepthMap => None,
        }
    }

    fn validate(self) -> Result<Self> {
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
            AvatarEngine::Stylized3d => VideoIdentity::Stylized3d,
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

    pub async fn set_audio(&self, audio: AudioSettings) -> Result<()> {
        self.replace(|settings| settings.audio = audio).await
    }

    pub async fn set_video_identity(&self, identity: VideoIdentity) -> Result<()> {
        self.replace(|settings| settings.video_identity = identity)
            .await
    }

    pub async fn set_video_transform(
        &self,
        transform: crate::video_transform::VideoTransform,
    ) -> Result<()> {
        self.replace(|settings| settings.video_transform = transform)
            .await
    }

    pub async fn set_background(&self, enabled: bool, effect: BackgroundEffect) -> Result<()> {
        self.replace(|settings| {
            settings.background_enabled = enabled;
            settings.background_effect = effect;
        })
        .await
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
            capture_sources: vec!["disconnected-mic".into()],
            output_source: Some("disabled-mic".into()),
            output_enabled: true,
            output_muted: true,
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
