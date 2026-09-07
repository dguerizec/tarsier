use std::{convert::Infallible, mem::size_of_val, sync::Arc, time::Instant};

use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, oneshot, watch};
use tower_http::trace::TraceLayer;

use crate::{
    camera::{CameraHandle, PAN_TILT_LEASE},
    config::{CameraPresetConfig, Config, ScenarioConfig},
    effects::{AvatarFrame, DepthFrame, MAX_AVATAR_FRAME_BYTES, MAX_DEPTH_FRAME_BYTES, VideoMask},
    face_tracking::{FaceTrackingController, MANUAL_ZOOM_SETTLE_MS},
    hands_tracking::HandsTrackingController,
    model::{
        AutoZoomState, AvatarEngine, BackgroundEffect, BuiltInGesture, CameraAttitudeSource,
        CameraImageControl, FaceTrackingState, FaceTrackingTarget, HandsTrackingState, Landmark,
        PerceptionObservation, ScenarioActivation, VideoIdentity, VideoOutputMode, unix_ms,
    },
    pipeline::{PerceptionFrame, PreviewHub, VideoPipelineControl},
    runtime::Runtime,
    scenario::{FacePresenceStabilizer, OpenPalmStabilizer, PresenceChange},
    settings::UserSettingsStore,
};

#[derive(Clone)]
struct ApiState {
    video_applications: crate::video_clients::Monitor,
    auth: Option<crate::auth::Auth>,
    recorder: crate::recording::Recorder,
    audio: Option<crate::audio::AudioHub>,
    config: Config,
    runtime: Runtime,
    stabilizer: Arc<Mutex<OpenPalmStabilizer>>,
    face_presence: Arc<Mutex<FacePresenceStabilizer>>,
    preview: PreviewHub,
    camera: Option<CameraHandle>,
    pipeline: Option<VideoPipelineControl>,
    camera_power_control: Arc<Mutex<()>>,
    pan_tilt_motion: Arc<Mutex<PanTiltMotion>>,
    face_tracking: Arc<Mutex<FaceTrackingController>>,
    hands_tracking: Arc<Mutex<HandsTrackingController>>,
    video_output_control: Arc<Mutex<()>>,
    daemon_restart: Option<DaemonRestart>,
    user_settings: Option<UserSettingsStore>,
    audio_settings_control: Arc<Mutex<()>>,
    portrait_selection_control: Arc<Mutex<()>>,
    liveportrait: Arc<Mutex<crate::avatar_source::LivePortraitState>>,
    shutdown: watch::Receiver<bool>,
}

#[derive(Clone)]
pub struct DaemonRestart {
    sender: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl DaemonRestart {
    pub fn new(sender: oneshot::Sender<()>) -> Self {
        Self {
            sender: Arc::new(Mutex::new(Some(sender))),
        }
    }

    async fn request(&self) -> Result<(), &'static str> {
        let Some(sender) = self.sender.lock().await.take() else {
            return Err("daemon restart is already in progress");
        };
        sender
            .send(())
            .map_err(|_| "daemon restart control is unavailable")
    }
}

#[derive(Default)]
pub struct ApiOptions {
    pub auth: Option<crate::auth::Auth>,
    pub audio: Option<crate::audio::AudioHub>,
    pub recorder: crate::recording::Recorder,
    pub pipeline: Option<VideoPipelineControl>,
    pub daemon_restart: Option<DaemonRestart>,
    pub user_settings: Option<UserSettingsStore>,
    pub face_tracking_enabled: bool,
    pub auto_zoom_enabled: bool,
    pub hands_tracking_enabled: bool,
}

async fn video_applications(State(state): State<ApiState>) -> Response {
    let device = if state.config.video.loopback_enabled {
        state.config.video.output_device.clone()
    } else {
        String::new()
    };
    match state.video_applications.applications(device).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(error) => command_error(error),
    }
}

async fn mcp_recenter(state: State<ApiState>) -> Response {
    camera_action(state, axum::extract::Path("recenter".to_owned())).await
}

async fn guard_mcp_commands(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if request.method() == axum::http::Method::POST
        && request.uri().path().starts_with("/mcp/")
        && state.config.video.loopback_enabled
    {
        let snapshot = state
            .video_applications
            .fresh_applications(state.config.video.output_device.clone())
            .await;
        if let Some(response) = mcp_usage_rejection(snapshot) {
            return response;
        }
    }
    next.run(request).await
}

pub(crate) fn mcp_usage_rejection(
    snapshot: anyhow::Result<crate::video_clients::Snapshot>,
) -> Option<Response> {
    let (status, message) = match snapshot {
        Ok(snapshot) if snapshot.capture_active == Some(true) || !snapshot.applications.is_empty() => (
            StatusCode::CONFLICT,
            "MCP commands are blocked while an application is using the virtual camera. Close the virtual camera in that application before trying again.",
        ),
        Ok(snapshot) if snapshot.available && snapshot.capture_active.is_none() => (
            StatusCode::SERVICE_UNAVAILABLE,
            "MCP commands are blocked because the virtual camera driver could not report capture activity.",
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "MCP commands are blocked because virtual camera usage could not be inspected.",
        ),
        Ok(_) => return None,
    };
    Some((status, Json(json!({"error": message}))).into_response())
}

#[cfg(test)]
pub fn router(
    config: Config,
    runtime: Runtime,
    preview: PreviewHub,
    camera: Option<CameraHandle>,
    shutdown: watch::Receiver<bool>,
) -> Router {
    router_with_controls(
        config,
        runtime,
        preview,
        camera,
        ApiOptions::default(),
        shutdown,
    )
}

pub fn router_with_controls(
    config: Config,
    runtime: Runtime,
    preview: PreviewHub,
    camera: Option<CameraHandle>,
    options: ApiOptions,
    shutdown: watch::Receiver<bool>,
) -> Router {
    let mut face_tracking = FaceTrackingController::default();
    face_tracking.set_enabled(options.face_tracking_enabled);
    face_tracking.set_auto_zoom_enabled(options.auto_zoom_enabled, unix_ms());
    let mut hands_tracking = HandsTrackingController::default();
    hands_tracking.set_enabled(options.hands_tracking_enabled);
    let auth = options.auth.clone();
    let liveportrait = Arc::new(Mutex::new(crate::avatar_source::LivePortraitState::new(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(&config.avatar.source_image),
    )));
    let state = ApiState {
        auth: options.auth,
        recorder: options.recorder,
        audio: options.audio,
        stabilizer: Arc::new(Mutex::new(OpenPalmStabilizer::new(&config.perception))),
        face_presence: Arc::new(Mutex::new(FacePresenceStabilizer::new(&config.perception))),
        config,
        runtime,
        preview,
        camera,
        pipeline: options.pipeline,
        camera_power_control: Arc::new(Mutex::new(())),
        pan_tilt_motion: Arc::new(Mutex::new(PanTiltMotion::default())),
        face_tracking: Arc::new(Mutex::new(face_tracking)),
        hands_tracking: Arc::new(Mutex::new(hands_tracking)),
        video_output_control: Arc::new(Mutex::new(())),
        daemon_restart: options.daemon_restart,
        user_settings: options.user_settings,
        audio_settings_control: Arc::new(Mutex::new(())),
        portrait_selection_control: Arc::new(Mutex::new(())),
        liveportrait,
        video_applications: crate::video_clients::Monitor::default(),
        shutdown,
    };
    let router = Router::new()
        .route("/", get(index))
        .route("/settings", get(settings_page))
        .route(
            "/api/v1/video/liveportrait/source",
            get(liveportrait_sources).post(set_liveportrait_source),
        )
        .route("/api/v1/video/liveportrait/source/{id}", get(liveportrait_thumbnail))
        .route("/api/v1/video/liveportrait/status", get(liveportrait_status).post(report_liveportrait_error))
        .route("/assets/logo.svg", get(logo_svg))
        .route("/assets/favicon.svg", get(favicon_svg))
        .route("/assets/settings.js", get(settings_js))
        .route(
            "/api/v1/settings/network",
            get(network_settings).post(set_network_settings),
        )
        .route("/assets/app.js", get(app_js))
        .route("/assets/preview-drag.js", get(preview_drag_js))
        .route(
            "/assets/audio.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    include_str!("../web/audio.js"),
                )
            }),
        )
        .route("/api/v1/video/applications", get(video_applications))
        .route(
            "/assets/video-applications.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    include_str!("../web/video-applications.js"),
                )
            }),
        )
        .route(
            "/api/v1/settings/devices",
            get(device_settings).post(set_device_settings),
        )
        .route(
            "/api/v1/audio/voice",
            get(voice_settings).post(set_voice_settings),
        )
        .route(
            "/api/v1/audio/voice/models",
            get(voice_models)
                .post(upload_voice_model)
                .layer(DefaultBodyLimit::max(128 * 1024 * 1024)),
        )
        .route("/api/v1/audio/sources", get(audio_sources))
        .route("/api/v1/audio/meter", get(audio_meter))
        .route("/api/v1/audio/capture", post(set_audio_capture))
        .route("/api/v1/audio/exclusive", post(set_audio_exclusive))
        .route("/api/v1/audio/applications", get(audio_applications))
        .route(
            "/api/v1/audio/applications/kill",
            post(kill_audio_application),
        )
        .route(
            "/api/v1/audio/virtual",
            get(virtual_audio).post(set_virtual_audio),
        )
        .route("/assets/lucide.js", get(lucide_js))
        .route("/assets/styles.css", get(styles_css))
        .route("/api/v1/health", get(health))
        .route("/api/v1/daemon/restart", post(restart_daemon))
        .route("/api/v1/state", get(current_state))
        .route("/api/v1/camera/state", get(camera_state))
        .route("/api/v1/camera/power", post(set_camera_power))
        .route("/api/v1/camera/move", post(move_camera))
        .route("/api/v1/camera/nudge/{direction}", post(nudge_camera))
        .route("/api/v1/camera/zoom", post(set_zoom))
        .route(
            "/api/v1/camera/image-settings/{control}",
            post(set_image_control),
        )
        .route("/api/v1/camera/auto-zoom", post(set_auto_zoom))
        .route("/api/v1/camera/hdr", post(set_hdr))
        .route("/api/v1/camera/tracking", post(set_tracking))
        .route("/api/v1/camera/face-tracking", post(set_face_tracking))
        .route("/api/v1/camera/hands-tracking", post(set_hands_tracking))
        .route(
            "/api/v1/camera/built-in-gestures/{feature}",
            post(set_built_in_gesture),
        )
        .route("/api/v1/camera/actions/{action}", post(camera_action))
        .route("/api/v1/camera/presets", get(camera_presets))
        .route(
            "/api/v1/camera/presets/{id}/recall",
            post(recall_camera_preset),
        )
        .route("/api/v1/config", get(effective_config))
        .route("/api/v1/events", get(events_socket))
        .route("/api/v1/events/recent", get(recent_events))
        .route("/api/v1/scenarios", get(scenarios))
        .route("/api/v1/scenarios/{id}/trigger", post(trigger_scenario))
        .route(
            "/api/v1/perception/observations",
            post(perception_observation),
        )
        .route("/api/v1/perception/mask", post(perception_mask))
        .route(
            "/api/v1/avatar/frame",
            post(avatar_frame).layer(DefaultBodyLimit::max(MAX_AVATAR_FRAME_BYTES)),
        )
        .route(
            "/api/v1/depth/frame",
            get(latest_depth)
                .post(depth_frame)
                .layer(DefaultBodyLimit::max(MAX_DEPTH_FRAME_BYTES)),
        )
        .route(
            "/api/v1/perception/input.mjpeg",
            get(perception_input_mjpeg),
        )
        .route(
            "/api/v1/video/recording",
            get(recording_status).post(start_recording),
        )
        .route("/api/v1/video/recording/stop", post(stop_recording))
        .route("/api/v1/video/recordings/{filename}", get(saved_recording))
        .route(
            "/api/v1/video/recordings/{filename}/open",
            post(open_saved_recording),
        )
        .route("/api/v1/video/resolution", post(set_resolution))
        .route("/api/v1/video/background", post(set_background))
        .route(
            "/api/v1/video/transform",
            get(current_transform).post(set_transform),
        )
        .route(
            "/api/v1/video/identity",
            get(current_identity).post(set_identity),
        )
        .route("/api/v1/video/output-mode", post(set_output_mode))
        .route("/api/v1/video/green-screen", post(set_green_screen))
        .route("/api/v1/preview.mjpeg", get(preview_mjpeg))
        .route("/api/v1/camera/snapshot", get(snapshot))
        .route("/api/v1/camera/photos", post(take_photo))
        .route("/api/v1/camera/photos/{filename}", get(saved_photo))
        .route(
            "/api/v1/camera/photos/{filename}/open",
            post(open_saved_photo),
        )
        // Only the eleven gateway operations are exposed on the MCP destination.
        .route("/mcp/api/v1/state", get(current_state))
        .route("/mcp/api/v1/config", get(effective_config))
        .route("/mcp/api/v1/events/recent", get(recent_events))
        .route("/mcp/api/v1/scenarios", get(scenarios))
        .route("/mcp/api/v1/camera/presets", get(camera_presets))
        .route("/mcp/api/v1/camera/snapshot", get(snapshot))
        .route("/mcp/api/v1/camera/move", post(move_camera))
        .route("/mcp/api/v1/camera/tracking", post(set_tracking))
        .route("/mcp/api/v1/camera/actions/recenter", post(mcp_recenter))
        .route("/mcp/api/v1/camera/presets/{id}/recall", post(recall_camera_preset))
        .route("/mcp/api/v1/scenarios/{id}/trigger", post(trigger_scenario))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            guard_mcp_commands,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    if let Some(auth) = auth {
        router
            .merge(crate::auth::routes(auth.clone()))
            .layer(axum::middleware::from_fn_with_state(
                auth,
                crate::auth::guard,
            ))
    } else {
        router
    }
}

async fn index() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("../web/index.html")),
    )
}

async fn settings_page() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("../web/settings.html")),
    )
}

async fn settings_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../web/settings.js"),
    )
}

async fn portrait_catalog(state: &ApiState) -> anyhow::Result<Vec<crate::avatar_source::Portrait>> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut directories = vec![root.join("assets/avatars")];
    if let Some(settings) = &state.user_settings {
        directories.push(settings.portrait_directory());
    }
    let current = state.liveportrait.lock().await.source.clone();
    tokio::task::spawn_blocking(move || crate::avatar_source::catalog(&directories, &current))
        .await?
}

async fn liveportrait_sources(State(state): State<ApiState>) -> Response {
    match portrait_catalog(&state).await {
        Ok(portraits) => ([(header::CACHE_CONTROL, "no-store")], Json(portraits)).into_response(),
        Err(error) => command_error(error),
    }
}

async fn liveportrait_thumbnail(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let portraits = match portrait_catalog(&state).await {
        Ok(portraits) => portraits,
        Err(error) => return command_error(error),
    };
    let Some(portrait) = portraits.into_iter().find(|portrait| portrait.id == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::task::spawn_blocking(move || crate::avatar_source::thumbnail(&portrait.path)).await
    {
        Ok(Ok(bytes)) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        _ => StatusCode::UNPROCESSABLE_ENTITY.into_response(),
    }
}

#[derive(Deserialize)]
struct PortraitSelection {
    id: String,
}

async fn set_liveportrait_source(
    State(state): State<ApiState>,
    Json(selection): Json<PortraitSelection>,
) -> Response {
    let _selection = state.portrait_selection_control.lock().await;
    if !state.config.avatar.enabled {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Avatar output is disabled"})),
        )
            .into_response();
    }
    let Some(settings) = &state.user_settings else {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Portrait changes require persistent settings"})),
        )
            .into_response();
    };
    let portraits = match portrait_catalog(&state).await {
        Ok(portraits) => portraits,
        Err(error) => return command_error(error),
    };
    let Some(portrait) = portraits
        .into_iter()
        .find(|portrait| portrait.id == selection.id)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "This portrait is no longer available"})),
        )
            .into_response();
    };
    let path = portrait.path;
    let validation_path = path.clone();
    if !matches!(
        tokio::task::spawn_blocking(move || crate::avatar_source::thumbnail(&validation_path))
            .await,
        Ok(Ok(_))
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "This file is not a supported portrait image"})),
        )
            .into_response();
    }
    if let Err(error) = settings.set_liveportrait_source(path.clone()).await {
        return user_settings_error(error);
    }
    let mut portrait = state.liveportrait.lock().await;
    portrait.select(path);
    (StatusCode::ACCEPTED, Json(portrait.clone())).into_response()
}

async fn liveportrait_status(
    State(state): State<ApiState>,
) -> Json<crate::avatar_source::LivePortraitState> {
    Json(state.liveportrait.lock().await.clone())
}

#[derive(Deserialize)]
struct PortraitError {
    revision: u64,
    error: String,
}

async fn report_liveportrait_error(
    State(state): State<ApiState>,
    Json(error): Json<PortraitError>,
) -> StatusCode {
    let mut portrait = state.liveportrait.lock().await;
    if portrait.revision != error.revision || portrait.active_revision == error.revision {
        return StatusCode::CONFLICT;
    }
    portrait.error = Some(error.error.chars().take(500).collect());
    StatusCode::NO_CONTENT
}

async fn device_settings(State(state): State<ApiState>) -> Response {
    let cameras = match crate::devices::cameras() {
        Ok(devices) => devices,
        Err(error) => return command_error(error.into()),
    };
    let microphones = match crate::audio::sources(&state.config.audio).await {
        Ok(devices) => devices,
        Err(error) => return command_error(error),
    };
    let runtime = state.runtime.state().await;
    Json(json!({
        "cameras": cameras, "microphones": microphones,
        "camera": if state.config.video.source == crate::config::VideoSource::Camera { state.config.video.input_device.as_str() } else { "" },
        "capture_sources": runtime.audio_capture_sources,
        "output_source": runtime.audio_virtual.source,
        "can_apply": state.daemon_restart.is_some() && state.user_settings.is_some(),
        "started_at_ms": runtime.started_at_ms,
    })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceSettingsRequest {
    camera: String,
    capture_sources: Vec<String>,
    output_source: Option<String>,
}

async fn set_device_settings(
    State(state): State<ApiState>,
    Json(request): Json<DeviceSettingsRequest>,
) -> Response {
    let _video = state.video_output_control.lock().await;
    let _audio = state.audio_settings_control.lock().await;
    if state.recorder.status().await.active {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Stop recording before changing devices"})),
        )
            .into_response();
    }
    let (Some(restart), Some(settings)) = (&state.daemon_restart, &state.user_settings) else {
        return (StatusCode::CONFLICT, Json(json!({"error": "Device changes require a supervised daemon and persistent settings"}))).into_response();
    };
    let cameras = match crate::devices::cameras() {
        Ok(c) => c,
        Err(e) => return command_error(e.into()),
    };
    if !request.camera.is_empty()
        && !cameras.iter().any(|c| c.id == request.camera)
        && !(state.config.video.source == crate::config::VideoSource::Camera
            && request.camera == state.config.video.input_device)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Select an available camera"})),
        )
            .into_response();
    }
    let microphones = match crate::audio::sources(&state.config.audio).await {
        Ok(m) => m,
        Err(e) => return command_error(e),
    };
    let current = state.runtime.state().await;
    if request.capture_sources.iter().any(|id| {
        !state.config.audio.allows(id)
            || (!microphones.iter().any(|m| &m.id == id)
                && !current.audio_capture_sources.contains(id))
    }) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Select available microphones"})),
        )
            .into_response();
    }
    if request
        .output_source
        .as_ref()
        .is_some_and(|id| !request.capture_sources.contains(id))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "The output microphone must be selected for capture"})),
        )
            .into_response();
    }
    let mut audio = crate::settings::AudioSettings::from_state(&current);
    audio.capture_sources = request.capture_sources;
    audio.capture_sources.sort();
    audio.capture_sources.dedup();
    audio.output_source = request.output_source;
    if audio.output_source.is_none() {
        audio.output_enabled = false;
    }
    if let Err(error) = settings.set_devices(request.camera, audio).await {
        return user_settings_error(error);
    }
    if let Err(error) = restart.request().await {
        return (StatusCode::CONFLICT, Json(json!({"error": error}))).into_response();
    }
    (StatusCode::ACCEPTED, Json(json!({"restarting": true}))).into_response()
}

