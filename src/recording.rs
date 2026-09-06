use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
    sync::Mutex,
};

use crate::{config::VideoConfig, model::unix_ms};

#[derive(Clone, Debug, Default, Serialize)]
pub struct RecordingStatus {
    pub active: bool,
    pub started_at_ms: Option<u64>,
    pub path: Option<PathBuf>,
    pub url: Option<String>,
    pub error: Option<String>,
}

#[derive(Default)]
struct State {
    shutting_down: bool,
    child: Option<Child>,
    status: RecordingStatus,
}

#[derive(Clone, Default)]
pub struct Recorder(Arc<Mutex<State>>);

pub fn directory() -> Result<PathBuf> {
    std::env::var_os("TARSIER_VIDEOS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Videos/Tarsier")))
        .context("Set TARSIER_VIDEOS_DIR or HOME to save videos")
}

pub fn valid_filename(name: &str) -> bool {
    name.strip_prefix("video-")
        .and_then(|name| name.strip_suffix(".mp4"))
        .is_some_and(|name| {
            let parts: Vec<_> = name.split('-').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
}

impl Recorder {
    async fn refresh(state: &mut State) {
        let Some(child) = &mut state.child else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                state.child = None;
                state.status.active = false;
                if !status.success() {
                    state.status.error = Some(format!(
                        "Video recording failed ({status}). See the daemon log for details."
                    ));
                    state.status.url = None;
                }
            }
            Err(error) => state.status.error = Some(error.to_string()),
            Ok(None) => {}
        }
    }

    pub async fn status(&self) -> RecordingStatus {
        let mut state = self.0.lock().await;
        Self::refresh(&mut state).await;
        state.status.clone()
    }

    pub async fn start(&self, config: &VideoConfig) -> Result<RecordingStatus> {
        let mut state = self.0.lock().await;
        Self::refresh(&mut state).await;
        if state.shutting_down {
            bail!("The daemon is shutting down");
        }
        if state.status.active {
            bail!("A recording is already in progress");
        }
        if !config.loopback_enabled {
            bail!("Video recording requires the virtual camera output");
        }
        let directory = directory()?;
        tokio::fs::create_dir_all(&directory).await?;
        let started = unix_ms();
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let filename = format!(
            "video-{started}-{}-{}.mp4",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let path = directory.join(&filename);
        let child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-n",
                "-f",
                "v4l2",
                "-input_format",
                "yuyv422",
                "-video_size",
            ])
            .arg(format!("{}x{}", config.width, config.height))
            .arg("-framerate")
            .arg(config.fps.to_string())
            .arg("-i")
            .arg(&config.output_device)
            .args([
                "-an",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-crf",
                "20",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
            ])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("Could not start FFmpeg")?;
        state.child = Some(child);
        state.status = RecordingStatus {
            active: true,
            started_at_ms: Some(started),
            path: Some(path),
            url: Some(format!("/api/v1/video/recordings/{filename}")),
            error: None,
        };
        // Catch immediate device/encoder failures before reporting a started recording.
        tokio::time::sleep(Duration::from_millis(300)).await;
        Self::refresh(&mut state).await;
        if let Some(error) = &state.status.error {
            bail!("{error}");
        }
        Ok(state.status.clone())
    }

    pub async fn shutdown(&self) {
        self.0.lock().await.shutting_down = true;
        let _ = self.stop().await;
    }

    pub async fn stop(&self) -> Result<RecordingStatus> {
        let mut state = self.0.lock().await;
        Self::refresh(&mut state).await;
        let Some(mut child) = state.child.take() else {
            return Ok(state.status.clone());
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"q\n").await;
        }
        let result = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
        state.status.active = false;
        match result {
            Ok(Ok(status)) if status.success() => {}
            other => {
                let _ = child.kill().await;
                state.status.url = None;
                state.status.error = Some(format!("Could not finalize the video: {other:?}"));
            }
        }
        Ok(state.status.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recording_names_are_confined() {
        assert!(valid_filename("video-123-456-0.mp4"));
        for name in [
            "../video-123-456-0.mp4",
            "video-1-2-3.mp4/other",
            "video--2-3.mp4",
            "other.mp4",
        ] {
            assert!(!valid_filename(name));
        }
    }
    #[tokio::test]
    async fn stop_is_idle_safe_and_missing_loopback_is_rejected() {
        let recorder = Recorder::default();
        assert!(!recorder.stop().await.unwrap().active);
        let config = VideoConfig {
            loopback_enabled: false,
            ..Default::default()
        };
        assert!(recorder.start(&config).await.is_err());
        assert!(!recorder.status().await.active);
        recorder.shutdown().await;
        assert!(
            recorder
                .start(&VideoConfig::default())
                .await
                .unwrap_err()
                .to_string()
                .contains("shutting down")
        );
    }
}
