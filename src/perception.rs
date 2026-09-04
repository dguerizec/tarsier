use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};

use serde_json::json;
use tokio::{process::Command, sync::watch, task::JoinHandle, time::sleep};

use crate::{config::PerceptionConfig, runtime::Runtime};

pub struct PerceptionSupervisor {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl PerceptionSupervisor {
    pub fn start(
        config: PerceptionConfig,
        server_address: SocketAddr,
        runtime: Runtime,
    ) -> Option<Self> {
        if !config.enabled || !config.supervise_worker {
            return None;
        }
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(supervise(config, server_address, runtime, receiver));
        Some(Self { shutdown, task })
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

async fn supervise(
    config: PerceptionConfig,
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
            .args(worker_arguments(&config, &daemon_url))
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

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

fn worker_arguments(config: &PerceptionConfig, daemon_url: &str) -> Vec<String> {
    let project = if config.worker_project.is_absolute() {
        config.worker_project.clone()
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(&config.worker_project)
    };
    vec![
        "run".into(),
        "--project".into(),
        project.to_string_lossy().into_owned(),
        "--locked".into(),
        "tarsier-perception".into(),
        "--daemon-url".into(),
        daemon_url.into(),
        "serve".into(),
        "--device".into(),
        config.device.clone(),
        "--width".into(),
        config.width.to_string(),
        "--height".into(),
        config.height.to_string(),
        "--fps".into(),
        config.fps.to_string(),
        "--minimum-confidence".into(),
        config.detection_confidence.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_command_uses_configured_stream_and_loopback_api() {
        let config = PerceptionConfig::default();
        let args = worker_arguments(&config, "http://127.0.0.1:8742");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--device", "/dev/video42"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--daemon-url", "http://127.0.0.1:8742"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--fps", "10"]));
        assert!(args.iter().any(|argument| argument == "--locked"));
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