async fn network_settings(State(state): State<ApiState>) -> Json<Value> {
    Json(json!({
        "lan_access": !state.config.server.bind.ip().is_loopback(),
        "bind": state.config.server.bind.to_string(),
        "port": state.config.server.bind.port(),
        "can_apply": state.daemon_restart.is_some() && state.user_settings.is_some(),
        "started_at_ms": state.runtime.state().await.started_at_ms,
    }))
}

#[derive(Deserialize)]
struct NetworkSettingsRequest {
    lan_access: bool,
}

async fn set_network_settings(
    State(state): State<ApiState>,
    Json(request): Json<NetworkSettingsRequest>,
) -> Response {
    let _guard = state.video_output_control.lock().await;
    if state.recorder.status().await.active {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Stop recording before changing network access"})),
        )
            .into_response();
    }
    let (Some(restart), Some(settings)) = (&state.daemon_restart, &state.user_settings) else {
        return (StatusCode::CONFLICT, Json(json!({"error": "Network changes require a supervised daemon and persistent settings"}))).into_response();
    };
    if let Err(error) = settings.set_network_lan_access(request.lan_access).await {
        return user_settings_error(error);
    }
    if let Err(error) = restart.request().await {
        return (StatusCode::CONFLICT, Json(json!({"error": error}))).into_response();
    }
    (StatusCode::ACCEPTED, Json(json!({"restarting": true}))).into_response()
}

async fn app_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../web/app.js"),
    )
}

async fn preview_drag_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../web/preview-drag.js"),
    )
}

async fn lucide_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../web/vendor/lucide.js"),
    )
}

async fn logo_svg() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../web/logo.svg"),
    )
}

async fn favicon_svg() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../web/favicon.svg"),
    )
}

async fn styles_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../web/styles.css"),
    )
}

async fn health(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.runtime.state().await;
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "status": if snapshot.pipeline.error.is_some()
                || snapshot.camera.error.is_some()
                || snapshot.camera.power_error.is_some()
                || snapshot.perception.error.is_some()
            {
                "degraded"
            } else {
                "ok"
            },
            "version": snapshot.version,
            "started_at_ms": snapshot.started_at_ms,
            "uptime_ms": unix_ms().saturating_sub(snapshot.started_at_ms),
            "restart_available": state.daemon_restart.is_some(),
        })),
    )
}

async fn restart_daemon(State(state): State<ApiState>) -> Response {
    let Some(restart) = state.daemon_restart else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "daemon restart requires a supervised service"})),
        )
            .into_response();
    };
    match restart.request().await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(json!({"error": error}))).into_response(),
    }
}

async fn current_state(State(state): State<ApiState>) -> Json<crate::model::RuntimeState> {
    Json(state.runtime.state().await)
}

async fn camera_state(State(state): State<ApiState>) -> Json<crate::model::CameraState> {
    Json(state.runtime.state().await.camera)
}

async fn camera_presets(State(state): State<ApiState>) -> Json<Vec<CameraPresetConfig>> {
    Json(state.config.presets)
}

#[derive(Deserialize)]
struct MoveCameraRequest {
    yaw: f32,
    pitch: f32,
    #[serde(default)]
    roll: f32,
}

#[derive(Deserialize)]
struct TrackingRequest {
    enabled: bool,
}

#[derive(Deserialize)]
struct CameraPowerRequest {
    enabled: bool,
}

async fn set_camera_power(
    State(state): State<ApiState>,
    Json(request): Json<CameraPowerRequest>,
) -> Response {
    let _power_change = state.camera_power_control.lock().await;
    let _video_change = state.video_output_control.lock().await;
    if !request.enabled {
        let _ = state.recorder.stop().await;
    }
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    let Some(pipeline) = state.pipeline.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "video pipeline control is unavailable"})),
        )
            .into_response();
    };

    if request.enabled {
        if !camera.is_powered_on()
            && let Err(error) = camera.set_powered_on(true).await
        {
            record_camera_power_error(&state, &error).await;
            return command_error(error);
        }
        state
            .runtime
            .update(|runtime| {
                runtime.camera.powered_on = Some(true);
                runtime.camera.power_error = None;
            })
            .await;
        if let Err(error) = pipeline.set_enabled(true).await {
            record_camera_power_error(&state, &error).await;
            return command_error(error);
        }
    } else {
        if camera.is_powered_on() {
            let source = state.runtime.state().await.audio_virtual.source;
            if source
                .as_deref()
                .is_some_and(|source| !crate::audio::is_camera_source(source))
            {
                let response = set_virtual_audio_locked(
                    state.clone(),
                    VirtualAudioRequest {
                        auto_gain: None,
                        enabled: None,
                        source: None,
                        muted: Some(true),
                    },
                )
                .await;
                if !response.status().is_success() {
                    return response;
                }
            }
            if let Err(error) = camera.set_face_tracking_speed(0, 0, 0.0).await {
                record_camera_power_error(&state, &error).await;
                return command_error(error);
            }
            state.face_tracking.lock().await.set_enabled(false);
            state.hands_tracking.lock().await.set_enabled(false);
            clear_pan_tilt_motion(&state).await;
            state
                .runtime
                .update(|runtime| {
                    runtime.camera.face_tracking = FaceTrackingState::default();
                    runtime.camera.hands_tracking = HandsTrackingState::default();
                })
                .await;
            if let Err(error) = save_face_tracking_setting(&state, false).await {
                return user_settings_error(error);
            }
            if let Err(error) = save_hands_tracking_setting(&state, false).await {
                return user_settings_error(error);
            }
            camera.begin_power_transition();
        }
        if let Err(error) = pipeline.set_enabled(false).await {
            let rollback_error = pipeline.set_enabled(true).await.err();
            camera.end_power_transition();
            let error = if let Some(rollback_error) = rollback_error {
                anyhow::anyhow!(
                    "failed to stop video capture: {error}; video pipeline rollback failed: {rollback_error}"
                )
            } else {
                anyhow::anyhow!("failed to stop video capture: {error}")
            };
            record_camera_power_error(&state, &error).await;
            return command_error(error);
        }
        if camera.is_powered_on()
            && let Err(error) = camera.set_powered_on(false).await
        {
            let rollback_error = pipeline.set_enabled(true).await.err();
            camera.end_power_transition();
            let error = if let Some(rollback_error) = rollback_error {
                anyhow::anyhow!(
                    "failed to put camera to sleep: {error}; video pipeline rollback failed: {rollback_error}"
                )
            } else {
                anyhow::anyhow!("failed to put camera to sleep: {error}")
            };
            record_camera_power_error(&state, &error).await;
            return command_error(error);
        }
        camera.end_power_transition();
        state
            .runtime
            .update(|runtime| {
                runtime.camera.powered_on = Some(false);
                runtime.camera.power_error = None;
                runtime.camera.face_tracking = FaceTrackingState::default();
                runtime.camera.hands_tracking = HandsTrackingState::default();
                runtime.camera.yaw_degrees = None;
                runtime.camera.pitch_degrees = None;
                runtime.camera.roll_degrees = None;
                runtime.camera.euler_yaw_degrees = None;
                runtime.camera.euler_pitch_degrees = None;
                runtime.camera.euler_roll_degrees = None;
                runtime.camera.yaw_velocity_degrees_per_second = None;
                runtime.camera.pitch_velocity_degrees_per_second = None;
                runtime.camera.roll_velocity_degrees_per_second = None;
                runtime.camera.attitude_source = CameraAttitudeSource::Unavailable;
                runtime.camera.sample_at_ms = None;
                runtime.camera.telemetry_error = None;
                runtime.perception.worker_connected = false;
                runtime.perception.error = None;
            })
            .await;
    }

    record_camera_command(&state, "camera.power", json!({"enabled": request.enabled})).await;
    StatusCode::ACCEPTED.into_response()
}

async fn record_camera_power_error(state: &ApiState, error: &anyhow::Error) {
    state
        .runtime
        .update(|runtime| runtime.camera.power_error = Some(error.to_string()))
        .await;
}

#[derive(Deserialize)]
struct HdrRequest {
    enabled: bool,
}

#[derive(Deserialize)]
struct BuiltInGestureRequest {
    enabled: bool,
}

#[derive(Deserialize)]
struct ZoomRequest {
    magnification: f32,
}

#[derive(Deserialize)]
struct AutoZoomRequest {
    enabled: bool,
}

#[derive(Deserialize)]
struct ImageControlRequest {
    value: i32,
}

#[derive(Deserialize)]
struct GreenScreenRequest {
    enabled: bool,
}

#[derive(Deserialize)]
struct BackgroundRequest {
    enabled: bool,
    effect: BackgroundEffect,
}

#[derive(Deserialize)]
struct OutputModeRequest {
    mode: VideoOutputMode,
}

#[derive(Deserialize)]
struct IdentityRequest {
    identity: VideoIdentity,
}

#[derive(Serialize)]
struct IdentityResponse {
    liveportrait: crate::avatar_source::LivePortraitState,
    identity: VideoIdentity,
    background_enabled: bool,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum PanTiltDirection {
    UpLeft,
    UpRight,
    DownLeft,
    DownRight,
    Left,
    Right,
    Up,
    Down,
    Stop,
}

#[derive(Default)]
struct PanTiltMotion {
    direction: Option<PanTiltDirection>,
    expires_at: Option<Instant>,
}

impl PanTiltDirection {
    fn vector(self) -> (i8, i8) {
        match self {
            Self::UpLeft => (-1, 1),
            Self::UpRight => (1, 1),
            Self::DownLeft => (-1, -1),
            Self::DownRight => (1, -1),
            Self::Left => (-1, 0),
            Self::Right => (1, 0),
            Self::Up => (0, 1),
            Self::Down => (0, -1),
            Self::Stop => (0, 0),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::UpLeft => "up-left",
            Self::UpRight => "up-right",
            Self::DownLeft => "down-left",
            Self::DownRight => "down-right",
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
            Self::Stop => "stop",
        }
    }
}

async fn move_camera(
    State(state): State<ApiState>,
    Json(request): Json<MoveCameraRequest>,
) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    if let Err(error) = disable_tracking_for_manual_control(&state, &camera).await {
        return command_error(error);
    }
    match camera
        .move_to(request.yaw, request.pitch, request.roll)
        .await
    {
        Ok(()) => {
            record_commanded_attitude(&state, request.yaw, request.pitch, request.roll).await;
            record_camera_command(
                &state,
                "camera.move",
                json!({"yaw": request.yaw, "pitch": request.pitch, "roll": request.roll}),
            )
            .await;
            StatusCode::ACCEPTED.into_response()
        }
        Err(error) => command_error(error),
    }
}

async fn nudge_camera(
    State(state): State<ApiState>,
    axum::extract::Path(direction): axum::extract::Path<PanTiltDirection>,
) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    if direction != PanTiltDirection::Stop
        && let Err(error) = disable_tracking_for_manual_control(&state, &camera).await
    {
        return command_error(error);
    }
    let (pan_direction, tilt_direction) = direction.vector();
    match camera
        .set_pan_tilt_speed(pan_direction, tilt_direction)
        .await
    {
        Ok(()) => {
            let now = Instant::now();
            let previous = {
                let mut motion = state.pan_tilt_motion.lock().await;
                let previous = if motion.expires_at.is_some_and(|expires_at| expires_at > now) {
                    motion.direction
                } else {
                    None
                };
                if direction == PanTiltDirection::Stop {
                    motion.direction = None;
                    motion.expires_at = None;
                } else {
                    motion.direction = Some(direction);
                    motion.expires_at = Some(now + PAN_TILT_LEASE);
                }
                previous
            };

            if direction == PanTiltDirection::Stop {
                if let Some(previous) = previous {
                    record_camera_command(
                        &state,
                        "camera.nudge.stopped",
                        json!({"direction": previous.as_str()}),
                    )
                    .await;
                }
            } else if previous != Some(direction) {
                state
                    .runtime
                    .update(|runtime| {
                        runtime.camera.yaw_degrees = None;
                        runtime.camera.pitch_degrees = None;
                        runtime.camera.roll_degrees = None;
                        runtime.camera.euler_yaw_degrees = None;
                        runtime.camera.euler_pitch_degrees = None;
                        runtime.camera.euler_roll_degrees = None;
                        runtime.camera.yaw_velocity_degrees_per_second = None;
                        runtime.camera.pitch_velocity_degrees_per_second = None;
                        runtime.camera.roll_velocity_degrees_per_second = None;
                        runtime.camera.attitude_source = CameraAttitudeSource::Unavailable;
                        runtime.camera.sample_at_ms = None;
                    })
                    .await;
                record_camera_command(
                    &state,
                    "camera.nudge",
                    json!({"direction": direction.as_str()}),
                )
                .await;
            }
            StatusCode::ACCEPTED.into_response()
        }
        Err(error) => command_error(error),
    }
}

async fn set_tracking(
    State(state): State<ApiState>,
    Json(request): Json<TrackingRequest>,
) -> Response {
    set_tracking_inner(&state, request.enabled).await
}

async fn set_face_tracking(
    State(state): State<ApiState>,
    Json(request): Json<TrackingRequest>,
) -> Response {
    set_face_tracking_inner(&state, request.enabled).await
}

async fn set_hands_tracking(
    State(state): State<ApiState>,
    Json(request): Json<TrackingRequest>,
) -> Response {
    set_hands_tracking_inner(&state, request.enabled).await
}

async fn set_hdr(State(state): State<ApiState>, Json(request): Json<HdrRequest>) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    match camera.set_hdr(request.enabled).await {
        Ok(()) => {
            state
                .runtime
                .update(|runtime| {
                    runtime.camera.hdr = Some(request.enabled);
                    runtime.camera.hdr_sample_at_ms = None;
                    runtime.camera.hdr_error = None;
                })
                .await;
            record_camera_command(&state, "camera.hdr", json!({"enabled": request.enabled})).await;
            StatusCode::ACCEPTED.into_response()
        }
        Err(error) => command_error(error),
    }
}

async fn set_built_in_gesture(
    State(state): State<ApiState>,
    axum::extract::Path(feature): axum::extract::Path<BuiltInGesture>,
    Json(request): Json<BuiltInGestureRequest>,
) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    match camera.set_built_in_gesture(feature, request.enabled).await {
        Ok(()) => {
            state
                .runtime
                .update(|runtime| {
                    runtime
                        .camera
                        .built_in_gestures
                        .set(feature, request.enabled);
                    runtime.camera.built_in_gestures.sample_at_ms = None;
                    runtime.camera.built_in_gestures.error = None;
                })
                .await;
            record_camera_command(
                &state,
                "camera.built_in_gesture",
                json!({"feature": feature, "enabled": request.enabled}),
            )
            .await;
            StatusCode::ACCEPTED.into_response()
        }
        Err(error) => command_error(error),
    }
}

async fn set_zoom(State(state): State<ApiState>, Json(request): Json<ZoomRequest>) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    match camera.set_zoom(request.magnification).await {
        Ok(()) => {
            let now = unix_ms();
            let auto_zoom_enabled = {
                let mut face_tracking = state.face_tracking.lock().await;
                face_tracking
                    .recalibrate_auto_zoom_after(now.saturating_add(MANUAL_ZOOM_SETTLE_MS));
                face_tracking.auto_zoom_enabled()
            };
            let hands_tracking_enabled = {
                let mut hands_tracking = state.hands_tracking.lock().await;
                hands_tracking.recalibrate_zoom_after(now.saturating_add(MANUAL_ZOOM_SETTLE_MS));
                hands_tracking.enabled()
            };
            state
                .runtime
                .update(|runtime| {
                    runtime.camera.zoom_magnification = Some(request.magnification);
                    runtime.camera.zoom_sample_at_ms = None;
                    runtime.camera.zoom_error = None;
                    if auto_zoom_enabled {
                        runtime.camera.face_tracking.auto_zoom = AutoZoomState {
                            enabled: true,
                            zoom_magnification: Some(request.magnification),
                            ..AutoZoomState::default()
                        };
                    }
                    if hands_tracking_enabled {
                        runtime.camera.hands_tracking.calibrated = false;
                        runtime.camera.hands_tracking.zoom_magnification =
                            Some(request.magnification);
                        runtime.camera.hands_tracking.target_span = None;
                        runtime.camera.hands_tracking.hand_span = None;
                        runtime.camera.hands_tracking.at_limit = false;
                        runtime.camera.hands_tracking.zoom_error = None;
                    }
                })
                .await;
            record_camera_command(
                &state,
                "camera.zoom",
                json!({"magnification": request.magnification}),
            )
            .await;
            StatusCode::ACCEPTED.into_response()
        }
        Err(error) => command_error(error),
    }
}

async fn set_image_control(
    State(state): State<ApiState>,
    axum::extract::Path(control): axum::extract::Path<CameraImageControl>,
    Json(request): Json<ImageControlRequest>,
) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    match camera.set_image_control(control, request.value).await {
        Ok(controls) => {
            let readback = controls
                .iter()
                .find(|state| state.control == control)
                .and_then(|state| state.value);
            state
                .runtime
                .update(|runtime| {
                    for control in controls {
                        runtime.camera.image_settings.upsert(control);
                    }
                    runtime.camera.image_settings.error = None;
                })
                .await;
            record_camera_command(
                &state,
                "camera.image_setting",
                json!({"control": control, "value": request.value, "readback": readback}),
            )
            .await;
            (
                StatusCode::ACCEPTED,
                Json(json!({"control": control, "value": readback})),
            )
                .into_response()
        }
        Err(error) => command_error(error),
    }
}

async fn set_auto_zoom(
    State(state): State<ApiState>,
    Json(request): Json<AutoZoomRequest>,
) -> Response {
    if state.camera.is_none() {
        return camera_unavailable();
    }
    let controlled_zoom = state
        .camera
        .as_ref()
        .and_then(CameraHandle::controlled_zoom_magnification);
    if request.enabled && controlled_zoom.is_none() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "current camera zoom is unavailable"})),
        )
            .into_response();
    }
    let mut face_tracking = state.face_tracking.lock().await;
    if request.enabled && !face_tracking.enabled() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "enable face tracking before auto zoom"})),
        )
            .into_response();
    }
    face_tracking.set_auto_zoom_enabled(request.enabled, unix_ms());
    drop(face_tracking);
    state
        .runtime
        .update(|runtime| {
            runtime.camera.face_tracking.auto_zoom = AutoZoomState {
                enabled: request.enabled,
                zoom_magnification: request.enabled.then_some(controlled_zoom).flatten(),
                ..AutoZoomState::default()
            };
        })
        .await;
    if let Err(error) = save_auto_zoom_setting(&state, request.enabled).await {
        return user_settings_error(error);
    }
    record_camera_command(
        &state,
        "camera.auto_zoom",
        json!({"enabled": request.enabled}),
    )
    .await;
    StatusCode::ACCEPTED.into_response()
}

