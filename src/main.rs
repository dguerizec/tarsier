mod api;
mod config;
mod model;
mod pipeline;
mod runtime;
mod scenario;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use pipeline::{PreviewHub, VideoPipeline};
use runtime::Runtime;
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
    let _pipeline = VideoPipeline::start(config.video.clone(), runtime.clone(), preview.clone())
        .await
        .context("failed to start video pipeline")?;
    let app = api::router(config.clone(), runtime, preview);
    let listener = tokio::net::TcpListener::bind(config.server.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.server.bind))?;
    tracing::info!(address = %config.server.bind, "Tarsier control surface is ready");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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
