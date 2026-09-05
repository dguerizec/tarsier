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
pub struct UserSettings {
    version: u32,
    pub video_identity: VideoIdentity,
    pub background_enabled: bool,
    pub background_effect: BackgroundEffect,
    pub face_tracking_enabled: bool,
}

impl UserSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            version: SETTINGS_VERSION,
            video_identity: identity_from_mode(config.video.output_mode, config.avatar.engine),
            background_enabled: config.video.background_enabled,
            background_effect: config.video.background_effect,
            face_tracking_enabled: false,
        }
    }

    pub fn output_mode(self) -> VideoOutputMode {
        match self.video_identity {
            VideoIdentity::Camera => VideoOutputMode::Camera,
            VideoIdentity::DepthMap => VideoOutputMode::DepthMap,
            VideoIdentity::Stylized3d | VideoIdentity::Liveportrait => VideoOutputMode::ComicAvatar,
        }
    }

    pub fn avatar_engine(self) -> Option<AvatarEngine> {
        match self.video_identity {
            VideoIdentity::Stylized3d => Some(AvatarEngine::Stylized3d),
            VideoIdentity::Liveportrait => Some(AvatarEngine::Liveportrait),
            VideoIdentity::Camera | VideoIdentity::DepthMap => None,
        }
    }

    fn validate(self) -> Result<Self> {
        if self.version != SETTINGS_VERSION {
            bail!(
                "unsupported user settings version {}, expected {SETTINGS_VERSION}",
                self.version
            );
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
                current: Arc::new(Mutex::new(settings)),
            },
            settings,
        ))
    }

    pub async fn set_video_identity(&self, identity: VideoIdentity) -> Result<()> {
        self.replace(|settings| settings.video_identity = identity)
            .await
    }

    pub async fn set_background(&self, enabled: bool, effect: BackgroundEffect) -> Result<()> {
        self.replace(|settings| {
            settings.background_enabled = enabled;
            settings.background_effect = effect;
        })
        .await
    }

    pub async fn set_face_tracking(&self, enabled: bool) -> Result<()> {
        self.replace(|settings| settings.face_tracking_enabled = enabled)
            .await
    }

    async fn replace(&self, update: impl FnOnce(&mut UserSettings)) -> Result<()> {
        let mut current = self.current.lock().await;
        let mut next = *current;
        update(&mut next);
        let path = Arc::clone(&self.path);
        tokio::task::spawn_blocking(move || persist(&path, next))
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
    async fn settings_are_atomically_persisted_and_loaded() {
        let path = test_path("round-trip");
        let fallback = UserSettings::from_config(&Config::default());
        let (store, loaded) = UserSettingsStore::load(path.clone(), fallback)
            .await
            .unwrap();
        assert_eq!(loaded, fallback);

        store
            .set_video_identity(VideoIdentity::Liveportrait)
            .await
            .unwrap();
        store
            .set_background(true, BackgroundEffect::Blur)
            .await
            .unwrap();
        store.set_face_tracking(true).await.unwrap();

        let (_, restored) = UserSettingsStore::load(path.clone(), fallback)
            .await
            .unwrap();
        assert_eq!(restored.video_identity, VideoIdentity::Liveportrait);
        assert!(restored.background_enabled);
        assert_eq!(restored.background_effect, BackgroundEffect::Blur);
        assert!(restored.face_tracking_enabled);
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
}