async fn camera_action(
    State(state): State<ApiState>,
    axum::extract::Path(action): axum::extract::Path<String>,
) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    match action.as_str() {
        "recenter" => {
            if let Err(error) = disable_tracking_for_manual_control(&state, &camera).await {
                return command_error(error);
            }
            match camera.recenter().await {
                Ok(()) => {
                    record_commanded_attitude(&state, 0.0, 0.0, 0.0).await;
                    record_camera_command(&state, "camera.recenter", Value::Null).await;
                    StatusCode::ACCEPTED.into_response()
                }
                Err(error) => command_error(error),
            }
        }
        "tracking-on" => set_tracking_inner(&state, true).await,
        "tracking-off" => set_tracking_inner(&state, false).await,
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown camera action"})),
        )
            .into_response(),
    }
}

async fn recall_camera_preset(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let Some(preset) = state
        .config
        .presets
        .iter()
        .find(|preset| preset.id == id)
        .cloned()
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "camera preset not found"})),
        )
            .into_response();
    };
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    if let Err(error) = disable_tracking_for_manual_control(&state, &camera).await {
        return command_error(error);
    }
    match camera.move_to(preset.yaw, preset.pitch, preset.roll).await {
        Ok(()) => {
            record_commanded_attitude(&state, preset.yaw, preset.pitch, preset.roll).await;
            record_camera_command(
                &state,
                "camera.preset.recalled",
                json!({
                    "id": preset.id,
                    "yaw": preset.yaw,
                    "pitch": preset.pitch,
                    "roll": preset.roll,
                }),
            )
            .await;
            StatusCode::ACCEPTED.into_response()
        }
        Err(error) => command_error(error),
    }
}

async fn set_tracking_inner(state: &ApiState, enabled: bool) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    if enabled {
        if let Err(error) =
            disable_face_tracking_mode(state, &camera, "camera-tracking-enabled").await
        {
            return command_error(error);
        }
        if let Err(error) =
            disable_hands_tracking_mode(state, &camera, "camera-tracking-enabled").await
        {
            return command_error(error);
        }
    }
    match camera.set_tracking(enabled).await {
        Ok(()) => {
            state
                .runtime
                .update(|runtime| {
                    runtime.camera.tracking = Some(enabled);
                    runtime.camera.tracking_sample_at_ms = None;
                    runtime.camera.tracking_error = None;
                })
                .await;
            record_camera_command(state, "camera.tracking", json!({"enabled": enabled})).await;
            StatusCode::ACCEPTED.into_response()
        }
        Err(error) => command_error(error),
    }
}

async fn set_face_tracking_inner(state: &ApiState, enabled: bool) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    if state.face_tracking.lock().await.enabled() == enabled {
        if let Err(error) = save_face_tracking_setting(state, enabled).await {
            return user_settings_error(error);
        }
        return StatusCode::ACCEPTED.into_response();
    }

    if enabled {
        if let Err(error) =
            disable_hands_tracking_mode(state, &camera, "face-tracking-enabled").await
        {
            return command_error(error);
        }
        let previous_camera_tracking = state.runtime.state().await.camera.tracking;
        if let Err(error) = camera.set_tracking(false).await {
            return command_error(error);
        }
        state
            .runtime
            .update(|runtime| {
                runtime.camera.tracking = Some(false);
                runtime.camera.tracking_sample_at_ms = None;
                runtime.camera.tracking_error = None;
            })
            .await;
        if previous_camera_tracking != Some(false) {
            record_camera_command(
                state,
                "camera.tracking",
                json!({"enabled": false, "reason": "face-tracking-enabled"}),
            )
            .await;
        }
        if let Err(error) = camera.set_face_tracking_speed(0, 0, 0.0).await {
            return command_error(error);
        }
        clear_pan_tilt_motion(state).await;
        state.face_tracking.lock().await.set_enabled(true);
        state
            .runtime
            .update(|runtime| {
                runtime.camera.face_tracking = FaceTrackingState {
                    enabled: true,
                    ..FaceTrackingState::default()
                };
            })
            .await;
        if let Err(error) = save_face_tracking_setting(state, true).await {
            return user_settings_error(error);
        }
    } else {
        let stop_error = camera.set_face_tracking_speed(0, 0, 0.0).await.err();
        state.face_tracking.lock().await.set_enabled(false);
        clear_pan_tilt_motion(state).await;
        state
            .runtime
            .update(|runtime| {
                runtime.camera.face_tracking = FaceTrackingState {
                    error: stop_error.as_ref().map(ToString::to_string),
                    ..FaceTrackingState::default()
                };
            })
            .await;
        if let Err(error) = save_face_tracking_setting(state, false).await {
            return user_settings_error(error);
        }
        record_camera_command(state, "camera.face_tracking", json!({"enabled": false})).await;
        return match stop_error {
            Some(error) => command_error(error),
            None => StatusCode::ACCEPTED.into_response(),
        };
    }
    record_camera_command(state, "camera.face_tracking", json!({"enabled": enabled})).await;
    StatusCode::ACCEPTED.into_response()
}

async fn set_hands_tracking_inner(state: &ApiState, enabled: bool) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    if state.hands_tracking.lock().await.enabled() == enabled {
        if let Err(error) = save_hands_tracking_setting(state, enabled).await {
            return user_settings_error(error);
        }
        return StatusCode::ACCEPTED.into_response();
    }

    if enabled {
        if let Err(error) =
            disable_face_tracking_mode(state, &camera, "hands-tracking-enabled").await
        {
            return command_error(error);
        }
        let previous_camera_tracking = state.runtime.state().await.camera.tracking;
        if let Err(error) = camera.set_tracking(false).await {
            return command_error(error);
        }
        state
            .runtime
            .update(|runtime| {
                runtime.camera.tracking = Some(false);
                runtime.camera.tracking_sample_at_ms = None;
                runtime.camera.tracking_error = None;
            })
            .await;
        if previous_camera_tracking != Some(false) {
            record_camera_command(
                state,
                "camera.tracking",
                json!({"enabled": false, "reason": "hands-tracking-enabled"}),
            )
            .await;
        }
        if let Err(error) = camera.set_face_tracking_speed(0, 0, 0.0).await {
            return command_error(error);
        }
        clear_pan_tilt_motion(state).await;
        state.hands_tracking.lock().await.set_enabled(true);
        state
            .runtime
            .update(|runtime| {
                runtime.camera.hands_tracking = HandsTrackingState {
                    enabled: true,
                    zoom_frozen: true,
                    zoom_magnification: camera.controlled_zoom_magnification(),
                    ..HandsTrackingState::default()
                };
            })
            .await;
        if let Err(error) = save_hands_tracking_setting(state, true).await {
            return user_settings_error(error);
        }
    } else {
        let stop_error = camera.set_face_tracking_speed(0, 0, 0.0).await.err();
        state.hands_tracking.lock().await.set_enabled(false);
        clear_pan_tilt_motion(state).await;
        state
            .runtime
            .update(|runtime| {
                runtime.camera.hands_tracking = HandsTrackingState {
                    error: stop_error.as_ref().map(ToString::to_string),
                    ..HandsTrackingState::default()
                };
            })
            .await;
        if let Err(error) = save_hands_tracking_setting(state, false).await {
            return user_settings_error(error);
        }
        record_camera_command(state, "camera.hands_tracking", json!({"enabled": false})).await;
        return match stop_error {
            Some(error) => command_error(error),
            None => StatusCode::ACCEPTED.into_response(),
        };
    }
    record_camera_command(state, "camera.hands_tracking", json!({"enabled": enabled})).await;
    StatusCode::ACCEPTED.into_response()
}

async fn clear_pan_tilt_motion(state: &ApiState) {
    let mut motion = state.pan_tilt_motion.lock().await;
    motion.direction = None;
    motion.expires_at = None;
}

async fn disable_face_tracking_mode(
    state: &ApiState,
    camera: &CameraHandle,
    reason: &str,
) -> anyhow::Result<bool> {
    let mut controller = state.face_tracking.lock().await;
    if !controller.enabled() {
        return Ok(false);
    }
    controller.set_enabled(false);
    drop(controller);
    let stop_error = camera.set_face_tracking_speed(0, 0, 0.0).await.err();
    clear_pan_tilt_motion(state).await;
    state
        .runtime
        .update(|runtime| {
            runtime.camera.face_tracking = FaceTrackingState {
                error: stop_error.as_ref().map(ToString::to_string),
                ..FaceTrackingState::default()
            };
        })
        .await;
    save_face_tracking_setting(state, false).await?;
    record_camera_command(
        state,
        "camera.face_tracking",
        json!({"enabled": false, "reason": reason}),
    )
    .await;
    match stop_error {
        Some(error) => Err(error),
        None => Ok(true),
    }
}

async fn disable_hands_tracking_mode(
    state: &ApiState,
    camera: &CameraHandle,
    reason: &str,
) -> anyhow::Result<bool> {
    let mut controller = state.hands_tracking.lock().await;
    if !controller.enabled() {
        return Ok(false);
    }
    controller.set_enabled(false);
    drop(controller);
    let stop_error = camera.set_face_tracking_speed(0, 0, 0.0).await.err();
    clear_pan_tilt_motion(state).await;
    state
        .runtime
        .update(|runtime| {
            runtime.camera.hands_tracking = HandsTrackingState {
                error: stop_error.as_ref().map(ToString::to_string),
                ..HandsTrackingState::default()
            };
        })
        .await;
    save_hands_tracking_setting(state, false).await?;
    record_camera_command(
        state,
        "camera.hands_tracking",
        json!({"enabled": false, "reason": reason}),
    )
    .await;
    match stop_error {
        Some(error) => Err(error),
        None => Ok(true),
    }
}

async fn disable_tracking_for_manual_control(
    state: &ApiState,
    camera: &CameraHandle,
) -> anyhow::Result<()> {
    disable_face_tracking_mode(state, camera, "manual-gimbal-control").await?;
    disable_hands_tracking_mode(state, camera, "manual-gimbal-control").await?;

    if state.runtime.state().await.camera.tracking != Some(true) {
        return Ok(());
    }

    camera.set_tracking(false).await?;
    state
        .runtime
        .update(|runtime| {
            runtime.camera.tracking = Some(false);
            runtime.camera.tracking_sample_at_ms = None;
            runtime.camera.tracking_error = None;
        })
        .await;
    record_camera_command(
        state,
        "camera.tracking",
        json!({"enabled": false, "reason": "manual-gimbal-control"}),
    )
    .await;
    Ok(())
}

async fn save_face_tracking_setting(state: &ApiState, enabled: bool) -> anyhow::Result<()> {
    if let Some(settings) = &state.user_settings {
        settings.set_face_tracking(enabled).await?;
    }
    Ok(())
}

async fn save_auto_zoom_setting(state: &ApiState, enabled: bool) -> anyhow::Result<()> {
    if let Some(settings) = &state.user_settings {
        settings.set_auto_zoom(enabled).await?;
    }
    Ok(())
}

async fn save_hands_tracking_setting(state: &ApiState, enabled: bool) -> anyhow::Result<()> {
    if let Some(settings) = &state.user_settings {
        settings.set_hands_tracking(enabled).await?;
    }
    Ok(())
}

async fn record_camera_command(state: &ApiState, kind: &str, data: Value) {
    let at_ms = unix_ms();
    state
        .runtime
        .update(|runtime| runtime.camera.last_command_at_ms = Some(at_ms))
        .await;
    state.runtime.emit(kind, "api", None, data).await;
}

async fn record_commanded_attitude(state: &ApiState, yaw: f32, pitch: f32, roll: f32) {
    let at_ms = unix_ms();
    state
        .runtime
        .update(|runtime| {
            runtime.camera.yaw_degrees = Some(yaw);
            runtime.camera.pitch_degrees = Some(pitch);
            runtime.camera.roll_degrees = Some(roll);
            runtime.camera.euler_yaw_degrees = None;
            runtime.camera.euler_pitch_degrees = None;
            runtime.camera.euler_roll_degrees = None;
            runtime.camera.yaw_velocity_degrees_per_second = None;
            runtime.camera.pitch_velocity_degrees_per_second = None;
            runtime.camera.roll_velocity_degrees_per_second = None;
            runtime.camera.attitude_source = CameraAttitudeSource::LastCommanded;
            runtime.camera.sample_at_ms = Some(at_ms);
        })
        .await;
}

fn camera_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "camera adapter is disabled"})),
    )
        .into_response()
}

fn command_error(error: anyhow::Error) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"error": error.to_string()})),
    )
        .into_response()
}

async fn effective_config(State(state): State<ApiState>) -> Json<Config> {
    Json(state.config)
}

async fn scenarios(State(state): State<ApiState>) -> Json<Vec<ScenarioConfig>> {
    Json(state.config.scenarios)
}

async fn recent_events(State(state): State<ApiState>) -> Json<Vec<crate::model::SemanticEvent>> {
    Json(state.runtime.recent_events().await)
}

async fn take_photo(State(state): State<ApiState>) -> Response {
    let Some(frame) = state.preview.latest_photo() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "No fresh camera frame is available"})),
        )
            .into_response();
    };
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<std::path::PathBuf> {
        let directory = photos_directory()?;
        save_photo(&directory, &frame.jpeg()?)
    })
    .await;
    match result {
        Ok(Ok(path)) => {
            let filename = path.file_name().unwrap().to_string_lossy();
            let url = format!("/api/v1/camera/photos/{filename}");
            let photo = json!({"path": path, "url": url});
            state
                .runtime
                .update(|current| current.last_photo = Some(photo.clone()))
                .await;
            (StatusCode::CREATED, Json(photo)).into_response()
        }
        Ok(Err(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Could not save photo: {error}")})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Photo task failed: {error}")})),
        )
            .into_response(),
    }
}

fn photos_directory() -> anyhow::Result<std::path::PathBuf> {
    std::env::var_os("TARSIER_PHOTOS_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join("Pictures/Tarsier"))
        })
        .ok_or_else(|| anyhow::anyhow!("Set TARSIER_PHOTOS_DIR or HOME to save photos"))
}

fn read_saved_photo(directory: &std::path::Path, filename: &str) -> std::io::Result<Vec<u8>> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let valid = filename
        .strip_prefix("photo-")
        .unwrap_or(filename)
        .strip_suffix(".jpg")
        .is_some_and(|name| {
            let parts: Vec<_> = name.split('-').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        });
    if !valid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Invalid photo filename",
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(directory.join(filename))?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Not a photo file",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

async fn saved_photo(axum::extract::Path(filename): axum::extract::Path<String>) -> Response {
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        Ok(read_saved_photo(&photos_directory()?, &filename)?)
    })
    .await;
    match result {
        Ok(Ok(bytes)) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            bytes,
        )
            .into_response(),
        _ => (StatusCode::NOT_FOUND, "Photo not found").into_response(),
    }
}

async fn open_saved_photo(
    axum::extract::Path(filename): axum::extract::Path<String>,
    Json(_request): Json<serde_json::Value>,
) -> Response {
    let checked = tokio::task::spawn_blocking(move || -> anyhow::Result<std::path::PathBuf> {
        let directory = photos_directory()?;
        read_saved_photo(&directory, &filename)?;
        Ok(directory.join(filename))
    })
    .await;
    let Ok(Ok(path)) = checked else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Photo not found"})),
        )
            .into_response();
    };
    reveal_local_media(path).await
}

async fn reveal_local_media(path: std::path::PathBuf) -> Response {
    let uri = std::path::absolute(path)
        .ok()
        .and_then(|path| reqwest::Url::from_file_path(path).ok());
    let Some(uri) = uri else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Could not resolve the saved file location"})),
        )
            .into_response();
    };
    let mut command = tokio::process::Command::new("gdbus");
    command
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.FileManager1",
            "--object-path",
            "/org/freedesktop/FileManager1",
            "--method",
            "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(json!([uri.as_str()]).to_string())
        .arg("")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(std::time::Duration::from_secs(5), command.status()).await {
        Ok(Ok(status)) if status.success() => StatusCode::ACCEPTED.into_response(),
        result => {
            tracing::warn!(?result, "file manager reveal failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Could not show the file in its folder"})),
            )
                .into_response()
        }
    }
}

fn save_photo(directory: &std::path::Path, bytes: &[u8]) -> anyhow::Result<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(directory)?;
    loop {
        let path = directory.join(format!(
            "{}-{}.jpg",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        return Ok(path);
    }
}

async fn snapshot(State(state): State<ApiState>) -> Response {
    let Some(frame) = state.preview.latest() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no video frame is available",
        )
            .into_response();
    };
    let frame = match frame.jpeg() {
        Ok(bytes) => bytes,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        frame,
    )
        .into_response()
}

async fn preview_mjpeg(State(state): State<ApiState>) -> Response {
    mjpeg_response(state.preview.subscribe(), state.shutdown.clone())
}

async fn perception_input_mjpeg(State(state): State<ApiState>) -> Response {
    perception_mjpeg_response(
        state.preview.subscribe_perception(),
        state.shutdown.clone(),
        state.preview.effects().clone(),
    )
}

fn perception_mjpeg_response(
    mut receiver: watch::Receiver<Option<PerceptionFrame>>,
    mut shutdown: watch::Receiver<bool>,
    effects: crate::effects::VideoEffects,
) -> Response {
    let stream = async_stream::stream! {
        loop {
            tokio::select! {
                changed = receiver.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
            }
            let frame = receiver.borrow_and_update().clone();
            let Some(frame) = frame else {
                continue;
            };
            let part_header = Bytes::from(format!(
                "--tarsier-frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nX-Tarsier-Frame-Id: {}\r\nX-Tarsier-Captured-At-Ms: {}\r\nX-Tarsier-Inference-Rotation: {}\r\n\r\n",
                frame.bytes.len(), frame.frame_id, frame.captured_at_ms, effects.transform().rotation
            ));
            yield Ok::<Bytes, Infallible>(part_header);
            yield Ok::<Bytes, Infallible>(frame.bytes);
            yield Ok::<Bytes, Infallible>(Bytes::from_static(b"\r\n"));
        }
    };
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace; boundary=tarsier-frame",
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .expect("static perception response is valid")
}

fn mjpeg_response(
    mut receiver: watch::Receiver<Option<Bytes>>,
    mut shutdown: watch::Receiver<bool>,
) -> Response {
    let stream = async_stream::stream! {
        loop {
            tokio::select! {
                changed = receiver.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
            }
            let frame = receiver.borrow_and_update().clone();
            let Some(frame) = frame else {
                continue;
            };
            let part_header = Bytes::from(format!(
                "--tarsier-frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                frame.len()
            ));
            yield Ok::<Bytes, Infallible>(part_header);
            yield Ok::<Bytes, Infallible>(frame);
            yield Ok::<Bytes, Infallible>(Bytes::from_static(b"\r\n"));
        }
    };
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace; boundary=tarsier-frame",
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .expect("static preview response is valid")
}

async fn set_green_screen(
    State(state): State<ApiState>,
    Json(request): Json<GreenScreenRequest>,
) -> Response {
    let _guard = state.video_output_control.lock().await;
    if let Some(response) =
        persist_background(&state, request.enabled, BackgroundEffect::GreenScreen).await
    {
        return response;
    }
    state
        .preview
        .effects()
        .set_green_screen_enabled(request.enabled);
    state
        .runtime
        .update(|runtime| {
            runtime.video_effects.background_enabled = request.enabled;
            runtime.video_effects.background_effect = BackgroundEffect::GreenScreen;
            runtime.video_effects.green_screen_enabled = request.enabled;
        })
        .await;
    state
        .runtime
        .emit(
            "video.effect.green_screen",
            "api",
            None,
            json!({"enabled": request.enabled}),
        )
        .await;
    StatusCode::ACCEPTED.into_response()
}

