mod api;
mod camera;
mod config;
mod effects;
mod face_tracking;
mod model;
mod perception;
mod pipeline;
mod runtime;
mod scenario;

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
    let config = Config::load(path.as_deref())?;
    config.validate()?;
    let runtime = Runtime::new();
    let preview = PreviewHub::new();
    preview.effects().set_output_mode(config.video.output_mode);
    preview.effects().set_background(
        config.video.background_enabled,
        config.video.background_effect,
    );
    runtime
        .update(|state| {
            state.video_effects.output_mode = config.video.output_mode;
            state.video_effects.avatar_engine =
                config.avatar.enabled.then_some(config.avatar.engine);
            state.video_effects.background_enabled = config.video.background_enabled;
            state.video_effects.background_effect = config.video.background_effect;
            state.video_effects.green_screen_enabled = config.video.background_enabled
                && config.video.background_effect == crate::model::BackgroundEffect::GreenScreen;
        })
        .await;
    let pipeline = VideoPipeline::start(config.video.clone(), runtime.clone(), preview.clone())
        .await
        .context("failed to start video pipeline")?;
    let pipeline_control = pipeline.control();
    let camera = camera::start(config.camera.clone(), runtime.clone())
        .await
        .context("failed to start camera adapter")?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (daemon_restart, restart_rx) = if std::env::var_os("INVOCATION_ID").is_some() {
        let (restart_tx, restart_rx) = tokio::sync::oneshot::channel();
        (Some(api::DaemonRestart::new(restart_tx)), Some(restart_rx))
    } else {
        (None, None)
    };
    let app = api::router_with_pipeline(
        config.clone(),
        runtime.clone(),
        preview,
        camera,
        Some(pipeline_control),
        daemon_restart,
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
