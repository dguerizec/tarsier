use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};

use serde_json::json;
use tokio::{process::Command, sync::watch, task::JoinHandle, time::sleep};

use crate::{
    config::{AvatarConfig, PerceptionConfig, PerceptionSource, VideoConfig},
    runtime::Runtime,
};

pub struct PerceptionSupervisor {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl PerceptionSupervisor {
    pub fn start(
        config: PerceptionConfig,
        avatar: AvatarConfig,
        video: VideoConfig,
        server_address: SocketAddr,
        runtime: Runtime,
    ) -> Option<Self> {
        if !config.enabled || !config.supervise_worker {
            return None;
        }
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(supervise(
            config,
            avatar,
            video,
            server_address,
            runtime,
            receiver,
        ));
        Some(Self { shutdown, task })
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

async fn supervise(
    config: PerceptionConfig,
    avatar: AvatarConfig,
    video: VideoConfig,
    server_address: SocketAddr,
    runtime: Runtime,
    mut shutdown: watch::Receiver<bool>,
) {
    let daemon_url = worker_daemon_url(server_address);
    loop {
        if *shutdown.borrow() {
            break;
        }
        let mut command = Command::new("uv");
        command
            .args(worker_arguments(&config, &avatar, &video, &daemon_url))
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        close_inherited_file_descriptors(&mut command);

        match command.spawn() {
            Ok(mut child) => {
                let pid = child.id();
                runtime.update(|state| state.perception.error = None).await;
                runtime
                    .emit(
                        "perception.worker.started",
                        "supervisor",
                        None,
                        json!({"pid": pid}),
                    )
                    .await;
                tokio::select! {
                    status = child.wait() => {
                        let message = match status {
                            Ok(status) => format!("perception worker exited with {status}"),
                            Err(error) => format!("failed to wait for perception worker: {error}"),
                        };
                        mark_worker_offline(&runtime, message).await;
                    }
                    changed = shutdown.changed() => {
                        if changed.is_ok() && *shutdown.borrow() {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            runtime
                                .update(|state| state.perception.worker_connected = false)
                                .await;
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                mark_worker_offline(
                    &runtime,
                    format!("failed to start perception worker: {error}"),
                )
                .await;
            }
        }

        tokio::select! {
            () = sleep(Duration::from_millis(config.restart_delay_ms)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

async fn mark_worker_offline(runtime: &Runtime, message: String) {
    tracing::warn!(error = %message, "perception worker is offline");
    runtime
        .update(|state| {
            state.perception.worker_connected = false;
            state.perception.error = Some(message.clone());
            state.camera.face_tracking.active = false;
            state.camera.face_tracking.target_visible = false;
            state.camera.face_tracking.target_x = None;
            state.camera.face_tracking.target_y = None;
            state.video_effects.avatar_available = false;
        })
        .await;
    runtime
        .emit(
            "perception.worker.offline",
            "supervisor",
            None,
            json!({"error": message}),
        )
        .await;
}

fn worker_daemon_url(address: SocketAddr) -> String {
    let host = match address.ip() {
        IpAddr::V4(_) => "127.0.0.1",
        IpAddr::V6(_) => "[::1]",
    };
    format!("http://{host}:{}", address.port())
}

fn worker_arguments(
    config: &PerceptionConfig,
    avatar: &AvatarConfig,
    video: &VideoConfig,
    daemon_url: &str,
) -> Vec<String> {
    let project = if config.worker_project.is_absolute() {
        config.worker_project.clone()
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(&config.worker_project)
    };
    let mut arguments = vec![
        "run".into(),
        "--project".into(),
        project.to_string_lossy().into_owned(),
        "--locked".into(),
    ];
    if avatar.enabled {
        arguments.extend(["--extra".into(), "avatar".into()]);
    }
    arguments.extend([
        "tarsier-perception".into(),
        "--daemon-url".into(),
        daemon_url.into(),
        "serve".into(),
        "--width".into(),
        config.width.to_string(),
        "--height".into(),
        config.height.to_string(),
        "--fps".into(),
        config.fps.to_string(),
        "--mask-fps".into(),
        config.mask_fps.to_string(),
        "--minimum-confidence".into(),
        config.detection_confidence.to_string(),
    ]);
    if config.source == PerceptionSource::Device {
        arguments.push("--source".into());
        arguments.push(config.device.clone());
    }
    if avatar.enabled {
        let source_image = if avatar.source_image.is_absolute() {
            avatar.source_image.clone()
        } else {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(&avatar.source_image)
        };
        arguments.extend([
            "--avatar-source".into(),
            source_image.to_string_lossy().into_owned(),
            "--avatar-fps".into(),
            avatar.fps.to_string(),
            "--avatar-width".into(),
            video.width.to_string(),
            "--avatar-height".into(),
            video.height.to_string(),
        ]);
        if avatar.compile {
            arguments.push("--avatar-compile".into());
        }
    }
    arguments
}

#[cfg(target_os = "linux")]
fn close_inherited_file_descriptors(command: &mut Command) {
    // GStreamer may leave device descriptors without FD_CLOEXEC. Mark every
    // non-stdio descriptor close-on-exec so the worker cannot inherit camera ownership.
    unsafe {
        command.pre_exec(|| {
            let result = nix::libc::syscall(
                nix::libc::SYS_close_range,
                3_u32,
                u32::MAX,
                nix::libc::CLOSE_RANGE_CLOEXEC,
            );
            if result == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn close_inherited_file_descriptors(_: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_command_uses_configured_stream_and_loopback_api() {
        let config = PerceptionConfig::default();
        let args = worker_arguments(
            &config,
            &AvatarConfig::default(),
            &VideoConfig::default(),
            "http://127.0.0.1:8742",
        );
        assert!(!args.iter().any(|argument| argument == "--source"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--daemon-url", "http://127.0.0.1:8742"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--fps", "10"]));
        assert!(args.windows(2).any(|pair| pair == ["--mask-fps", "30"]));
        assert!(args.iter().any(|argument| argument == "--locked"));
    }

    #[test]
    fn device_source_is_passed_explicitly() {
        let config = PerceptionConfig {
            source: PerceptionSource::Device,
            device: "/dev/video43".into(),
            ..PerceptionConfig::default()
        };
        let args = worker_arguments(
            &config,
            &AvatarConfig::default(),
            &VideoConfig::default(),
            "http://127.0.0.1:8742",
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--source", "/dev/video43"])
        );
    }

    #[test]
    fn enabled_avatar_adds_the_optional_runtime_and_output_geometry() {
        let avatar = AvatarConfig {
            enabled: true,
            source_image: "assets/avatars/liveportrait-source.png".into(),
            fps: 15,
            compile: true,
        };
        let video = VideoConfig {
            width: 1280,
            height: 720,
            ..VideoConfig::default()
        };
        let args = worker_arguments(
            &PerceptionConfig::default(),
            &avatar,
            &video,
            "http://127.0.0.1:8742",
        );

        assert!(args.windows(2).any(|pair| pair == ["--extra", "avatar"]));
        assert!(args.windows(2).any(|pair| pair == ["--avatar-fps", "15"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--avatar-width", "1280"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--avatar-height", "720"])
        );
        assert!(args.iter().any(|argument| argument == "--avatar-compile"));
    }

    #[test]
    fn wildcard_bind_addresses_become_loopback_worker_urls() {
        assert_eq!(
            worker_daemon_url("0.0.0.0:9000".parse().unwrap()),
            "http://127.0.0.1:9000"
        );
        assert_eq!(
            worker_daemon_url("[::]:9000".parse().unwrap()),
            "http://[::1]:9000"
        );
    }
}