async fn set_background(
    State(state): State<ApiState>,
    Json(request): Json<BackgroundRequest>,
) -> Response {
    let _guard = state.video_output_control.lock().await;
    if let Some(response) = persist_background(&state, request.enabled, request.effect).await {
        return response;
    }
    state
        .preview
        .effects()
        .set_background(request.enabled, request.effect);
    state
        .runtime
        .update(|runtime| {
            runtime.video_effects.background_enabled = request.enabled;
            runtime.video_effects.background_effect = request.effect;
            runtime.video_effects.green_screen_enabled =
                request.enabled && request.effect == BackgroundEffect::GreenScreen;
        })
        .await;
    state
        .runtime
        .emit(
            "video.effect.background",
            "api",
            None,
            json!({"enabled": request.enabled, "effect": request.effect}),
        )
        .await;
    StatusCode::ACCEPTED.into_response()
}

async fn recording_status(
    State(state): State<ApiState>,
) -> Json<crate::recording::RecordingStatus> {
    Json(state.recorder.status().await)
}

async fn start_recording(State(state): State<ApiState>, Json(_): Json<Value>) -> Response {
    let _guard = state.video_output_control.lock().await;
    let settings = state.runtime.state().await;
    let pipeline = &settings.pipeline;
    if !pipeline.running
        || pipeline
            .last_frame_at_ms
            .is_none_or(|at| unix_ms().saturating_sub(at) > 2000)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "No live video output is available"})),
        )
            .into_response();
    }
    match state
        .recorder
        .start(
            &state.config.video,
            &settings,
            state.preview.subscribe_recording(),
        )
        .await
    {
        Ok(status) => (StatusCode::CREATED, Json(status)).into_response(),
        Err(error) => command_error(error),
    }
}

async fn stop_recording(State(state): State<ApiState>, Json(_): Json<Value>) -> Response {
    match state.recorder.stop().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => command_error(error),
    }
}

async fn saved_recording(
    axum::extract::Path(filename): axum::extract::Path<String>,
    request: axum::extract::Request,
) -> Response {
    if !crate::recording::valid_filename(&filename) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(directory) = crate::recording::directory() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = directory.join(filename);
    if !tokio::fs::symlink_metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tower_http::services::ServeFile::new(path)
        .try_call(request)
        .await
    {
        Ok(response) => response.into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn open_saved_recording(
    axum::extract::Path(filename): axum::extract::Path<String>,
    Json(_request): Json<Value>,
) -> Response {
    let checked = async {
        if !crate::recording::valid_filename(&filename) {
            return None;
        }
        let path = crate::recording::directory().ok()?.join(filename);
        tokio::fs::symlink_metadata(&path)
            .await
            .ok()?
            .is_file()
            .then_some(path)
    }
    .await;
    let Some(path) = checked else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Video not found"})),
        )
            .into_response();
    };
    reveal_local_media(path).await
}

fn effects_unavailable_in_4k() -> Response {
    (StatusCode::CONFLICT, Json(json!({"error": "Effects are unavailable in 4K camera mode. Select a lower resolution first."}))).into_response()
}

async fn set_resolution(
    State(state): State<ApiState>,
    Json(resolution): Json<crate::settings::VideoResolution>,
) -> Response {
    let _guard = state.video_output_control.lock().await;
    if state.recorder.status().await.active {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Stop recording before changing resolution"})),
        )
            .into_response();
    }
    if !resolution.valid() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Unsupported resolution"})),
        )
            .into_response();
    }
    let (Some(restart), Some(settings)) = (&state.daemon_restart, &state.user_settings) else {
        return (StatusCode::CONFLICT, Json(json!({"error": "Resolution changes require a supervised daemon and persistent settings"}))).into_response();
    };
    if let Err(error) = settings.set_video_resolution(resolution).await {
        return user_settings_error(error);
    }
    if let Err(error) = restart.request().await {
        return (StatusCode::CONFLICT, Json(json!({"error": error}))).into_response();
    }
    (StatusCode::ACCEPTED, Json(json!({"restarting": true}))).into_response()
}

async fn current_transform(
    State(state): State<ApiState>,
) -> Json<crate::video_transform::VideoTransform> {
    Json(state.runtime.state().await.video_effects.transform)
}

async fn set_transform(
    State(state): State<ApiState>,
    Json(transform): Json<crate::video_transform::VideoTransform>,
) -> Response {
    if state.config.video.width >= 3840 && transform != Default::default() {
        return effects_unavailable_in_4k();
    }
    if !transform.valid() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "rotation must be 0, 90, 180, or 270 degrees"})),
        )
            .into_response();
    }
    let _guard = state.video_output_control.lock().await;
    if let Some(settings) = &state.user_settings
        && let Err(error) = settings.set_video_transform(transform).await
    {
        return user_settings_error(error);
    }
    state.preview.effects().set_transform(transform);
    state
        .runtime
        .update(|runtime| runtime.video_effects.transform = transform)
        .await;
    state
        .runtime
        .emit("video.transform", "api", None, json!(transform))
        .await;
    Json(transform).into_response()
}

async fn current_identity(State(state): State<ApiState>) -> Json<IdentityResponse> {
    let effects = state.runtime.state().await.video_effects;
    Json(IdentityResponse {
        liveportrait: state.liveportrait.lock().await.clone(),
        identity: video_identity(effects.output_mode, effects.avatar_engine),
        background_enabled: effects.background_enabled,
    })
}

async fn set_identity(
    State(state): State<ApiState>,
    Json(request): Json<IdentityRequest>,
) -> Response {
    match request.identity {
        VideoIdentity::Portrait3d | VideoIdentity::Liveportrait if !state.config.avatar.enabled => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "avatar output is disabled in the daemon configuration"})),
            )
                .into_response();
        }
        VideoIdentity::DepthMap if !state.config.depth.enabled => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "depth processing is disabled in the daemon configuration"})),
            )
                .into_response();
        }
        _ => {}
    }
    let _guard = state.video_output_control.lock().await;
    let (mode, engine) = match request.identity {
        VideoIdentity::Camera => (VideoOutputMode::Camera, None),
        VideoIdentity::Portrait3d => (VideoOutputMode::ComicAvatar, Some(AvatarEngine::Portrait3d)),
        VideoIdentity::Liveportrait => (
            VideoOutputMode::ComicAvatar,
            Some(AvatarEngine::Liveportrait),
        ),
        VideoIdentity::DepthMap => (VideoOutputMode::DepthMap, None),
    };
    if let Some(response) = persist_video_identity(&state, request.identity).await {
        return response;
    }
    state.preview.effects().clear_avatar();
    state.preview.effects().clear_depth();
    state.preview.effects().set_output_mode(mode);
    state
        .runtime
        .update(|runtime| {
            runtime.video_effects.output_mode = mode;
            if let Some(engine) = engine {
                runtime.video_effects.avatar_engine = Some(engine);
            }
            clear_avatar_state(runtime);
            clear_depth_state(runtime);
        })
        .await;
    state
        .runtime
        .emit(
            "video.identity",
            "api",
            None,
            json!({"identity": request.identity}),
        )
        .await;
    StatusCode::ACCEPTED.into_response()
}

async fn set_output_mode(
    State(state): State<ApiState>,
    Json(request): Json<OutputModeRequest>,
) -> Response {
    let _guard = state.video_output_control.lock().await;
    let current = state.runtime.state().await.video_effects;
    let identity = video_identity(request.mode, current.avatar_engine);
    if let Some(response) = persist_video_identity(&state, identity).await {
        return response;
    }
    state.preview.effects().clear_avatar();
    state.preview.effects().clear_depth();
    state.preview.effects().set_output_mode(request.mode);
    state
        .runtime
        .update(|runtime| {
            runtime.video_effects.output_mode = request.mode;
            clear_avatar_state(runtime);
            clear_depth_state(runtime);
        })
        .await;
    state
        .runtime
        .emit(
            "video.output.mode",
            "api",
            None,
            json!({"mode": request.mode}),
        )
        .await;
    StatusCode::ACCEPTED.into_response()
}

async fn persist_video_identity(state: &ApiState, identity: VideoIdentity) -> Option<Response> {
    if state.config.video.width >= 3840 && identity != VideoIdentity::Camera {
        return Some(effects_unavailable_in_4k());
    }
    let Some(settings) = &state.user_settings else {
        return None;
    };
    settings
        .set_video_identity(identity)
        .await
        .err()
        .map(user_settings_error)
}

async fn persist_background(
    state: &ApiState,
    enabled: bool,
    effect: BackgroundEffect,
) -> Option<Response> {
    if state.config.video.width >= 3840 && enabled {
        return Some(effects_unavailable_in_4k());
    }
    let Some(settings) = &state.user_settings else {
        return None;
    };
    settings
        .set_background(enabled, effect)
        .await
        .err()
        .map(user_settings_error)
}

fn user_settings_error(error: anyhow::Error) -> Response {
    tracing::error!(%error, "failed to persist user settings");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": format!("failed to persist user settings: {error}")})),
    )
        .into_response()
}

async fn avatar_frame(State(state): State<ApiState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = reject_stale_worker_update(&state).await {
        return response;
    }
    let engine = match required_avatar_engine_header(&headers) {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let frame_id = match required_u64_header(&headers, "x-tarsier-frame-id") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let captured_at_ms = match required_u64_header(&headers, "x-tarsier-captured-at-ms") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let width = match required_u32_header(&headers, "x-tarsier-avatar-width") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let height = match required_u32_header(&headers, "x-tarsier-avatar-height") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let has_alpha = match headers
        .get("x-tarsier-avatar-pixel-format")
        .and_then(|h| h.to_str().ok())
    {
        Some("bgra") => true,
        None | Some("bgrx") if engine != AvatarEngine::Portrait3d => false,
        _ => {
            return unprocessable_entity(
                "portrait3d requires bgra with a synchronized foreground mask".into(),
            );
        }
    };
    let avatar = match AvatarFrame::new(frame_id, captured_at_ms, width, height, body.to_vec())
        .map(|frame| frame.with_alpha(has_alpha))
    {
        Ok(avatar) => avatar,
        Err(error) => return unprocessable_entity(error.to_string()),
    };
    let _guard = state.video_output_control.lock().await;
    let effects = state.runtime.state().await.video_effects;
    if effects.output_mode != VideoOutputMode::ComicAvatar || effects.avatar_engine != Some(engine)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "avatar frame does not match the selected video identity"})),
        )
            .into_response();
    }
    if engine == AvatarEngine::Liveportrait {
        let revision = if headers.contains_key("x-tarsier-avatar-source-revision") {
            match required_u64_header(&headers, "x-tarsier-avatar-source-revision") {
                Ok(value) => value,
                Err(error) => return unprocessable_entity(error),
            }
        } else {
            0
        };
        let mut portrait = state.liveportrait.lock().await;
        if revision != portrait.active_revision
            && unix_ms().saturating_sub(captured_at_ms) > crate::effects::AVATAR_MAX_AGE_MS
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "new portrait frame is not fresh yet"})),
            )
                .into_response();
        }
        if !portrait.accept_frame(revision) {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "obsolete portrait revision"})),
            )
                .into_response();
        }
    }
    let frame_id = avatar.frame_id;
    let published_at_ms = avatar.published_at_ms;
    state.preview.effects().publish_avatar(avatar);
    state
        .runtime
        .update(|runtime| {
            runtime.video_effects.avatar_available = true;
            runtime.video_effects.avatar_frame_id = Some(frame_id);
            runtime.video_effects.avatar_width = Some(width);
            runtime.video_effects.avatar_height = Some(height);
            runtime.video_effects.avatar_captured_at_ms = Some(captured_at_ms);
            runtime.video_effects.avatar_published_at_ms = Some(published_at_ms);
        })
        .await;
    StatusCode::NO_CONTENT.into_response()
}

fn video_identity(mode: VideoOutputMode, engine: Option<AvatarEngine>) -> VideoIdentity {
    match (mode, engine) {
        (VideoOutputMode::Camera, _) => VideoIdentity::Camera,
        (VideoOutputMode::DepthMap, _) => VideoIdentity::DepthMap,
        (VideoOutputMode::ComicAvatar, Some(AvatarEngine::Portrait3d)) => VideoIdentity::Portrait3d,
        (VideoOutputMode::ComicAvatar, Some(AvatarEngine::Liveportrait)) => {
            VideoIdentity::Liveportrait
        }
        (VideoOutputMode::ComicAvatar, _) => VideoIdentity::Liveportrait,
    }
}

fn clear_avatar_state(runtime: &mut crate::model::RuntimeState) {
    runtime.video_effects.avatar_available = false;
    runtime.video_effects.avatar_frame_id = None;
    runtime.video_effects.avatar_width = None;
    runtime.video_effects.avatar_height = None;
    runtime.video_effects.avatar_captured_at_ms = None;
    runtime.video_effects.avatar_published_at_ms = None;
}

fn clear_depth_state(runtime: &mut crate::model::RuntimeState) {
    runtime.video_effects.depth_available = false;
    runtime.video_effects.depth_frame_id = None;
    runtime.video_effects.depth_width = None;
    runtime.video_effects.depth_height = None;
    runtime.video_effects.depth_far = None;
    runtime.video_effects.depth_near = None;
    runtime.video_effects.depth_captured_at_ms = None;
    runtime.video_effects.depth_published_at_ms = None;
}

async fn depth_frame(State(state): State<ApiState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = reject_stale_worker_update(&state).await {
        return response;
    }
    if let Err(error) = required_exact_header(
        &headers,
        "x-tarsier-depth-representation",
        "relative-inverse-depth-f32le",
    ) {
        return unprocessable_entity(error);
    }
    let frame_id = match required_u64_header(&headers, "x-tarsier-frame-id") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let captured_at_ms = match required_u64_header(&headers, "x-tarsier-captured-at-ms") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let width = match required_u32_header(&headers, "x-tarsier-depth-width") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let height = match required_u32_header(&headers, "x-tarsier-depth-height") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let far = match required_f32_header(&headers, "x-tarsier-depth-far") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let near = match required_f32_header(&headers, "x-tarsier-depth-near") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let depth = match DepthFrame::new(frame_id, captured_at_ms, width, height, far, near, &body) {
        Ok(depth) => depth,
        Err(error) => return unprocessable_entity(error.to_string()),
    };
    let _guard = state.video_output_control.lock().await;
    let effects = state.runtime.state().await.video_effects;
    let accepts_depth = effects.output_mode == VideoOutputMode::DepthMap
        || (effects.output_mode == VideoOutputMode::Camera && effects.background_enabled);
    if !accepts_depth {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "depth frame is not required by the selected video output"})),
        )
            .into_response();
    }
    let published_at_ms = depth.published_at_ms;
    state.preview.effects().publish_depth(depth);
    state
        .runtime
        .update(|runtime| {
            runtime.video_effects.depth_available = true;
            runtime.video_effects.depth_frame_id = Some(frame_id);
            runtime.video_effects.depth_width = Some(width);
            runtime.video_effects.depth_height = Some(height);
            runtime.video_effects.depth_far = Some(far);
            runtime.video_effects.depth_near = Some(near);
            runtime.video_effects.depth_captured_at_ms = Some(captured_at_ms);
            runtime.video_effects.depth_published_at_ms = Some(published_at_ms);
        })
        .await;
    StatusCode::NO_CONTENT.into_response()
}

async fn latest_depth(State(state): State<ApiState>) -> Response {
    let Some(depth) = state.preview.effects().latest_depth() else {
        return (StatusCode::NOT_FOUND, "no depth frame is available").into_response();
    };
    let mut bytes = Vec::with_capacity(size_of_val(depth.values()));
    for value in depth.values() {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            "x-tarsier-depth-representation",
            "relative-inverse-depth-f32le",
        )
        .header("x-tarsier-frame-id", depth.frame_id)
        .header("x-tarsier-captured-at-ms", depth.captured_at_ms)
        .header("x-tarsier-published-at-ms", depth.published_at_ms)
        .header("x-tarsier-depth-width", depth.width)
        .header("x-tarsier-depth-height", depth.height)
        .header("x-tarsier-depth-far", depth.far.to_string())
        .header("x-tarsier-depth-near", depth.near.to_string())
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn perception_mask(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = reject_stale_worker_update(&state).await {
        return response;
    }
    let frame_id = match required_u64_header(&headers, "x-tarsier-frame-id") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let captured_at_ms = match required_u64_header(&headers, "x-tarsier-captured-at-ms") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let width = match required_u32_header(&headers, "x-tarsier-mask-width") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let height = match required_u32_header(&headers, "x-tarsier-mask-height") {
        Ok(value) => value,
        Err(error) => return unprocessable_entity(error),
    };
    let mask = match VideoMask::new(frame_id, captured_at_ms, width, height, body.to_vec()) {
        Ok(mask) => mask,
        Err(error) => return unprocessable_entity(error.to_string()),
    };
    let published_at_ms = mask.published_at_ms;
    state.preview.effects().publish_mask(mask);
    state
        .runtime
        .update(|runtime| {
            runtime.video_effects.mask_available = true;
            runtime.video_effects.mask_frame_id = Some(frame_id);
            runtime.video_effects.mask_width = Some(width);
            runtime.video_effects.mask_height = Some(height);
            runtime.video_effects.mask_captured_at_ms = Some(captured_at_ms);
            runtime.video_effects.mask_published_at_ms = Some(published_at_ms);
        })
        .await;
    StatusCode::NO_CONTENT.into_response()
}

fn unprocessable_entity(error: String) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"error": error})),
    )
        .into_response()
}

fn required_u64_header(headers: &HeaderMap, name: &'static str) -> Result<u64, String> {
    required_header(headers, name)
}

fn required_u32_header(headers: &HeaderMap, name: &'static str) -> Result<u32, String> {
    required_header(headers, name)
}

fn required_f32_header(headers: &HeaderMap, name: &'static str) -> Result<f32, String> {
    required_header(headers, name)
}

fn required_exact_header(
    headers: &HeaderMap,
    name: &'static str,
    expected: &'static str,
) -> Result<(), String> {
    let value = headers
        .get(name)
        .ok_or_else(|| format!("missing {name} header"))?
        .to_str()
        .map_err(|_| format!("invalid {name} header"))?;
    if value != expected {
        return Err(format!("unsupported {name} header value {value:?}"));
    }
    Ok(())
}

fn required_avatar_engine_header(headers: &HeaderMap) -> Result<AvatarEngine, String> {
    let name = "x-tarsier-avatar-engine";
    let value = headers
        .get(name)
        .ok_or_else(|| format!("missing {name} header"))?
        .to_str()
        .map_err(|_| format!("invalid {name} header"))?;
    match value {
        "portrait3d" => Ok(AvatarEngine::Portrait3d),
        "liveportrait" => Ok(AvatarEngine::Liveportrait),
        _ => Err(format!("invalid {name} header")),
    }
}

fn required_header<T>(headers: &HeaderMap, name: &'static str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("missing or invalid {name} header"))
}

async fn trigger_scenario(
    State(state): State<ApiState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let Some(scenario) = state
        .config
        .scenarios
        .iter()
        .find(|scenario| scenario.id == id && scenario.enabled)
        .cloned()
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "scenario not found"})),
        )
            .into_response();
    };

    let trigger = state
        .runtime
        .emit(
            "scenario.manual_trigger",
            "api",
            None,
            json!({"scenario_id": scenario.id}),
        )
        .await;
    activate_scenario(&state, &scenario, trigger.sequence).await;
    (
        StatusCode::ACCEPTED,
        Json(json!({"ok": true, "trigger_sequence": trigger.sequence})),
    )
        .into_response()
}

