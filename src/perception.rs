use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};

use serde_json::json;
use tokio::{process::Command, sync::watch, task::JoinHandle, time::sleep};

use crate::{
    config::{AvatarConfig, DepthConfig, PerceptionConfig, PerceptionSource, VideoConfig},
    model::AvatarEngine,
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
        depth: DepthConfig,
        video: VideoConfig,
        server_address: SocketAddr,
        runtime: Runtime,
        worker_token: String,
    ) -> Option<Self> {
        if !config.enabled || !config.supervise_worker {
            return None;
        }
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(supervise(
            config,
            avatar,
            depth,
            video,
            (server_address, worker_token),
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
    depth: DepthConfig,
    video: VideoConfig,
    connection: (SocketAddr, String),
    runtime: Runtime,
    mut shutdown: watch::Receiver<bool>,
) {
    let (server_address, worker_token) = connection;
    let daemon_url = worker_daemon_url(server_address);
    let mut states = runtime.subscribe_state();
    loop {
        if *shutdown.borrow() {
            break;
        }
        if !states.borrow().pipeline.running {
            tokio::select! {
                changed = states.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
            continue;
        }
        let mut command = Command::new("uv");
        command
            .env("TARSIER_API_TOKEN", &worker_token)
            .args(worker_arguments(
                &config,
                &avatar,
                &depth,
                &video,
                &daemon_url,
            ))
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
                enum WorkerOutcome {
                    Exited(String),
                    PipelineStopped,
                    Shutdown,
                }
                let outcome = tokio::select! {
                    biased;
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            WorkerOutcome::Shutdown
                        } else {
                            continue;
                        }
                    }
                    () = wait_for_pipeline_stop(&mut states) => WorkerOutcome::PipelineStopped,
                    status = child.wait() => WorkerOutcome::Exited(match status {
                        Ok(status) => format!("perception worker exited with {status}"),
                        Err(error) => format!("failed to wait for perception worker: {error}"),
                    }),
                };
                match outcome {
                    WorkerOutcome::Exited(message) => mark_worker_offline(&runtime, message).await,
                    WorkerOutcome::PipelineStopped => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        mark_worker_paused(&runtime).await;
                        continue;
                    }
                    WorkerOutcome::Shutdown => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        runtime
                            .update(|state| state.perception.worker_connected = false)
                            .await;
                        break;
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

async fn wait_for_pipeline_stop(states: &mut watch::Receiver<crate::model::RuntimeState>) {
    loop {
        if !states.borrow().pipeline.running || states.changed().await.is_err() {
            break;
        }
    }
}

async fn mark_worker_paused(runtime: &Runtime) {
    runtime
        .update(|state| {
            state.perception.worker_connected = false;
            state.perception.error = None;
            state.camera.face_tracking.active = false;
            state.camera.face_tracking.target_visible = false;
            state.camera.face_tracking.target_x = None;
            state.camera.face_tracking.target_y = None;
            state.video_effects.avatar_available = false;
            state.video_effects.depth_available = false;
        })
        .await;
    runtime
        .emit(
            "perception.worker.paused",
            "supervisor",
            None,
            json!({"reason": "video-pipeline-stopped"}),
        )
        .await;
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
            state.video_effects.depth_available = false;
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
    depth: &DepthConfig,
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
        arguments.extend([
            "--extra".into(),
            "avatar".into(),
            "--extra".into(),
            "liveportrait".into(),
        ]);
    }
    if depth.enabled {
        arguments.extend(["--extra".into(), "depth".into()]);
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
        arguments.extend([
            "--avatar-engine".into(),
            match avatar.engine {
                AvatarEngine::Stylized3d => "stylized-3d".into(),
                AvatarEngine::Portrait3d => "portrait3d".into(),
                AvatarEngine::Liveportrait => "liveportrait".into(),
            },
            "--avatar-fps".into(),
            avatar.fps.to_string(),
            "--avatar-width".into(),
            video.width.to_string(),
            "--avatar-height".into(),
            video.height.to_string(),
            "--portrait-model".into(),
            project_path(&avatar.portrait_model)
                .to_string_lossy()
                .into_owned(),
            "--avatar-profile".into(),
            project_path(&avatar.profile).to_string_lossy().into_owned(),
            "--avatar-source".into(),
            project_path(&avatar.source_image)
                .to_string_lossy()
                .into_owned(),
        ]);
        if avatar.compile {
            arguments.push("--avatar-compile".into());
        }
    }
    if depth.enabled {
        arguments.extend([
            "--depth-enabled".into(),
            "--depth-fps".into(),
            depth.fps.to_string(),
            "--depth-input-height".into(),
            depth.input_height.to_string(),
        ]);
    }
    arguments
}

fn project_path(path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
    }
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

    #[tokio::test]
    async fn pipeline_stop_pauses_the_worker_without_recording_a_failure() {
        let runtime = Runtime::new();
        runtime
            .update(|state| {
                state.pipeline.running = true;
                state.perception.worker_connected = true;
                state.perception.error = Some("stale error".into());
            })
            .await;
        let mut states = runtime.subscribe_state();
        let stopped = tokio::spawn(async move {
            wait_for_pipeline_stop(&mut states).await;
        });

        runtime.update(|state| state.pipeline.running = false).await;
        tokio::time::timeout(Duration::from_secs(1), stopped)
            .await
            .unwrap()
            .unwrap();
        mark_worker_paused(&runtime).await;

        let state = runtime.state().await;
        assert!(!state.perception.worker_connected);
        assert_eq!(state.perception.error, None);
        let events = runtime.recent_events().await;
        assert_eq!(events.last().unwrap().kind, "perception.worker.paused");
    }

    #[test]
    fn worker_command_uses_configured_stream_and_loopback_api() {
        let config = PerceptionConfig::default();
        let args = worker_arguments(
            &config,
            &AvatarConfig::default(),
            &DepthConfig::default(),
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
            &DepthConfig::default(),
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
            engine: AvatarEngine::Liveportrait,
            profile: "assets/avatars/stylized-3d.json".into(),
            portrait_model: "assets/avatars/portrait/current".into(),
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
            &DepthConfig::default(),
            &video,
            "http://127.0.0.1:8742",
        );

        assert!(
            args.windows(2)
                .any(|pair| pair == ["--extra", "liveportrait"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--extra", "avatar"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--avatar-engine", "liveportrait"])
        );
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
    fn stylized_avatar_uses_the_opengl_runtime_and_profile() {
        let avatar = AvatarConfig {
            enabled: true,
            engine: AvatarEngine::Stylized3d,
            profile: "assets/avatars/stylized-3d.json".into(),
            portrait_model: "assets/avatars/portrait/current".into(),
            source_image: "assets/avatars/liveportrait-source.png".into(),
            fps: 30,
            compile: true,
        };
        let args = worker_arguments(
            &PerceptionConfig::default(),
            &avatar,
            &DepthConfig::default(),
            &VideoConfig::default(),
            "http://127.0.0.1:8742",
        );

        assert!(args.windows(2).any(|pair| pair == ["--extra", "avatar"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--extra", "liveportrait"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--avatar-engine", "stylized-3d"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--avatar-fps", "30"]));
        assert!(args.windows(2).any(
            |pair| pair[0] == "--avatar-profile" && pair[1].ends_with("stylized-3d.json")
        ));
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--avatar-source"
                    && pair[1].ends_with("liveportrait-source.png"))
        );
        assert!(args.iter().any(|argument| argument == "--avatar-compile"));
    }

    #[test]
    fn enabled_depth_adds_the_lazy_runtime_and_inference_geometry() {
        let depth = DepthConfig {
            enabled: true,
            fps: 30,
            input_height: 252,
        };
        let args = worker_arguments(
            &PerceptionConfig::default(),
            &AvatarConfig::default(),
            &depth,
            &VideoConfig::default(),
            "http://127.0.0.1:8742",
        );

        assert!(args.windows(2).any(|pair| pair == ["--extra", "depth"]));
        assert!(args.iter().any(|argument| argument == "--depth-enabled"));
        assert!(args.windows(2).any(|pair| pair == ["--depth-fps", "30"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--depth-input-height", "252"])
        );
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
