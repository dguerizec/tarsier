mod api;
mod background;
mod audio;
mod audio_gain;
mod audio_noise;
mod audit;
mod auth;
mod avatar_source;
mod avatar_import;
mod portrait_models;
mod camera;
mod config;
mod devices;
mod doctor;
mod effects;
mod face_tracking;
mod hands_tracking;
mod media_metadata;
mod model;
mod mute_media;
mod perception;
mod perception_demand;
mod phone_gesture;
mod pipeline;
mod shared_frames;
mod pipeline_reservation;
mod recording;
mod runtime;
mod scenario;
mod settings;
mod service;
mod telemetry;
mod telemetry_record;
mod gpu_process;
mod nvml_telemetry;
mod utterances;
mod video_clients;
mod video_transform;
mod voice;

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
    /// Install and enable this binary as the single user service.
    Install {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Start or restart the installed service immediately.
        #[arg(long)]
        now: bool,
        /// Explicitly replace a different or unmanaged installation.
        #[arg(long)]
        replace: bool,
    },
    /// Remove the managed service, preserving settings and media.
    Uninstall {
        /// Stop the running service before removing it.
        #[arg(long)]
        now: bool,
    },
    /// Diagnose the installation without opening the physical camera.
    Doctor {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Attempt safe repairs, without sudo or stopping processes.
        #[arg(long)]
        fix: bool,
    },
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
    /// Record resource and worker-stage samples to a bounded JSONL file.
    TelemetryRecord(telemetry_record::Options),
    /// Manage the optional master password locally, including forgotten-password recovery.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Validate and print the effective configuration.
    Config {
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Set or reset the master password using a hidden interactive prompt.
    SetPassword,
    /// Disable authentication and revoke every client token.
    Disable {
        #[arg(long)]
        confirm: bool,
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
        Command::Install {
            config,
            now,
            replace,
        } => service::install(config, now, replace).await,
        Command::Uninstall { now } => service::uninstall(now).await,
        Command::Doctor { config, fix } => doctor::run(config, fix).await,
        Command::Serve { config } => {
            audit::init()?;
            let result = serve(config).await;
            audit::record("daemon.exited", serde_json::json!({"success": result.is_ok()}));
            result
        }
        Command::Auth { command } => {
            let auth = auth::Auth::new(auth::default_path()?, &auth::worker_token())?;
            match command {
                AuthCommand::SetPassword => {
                    let password = rpassword::prompt_password("New master password: ")?;
                    let confirmation = rpassword::prompt_password("Confirm master password: ")?;
                    if password != confirmation {
                        bail!("Passwords do not match");
                    }
                    auth.reset_password(&password)?;
                    println!(
                        "Master password updated. Existing web sessions are invalidated; client tokens are preserved."
                    );
                }
                AuthCommand::Disable { confirm } => {
                    if !confirm {
                        bail!("Use --confirm to disable authentication and revoke every token");
                    }
                    auth.disable()?;
                    println!("Authentication disabled and all client tokens revoked.");
                }
            }
            Ok(())
        }
        Command::TelemetryRecord(options) => telemetry_record::run(options).await,
        Command::Status { url } => {
            let client = reqwest::Client::new();
            let mut request = client.get(format!("{url}/api/v1/state"));
            if let Ok(token) = std::env::var("TARSIER_API_TOKEN") {
                request = request.bearer_auth(token);
            }
            let state: serde_json::Value = request
                .send()
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
    let worker_token = auth::worker_token();
    let auth = auth::Auth::new(auth::default_path()?, &worker_token)?;
    let mut config = Config::load(path.as_deref())?;
    config.validate()?;
    let settings_path = settings::default_path()?;
    let (settings_store, mut user_settings) = settings::UserSettingsStore::load(
        settings_path,
        settings::UserSettings::from_config(&config),
    )
    .await?;
    if let Some(model) = &user_settings.portrait3d_model {
        config.avatar.portrait_model = model.clone();
    }
    if let Some(source) = &user_settings.liveportrait_source {
        config.avatar.source_image = source.clone();
    }
    if let Some(enabled) = user_settings.audio_reserve_inputs {
        config.audio.reserve_inputs = enabled;
    }
    config
        .audio
        .input_reservations
        .extend(user_settings.audio_input_reservations.clone());
    if let Some(camera) = &user_settings.camera_device {
        devices::apply_camera(&mut config, camera);
        config.audio.capture_selected_only = true;
    }
    if let Some(delegates) = &user_settings.perception_delegates {
        config.perception.delegates = delegates.clone();
    }
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
    if config.audio.voice_worker.is_empty() {
        user_settings.audio.voice_enabled = false;
    }
    if !config.audio.virtual_output_enabled {
        user_settings.audio.output_enabled = false;
    }
    user_settings
        .audio
        .capture_sources
        .retain(|source| config.audio.allows(source));
    let runtime = Runtime::new();
    let mut preview = PreviewHub::new();
    if config.perception.enabled && config.perception.supervise_worker
        && config.perception.source == config::PerceptionSource::Preview
        && config.perception.transport == config::PerceptionTransport::SharedMemory
    {
        preview.enable_shared_input(config.perception.width, config.perception.height)?;
    }
    let shared_source = preview.shared_source();
    // Never expose capture on startup before a connected client is manually approved.
    if config.video.loopback_enabled {
        user_settings.video_output_muted = true;
        settings_store.set_video_output_muted(true).await?;
    }
    preview.set_output_muted(user_settings.video_output_muted);
    if let Some(selection) = user_settings.video_mute_media.clone() {
        let directory = settings_store.mute_media_directory();
        let selected = selection.clone();
        let (width, height) = (config.video.width, config.video.height);
        let loaded = tokio::task::spawn_blocking(move || {
            crate::mute_media::Media::open(&selected, &directory, width, height)
        })
        .await?;
        match loaded {
            Ok(media) => preview.set_replacement(Some(selection), Some(media), None),
            Err(error) => {
                tracing::warn!(%error, "mute media unavailable; keeping black output fallback");
                preview.set_replacement(Some(selection), None, Some(error.to_string()));
            }
        }
    }
    let output_mode = user_settings.output_mode();
    let avatar_engine = user_settings
        .avatar_engine()
        .or(config.avatar.enabled.then_some(config.avatar.engine));
    preview.effects().set_output_mode(output_mode);
    preview
        .effects()
        .set_transform(user_settings.video_transform);
    preview.effects().set_background_plugin(&user_settings.background_plugin);
    preview.effects().set_background(
        user_settings.background_enabled,
        user_settings.background_effect,
    );
    runtime
        .update(|state| {
            user_settings.audio.apply(state);
            state.pipeline.output_muted = user_settings.video_output_muted;
            state.audio_virtual.output_id = config.audio.virtual_source.clone();
            state.video_effects.transform = user_settings.video_transform;
            state.video_effects.output_mode = output_mode;
            state.video_effects.avatar_engine = avatar_engine;
            state.video_effects.background_enabled = user_settings.background_enabled;
            state.video_effects.background_effect = user_settings.background_effect;
            state.video_effects.background_plugin = user_settings.background_plugin.clone();
            state.video_effects.green_screen_enabled = user_settings.background_enabled
                && user_settings.background_effect == crate::model::BackgroundEffect::GreenScreen;
        })
        .await;
    audit::record(
        "camera.capture.starting",
        serde_json::json!({"source": config.video.source, "reason": "daemon_startup"}),
    );
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
    telemetry::start(runtime.clone(), shutdown_rx.clone());
    let background_task = background::start(preview.effects().clone(), runtime.clone(), shutdown_rx.clone());
    let (daemon_restart, restart_rx) = if std::env::var_os("INVOCATION_ID").is_some() {
        let (restart_tx, restart_rx) = tokio::sync::oneshot::channel();
        (Some(api::DaemonRestart::new(restart_tx)), Some(restart_rx))
    } else {
        (None, None)
    };
    let recorder = recording::Recorder::default();
    let (audio, audio_task) =
        audio::AudioHub::start(config.audio.clone(), runtime.clone(), shutdown_rx.clone());
    let app = api::router_with_controls(
        config.clone(),
        runtime.clone(),
        preview,
        camera,
        api::ApiOptions {
            avatar_library: None,
            auth: Some(auth),
            audio: Some(audio),
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
        perception::WorkerConnection {
            server_address: config.server.bind,
            worker_token,
            shared_source,
        },
        runtime,
    );
    audit::record(
        "daemon.ready",
        serde_json::json!({"address": config.server.bind.to_string()}),
    );
    tracing::info!(address = %config.server.bind, "Tarsier control surface is ready");
    let restart_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let graceful_restart_requested = std::sync::Arc::clone(&restart_requested);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            () = shutdown_signal() => { audit::record("daemon.stopping", serde_json::json!({"reason": "signal"})); },
            () = restart_signal(restart_rx) => {
                audit::record("daemon.stopping", serde_json::json!({"reason": "restart_request"}));
                graceful_restart_requested
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        recorder.shutdown().await;
        let _ = shutdown_tx.send(true);
        let _ = audio_task.await;
        let _ = background_task.await;
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