async fn perception_observation(
    State(state): State<ApiState>,
    Json(observation): Json<PerceptionObservation>,
) -> Response {
    if let Some(response) = reject_stale_worker_update(&state).await {
        return response;
    }
    if observation.confidence.is_nan() || !(0.0..=1.0).contains(&observation.confidence) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "confidence must be between 0 and 1"})),
        )
            .into_response();
    }
    if !observation.face_landmarks.is_empty() && observation.face_landmarks.len() != 478 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "face_landmarks must contain exactly 478 points"})),
        )
            .into_response();
    }
    if !observation.hand_landmarks.is_empty()
        && !matches!(observation.hand_landmarks.len(), 21 | 42)
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "hand_landmarks must contain exactly 21 or 42 points"})),
        )
            .into_response();
    }
    if !observation.pose_landmarks.is_empty() && observation.pose_landmarks.len() != 33 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "pose_landmarks must contain exactly 33 points"})),
        )
            .into_response();
    }

    state
        .runtime
        .update(|runtime| {
            runtime.perception.worker_connected = true;
            runtime.perception.error = None;
            runtime.perception.frame_id = Some(observation.frame_id);
            runtime.perception.face_detected = observation.face_detected;
            runtime.perception.face_landmarks = if observation.face_detected {
                observation.face_landmarks.clone()
            } else {
                Vec::new()
            };
            runtime.perception.hand_detected = observation.hand_detected;
            runtime.perception.hand_landmarks = if observation.hand_detected {
                observation.hand_landmarks.clone()
            } else {
                Vec::new()
            };
            runtime.perception.pose_detected = observation.pose_detected;
            runtime.perception.pose_landmarks = if observation.pose_detected {
                observation.pose_landmarks.clone()
            } else {
                Vec::new()
            };
            if observation.hand_detected {
                runtime.perception.last_hand_at_ms = Some(observation.captured_at_ms);
            }
            runtime.perception.gesture = observation.gesture.clone();
            runtime.perception.confidence = Some(observation.confidence);
            if observation.gesture.is_some()
                && observation.confidence
                    > runtime
                        .perception
                        .peak_gesture_confidence
                        .unwrap_or_default()
            {
                runtime.perception.peak_gesture = observation.gesture.clone();
                runtime.perception.peak_gesture_confidence = Some(observation.confidence);
                runtime.perception.peak_gesture_at_ms = Some(observation.captured_at_ms);
            }
            runtime.perception.sample_at_ms = Some(observation.captured_at_ms);
            runtime.perception.latency_ms = observation.latency_ms;
        })
        .await;

    let tracking_landmarks = if observation.face_detected {
        observation.face_landmarks.as_slice()
    } else {
        &[]
    };
    let tracking_pose_landmarks = if observation.pose_detected {
        observation.pose_landmarks.as_slice()
    } else {
        &[]
    };
    drive_face_tracking(
        &state,
        tracking_landmarks,
        tracking_pose_landmarks,
        observation.captured_at_ms,
    )
    .await;
    let tracking_hand_landmarks = if observation.hand_detected {
        observation.hand_landmarks.as_slice()
    } else {
        &[]
    };
    drive_hands_tracking(&state, tracking_hand_landmarks, observation.captured_at_ms).await;

    let presence_change = state
        .face_presence
        .lock()
        .await
        .observe(observation.face_detected, observation.captured_at_ms);
    if let Some(change) = presence_change {
        let kind = match change {
            PresenceChange::Started => "face.present.started",
            PresenceChange::Ended => "face.present.ended",
        };
        state
            .runtime
            .emit(
                kind,
                "perception",
                None,
                json!({"frame_id": observation.frame_id}),
            )
            .await;
    }

    let open_palm = observation.gesture.as_deref() == Some("open_palm");
    let held = state.stabilizer.lock().await.observe(
        open_palm,
        observation.confidence,
        observation.captured_at_ms,
    );
    if held {
        let event = state
            .runtime
            .emit(
                "gesture.open_palm.held",
                "perception",
                Some(observation.confidence),
                json!({"frame_id": observation.frame_id}),
            )
            .await;
        let matches: Vec<_> = state
            .config
            .scenarios
            .iter()
            .filter(|scenario| scenario.enabled && scenario.event == event.kind)
            .cloned()
            .collect();
        for scenario in matches {
            activate_scenario(&state, &scenario, event.sequence).await;
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn reject_stale_worker_update(state: &ApiState) -> Option<Response> {
    if state.pipeline.is_some() && !state.runtime.state().await.pipeline.running {
        Some(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "video pipeline is paused"})),
            )
                .into_response(),
        )
    } else {
        None
    }
}

async fn drive_face_tracking(
    state: &ApiState,
    face_landmarks: &[Landmark],
    pose_landmarks: &[Landmark],
    captured_at_ms: u64,
) {
    let Some(camera) = state.camera.clone() else {
        return;
    };
    let mut controller = state.face_tracking.lock().await;
    if !controller.enabled() {
        return;
    }

    let camera_state = state.runtime.state().await.camera;
    if camera_state.tracking == Some(true) {
        let error = camera
            .set_face_tracking_speed(0, 0, 0.0)
            .await
            .err()
            .map(|error| error.to_string());
        controller.set_enabled(false);
        state
            .runtime
            .update(|runtime| {
                runtime.camera.face_tracking = FaceTrackingState {
                    error,
                    ..FaceTrackingState::default()
                };
            })
            .await;
        record_camera_command(
            state,
            "camera.face_tracking",
            json!({"enabled": false, "reason": "camera-tracking-detected"}),
        )
        .await;
        return;
    }

    let target = controller
        .face_target(face_landmarks, pose_landmarks)
        .map(|target| (target, FaceTrackingTarget::Face))
        .or_else(|| {
            controller
                .shoulder_target(pose_landmarks)
                .map(|target| (target, FaceTrackingTarget::Shoulders))
        });
    let desired_motion = target
        .as_ref()
        .map(|(target, _)| target.motion)
        .unwrap_or_default();
    let auto_zoom = controller.auto_zoom(
        target.as_ref().and_then(|(target, source)| {
            (*source == FaceTrackingTarget::Face)
                .then_some(target.size)
                .flatten()
        }),
        camera.controlled_zoom_magnification(),
        captured_at_ms,
    );
    let should_command = desired_motion != controller.motion() || desired_motion.active();
    let command_error = if should_command {
        camera
            .set_face_tracking_speed(
                desired_motion.pan_direction,
                desired_motion.tilt_direction,
                desired_motion.speed_fraction,
            )
            .await
            .err()
            .map(|error| error.to_string())
    } else {
        None
    };
    if command_error.is_none() {
        controller.record_motion(desired_motion);
    }
    let auto_zoom_error = if let Some(magnification) = auto_zoom.requested_magnification {
        camera
            .set_zoom(magnification)
            .await
            .err()
            .map(|error| error.to_string())
    } else {
        None
    };
    let motion = controller.motion();
    drop(controller);
    let controlled_zoom = camera.controlled_zoom_magnification();
    let zoom_changed = auto_zoom.requested_magnification.is_some() && auto_zoom_error.is_none();
    let auto_zoom_attempted = auto_zoom.requested_magnification.is_some();
    state
        .runtime
        .update(|runtime| {
            if let Some(magnification) = auto_zoom
                .requested_magnification
                .filter(|_| auto_zoom_error.is_none())
            {
                runtime.camera.zoom_magnification = Some(magnification);
                runtime.camera.zoom_sample_at_ms = None;
                runtime.camera.zoom_error = None;
                runtime.camera.last_command_at_ms = Some(unix_ms());
            }
            let previous_auto_zoom_error = runtime.camera.face_tracking.auto_zoom.error.clone();
            runtime.camera.face_tracking = FaceTrackingState {
                enabled: true,
                active: motion.active(),
                target_visible: target.is_some(),
                target_source: target.as_ref().map(|(_, source)| *source),
                target_x: target.as_ref().map(|(target, _)| target.x),
                target_y: target.as_ref().map(|(target, _)| target.y),
                speed_fraction: motion.speed_fraction as f32,
                auto_zoom: AutoZoomState {
                    enabled: auto_zoom.enabled,
                    calibrated: auto_zoom.calibrated,
                    zoom_magnification: controlled_zoom,
                    target_face_size: auto_zoom.target_face_size,
                    face_size: auto_zoom.face_size,
                    at_limit: auto_zoom.at_limit,
                    error: if auto_zoom_attempted {
                        auto_zoom_error.clone()
                    } else {
                        previous_auto_zoom_error
                    },
                },
                error: command_error,
            };
        })
        .await;
    if zoom_changed {
        state
            .runtime
            .emit(
                "camera.auto_zoom.adjusted",
                "face-tracking",
                None,
                json!({"magnification": auto_zoom.requested_magnification}),
            )
            .await;
    }
}

async fn drive_hands_tracking(state: &ApiState, hand_landmarks: &[Landmark], captured_at_ms: u64) {
    let Some(camera) = state.camera.clone() else {
        return;
    };
    let mut controller = state.hands_tracking.lock().await;
    if !controller.enabled() {
        return;
    }

    let camera_state = state.runtime.state().await.camera;
    if camera_state.tracking == Some(true) {
        let error = camera
            .set_face_tracking_speed(0, 0, 0.0)
            .await
            .err()
            .map(|error| error.to_string());
        controller.set_enabled(false);
        state
            .runtime
            .update(|runtime| {
                runtime.camera.hands_tracking = HandsTrackingState {
                    error,
                    ..HandsTrackingState::default()
                };
            })
            .await;
        record_camera_command(
            state,
            "camera.hands_tracking",
            json!({"enabled": false, "reason": "camera-tracking-detected"}),
        )
        .await;
        return;
    }

    let decision = controller.observe(
        hand_landmarks,
        camera.controlled_zoom_magnification(),
        captured_at_ms,
    );
    let desired_motion = decision.motion;
    let should_command = desired_motion != controller.motion() || desired_motion.active();
    let command_error = if should_command {
        camera
            .set_face_tracking_speed(
                desired_motion.pan_direction,
                desired_motion.tilt_direction,
                desired_motion.speed_fraction,
            )
            .await
            .err()
            .map(|error| error.to_string())
    } else {
        None
    };
    if command_error.is_none() {
        controller.record_motion(desired_motion);
    }
    let zoom_error = if let Some(magnification) = decision.requested_magnification {
        camera
            .set_zoom(magnification)
            .await
            .err()
            .map(|error| error.to_string())
    } else {
        None
    };
    let motion = controller.motion();
    drop(controller);
    let controlled_zoom = camera.controlled_zoom_magnification();
    let zoom_changed = decision.requested_magnification.is_some() && zoom_error.is_none();
    let zoom_attempted = decision.requested_magnification.is_some();
    state
        .runtime
        .update(|runtime| {
            if let Some(magnification) = decision
                .requested_magnification
                .filter(|_| zoom_error.is_none())
            {
                runtime.camera.zoom_magnification = Some(magnification);
                runtime.camera.zoom_sample_at_ms = None;
                runtime.camera.zoom_error = None;
                runtime.camera.last_command_at_ms = Some(unix_ms());
            }
            let previous_zoom_error = runtime.camera.hands_tracking.zoom_error.clone();
            runtime.camera.hands_tracking = HandsTrackingState {
                enabled: true,
                active: motion.active(),
                hands_visible: decision.hands_visible,
                rapid_motion: decision.rapid_motion,
                zoom_frozen: decision.zoom_frozen,
                target_x: decision.target_x,
                target_y: decision.target_y,
                speed_fraction: motion.speed_fraction as f32,
                calibrated: decision.calibrated,
                zoom_magnification: controlled_zoom,
                target_span: decision.target_span,
                hand_span: decision.hand_span,
                at_limit: decision.at_limit,
                error: command_error,
                zoom_error: if zoom_attempted {
                    zoom_error.clone()
                } else {
                    previous_zoom_error
                },
            };
        })
        .await;
    if zoom_changed {
        state
            .runtime
            .emit(
                "camera.hands_tracking.zoom_adjusted",
                "hands-tracking",
                None,
                json!({"magnification": decision.requested_magnification}),
            )
            .await;
    }
}

async fn activate_scenario(state: &ApiState, scenario: &ScenarioConfig, trigger_sequence: u64) {
    let activation = ScenarioActivation {
        scenario_id: scenario.id.clone(),
        action: scenario.action.clone(),
        triggered_at_ms: unix_ms(),
        trigger_sequence,
    };
    state
        .runtime
        .update(|runtime| runtime.last_scenario = Some(activation.clone()))
        .await;
    state
        .runtime
        .emit(
            "scenario.activated",
            "scenario-engine",
            None,
            serde_json::to_value(activation).unwrap_or(Value::Null),
        )
        .await;
}

async fn voice_settings(State(state): State<ApiState>) -> Json<Value> {
    Json(
        json!({"available": !state.config.audio.voice_worker.is_empty(), "state": state.runtime.state().await.audio_voice}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VoiceRequest {
    enabled: bool,
    pitch: i32,
    #[serde(default)]
    model: Option<String>,
}

async fn set_voice_settings(
    State(state): State<ApiState>,
    Json(request): Json<VoiceRequest>,
) -> Response {
    if !(-12..=12).contains(&request.pitch)
        || (request.enabled && state.config.audio.voice_worker.is_empty())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Voice conversion is unavailable or pitch is outside -12 to 12"})),
        )
            .into_response();
    }
    let _guard = state.audio_settings_control.lock().await;
    let mut audio = crate::settings::AudioSettings::from_state(&state.runtime.state().await);
    audio.voice_enabled = request.enabled;
    audio.voice_pitch = request.pitch;
    if let Some(model) = request.model {
        let Some(directory) = &state.config.audio.voice_models_dir else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"Model selection is unavailable"})),
            )
                .into_response();
        };
        if crate::voice::model_path(directory, &model).is_err() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"Model is not installed"})),
            )
                .into_response();
        }
        audio.voice_model = model;
    }
    if let Some(settings) = &state.user_settings {
        if let Err(error) = settings.set_audio(audio.clone()).await {
            return user_settings_error(error);
        }
    }
    state.runtime.update(|s| audio.apply(s)).await;
    Json(state.runtime.state().await.audio_voice).into_response()
}

async fn voice_models(State(state): State<ApiState>) -> Response {
    let Some(directory) = &state.config.audio.voice_models_dir else {
        return Json(json!({"available":false,"models":[]})).into_response();
    };
    match crate::voice::models(directory) {
        Ok(models) => Json(json!({"available":true,"models":models})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelUpload {
    name: String,
}

async fn upload_voice_model(
    State(state): State<ApiState>,
    Query(query): Query<ModelUpload>,
    bytes: Bytes,
) -> Response {
    let Some(directory) = state.config.audio.voice_models_dir.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Model import is unavailable"})),
        )
            .into_response();
    };
    let result = tokio::task::spawn_blocking(move || {
        crate::voice::install_model(&directory, &query.name, &bytes)
    })
    .await;
    match result {
        Ok(Ok(())) => voice_models(State(state)).await,
        Ok(Err(error)) => (StatusCode::BAD_REQUEST, Json(json!({"error":format!("Could not import model (existing files are not replaced): {error}")}))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"Model import failed"}))).into_response(),
    }
}

