use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
    sync::{Mutex, watch},
    task::JoinHandle,
};

use crate::{
    config::VideoConfig,
    media_metadata::settings_metadata,
    model::{RuntimeState, unix_ms},
};

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RecordingStatus {
    pub active: bool,
    pub audio: bool,
    pub started_at_ms: Option<u64>,
    pub path: Option<PathBuf>,
    pub url: Option<String>,
    pub error: Option<String>,
}

#[derive(Default)]
struct State {
    shutting_down: bool,
    child: Option<Child>,
    writer: Option<JoinHandle<()>>,
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
        .unwrap_or(name)
        .strip_suffix(".mp4")
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
                if let Some(writer) = state.writer.take() {
                    writer.abort();
                }
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

    pub async fn start(
        &self,
        config: &VideoConfig,
        settings: &RuntimeState,
        mut frames: watch::Receiver<Option<gstreamer::Buffer>>,
    ) -> Result<RecordingStatus> {
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
        let audio = settings.audio_virtual.enabled;
        if audio && !settings.audio_virtual.running {
            bail!(
                "Tarsier Microphone is unavailable; wait for audio output or turn it off to record video only"
            );
        }
        let directory = directory()?;
        tokio::fs::create_dir_all(&directory).await?;
        let started = unix_ms();
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let (filename, path) = loop {
            let filename = format!(
                "{timestamp}-{}.mp4",
                SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let path = directory.join(&filename);
            if !tokio::fs::try_exists(&path).await? {
                break (filename, path);
            }
        };
        let mut metadata = settings_metadata(
            settings,
            started,
            "recording-start",
            None,
            (config.width, config.height),
        );
        metadata["output"]["recording_fps"] = serde_json::json!(config.fps);
        metadata["output"]["recording_audio"] = serde_json::json!(audio);
        let mut command = Command::new("ffmpeg");
        command
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-n",
                "-thread_queue_size",
                "512",
                "-f",
                "rawvideo",
                "-use_wallclock_as_timestamps",
                "1",
                "-pixel_format",
                "bgr0",
                "-video_size",
            ])
            .arg(format!("{}x{}", config.width, config.height))
            .arg("-framerate")
            .arg(config.fps.to_string())
            .arg("-i")
            .arg("pipe:0");
        if audio {
            command.args([
                "-thread_queue_size",
                "512",
                "-f",
                "pulse",
                "-isync",
                "0",
                "-name",
                "Tarsier recording",
                "-sample_rate",
                "48000",
                "-channels",
                "2",
                "-i",
                crate::audio::VIRTUAL_SOURCE,
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-af",
                "aresample=async=1",
            ]);
        } else {
            command.args(["-map", "0:v:0", "-an"]);
        }
        let mut child = command
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-crf",
                "20",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart+use_metadata_tags",
            ])
            .arg("-metadata")
            .arg(format!("tarsier_settings={metadata}"))
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("Could not start FFmpeg")?;
        let mut stdin = child
            .stdin
            .take()
            .context("FFmpeg video input is missing")?;
        state.writer = Some(tokio::spawn(async move {
            loop {
                if frames.changed().await.is_err() {
                    break;
                }
                let frame = frames.borrow_and_update().clone();
                if let Some(frame) = frame {
                    let Ok(map) = frame.into_mapped_buffer_readable() else {
                        break;
                    };
                    if stdin.write_all(map.as_slice()).await.is_err() {
                        break;
                    }
                }
            }
        }));
        state.child = Some(child);
        state.status = RecordingStatus {
            active: true,
            audio,
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
        // The owned, unreaped child cannot have its PID reused. SIGINT asks
        // FFmpeg to flush both encoders and write the MP4 trailer.
        if let Some(pid) = child.id() {
            unsafe {
                nix::libc::kill(pid as i32, nix::libc::SIGINT);
            }
        }
        if let Some(writer) = state.writer.take() {
            writer.abort();
        }
        let result = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
        state.status.active = false;
        match result {
            Ok(Ok(status)) if status.success() || status.code() == Some(255) => {}
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
        assert!(valid_filename("20260906-143025-7.mp4"));
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
    async fn unavailable_enabled_audio_is_not_silently_omitted() {
        let recorder = Recorder::default();
        let mut settings = RuntimeState::default();
        settings.audio_virtual.enabled = true;
        let error = recorder
            .start(&VideoConfig::default(), &settings, watch::channel(None).1)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Tarsier Microphone is unavailable")
        );
        assert!(!recorder.status().await.active);
    }

    #[tokio::test]
    async fn stop_is_idle_safe_and_missing_loopback_is_rejected() {
        let recorder = Recorder::default();
        assert!(!recorder.stop().await.unwrap().active);
        let config = VideoConfig {
            loopback_enabled: false,
            ..Default::default()
        };
        assert!(
            recorder
                .start(&config, &RuntimeState::default(), watch::channel(None).1)
                .await
                .is_err()
        );
        assert!(!recorder.status().await.active);
        recorder.shutdown().await;
        assert!(
            recorder
                .start(
                    &VideoConfig::default(),
                    &RuntimeState::default(),
                    watch::channel(None).1
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("shutting down")
        );
    }
}
