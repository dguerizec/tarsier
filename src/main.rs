mod api;
mod camera;
mod config;
mod effects;
mod face_tracking;
mod hands_tracking;
mod model;
mod perception;
mod pipeline;
mod recording;
mod runtime;
mod scenario;
mod settings;
mod video_transform;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::Config;
use pipeline::{PreviewHub, VideoPipeline};
use runtime::Runtime;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the local daemon and web control surface.
    Serve {
        /// TOML configuration file. Built-in safe defaults are used when omitted.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Read the daemon's current state over its public API.
    Status {
        #[arg(long, default_value = "http://127.0.0.1:8742")]
        url: String,
    },
    /// Validate and print the effective configuration.
    Config {
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("tarsier=info")),
        )
        .init();

    match Cli::parse().command {
        Command::Serve { config } => serve(config).await,
        Command::Status { url } => {
            let state: serde_json::Value = reqwest::get(format!("{url}/api/v1/state"))
                .await
                .context("failed to reach Tarsier daemon")?
                .error_for_status()?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&state)?);
            Ok(())
        }
        Command::Config { config } => {
            let config = Config::load(config.as_deref())?;
            println!("{}", toml::to_string_pretty(&config)?);
            Ok(())
        }
    }
}

async fn serve(path: Option<PathBuf>) -> Result<()> {
    let mut config = Config::load(path.as_deref())?;
    config.validate()?;
    let settings_path = settings::default_path()?;
    let (settings_store, mut user_settings) = settings::UserSettingsStore::load(
        settings_path,
        settings::UserSettings::from_config(&config),
    )
    .await?;
    if let Some(lan_access) = user_settings.network_lan_access {
        config.server.bind.set_ip(if lan_access {
            std::net::Ipv4Addr::UNSPECIFIED.into()
        } else {
            std::net::Ipv4Addr::LOCALHOST.into()
        });
    }
    if let Some(resolution) = user_settings.video_resolution {
        config.video.width = resolution.width;
        config.video.height = resolution.height;
    }
    if config.video.width >= 3840 {
        user_settings.video_identity = crate::model::VideoIdentity::Camera;
        user_settings.background_enabled = false;
        user_settings.video_transform = Default::default();
        config.avatar.enabled = false;
        config.depth.enabled = false;
    }
    let runtime = Runtime::new();
    let preview = PreviewHub::new();
    let output_mode = user_settings.output_mode();
    let avatar_engine = user_settings
        .avatar_engine()
        .or(config.avatar.enabled.then_some(config.avatar.engine));
    preview.effects().set_output_mode(output_mode);
    preview
        .effects()
        .set_transform(user_settings.video_transform);
    preview.effects().set_background(
        user_settings.background_enabled,
        user_settings.background_effect,
    );
    runtime
        .update(|state| {
            state.video_effects.transform = user_settings.video_transform;
            state.video_effects.output_mode = output_mode;
            state.video_effects.avatar_engine = avatar_engine;
            state.video_effects.background_enabled = user_settings.background_enabled;
            state.video_effects.background_effect = user_settings.background_effect;
            state.video_effects.green_screen_enabled = user_settings.background_enabled
                && user_settings.background_effect == crate::model::BackgroundEffect::GreenScreen;
        })
        .await;
    let pipeline = VideoPipeline::start(config.video.clone(), runtime.clone(), preview.clone())
        .await
        .context("failed to start video pipeline")?;
    let pipeline_control = pipeline.control();
    let camera = camera::start(config.camera.clone(), runtime.clone())
        .await
        .context("failed to start camera adapter")?;
    let local_tracking_enabled =
        user_settings.face_tracking_enabled || user_settings.hands_tracking_enabled;
    if local_tracking_enabled {
        let camera = camera
            .as_ref()
            .context("cannot restore local tracking without a camera adapter")?;
        camera
            .set_tracking(false)
            .await
            .context("failed to disable built-in tracking while restoring local tracking")?;
        camera
            .set_face_tracking_speed(0, 0, 0.0)
            .await
            .context("failed to initialize restored local tracking")?;
        runtime
            .update(|state| {
                state.camera.tracking = Some(false);
                if user_settings.face_tracking_enabled {
                    state.camera.face_tracking = model::FaceTrackingState {
                        enabled: true,
                        auto_zoom: model::AutoZoomState {
                            enabled: user_settings.auto_zoom_enabled,
                            zoom_magnification: camera.controlled_zoom_magnification(),
                            ..model::AutoZoomState::default()
                        },
                        ..model::FaceTrackingState::default()
                    };
                } else {
                    state.camera.hands_tracking = model::HandsTrackingState {
                        enabled: true,
                        zoom_frozen: true,
                        zoom_magnification: camera.controlled_zoom_magnification(),
                        ..model::HandsTrackingState::default()
                    };
                }
            })
            .await;
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (daemon_restart, restart_rx) = if std::env::var_os("INVOCATION_ID").is_some() {
        let (restart_tx, restart_rx) = tokio::sync::oneshot::channel();
        (Some(api::DaemonRestart::new(restart_tx)), Some(restart_rx))
    } else {
        (None, None)
    };
    let recorder = recording::Recorder::default();
    let app = api::router_with_controls(
        config.clone(),
        runtime.clone(),
        preview,
        camera,
        api::ApiOptions {
            recorder: recorder.clone(),
            pipeline: Some(pipeline_control),
            daemon_restart,
            user_settings: Some(settings_store),
            face_tracking_enabled: user_settings.face_tracking_enabled,
            auto_zoom_enabled: user_settings.face_tracking_enabled
                && user_settings.auto_zoom_enabled,
            hands_tracking_enabled: user_settings.hands_tracking_enabled,
        },
        shutdown_rx,
    );
    let listener = tokio::net::TcpListener::bind(config.server.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.server.bind))?;
    let perception = perception::PerceptionSupervisor::start(
        config.perception.clone(),
        config.avatar.clone(),
        config.depth.clone(),
        config.video.clone(),
        config.server.bind,
        runtime,
    );
    tracing::info!(address = %config.server.bind, "Tarsier control surface is ready");
    let restart_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let graceful_restart_requested = std::sync::Arc::clone(&restart_requested);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                () = shutdown_signal() => {},
                () = restart_signal(restart_rx) => {
                    graceful_restart_requested
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            recorder.shutdown().await;
            let _ = shutdown_tx.send(true);
            if let Some(perception) = perception {
                // Stop the worker before Axum drains any remaining requests.
                perception.shutdown().await;
            }
        })
        .await?;
    if restart_requested.load(std::sync::atomic::Ordering::Relaxed) {
        bail!("daemon restart requested");
    }
    Ok(())
}

async fn restart_signal(receiver: Option<tokio::sync::oneshot::Receiver<()>>) {
    let Some(receiver) = receiver else {
        std::future::pending::<()>().await;
        return;
    };
    if receiver.await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