async fn audio_sources(State(state): State<ApiState>) -> Response {
    match crate::audio::sources(&state.config.audio).await {
        Ok(sources) => {
            let enabled = state.runtime.state().await.audio_capture_sources;
            Json(
                sources
                    .into_iter()
                    .map(|source| {
                        json!({
                            "id": source.id, "name": source.name, "muted": source.muted,
                            "enabled": enabled.contains(&source.id),
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .into_response()
        }
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn audio_applications(
    State(state): State<ApiState>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(source) = query.get("source").filter(|source| !source.is_empty()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(audio) = state.audio else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match audio.inspect_applications(source).await {
        Ok(applications) => Json(applications).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KillAudioApplicationRequest {
    source: String,
    process: crate::audio::ProcessIdentity,
    signal: i32,
}

async fn kill_audio_application(
    State(state): State<ApiState>,
    Json(request): Json<KillAudioApplicationRequest>,
) -> Response {
    let Some(audio) = state.audio else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match audio
        .kill_application(&request.source, request.process, request.signal)
        .await
    {
        Ok(()) => Json(json!({"signal": request.signal, "sent": true})).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct AudioExclusiveRequest {
    source: String,
    exclusive: bool,
}

async fn set_audio_exclusive(
    State(state): State<ApiState>,
    Json(request): Json<AudioExclusiveRequest>,
) -> Response {
    if !state.config.audio.reserve_inputs || request.source == state.config.audio.virtual_source {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Tarsier Microphone must stay shared"})),
        )
            .into_response();
    }
    if !state
        .runtime
        .state()
        .await
        .audio_reservations
        .contains_key(&request.source)
    {
        match crate::audio::sources(&state.config.audio).await {
            Ok(sources) if sources.iter().any(|s| s.id == request.source) => {}
            Ok(_) => return StatusCode::NOT_FOUND.into_response(),
            Err(e) => return command_error(e),
        }
    }
    state
        .runtime
        .update(|s| {
            s.audio_released_sources.retain(|id| id != &request.source);
            if !request.exclusive {
                s.audio_released_sources.push(request.source.clone());
            }
        })
        .await;
    Json(json!({"source": request.source, "exclusive": request.exclusive})).into_response()
}

#[derive(Deserialize)]
struct AudioCaptureRequest {
    source: String,
    enabled: bool,
}

async fn set_audio_capture(
    State(state): State<ApiState>,
    Json(request): Json<AudioCaptureRequest>,
) -> Response {
    let _guard = state.audio_settings_control.lock().await;
    if request.enabled {
        match crate::audio::sources(&state.config.audio).await {
            Ok(sources) if sources.iter().any(|source| source.id == request.source) => {}
            Ok(_) => return StatusCode::NOT_FOUND.into_response(),
            Err(error) => return command_error(error),
        }
    }
    let mut next = state.runtime.state().await;
    {
        let current = &mut next;
        current
            .audio_capture_sources
            .retain(|source| source != &request.source);
        if request.enabled {
            current.audio_capture_sources.push(request.source.clone());
            current.audio_capture_sources.sort();
        }
    }
    let audio = crate::settings::AudioSettings::from_state(&next);
    if let Some(settings) = &state.user_settings
        && let Err(error) = settings.set_audio(audio.clone()).await
    {
        return user_settings_error(error);
    }
    state.runtime.update(|current| audio.apply(current)).await;
    Json(json!({"source": request.source, "enabled": request.enabled})).into_response()
}

async fn audio_meter(
    websocket: WebSocketUpgrade,
    State(state): State<ApiState>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(source) = query.get("source") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(audio) = state.audio else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let states = state.runtime.subscribe_state();
    let enabled = if source == &state.config.audio.virtual_source {
        states.borrow().audio_virtual.enabled
    } else {
        states.borrow().audio_capture_sources.contains(source)
    };
    if !enabled {
        return StatusCode::CONFLICT.into_response();
    }
    let source = source.clone();
    websocket.on_upgrade(move |socket| async move {
        audio.stream(socket, source, state.shutdown, states).await;
    })
}

async fn virtual_audio(State(state): State<ApiState>) -> Json<crate::audio::VirtualMicrophone> {
    Json(state.runtime.state().await.audio_virtual)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VirtualAudioRequest {
    auto_gain: Option<bool>,
    enabled: Option<bool>,
    source: Option<String>,
    muted: Option<bool>,
}

async fn set_virtual_audio(
    State(state): State<ApiState>,
    Json(request): Json<VirtualAudioRequest>,
) -> Response {
    let _video_guard = state.video_output_control.lock().await;
    set_virtual_audio_locked(state.clone(), request).await
}

// Callers must hold video_output_control before changing virtual audio.
async fn set_virtual_audio_locked(state: ApiState, request: VirtualAudioRequest) -> Response {
    let _guard = state.audio_settings_control.lock().await;
    if request.enabled == Some(true) && !state.config.audio.virtual_output_enabled {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Virtual output is disabled by daemon configuration"})),
        )
            .into_response();
    }
    let recording = state.recorder.status().await;
    if request.enabled == Some(false) && recording.active && recording.audio {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Mute output to silence the recording, or stop recording before turning audio output off"})),
        ).into_response();
    }
    if let Some(source) = &request.source {
        match crate::audio::sources(&state.config.audio).await {
            Ok(sources) if sources.iter().any(|s| &s.id == source) => {}
            Ok(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "Select an available input microphone"})),
                )
                    .into_response();
            }
            Err(e) => return command_error(e),
        }
    }
    let mut next = state.runtime.state().await;
    {
        let s = &mut next;
        if let Some(automatic) = request.auto_gain {
            s.audio_virtual.auto_gain = automatic;
        }
        if let Some(source) = &request.source {
            s.audio_virtual.source = Some(source.clone());
        }
        if let Some(enabled) = request.enabled {
            s.audio_virtual.enabled = enabled;
        }
        if let Some(muted) = request.muted {
            s.audio_virtual.muted = muted;
        }
        if s.audio_virtual.enabled
            && (request.enabled == Some(true) || request.source.is_some())
            && let Some(source) = &s.audio_virtual.source
            && !s.audio_capture_sources.contains(source)
        {
            s.audio_capture_sources.push(source.clone());
            s.audio_capture_sources.sort();
        }
    }
    let audio = crate::settings::AudioSettings::from_state(&next);
    if let Some(settings) = &state.user_settings
        && let Err(error) = settings.set_audio(audio.clone()).await
    {
        return user_settings_error(error);
    }
    state.runtime.update(|current| audio.apply(current)).await;
    Json(state.runtime.state().await.audio_virtual).into_response()
}

async fn events_socket(
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
    State(state): State<ApiState>,
) -> impl IntoResponse {
    websocket.on_upgrade(move |socket| {
        stream_events(
            socket,
            state.runtime,
            state.shutdown,
            state.recorder,
            state.auth,
            headers,
        )
    })
}

async fn stream_events(
    socket: WebSocket,
    runtime: Runtime,
    mut shutdown: watch::Receiver<bool>,
    recorder: crate::recording::Recorder,
    auth: Option<crate::auth::Auth>,
    headers: HeaderMap,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = runtime.subscribe_events();
    let mut states = runtime.subscribe_state();
    let mut recording_tick = tokio::time::interval(std::time::Duration::from_millis(250));
    recording_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_recording = None;
    let mut auth_tick = tokio::time::interval(std::time::Duration::from_secs(1));

    let initial = json!({"type": "state", "data": states.borrow().clone()});
    if sender
        .send(Message::Text(initial.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            },
            _ = auth_tick.tick() => {
                if let Some(auth) = &auth
                    && !auth.allowed(&headers, "/api/v1/events", "GET").await { break; }
            }
            _ = recording_tick.tick() => {
                let recording = recorder.status().await;
                if last_recording.as_ref() != Some(&recording) {
                    let payload = json!({"type": "recording", "data": recording});
                    if sender.send(Message::Text(payload.to_string().into())).await.is_err() {
                        break;
                    }
                    last_recording = Some(recording);
                }
            },
            event = events.recv() => match event {
                Ok(event) => {
                    let payload = json!({"type": "event", "data": event});
                    if sender.send(Message::Text(payload.to_string().into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            changed = states.changed() => {
                if changed.is_err() {
                    break;
                }
                let payload = json!({"type": "state", "data": states.borrow().clone()});
                if sender.send(Message::Text(payload.to_string().into())).await.is_err() {
                    break;
                }
            },
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    }
}

#[allow(dead_code)]
fn infallible(_: Infallible) {}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::{camera, config::CameraAdapter, settings::UserSettings};

    #[test]
    fn mcp_usage_guard_detects_capture_without_visible_processes() {
        for (capture_active, expected) in [
            (Some(true), Some(StatusCode::CONFLICT)),
            (Some(false), None),
            (None, Some(StatusCode::SERVICE_UNAVAILABLE)),
        ] {
            let result = mcp_usage_rejection(Ok(crate::video_clients::Snapshot {
                available: true,
                partial: true,
                capture_active,
                applications: vec![],
            }));
            assert_eq!(result.map(|r| r.status()), expected);
        }
    }

    #[test]
    fn mcp_usage_guard_allows_partial_scans_but_blocks_inspection_errors() {
        for partial in [false, true] {
            let result = mcp_usage_rejection(Ok(crate::video_clients::Snapshot {
                available: true,
                partial,
                capture_active: Some(false),
                applications: vec![],
            }));
            assert!(result.is_none());
        }
        assert_eq!(
            mcp_usage_rejection(Err(anyhow::anyhow!("scan failed")))
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn mcp_commands_work_without_a_virtual_camera_reader() {
        for enabled in [true, false] {
            let mut config = Config::default();
            config.video.loopback_enabled = enabled;
            config.video.output_device = String::new();
            let (_stop, shutdown) = watch::channel(false);
            let app = router(config, Runtime::new(), PreviewHub::new(), None, shutdown);
            let response = app
                .oneshot(
                    Request::post("/mcp/api/v1/scenarios/open-palm-demo/trigger")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }
    }

    #[tokio::test]
    async fn mcp_mutations_require_inspection_but_reads_and_ui_do_not() {
        let mut config = Config::default();
        config.video.loopback_enabled = true;
        // ENOTDIR makes inspection fail deterministically without touching hardware.
        config.video.output_device = "/dev/null/not-a-device".into();
        let (_stop, shutdown) = watch::channel(false);
        let app = router(config, Runtime::new(), PreviewHub::new(), None, shutdown);
        for path in [
            "/api/v1/camera/move",
            "/api/v1/camera/actions/recenter",
            "/api/v1/camera/tracking",
            "/api/v1/camera/presets/test/recall",
            "/api/v1/scenarios/test/trigger",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(format!("/mcp{path}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            assert!(String::from_utf8_lossy(&body).contains("MCP commands are blocked"));
        }
        let read = app
            .clone()
            .oneshot(
                Request::get("/mcp/api/v1/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(read.status(), StatusCode::OK);
        // A regular HTTP/UI request reaches the scenario handler (unknown ID).
        let manual = app
            .oneshot(
                Request::post("/api/v1/scenarios/missing/trigger")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(manual.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn voice_model_import_and_selection_preserve_output_settings() {
        let directory =
            std::env::temp_dir().join(format!("tarsier-voice-api-{:032x}", rand::random::<u128>()));
        let runtime = Runtime::new();
        runtime
            .update(|s| {
                s.audio_virtual.muted = true;
                s.audio_virtual.auto_gain = false;
            })
            .await;
        let mut config = Config::default();
        config.audio.voice_worker = vec!["unused-test-worker".into()];
        config.audio.voice_models_dir = Some(directory.clone());
        let (_stop, shutdown) = watch::channel(false);
        let app = router(config, runtime.clone(), PreviewHub::new(), None, shutdown);
        let imported = app
            .clone()
            .oneshot(
                Request::post("/api/v1/audio/voice/models?name=Trial.pth")
                    .body(Body::from("test checkpoint placeholder"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(imported.status(), StatusCode::OK);
        assert_eq!(
            runtime.state().await.audio_voice.model,
            crate::voice::default_model()
        );
        for (model, expected) in [
            ("Missing.pth", StatusCode::BAD_REQUEST),
            ("../Trial.pth", StatusCode::BAD_REQUEST),
            ("Trial.pth", StatusCode::OK),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/audio/voice")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"enabled":true,"pitch":4,"model":model}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        let selected = runtime.state().await;
        assert_eq!(selected.audio_voice.model, "Trial.pth");
        assert_eq!(selected.audio_voice.generation, 1);
        assert!(!selected.audio_voice.ready);
        assert!(selected.audio_virtual.muted);
        assert!(!selected.audio_virtual.auto_gain);
        let response = app
            .oneshot(
                Request::post("/api/v1/audio/voice")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":false,"pitch":3}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(runtime.state().await.audio_voice.model, "Trial.pth");
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[tokio::test]
    async fn voice_controls_validate_pitch_and_preserve_mute() {
        let runtime = Runtime::new();
        runtime.update(|s| s.audio_virtual.muted = true).await;
        let (_stop, shutdown) = watch::channel(false);
        let app = router(
            Config::default(),
            runtime.clone(),
            PreviewHub::new(),
            None,
            shutdown,
        );
        for (body, expected) in [
            (r#"{"enabled":true,"pitch":0}"#, StatusCode::BAD_REQUEST),
            (r#"{"enabled":false,"pitch":13}"#, StatusCode::BAD_REQUEST),
            (r#"{"enabled":false,"pitch":3}"#, StatusCode::OK),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/audio/voice")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        let state = runtime.state().await;
        assert!(state.audio_virtual.muted);
        assert_eq!(state.audio_voice.pitch, 3);
        assert!(!state.audio_voice.enabled);
    }

    #[tokio::test]
    async fn isolated_profile_rejects_output_and_exclusive_capture() {
        let mut config = Config::default();
        config.audio.reserve_inputs = false;
        config.audio.virtual_output_enabled = false;
        let (_stop, shutdown) = watch::channel(false);
        let runtime = Runtime::new();
        let app = router(config, runtime.clone(), PreviewHub::new(), None, shutdown);
        for (path, body) in [
            ("/api/v1/audio/virtual", r#"{"enabled":true}"#),
            (
                "/api/v1/audio/exclusive",
                r#"{"source":"tarsier_microphone","exclusive":true}"#,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                response.status(),
                StatusCode::CONFLICT | StatusCode::BAD_REQUEST
            ));
        }
        assert!(!runtime.state().await.audio_virtual.enabled);
        assert!(runtime.state().await.audio_released_sources.is_empty());
    }

    #[tokio::test]
    async fn release_preserves_capture_and_virtual_output_and_never_reserves_own_output() {
        let runtime = Runtime::new();
        runtime
            .update(|s| {
                s.audio_capture_sources = vec!["mic".into()];
                s.audio_reservations
                    .insert("mic".into(), crate::audio::Reservation::default());
                s.audio_virtual.enabled = true;
                s.audio_virtual.source = Some("mic".into());
            })
            .await;
        let (_stop, shutdown) = watch::channel(false);
        let app = router(
            Config::default(),
            runtime.clone(),
            PreviewHub::new(),
            None,
            shutdown,
        );
        for exclusive in [false, true] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/audio/exclusive")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"source":"mic", "exclusive":exclusive}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let state = runtime.state().await;
            assert_eq!(
                state.audio_released_sources.contains(&"mic".into()),
                !exclusive
            );
            assert_eq!(state.audio_capture_sources, vec!["mic"]);
            assert!(state.audio_virtual.enabled);
        }
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/audio/exclusive")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"source":"tarsier_microphone","exclusive":true}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn automatic_gain_defaults_on_and_can_be_disabled_without_muting() {
        let runtime = Runtime::new();
        assert!(runtime.state().await.audio_virtual.auto_gain);
        let (_stop, shutdown) = watch::channel(false);
        let app = router(
            Config::default(),
            runtime.clone(),
            PreviewHub::new(),
            None,
            shutdown,
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/audio/virtual")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"auto_gain":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let state = runtime.state().await;
        assert!(!state.audio_virtual.auto_gain);
        assert!(!state.audio_virtual.muted);
    }

    #[tokio::test]
    async fn virtual_microphone_mute_is_shared_without_stopping_input_capture() {
        let runtime = Runtime::new();
        runtime
            .update(|s| {
                s.audio_capture_sources = vec!["mic".into()];
                s.audio_virtual = crate::audio::VirtualMicrophone {
                    enabled: true,
                    source: Some("mic".into()),
                    running: true,
                    ..Default::default()
                };
            })
            .await;
        let mut client = runtime.subscribe_state();
        let (_stop, shutdown) = watch::channel(false);
        let app = router(
            Config::default(),
            runtime.clone(),
            PreviewHub::new(),
            None,
            shutdown,
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/audio/virtual")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"muted":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        client.changed().await.unwrap();
        let state = client.borrow_and_update();
        assert!(state.audio_virtual.muted);
        assert!(state.audio_virtual.enabled);
        assert_eq!(state.audio_capture_sources, vec!["mic"]);
        assert_eq!(state.audio_virtual.source.as_deref(), Some("mic"));
    }

    #[tokio::test]
    async fn disabling_disconnected_audio_updates_all_clients_and_reconnects() {
        let runtime = Runtime::new();
        runtime
            .update(|state| {
                state.audio_capture_sources = vec!["disconnected-mic".into(), "other-mic".into()];
                state.last_photo =
                    Some(json!({"path": "/photos/test.jpg", "url": "/photos/test.jpg"}));
            })
            .await;
        let mut first = runtime.subscribe_state();
        let mut second = runtime.subscribe_state();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            Config::default(),
            runtime.clone(),
            PreviewHub::new(),
            None,
            shutdown_rx,
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/audio/capture")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"source":"disconnected-mic","enabled":false}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        for client in [&mut first, &mut second] {
            tokio::time::timeout(Duration::from_secs(1), client.changed())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(client.borrow().audio_capture_sources, vec!["other-mic"]);
            assert_eq!(
                client.borrow().last_photo.as_ref().unwrap()["url"],
                "/photos/test.jpg"
            );
        }
        let reconnected = runtime.subscribe_state();
        assert_eq!(
            reconnected.borrow().audio_capture_sources,
            vec!["other-mic"]
        );
        assert_eq!(reconnected.borrow().last_photo, first.borrow().last_photo);
    }

    #[test]
    fn photos_are_saved_without_overwriting_and_write_errors_are_reported() {
        let directory = std::env::temp_dir().join(format!(
            "tarsier-photo-test-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let first = save_photo(&directory, b"first JPEG").unwrap();
        let second = save_photo(&directory, b"second JPEG").unwrap();
        assert_ne!(first, second);
        let name = first.file_stem().unwrap().to_str().unwrap();
        let parts: Vec<_> = name.split('-').collect();
        assert_eq!(parts.len(), 3);
        chrono::NaiveDateTime::parse_from_str(
            &format!("{}-{}", parts[0], parts[1]),
            "%Y%m%d-%H%M%S",
        )
        .unwrap();
        assert_eq!(
            read_saved_photo(&directory, first.file_name().unwrap().to_str().unwrap()).unwrap(),
            b"first JPEG"
        );
        assert!(read_saved_photo(&directory, "../secret.jpg").is_err());
        assert!(read_saved_photo(&directory, "photo-1-2-3.jpg").is_err());
        std::os::unix::fs::symlink(&first, directory.join("photo-1-2-3.jpg")).unwrap();
        assert!(read_saved_photo(&directory, "photo-1-2-3.jpg").is_err());
        assert_eq!(std::fs::read(&first).unwrap(), b"first JPEG");
        assert_eq!(std::fs::read(&second).unwrap(), b"second JPEG");
        assert!(save_photo(&first, b"cannot write inside a file").is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn photos_reject_missing_or_stale_output() {
        let preview = PreviewHub::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            Config::default(),
            Runtime::new(),
            preview.clone(),
            None,
            shutdown_rx,
        );
        for stale in [false, true] {
            if stale {
                preview.publish_photo(crate::media_metadata::CapturedImage::new(
                    PerceptionFrame {
                        bytes: Bytes::from_static(b"stale JPEG"),
                        frame_id: 1,
                        captured_at_ms: unix_ms() - 2000,
                    },
                    crate::model::RuntimeState::default(),
                    (640, 480),
                ));
            }
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/camera/photos")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    #[test]
    fn pan_tilt_directions_match_tiny_2_speed_signs() {
        assert_eq!(PanTiltDirection::Left.vector(), (-1, 0));
        assert_eq!(PanTiltDirection::Right.vector(), (1, 0));
        assert_eq!(PanTiltDirection::Up.vector(), (0, 1));
        assert_eq!(PanTiltDirection::Down.vector(), (0, -1));
        assert_eq!(PanTiltDirection::Stop.vector(), (0, 0));
        for (name, vector) in [
            ("up-left", (-1, 1)),
            ("up-right", (1, 1)),
            ("down-left", (-1, -1)),
            ("down-right", (1, -1)),
        ] {
            let direction: PanTiltDirection = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(direction.vector(), vector);
            assert_eq!(direction.as_str(), name);
        }
    }

    #[tokio::test]
    async fn network_settings_report_scope_and_require_supervision() {
        let mut config = Config::default();
        config.server.bind = "0.0.0.0:8742".parse().unwrap();
        config.perception.enabled = false;
        let (_tx, rx) = watch::channel(false);
        let app = router(config, Runtime::new(), PreviewHub::new(), None, rx);
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/v1/settings/network")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["lan_access"], true);
        assert_eq!(body["can_apply"], false);
        let response = app
            .oneshot(
                Request::post("/api/v1/settings/network")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"lan_access":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn embedded_ui_assets_are_not_cached() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, Runtime::new(), PreviewHub::new(), None, shutdown_rx);

        for path in [
            "/",
            "/assets/app.js",
            "/assets/styles.css",
            "/assets/lucide.js",
            "/assets/preview-drag.js",
            "/settings",
            "/assets/settings.js",
        ] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
        assert!(include_str!("../web/index.html").contains("id=\"face-tracking-toggle\""));
        assert!(include_str!("../web/app.js").contains("/api/v1/camera/face-tracking"));
        assert!(include_str!("../web/index.html").contains("id=\"hands-tracking-toggle\""));
        assert!(include_str!("../web/app.js").contains("/api/v1/camera/hands-tracking"));
        assert!(include_str!("../web/index.html").contains("id=\"camera-power-toggle\""));
        assert!(include_str!("../web/app.js").contains("/api/v1/camera/power"));
        assert!(include_str!("../web/index.html").contains("id=\"background-toggle\""));
        assert!(include_str!("../web/app.js").contains("/api/v1/video/background"));
        assert!(include_str!("../web/index.html").contains("data-output-mode"));
        assert!(include_str!("../web/app.js").contains("/api/v1/video/identity"));
        assert!(include_str!("../web/index.html").contains("id=\"daemon-restart-dialog\""));
        assert!(include_str!("../web/app.js").contains("/api/v1/daemon/restart"));
        assert!(include_str!("../web/index.html").contains("id=\"image-settings-groups\""));
        assert!(include_str!("../web/app.js").contains("/api/v1/camera/image-settings/"));
        assert!(include_str!("../web/app.js").contains("face-priority-auto-exposure"));
    }

    #[tokio::test]
    async fn camera_power_control_stops_and_restores_the_pipeline() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        runtime
            .update(|state| {
                state.audio_virtual.enabled = true;
                state.audio_virtual.source = Some("alsa_input.usb-DJI_MIC_MINI".into());
                state.pipeline.enabled = true;
                state.pipeline.running = true;
            })
            .await;
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let pipeline = VideoPipelineControl::mock(runtime.clone());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router_with_controls(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            ApiOptions {
                pipeline: Some(pipeline),
                ..ApiOptions::default()
            },
            shutdown_rx,
        );

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/power")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let state = runtime.state().await;
        assert_eq!(state.camera.powered_on, Some(false));
        assert!(!state.pipeline.enabled);
        assert!(!state.pipeline.running);
        assert!(state.audio_virtual.muted);
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/audio/virtual")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"muted":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!runtime.state().await.audio_virtual.muted);
        assert_eq!(runtime.state().await.camera.powered_on, Some(false));

        // Repeating Power Off must not override an explicit unmute while asleep.
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/power")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(!runtime.state().await.audio_virtual.muted);

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/perception/observations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "frame_id": 1,
                            "captured_at_ms": 100,
                            "face_detected": false,
                            "hand_detected": false,
                            "pose_detected": false,
                            "gesture": null,
                            "confidence": 0.0,
                            "latency_ms": 1.0
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!runtime.state().await.perception.worker_connected);

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/actions/recenter")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let payload: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(payload["error"], "camera is powered off");

        let response = app
            .oneshot(
                Request::post("/api/v1/camera/power")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let state = runtime.state().await;
        assert_eq!(state.camera.powered_on, Some(true));
        assert!(state.pipeline.enabled);
        assert!(state.pipeline.running);
        let power_events = runtime
            .recent_events()
            .await
            .into_iter()
            .filter(|event| event.kind == "camera.power")
            .collect::<Vec<_>>();
        assert_eq!(power_events.len(), 3);
        assert_eq!(power_events[0].data["enabled"], false);
        assert_eq!(power_events[1].data["enabled"], false);
        assert_eq!(power_events[2].data["enabled"], true);
    }

    #[tokio::test]
    async fn health_identifies_the_running_daemon_instance() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let expected_started_at_ms = runtime.state().await.started_at_ms;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime, PreviewHub::new(), None, shutdown_rx);

        let response = app
            .oneshot(Request::get("/api/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let payload: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(payload["started_at_ms"], expected_started_at_ms);
        assert_eq!(payload["restart_available"], false);
    }

    #[tokio::test]
    async fn daemon_restart_requires_supervision_and_signals_the_owner() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let unsupported = router(
            config.clone(),
            Runtime::new(),
            PreviewHub::new(),
            None,
            shutdown_rx,
        );
        let response = unsupported
            .oneshot(
                Request::post("/api/v1/daemon/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let (restart_tx, restart_rx) = oneshot::channel();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let supported = router_with_controls(
            config,
            Runtime::new(),
            PreviewHub::new(),
            None,
            ApiOptions {
                daemon_restart: Some(DaemonRestart::new(restart_tx)),
                ..ApiOptions::default()
            },
            shutdown_rx,
        );
        let health = supported
            .clone()
            .oneshot(Request::get("/api/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let payload: Value =
            serde_json::from_slice(&to_bytes(health.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(payload["restart_available"], true);

        let response = supported
            .oneshot(
                Request::post("/api/v1/daemon/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        tokio::time::timeout(Duration::from_secs(1), restart_rx)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn preview_stream_closes_when_shutdown_starts() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime, PreviewHub::new(), None, shutdown_rx);

        let response = app
            .clone()
            .oneshot(
                Request::get("/api/v1/preview.mjpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        shutdown_tx.send(true).unwrap();

        tokio::time::timeout(Duration::from_secs(1), to_bytes(response.into_body(), 1024))
            .await
            .expect("preview stream should stop after shutdown")
            .unwrap();
    }

    #[tokio::test]
    async fn mjpeg_stream_pauses_across_a_pipeline_power_cycle() {
        let (frames_tx, frames_rx) = watch::channel(None);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let response = mjpeg_response(frames_rx, shutdown_rx);
        let mut body = response.into_body().into_data_stream();

        frames_tx.send_replace(Some(Bytes::from_static(b"first-frame")));
        let _header = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            first
                .windows(b"first-frame".len())
                .any(|part| part == b"first-frame")
        );
        let _terminator = body.next().await.unwrap().unwrap();

        frames_tx.send_replace(None);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), body.next())
                .await
                .is_err()
        );

        frames_tx.send_replace(Some(Bytes::from_static(b"second-frame")));
        let _header = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            second
                .windows(b"second-frame".len())
                .any(|part| part == b"second-frame")
        );
        let _terminator = body.next().await.unwrap().unwrap();

        shutdown_tx.send(true).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), body.next())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn perception_mjpeg_carries_source_frame_provenance() {
        let (frames_tx, frames_rx) = watch::channel(None);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let effects = crate::effects::VideoEffects::new();
        effects.set_transform(crate::video_transform::VideoTransform {
            rotation: 180,
            mirror: true,
        });
        let response = perception_mjpeg_response(frames_rx, shutdown_rx, effects);
        let mut body = response.into_body().into_data_stream();

        frames_tx.send_replace(Some(PerceptionFrame {
            bytes: Bytes::from_static(b"jpeg-frame"),
            frame_id: 152,
            captured_at_ms: 1_725_000_000_033,
        }));
        let header = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let header = std::str::from_utf8(&header).unwrap();

        assert!(header.contains("X-Tarsier-Frame-Id: 152\r\n"));
        assert!(header.contains("X-Tarsier-Inference-Rotation: 180\r\n"));
        assert!(header.contains("X-Tarsier-Captured-At-Ms: 1725000000033\r\n"));
    }

    #[tokio::test]
    async fn resolution_validation_and_4k_effect_guards() {
        let mut config = Config::default();
        config.video.width = 3840;
        config.video.height = 2160;
        config.perception.enabled = false;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, Runtime::new(), PreviewHub::new(), None, shutdown_rx);
        for (path, body, expected) in [
            (
                "resolution",
                r#"{"width":9999,"height":2160}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                "resolution",
                r#"{"width":3840,"height":2160}"#,
                StatusCode::CONFLICT,
            ),
            (
                "background",
                r#"{"enabled":true,"effect":"blur"}"#,
                StatusCode::CONFLICT,
            ),
            ("green-screen", r#"{"enabled":true}"#, StatusCode::CONFLICT),
            (
                "output-mode",
                r#"{"mode":"comic-avatar"}"#,
                StatusCode::CONFLICT,
            ),
            (
                "transform",
                r#"{"rotation":90,"mirror":false}"#,
                StatusCode::CONFLICT,
            ),
            (
                "background",
                r#"{"enabled":false,"effect":"blur"}"#,
                StatusCode::ACCEPTED,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(format!("/api/v1/video/{path}"))
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
        }
    }

    #[tokio::test]
    async fn green_screen_control_updates_the_live_effect_and_runtime_state() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime.clone(), preview.clone(), None, shutdown_rx);

        let response = app
            .oneshot(
                Request::post("/api/v1/video/green-screen")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(preview.effects().green_screen_enabled());
        let state = runtime.state().await;
        assert!(state.video_effects.background_enabled);
        assert_eq!(
            state.video_effects.background_effect,
            BackgroundEffect::GreenScreen
        );
        assert!(state.video_effects.green_screen_enabled);
        let events = runtime.recent_events().await;
        assert_eq!(events[0].kind, "video.effect.green_screen");
        assert_eq!(events[0].data["enabled"], true);
    }

    #[tokio::test]
    async fn background_control_selects_one_live_effect() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime.clone(), preview.clone(), None, shutdown_rx);

        let response = app
            .oneshot(
                Request::post("/api/v1/video/background")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true,"effect":"pixel-party"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(preview.effects().background_enabled());
        assert_eq!(
            preview.effects().background_effect(),
            BackgroundEffect::PixelParty
        );
        assert!(!preview.effects().green_screen_enabled());
        let state = runtime.state().await;
        assert!(state.video_effects.background_enabled);
        assert_eq!(
            state.video_effects.background_effect,
            BackgroundEffect::PixelParty
        );
        assert!(!state.video_effects.green_screen_enabled);
        let events = runtime.recent_events().await;
        assert_eq!(events[0].kind, "video.effect.background");
        assert_eq!(events[0].data["enabled"], true);
        assert_eq!(events[0].data["effect"], "pixel-party");
    }

    #[tokio::test]
    async fn output_mode_control_switches_to_the_comic_avatar() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime.clone(), preview.clone(), None, shutdown_rx);

        let response = app
            .oneshot(
                Request::post("/api/v1/video/output-mode")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"comic-avatar"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            preview.effects().output_mode(),
            VideoOutputMode::ComicAvatar
        );
        assert_eq!(
            runtime.state().await.video_effects.output_mode,
            VideoOutputMode::ComicAvatar
        );
        let events = runtime.recent_events().await;
        assert_eq!(events[0].kind, "video.output.mode");
        assert_eq!(events[0].data["mode"], "comic-avatar");
    }

    #[tokio::test]
    async fn portrait_selection_requires_persistent_settings() {
        let mut config = Config::default();
        config.avatar.enabled = true;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, Runtime::new(), PreviewHub::new(), None, shutdown_rx);
        let response = app
            .oneshot(
                Request::post("/api/v1/video/liveportrait/source")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"id":"unknown"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn portrait_gallery_selects_existing_file_without_restart() {
        let mut config = Config::default();
        config.avatar.enabled = true;
        let directory =
            std::env::temp_dir().join(format!("tarsier-portrait-{:032x}", rand::random::<u128>()));
        let path = directory.join("settings.json");
        let fallback = UserSettings::from_config(&config);
        let (settings, _) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        let (restart_tx, mut restart_rx) = oneshot::channel();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let app = router_with_controls(
            config,
            runtime.clone(),
            preview.clone(),
            None,
            ApiOptions {
                user_settings: Some(settings),
                daemon_restart: Some(DaemonRestart::new(restart_tx)),
                ..ApiOptions::default()
            },
            shutdown_rx,
        );
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/v1/video/liveportrait/source")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let portraits: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
        let portrait = portraits
            .iter()
            .find(|p| p["name"] == "liveportrait-default.png")
            .unwrap();
        assert!(portrait.get("path").is_none());
        assert_eq!(portrait["selected"], true);
        let id = portrait["id"].as_str().unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/video/liveportrait/source/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        let invalid = app
            .clone()
            .oneshot(
                Request::post("/api/v1/video/liveportrait/source")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"id":"../../etc/passwd"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::NOT_FOUND);
        assert!(restart_rx.try_recv().is_err());
        assert!(!path.exists());
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/video/identity")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"identity":"liveportrait"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        preview
            .effects()
            .publish_avatar(AvatarFrame::new(9, unix_ms(), 2, 1, vec![9; 8]).unwrap());
        let runtime_before_selection = serde_json::to_value(runtime.state().await).unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/video/liveportrait/source")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"id":id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(preview.effects().latest_avatar().unwrap().frame_id, 9);
        assert_eq!(
            serde_json::to_value(runtime.state().await).unwrap(),
            runtime_before_selection
        );
        assert!(matches!(
            restart_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/v1/video/identity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let identity: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(identity["identity"], "liveportrait");
        assert_eq!(identity["liveportrait"]["revision"], 1);
        assert_eq!(identity["liveportrait"]["active_revision"], 0);
        let mut expected = 0;
        for (revision, value, age, status) in [
            (0, 10, 0, StatusCode::NO_CONTENT),
            (1, 15, 1000, StatusCode::CONFLICT),
            (1, 20, 0, StatusCode::NO_CONTENT),
            (0, 30, 0, StatusCode::CONFLICT),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/avatar/frame")
                        .header("x-tarsier-avatar-engine", "liveportrait")
                        .header("x-tarsier-avatar-source-revision", revision.to_string())
                        .header("x-tarsier-frame-id", (value as u64).to_string())
                        .header(
                            "x-tarsier-captured-at-ms",
                            unix_ms().saturating_sub(age).to_string(),
                        )
                        .header("x-tarsier-avatar-width", "2")
                        .header("x-tarsier-avatar-height", "1")
                        .body(Body::from(vec![value; 8]))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            if status == StatusCode::NO_CONTENT {
                expected = value;
            }
            let mut output = [255; 8];
            assert!(preview.effects().apply_output(&mut output, 2, 1, unix_ms()));
            assert_eq!(output, [expected; 8]);
        }
        assert!(matches!(
            restart_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let (_, restored) = UserSettingsStore::load(path, fallback.clone())
            .await
            .unwrap();
        assert_eq!(restored.video_identity, VideoIdentity::Liveportrait);
        assert_eq!(
            restored.liveportrait_source.unwrap(),
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets/avatars/liveportrait-default.png")
                .canonicalize()
                .unwrap()
        );
        assert!(!directory.join("portraits").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn video_transform_validates_persists_and_updates_output() {
        let config = Config::default();
        let path = std::env::temp_dir().join(format!(
            "tarsier-transform-{}-{}/settings.json",
            std::process::id(),
            unix_ms()
        ));
        let fallback = UserSettings::from_config(&config);
        let (settings, _) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router_with_controls(
            config,
            runtime.clone(),
            preview.clone(),
            None,
            ApiOptions {
                user_settings: Some(settings),
                ..ApiOptions::default()
            },
            shutdown_rx,
        );
        for (rotation, status) in [(90, StatusCode::OK), (45, StatusCode::BAD_REQUEST)] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/video/transform")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"rotation": rotation, "mirror": true}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
        let (_, restored) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        assert_eq!(restored.video_transform.rotation, 90);
        assert!(restored.video_transform.mirror);
        assert_eq!(
            runtime.state().await.video_effects.transform,
            restored.video_transform
        );
        let mut pixels = [1, 2, 3, 4]
            .into_iter()
            .flat_map(|v| [v; 4])
            .collect::<Vec<_>>();
        assert!(preview.effects().processing_enabled());
        preview.effects().apply_output(&mut pixels, 2, 2, unix_ms());
        assert_eq!(
            pixels.chunks_exact(4).map(|p| p[0]).collect::<Vec<_>>(),
            [1, 3, 2, 4]
        );
        let response = app
            .oneshot(
                Request::get("/api/v1/video/transform")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(runtime.recent_events().await[0].kind, "video.transform");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn presentation_and_face_tracking_settings_are_persisted() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.avatar.enabled = true;
        config.perception.enabled = false;
        let path = std::env::temp_dir().join(format!(
            "tarsier-api-settings-{}-{}/user-settings.json",
            std::process::id(),
            unix_ms()
        ));
        let fallback = UserSettings::from_config(&config);
        let (settings, _) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router_with_controls(
            config,
            runtime,
            PreviewHub::new(),
            camera,
            ApiOptions {
                user_settings: Some(settings),
                ..ApiOptions::default()
            },
            shutdown_rx,
        );

        for (path, body) in [
            ("/api/v1/video/identity", json!({"identity": "portrait3d"})),
            (
                "/api/v1/video/background",
                json!({"enabled": true, "effect": "blur"}),
            ),
            ("/api/v1/camera/face-tracking", json!({"enabled": true})),
            ("/api/v1/camera/auto-zoom", json!({"enabled": true})),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }

        let (_, restored) = UserSettingsStore::load(path.clone(), fallback.clone())
            .await
            .unwrap();
        assert_eq!(restored.video_identity, VideoIdentity::Portrait3d);
        assert!(restored.background_enabled);
        assert_eq!(restored.background_effect, BackgroundEffect::Blur);
        assert!(restored.face_tracking_enabled);
        assert!(restored.auto_zoom_enabled);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn personal_identity_requires_an_atomic_alpha_frame() {
        let mut config = Config::default();
        config.avatar.enabled = true;
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let (_tx, rx) = watch::channel(false);
        let app = router(config, runtime.clone(), preview.clone(), None, rx);
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/video/identity")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"identity":"portrait3d"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        for (format, expected) in [
            ("bgrx", StatusCode::UNPROCESSABLE_ENTITY),
            ("bgra", StatusCode::NO_CONTENT),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/avatar/frame")
                        .header("x-tarsier-avatar-engine", "portrait3d")
                        .header("x-tarsier-avatar-pixel-format", format)
                        .header("x-tarsier-frame-id", "90")
                        .header("x-tarsier-captured-at-ms", unix_ms().to_string())
                        .header("x-tarsier-avatar-width", "1")
                        .header("x-tarsier-avatar-height", "1")
                        .body(Body::from(vec![10, 20, 30, 0]))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        preview
            .effects()
            .set_background(true, BackgroundEffect::GreenScreen);
        let mut output = vec![213; 4];
        preview.effects().apply_output(&mut output, 1, 1, unix_ms());
        assert_eq!(output, vec![0, 255, 0, 0]);
        assert_eq!(
            runtime.state().await.video_effects.avatar_engine,
            Some(AvatarEngine::Portrait3d)
        );
    }

    #[tokio::test]
    async fn avatar_frame_is_published_with_output_provenance() {
        let mut config = Config::default();
        config.perception.enabled = false;
        config.avatar.enabled = true;
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime.clone(), preview.clone(), None, shutdown_rx);
        let captured_at_ms = unix_ms();

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/video/identity")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"identity":"portrait3d"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/avatar/frame")
                    .header("content-type", "application/octet-stream")
                    .header("x-tarsier-avatar-engine", "liveportrait")
                    .header("x-tarsier-frame-id", "41")
                    .header("x-tarsier-captured-at-ms", captured_at_ms.to_string())
                    .header("x-tarsier-avatar-width", "2")
                    .header("x-tarsier-avatar-height", "1")
                    .body(Body::from(vec![1, 2, 3, 0, 4, 5, 6, 0]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(preview.effects().latest_avatar().is_none());

        let response = app
            .oneshot(
                Request::post("/api/v1/avatar/frame")
                    .header("content-type", "application/octet-stream")
                    .header("x-tarsier-avatar-engine", "portrait3d")
                    .header("x-tarsier-avatar-pixel-format", "bgra")
                    .header("x-tarsier-frame-id", "42")
                    .header("x-tarsier-captured-at-ms", captured_at_ms.to_string())
                    .header("x-tarsier-avatar-width", "2")
                    .header("x-tarsier-avatar-height", "1")
                    .body(Body::from(vec![1, 2, 3, 0, 4, 5, 6, 0]))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let avatar = preview.effects().latest_avatar().unwrap();
        assert_eq!(avatar.frame_id, 42);
        assert_eq!((avatar.width, avatar.height), (2, 1));
        let effects = runtime.state().await.video_effects;
        assert!(effects.avatar_available);
        assert_eq!(effects.avatar_frame_id, Some(42));
        assert_eq!(
            (effects.avatar_width, effects.avatar_height),
            (Some(2), Some(1))
        );
    }

    #[tokio::test]
    async fn identity_control_selects_liveportrait_and_clears_the_previous_frame() {
        let mut config = Config::default();
        config.perception.enabled = false;
        config.avatar.enabled = true;
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        preview
            .effects()
            .publish_avatar(AvatarFrame::new(1, unix_ms(), 1, 1, vec![1, 2, 3, 0]).unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime.clone(), preview.clone(), None, shutdown_rx);

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/video/identity")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"identity":"liveportrait"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            preview.effects().output_mode(),
            VideoOutputMode::ComicAvatar
        );
        assert!(preview.effects().latest_avatar().is_none());
        let effects = runtime.state().await.video_effects;
        assert_eq!(effects.avatar_engine, Some(AvatarEngine::Liveportrait));
        assert!(!effects.avatar_available);

        let response = app
            .oneshot(
                Request::get("/api/v1/video/identity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let payload: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(payload["identity"], "liveportrait");
        let events = runtime.recent_events().await;
        assert_eq!(events[0].kind, "video.identity");
        assert_eq!(events[0].data["identity"], "liveportrait");
    }

    #[tokio::test]
    async fn depth_identity_retains_raw_values_and_exposes_the_visualization_bounds() {
        let mut config = Config::default();
        config.perception.enabled = false;
        config.depth.enabled = true;
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime.clone(), preview.clone(), None, shutdown_rx);
        let captured_at_ms = unix_ms();

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/video/identity")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"identity":"depth-map"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let values = [0.25_f32, 1.5, 2.75, 4.0];
        let bytes = values
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/depth/frame")
                    .header("content-type", "application/octet-stream")
                    .header("x-tarsier-frame-id", "42")
                    .header("x-tarsier-captured-at-ms", captured_at_ms.to_string())
                    .header("x-tarsier-depth-width", "2")
                    .header("x-tarsier-depth-height", "2")
                    .header("x-tarsier-depth-far", "0.25")
                    .header("x-tarsier-depth-near", "4.0")
                    .header(
                        "x-tarsier-depth-representation",
                        "relative-inverse-depth-f32le",
                    )
                    .body(Body::from(bytes.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = app
            .oneshot(
                Request::get("/api/v1/depth/frame")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["x-tarsier-depth-representation"],
            "relative-inverse-depth-f32le"
        );
        assert_eq!(response.headers()["x-tarsier-depth-width"], "2");
        assert_eq!(response.headers()["x-tarsier-depth-height"], "2");
        assert_eq!(
            to_bytes(response.into_body(), MAX_DEPTH_FRAME_BYTES)
                .await
                .unwrap(),
            Bytes::from(bytes)
        );

        let effects = runtime.state().await.video_effects;
        assert_eq!(effects.output_mode, VideoOutputMode::DepthMap);
        assert!(effects.depth_available);
        assert_eq!(effects.depth_frame_id, Some(42));
        assert_eq!(
            (effects.depth_width, effects.depth_height),
            (Some(2), Some(2))
        );
        assert_eq!(
            (effects.depth_far, effects.depth_near),
            (Some(0.25), Some(4.0))
        );
    }

    #[tokio::test]
    async fn depth_identity_is_rejected_when_processing_is_disabled() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime, preview, None, shutdown_rx);

        let response = app
            .oneshot(
                Request::post("/api/v1/video/identity")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"identity":"depth-map"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn camera_background_accepts_depth_for_mask_refinement() {
        let mut config = Config::default();
        config.perception.enabled = false;
        config.depth.enabled = true;
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime.clone(), preview, None, shutdown_rx);

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/video/background")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true,"effect":"green-screen"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let response = app
            .clone()
            .oneshot(
                Request::get("/api/v1/video/identity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let payload: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(payload["identity"], "camera");
        assert_eq!(payload["background_enabled"], true);

        let captured_at_ms = unix_ms();
        let values = [0.25_f32, 1.5, 2.75, 4.0];
        let bytes = values
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let response = app
            .oneshot(
                Request::post("/api/v1/depth/frame")
                    .header("content-type", "application/octet-stream")
                    .header("x-tarsier-frame-id", "43")
                    .header("x-tarsier-captured-at-ms", captured_at_ms.to_string())
                    .header("x-tarsier-depth-width", "2")
                    .header("x-tarsier-depth-height", "2")
                    .header("x-tarsier-depth-far", "0.25")
                    .header("x-tarsier-depth-near", "4.0")
                    .header(
                        "x-tarsier-depth-representation",
                        "relative-inverse-depth-f32le",
                    )
                    .body(Body::from(bytes))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(runtime.state().await.video_effects.depth_available);
    }

    #[tokio::test]
    async fn perception_mask_immediately_updates_runtime_frame_provenance() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, runtime.clone(), preview.clone(), None, shutdown_rx);

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/perception/mask")
                    .header("content-type", "application/octet-stream")
                    .header("x-tarsier-frame-id", "42")
                    .header("x-tarsier-captured-at-ms", "123456")
                    .header("x-tarsier-mask-width", "2")
                    .header("x-tarsier-mask-height", "2")
                    .body(Body::from(vec![0, 63, 127, 255]))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let mask = preview.effects().latest_mask().unwrap();
        assert_eq!(mask.frame_id, 42);
        assert_eq!(mask.captured_at_ms, 123456);
        assert_eq!((mask.width, mask.height), (2, 2));
        let effects = runtime.state().await.video_effects;
        assert!(effects.mask_available);
        assert_eq!(effects.mask_frame_id, Some(42));
        assert_eq!(
            (effects.mask_width, effects.mask_height),
            (Some(2), Some(2))
        );
        assert_eq!(effects.mask_captured_at_ms, Some(123456));
        assert_eq!(effects.mask_published_at_ms, Some(mask.published_at_ms));
    }

    #[tokio::test]
    async fn perception_mask_rejects_inconsistent_dimensions() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, Runtime::new(), PreviewHub::new(), None, shutdown_rx);

        let response = app
            .oneshot(
                Request::post("/api/v1/perception/mask")
                    .header("x-tarsier-frame-id", "1")
                    .header("x-tarsier-captured-at-ms", "100")
                    .header("x-tarsier-mask-width", "2")
                    .header("x-tarsier-mask-height", "2")
                    .body(Body::from(vec![0, 255]))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn preset_recall_uses_camera_owner_and_emits_an_event() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        config.presets.push(CameraPresetConfig::default());
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            shutdown_rx,
        );

        let response = app
            .oneshot(
                Request::post("/api/v1/camera/presets/center/recall")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let events = runtime.recent_events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "camera.preset.recalled");
        assert_eq!(events[0].data["id"], "center");
        let state = runtime.state().await;
        assert_eq!(
            state.camera.attitude_source,
            CameraAttitudeSource::LastCommanded
        );
        assert_eq!(state.camera.yaw_degrees, Some(0.0));
    }

    #[tokio::test]
    async fn built_in_gesture_control_updates_explicit_runtime_state() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            shutdown_rx,
        );

        let response = app
            .oneshot(
                Request::post("/api/v1/camera/built-in-gestures/dynamic-zoom")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let state = runtime.state().await;
        assert_eq!(state.camera.built_in_gestures.target_selection, None);
        assert_eq!(state.camera.built_in_gestures.zoom, None);
        assert_eq!(state.camera.built_in_gestures.dynamic_zoom, Some(false));
        let events = runtime.recent_events().await;
        assert_eq!(events[0].kind, "camera.built_in_gesture");
        assert_eq!(events[0].data["feature"], "dynamic-zoom");
        assert_eq!(events[0].data["enabled"], false);
    }

    #[tokio::test]
    async fn manual_zoom_updates_magnification_state() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            shutdown_rx,
        );

        let response = app
            .oneshot(
                Request::post("/api/v1/camera/zoom")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"magnification":3.4}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(runtime.state().await.camera.zoom_magnification, Some(3.4));
        let events = runtime.recent_events().await;
        assert_eq!(events[0].kind, "camera.zoom");
        assert!((events[0].data["magnification"].as_f64().unwrap() - 3.4).abs() < 1e-6);
    }

    #[tokio::test]
    async fn image_settings_require_their_manual_modes_and_keep_readback() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            shutdown_rx,
        );

        for (path, value, expected) in [
            ("brightness", 61, StatusCode::ACCEPTED),
            ("gain", 5, StatusCode::UNPROCESSABLE_ENTITY),
            ("auto-exposure", 1, StatusCode::ACCEPTED),
            ("gain", 5, StatusCode::ACCEPTED),
            (
                "face-priority-auto-exposure",
                1,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            ("auto-exposure", 0, StatusCode::ACCEPTED),
            ("face-priority-auto-exposure", 1, StatusCode::ACCEPTED),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(format!("/api/v1/camera/image-settings/{path}"))
                        .header("content-type", "application/json")
                        .body(Body::from(json!({"value": value}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "unexpected result for {path}");
        }

        let settings = runtime.state().await.camera.image_settings;
        assert_eq!(settings.value(CameraImageControl::Brightness), Some(61));
        assert_eq!(settings.value(CameraImageControl::Gain), Some(5));
        assert_eq!(settings.value(CameraImageControl::AutoExposure), Some(0));
        assert_eq!(
            settings.value(CameraImageControl::FacePriorityAutoExposure),
            Some(1)
        );
        let events = runtime.recent_events().await;
        assert_eq!(events.last().unwrap().kind, "camera.image_setting");
        assert_eq!(events.last().unwrap().data["readback"], 1);
    }

    #[tokio::test]
    async fn auto_zoom_holds_face_size_and_manual_zoom_recalibrates_it() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            shutdown_rx,
        );

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/auto-zoom")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        for (path, body) in [
            ("/api/v1/camera/zoom", json!({"magnification": 2.0})),
            ("/api/v1/camera/face-tracking", json!({"enabled": true})),
            ("/api/v1/camera/auto-zoom", json!({"enabled": true})),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }

        let observation = |frame_id: u64, captured_at_ms: u64, face_size: f64| {
            let mut face_landmarks = (0..478)
                .map(|_| json!({"x": 0.5, "y": 0.5, "z": 0.0}))
                .collect::<Vec<_>>();
            face_landmarks[0] = json!({"x": 0.5 - face_size / 2.0, "y": 0.5, "z": 0.0});
            face_landmarks[1] = json!({"x": 0.5 + face_size / 2.0, "y": 0.5, "z": 0.0});
            json!({
                "frame_id": frame_id,
                "captured_at_ms": captured_at_ms,
                "face_detected": true,
                "face_landmarks": face_landmarks,
                "hand_detected": false,
                "gesture": null,
                "confidence": 0.0,
                "latency_ms": 10.0
            })
        };
        let first_capture = unix_ms().saturating_add(100);
        for body in [
            observation(1, first_capture, 0.2),
            observation(2, first_capture + 600, 0.1),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/perception/observations")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        let state = runtime.state().await;
        assert_eq!(state.camera.zoom_magnification, Some(2.16));
        assert!(state.camera.face_tracking.auto_zoom.enabled);
        assert!(state.camera.face_tracking.auto_zoom.calibrated);
        assert!(
            (state
                .camera
                .face_tracking
                .auto_zoom
                .target_face_size
                .unwrap()
                - 0.2)
                .abs()
                < 1e-6
        );

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/zoom")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"magnification":3.0}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(
            !runtime
                .state()
                .await
                .camera
                .face_tracking
                .auto_zoom
                .calibrated
        );

        let response = app
            .oneshot(
                Request::post("/api/v1/perception/observations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        observation(3, unix_ms().saturating_add(1_000), 0.3).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let state = runtime.state().await;
        assert_eq!(state.camera.zoom_magnification, Some(3.0));
        assert!(
            (state
                .camera
                .face_tracking
                .auto_zoom
                .target_face_size
                .unwrap()
                - 0.3)
                .abs()
                < 1e-6
        );
    }

    #[tokio::test]
    async fn hdr_control_updates_state_and_emits_an_event() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            shutdown_rx,
        );

        let response = app
            .oneshot(
                Request::post("/api/v1/camera/hdr")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let state = runtime.state().await;
        assert_eq!(state.camera.hdr, Some(true));
        assert_eq!(state.camera.hdr_sample_at_ms, None);
        let events = runtime.recent_events().await;
        assert_eq!(events[0].kind, "camera.hdr");
        assert_eq!(events[0].data["enabled"], true);
    }

    #[tokio::test]
    async fn tracking_modes_and_manual_pan_tilt_switch_each_other_off() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            shutdown_rx,
        );

        for path in ["/api/v1/camera/tracking", "/api/v1/camera/face-tracking"] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"enabled":true}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }
        let state = runtime.state().await;
        assert_eq!(state.camera.tracking, Some(false));
        assert!(state.camera.face_tracking.enabled);

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/hands-tracking")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let state = runtime.state().await;
        assert!(!state.camera.face_tracking.enabled);
        assert!(state.camera.hands_tracking.enabled);

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/tracking")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let state = runtime.state().await;
        assert_eq!(state.camera.tracking, Some(true));
        assert!(!state.camera.face_tracking.enabled);
        assert!(!state.camera.hands_tracking.enabled);

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/nudge/left")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(runtime.state().await.camera.tracking, Some(false));

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/face-tracking")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let response = app
            .oneshot(
                Request::post("/api/v1/camera/nudge/right")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let state = runtime.state().await;
        assert!(!state.camera.face_tracking.enabled);
        assert_eq!(state.camera.tracking, Some(false));
    }

    #[tokio::test]
    async fn hands_tracking_follows_one_or_two_slow_hands_and_holds_for_rapid_motion() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            shutdown_rx,
        );

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/hands-tracking")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let hand = |x: f32| {
            (0..21)
                .map(|_| json!({"x": x, "y": 0.5, "z": 0.0}))
                .collect::<Vec<_>>()
        };
        let mut two_hands = hand(0.65);
        two_hands.extend(hand(0.85));
        for (frame_id, captured_at_ms, hand_landmarks) in [
            (1, 1_000, two_hands),
            (2, 1_200, hand(0.75)),
            (3, 1_300, hand(0.95)),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/perception/observations")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({
                                "frame_id": frame_id,
                                "captured_at_ms": captured_at_ms,
                                "face_detected": false,
                                "hand_detected": true,
                                "hand_landmarks": hand_landmarks,
                                "pose_detected": false,
                                "gesture": null,
                                "confidence": 0.0,
                                "latency_ms": 10.0
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);

            let tracking = runtime.state().await.camera.hands_tracking;
            match frame_id {
                1 => {
                    assert_eq!(tracking.hands_visible, 2);
                    assert!(!tracking.zoom_frozen);
                    assert!(tracking.calibrated);
                    assert!(tracking.active);
                }
                2 => {
                    assert_eq!(tracking.hands_visible, 1);
                    assert!(tracking.zoom_frozen);
                    assert!(tracking.active);
                    assert!(!tracking.rapid_motion);
                }
                3 => {
                    assert_eq!(tracking.hands_visible, 1);
                    assert!(tracking.zoom_frozen);
                    assert!(!tracking.active);
                    assert!(tracking.rapid_motion);
                }
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn face_tracking_follows_and_centers_a_detected_face() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            shutdown_rx,
        );

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/face-tracking")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        for (frame_id, x, y, expected_active) in [(1, 0.75, 0.25, true), (2, 0.5, 0.5, false)] {
            let face_landmarks = (0..478)
                .map(|_| json!({"x": x, "y": y, "z": 0.0}))
                .collect::<Vec<_>>();
            let observation = json!({
                "frame_id": frame_id,
                "captured_at_ms": 1000 + frame_id * 100,
                "face_detected": true,
                "face_landmarks": face_landmarks,
                "hand_detected": false,
                "gesture": null,
                "confidence": 0.0,
                "latency_ms": 10.0
            });
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/perception/observations")
                        .header("content-type", "application/json")
                        .body(Body::from(observation.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            let tracking = runtime.state().await.camera.face_tracking;
            assert!(tracking.enabled);
            assert!(tracking.target_visible);
            assert_eq!(tracking.target_source, Some(FaceTrackingTarget::Face));
            assert_eq!(tracking.active, expected_active);
            assert_eq!(tracking.target_x, Some(x));
            assert_eq!(tracking.target_y, Some(y));
            if expected_active {
                assert!((tracking.speed_fraction - 0.018).abs() < f32::EPSILON);
            } else {
                assert_eq!(tracking.speed_fraction, 0.0);
            }
        }

        let mut pose_landmarks = (0..33)
            .map(|_| json!({"x": 0.5, "y": 0.5, "z": 0.0, "visibility": 0.9}))
            .collect::<Vec<_>>();
        pose_landmarks[11] = json!({"x": 0.6, "y": 0.4, "z": 0.0, "visibility": 0.9});
        pose_landmarks[12] = json!({"x": 0.8, "y": 0.4, "z": 0.0, "visibility": 0.9});
        let response = app
            .oneshot(
                Request::post("/api/v1/perception/observations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "frame_id": 3,
                            "captured_at_ms": 1300,
                            "face_detected": false,
                            "hand_detected": false,
                            "pose_detected": true,
                            "pose_landmarks": pose_landmarks,
                            "gesture": null,
                            "confidence": 0.0,
                            "latency_ms": 10.0
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let tracking = runtime.state().await.camera.face_tracking;
        assert!(tracking.target_visible);
        assert_eq!(tracking.target_source, Some(FaceTrackingTarget::Shoulders));
        assert!((tracking.target_x.unwrap() - 0.7).abs() < f32::EPSILON);
        assert!((tracking.target_y.unwrap() - 0.3).abs() < f32::EPSILON);
        assert!(tracking.active);
        assert!((tracking.speed_fraction - 0.018).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn pan_tilt_nudge_uses_the_camera_owner_and_clears_stale_attitude() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            shutdown_rx,
        );

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/nudge/left")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let keepalive_response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/camera/nudge/left")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(keepalive_response.status(), StatusCode::ACCEPTED);

        let stop_response = app
            .oneshot(
                Request::post("/api/v1/camera/nudge/stop")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stop_response.status(), StatusCode::ACCEPTED);

        let state = runtime.state().await;
        assert_eq!(state.camera.yaw_degrees, None);
        assert_eq!(
            state.camera.attitude_source,
            CameraAttitudeSource::Unavailable
        );
        let events = runtime.recent_events().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "camera.nudge");
        assert_eq!(events[0].data["direction"], "left");
        assert_eq!(events[1].kind, "camera.nudge.stopped");
        assert_eq!(events[1].data["direction"], "left");
    }

    #[tokio::test]
    async fn perception_state_tracks_face_pose_and_two_hands_then_clears_landmarks() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(
            config,
            runtime.clone(),
            PreviewHub::new(),
            None,
            shutdown_rx,
        );

        let face_landmarks = (0..478)
            .map(|index| json!({"x": index as f32 / 477.0, "y": 0.4, "z": -0.2}))
            .collect::<Vec<_>>();
        let hand_landmarks = (0..42)
            .map(|index| json!({"x": index as f32 / 41.0, "y": 0.5, "z": -0.1}))
            .collect::<Vec<_>>();
        let pose_landmarks = (0..33)
            .map(|index| json!({"x": index as f32 / 32.0, "y": 0.6, "z": -0.3, "visibility": 0.9}))
            .collect::<Vec<_>>();
        for (index, observation) in [
            json!({
                "frame_id": 1,
                "captured_at_ms": 1000,
                "face_detected": true,
                "face_landmarks": face_landmarks,
                "hand_detected": true,
                "hand_landmarks": hand_landmarks,
                "pose_detected": true,
                "pose_landmarks": pose_landmarks,
                "gesture": "open_palm",
                "confidence": 0.72,
                "latency_ms": 10.0
            }),
            json!({
                "frame_id": 2,
                "captured_at_ms": 1100,
                "face_detected": false,
                "hand_detected": false,
                "pose_detected": false,
                "gesture": null,
                "confidence": 0.0,
                "latency_ms": 9.0
            }),
        ]
        .into_iter()
        .enumerate()
        {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/perception/observations")
                        .header("content-type", "application/json")
                        .body(Body::from(observation.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            if index == 0 {
                let perception = runtime.state().await.perception;
                assert_eq!(perception.face_landmarks.len(), 478);
                assert_eq!(perception.hand_landmarks.len(), 42);
                assert_eq!(perception.pose_landmarks.len(), 33);
                assert_eq!(perception.pose_landmarks[0].visibility, Some(0.9));
            }
        }

        let state = runtime.state().await;
        assert!(!state.perception.face_detected);
        assert!(state.perception.face_landmarks.is_empty());
        assert!(!state.perception.hand_detected);
        assert!(state.perception.hand_landmarks.is_empty());
        assert!(!state.perception.pose_detected);
        assert!(state.perception.pose_landmarks.is_empty());
        assert_eq!(state.perception.last_hand_at_ms, Some(1000));
        assert_eq!(state.perception.peak_gesture.as_deref(), Some("open_palm"));
        assert_eq!(state.perception.peak_gesture_confidence, Some(0.72));
        assert_eq!(state.perception.peak_gesture_at_ms, Some(1000));
    }
}
