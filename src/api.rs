use std::{convert::Infallible, sync::Arc, time::Instant};

use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, State, WebSocketUpgrade,
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
use tokio::sync::{Mutex, watch};
use tower_http::trace::TraceLayer;

use crate::{
    camera::{CameraHandle, PAN_TILT_LEASE},
    config::{CameraPresetConfig, Config, ScenarioConfig},
    effects::{AvatarFrame, MAX_AVATAR_FRAME_BYTES, VideoMask},
    face_tracking::FaceTrackingController,
    model::{
        AvatarEngine, BackgroundEffect, BuiltInGesture, CameraAttitudeSource, FaceTrackingState,
        FaceTrackingTarget, Landmark, PerceptionObservation, ScenarioActivation, VideoIdentity,
        VideoOutputMode, unix_ms,
    },
    pipeline::{PreviewHub, VideoPipelineControl},
    runtime::Runtime,
    scenario::{FacePresenceStabilizer, OpenPalmStabilizer, PresenceChange},
};

#[derive(Clone)]
struct ApiState {
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
    avatar_control: Arc<Mutex<()>>,
    shutdown: watch::Receiver<bool>,
}

#[cfg(test)]
pub fn router(
    config: Config,
    runtime: Runtime,
    preview: PreviewHub,
    camera: Option<CameraHandle>,
    shutdown: watch::Receiver<bool>,
) -> Router {
    router_with_pipeline(config, runtime, preview, camera, None, shutdown)
}

pub fn router_with_pipeline(
    config: Config,
    runtime: Runtime,
    preview: PreviewHub,
    camera: Option<CameraHandle>,
    pipeline: Option<VideoPipelineControl>,
    shutdown: watch::Receiver<bool>,
) -> Router {
    let state = ApiState {
        stabilizer: Arc::new(Mutex::new(OpenPalmStabilizer::new(&config.perception))),
        face_presence: Arc::new(Mutex::new(FacePresenceStabilizer::new(&config.perception))),
        config,
        runtime,
        preview,
        camera,
        pipeline,
        camera_power_control: Arc::new(Mutex::new(())),
        pan_tilt_motion: Arc::new(Mutex::new(PanTiltMotion::default())),
        face_tracking: Arc::new(Mutex::new(FaceTrackingController::default())),
        avatar_control: Arc::new(Mutex::new(())),
        shutdown,
    };
    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(app_js))
        .route("/assets/styles.css", get(styles_css))
        .route("/api/v1/health", get(health))
        .route("/api/v1/state", get(current_state))
        .route("/api/v1/camera/state", get(camera_state))
        .route("/api/v1/camera/power", post(set_camera_power))
        .route("/api/v1/camera/move", post(move_camera))
        .route("/api/v1/camera/nudge/{direction}", post(nudge_camera))
        .route("/api/v1/camera/zoom", post(set_zoom))
        .route("/api/v1/camera/hdr", post(set_hdr))
        .route("/api/v1/camera/tracking", post(set_tracking))
        .route("/api/v1/camera/face-tracking", post(set_face_tracking))
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
            "/api/v1/perception/input.mjpeg",
            get(perception_input_mjpeg),
        )
        .route("/api/v1/video/background", post(set_background))
        .route(
            "/api/v1/video/identity",
            get(current_identity).post(set_identity),
        )
        .route("/api/v1/video/output-mode", post(set_output_mode))
        .route("/api/v1/video/green-screen", post(set_green_screen))
        .route("/api/v1/preview.mjpeg", get(preview_mjpeg))
        .route("/api/v1/camera/snapshot", get(snapshot))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn index() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("../web/index.html")),
    )
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
        })),
    )
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
            if let Err(error) = camera.set_face_tracking_speed(0, 0, 0.0).await {
                record_camera_power_error(&state, &error).await;
                return command_error(error);
            }
            state.face_tracking.lock().await.set_enabled(false);
            clear_pan_tilt_motion(&state).await;
            state
                .runtime
                .update(|runtime| {
                    runtime.camera.face_tracking = FaceTrackingState::default();
                })
                .await;
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
    identity: VideoIdentity,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum PanTiltDirection {
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
            Self::Left => (-1, 0),
            Self::Right => (1, 0),
            Self::Up => (0, 1),
            Self::Down => (0, -1),
            Self::Stop => (0, 0),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
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
            state
                .runtime
                .update(|runtime| {
                    runtime.camera.zoom_magnification = Some(request.magnification);
                    runtime.camera.zoom_sample_at_ms = None;
                    runtime.camera.zoom_error = None;
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
        let mut face_tracking = state.face_tracking.lock().await;
        if face_tracking.enabled() {
            let stop_error = camera.set_face_tracking_speed(0, 0, 0.0).await.err();
            face_tracking.set_enabled(false);
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
            record_camera_command(
                state,
                "camera.face_tracking",
                json!({"enabled": false, "reason": "camera-tracking-enabled"}),
            )
            .await;
            if let Some(error) = stop_error {
                return command_error(error);
            }
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
    let mut face_tracking = state.face_tracking.lock().await;
    if face_tracking.enabled() == enabled {
        return StatusCode::ACCEPTED.into_response();
    }

    if enabled {
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
        face_tracking.set_enabled(true);
        state
            .runtime
            .update(|runtime| {
                runtime.camera.face_tracking = FaceTrackingState {
                    enabled: true,
                    ..FaceTrackingState::default()
                };
            })
            .await;
    } else {
        let stop_error = camera.set_face_tracking_speed(0, 0, 0.0).await.err();
        face_tracking.set_enabled(false);
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
        record_camera_command(state, "camera.face_tracking", json!({"enabled": false})).await;
        return match stop_error {
            Some(error) => command_error(error),
            None => StatusCode::ACCEPTED.into_response(),
        };
    }
    record_camera_command(state, "camera.face_tracking", json!({"enabled": enabled})).await;
    StatusCode::ACCEPTED.into_response()
}

async fn clear_pan_tilt_motion(state: &ApiState) {
    let mut motion = state.pan_tilt_motion.lock().await;
    motion.direction = None;
    motion.expires_at = None;
}

async fn disable_tracking_for_manual_control(
    state: &ApiState,
    camera: &CameraHandle,
) -> anyhow::Result<()> {
    let face_tracking_disabled = {
        let mut face_tracking = state.face_tracking.lock().await;
        if face_tracking.enabled() {
            camera.set_face_tracking_speed(0, 0, 0.0).await?;
            face_tracking.set_enabled(false);
            true
        } else {
            false
        }
    };
    if face_tracking_disabled {
        clear_pan_tilt_motion(state).await;
        state
            .runtime
            .update(|runtime| {
                runtime.camera.face_tracking = FaceTrackingState::default();
            })
            .await;
        record_camera_command(
            state,
            "camera.face_tracking",
            json!({"enabled": false, "reason": "manual-gimbal-control"}),
        )
        .await;
    }

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

async fn snapshot(State(state): State<ApiState>) -> Response {
    let Some(frame) = state.preview.latest() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no video frame is available",
        )
            .into_response();
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
    mjpeg_response(state.preview.subscribe_perception(), state.shutdown.clone())
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

async fn current_identity(State(state): State<ApiState>) -> Json<IdentityResponse> {
    let effects = state.runtime.state().await.video_effects;
    Json(IdentityResponse {
        identity: video_identity(effects.output_mode, effects.avatar_engine),
    })
}

async fn set_identity(
    State(state): State<ApiState>,
    Json(request): Json<IdentityRequest>,
) -> Response {
    if request.identity != VideoIdentity::Camera && !state.config.avatar.enabled {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "avatar output is disabled in the daemon configuration"})),
        )
            .into_response();
    }
    let _guard = state.avatar_control.lock().await;
    let (mode, engine) = match request.identity {
        VideoIdentity::Camera => (VideoOutputMode::Camera, None),
        VideoIdentity::Stylized3d => (VideoOutputMode::ComicAvatar, Some(AvatarEngine::Stylized3d)),
        VideoIdentity::Liveportrait => (
            VideoOutputMode::ComicAvatar,
            Some(AvatarEngine::Liveportrait),
        ),
    };
    state.preview.effects().clear_avatar();
    state.preview.effects().set_output_mode(mode);
    state
        .runtime
        .update(|runtime| {
            runtime.video_effects.output_mode = mode;
            if let Some(engine) = engine {
                runtime.video_effects.avatar_engine = Some(engine);
            }
            clear_avatar_state(runtime);
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
    let _guard = state.avatar_control.lock().await;
    state.preview.effects().clear_avatar();
    state.preview.effects().set_output_mode(request.mode);
    state
        .runtime
        .update(|runtime| {
            runtime.video_effects.output_mode = request.mode;
            clear_avatar_state(runtime);
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
    let avatar = match AvatarFrame::new(frame_id, captured_at_ms, width, height, body.to_vec()) {
        Ok(avatar) => avatar,
        Err(error) => return unprocessable_entity(error.to_string()),
    };
    let _guard = state.avatar_control.lock().await;
    let effects = state.runtime.state().await.video_effects;
    if effects.output_mode != VideoOutputMode::ComicAvatar || effects.avatar_engine != Some(engine)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "avatar frame does not match the selected video identity"})),
        )
            .into_response();
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
        (VideoOutputMode::ComicAvatar, Some(AvatarEngine::Liveportrait)) => {
            VideoIdentity::Liveportrait
        }
        (VideoOutputMode::ComicAvatar, _) => VideoIdentity::Stylized3d,
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
    state.preview.effects().publish_mask(mask);
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

fn required_avatar_engine_header(headers: &HeaderMap) -> Result<AvatarEngine, String> {
    let name = "x-tarsier-avatar-engine";
    let value = headers
        .get(name)
        .ok_or_else(|| format!("missing {name} header"))?
        .to_str()
        .map_err(|_| format!("invalid {name} header"))?;
    match value {
        "stylized-3d" => Ok(AvatarEngine::Stylized3d),
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

    let mask_state = state.preview.effects().latest_mask().map(|mask| {
        (
            mask.frame_id,
            mask.width,
            mask.height,
            mask.captured_at_ms,
            mask.published_at_ms,
        )
    });
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
            if let Some(mask) = mask_state {
                runtime.video_effects.mask_available = true;
                runtime.video_effects.mask_frame_id = Some(mask.0);
                runtime.video_effects.mask_width = Some(mask.1);
                runtime.video_effects.mask_height = Some(mask.2);
                runtime.video_effects.mask_captured_at_ms = Some(mask.3);
                runtime.video_effects.mask_published_at_ms = Some(mask.4);
            }
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
    drive_face_tracking(&state, tracking_landmarks, tracking_pose_landmarks).await;

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
) {
    let Some(camera) = state.camera.clone() else {
        return;
    };
    let mut controller = state.face_tracking.lock().await;
    if !controller.enabled() {
        return;
    }

    if state.runtime.state().await.camera.tracking == Some(true) {
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
    let motion = controller.motion();
    state
        .runtime
        .update(|runtime| {
            runtime.camera.face_tracking = FaceTrackingState {
                enabled: true,
                active: motion.active(),
                target_visible: target.is_some(),
                target_source: target.as_ref().map(|(_, source)| *source),
                target_x: target.as_ref().map(|(target, _)| target.x),
                target_y: target.as_ref().map(|(target, _)| target.y),
                speed_fraction: motion.speed_fraction as f32,
                error: command_error,
            };
        })
        .await;
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

async fn events_socket(
    websocket: WebSocketUpgrade,
    State(state): State<ApiState>,
) -> impl IntoResponse {
    websocket.on_upgrade(move |socket| stream_events(socket, state.runtime, state.shutdown))
}

async fn stream_events(socket: WebSocket, runtime: Runtime, mut shutdown: watch::Receiver<bool>) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = runtime.subscribe_events();
    let mut states = runtime.subscribe_state();

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
    use crate::{camera, config::CameraAdapter};

    #[test]
    fn pan_tilt_directions_match_tiny_2_speed_signs() {
        assert_eq!(PanTiltDirection::Left.vector(), (-1, 0));
        assert_eq!(PanTiltDirection::Right.vector(), (1, 0));
        assert_eq!(PanTiltDirection::Up.vector(), (0, 1));
        assert_eq!(PanTiltDirection::Down.vector(), (0, -1));
        assert_eq!(PanTiltDirection::Stop.vector(), (0, 0));
    }

    #[tokio::test]
    async fn embedded_ui_assets_are_not_cached() {
        let mut config = Config::default();
        config.perception.enabled = false;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router(config, Runtime::new(), PreviewHub::new(), None, shutdown_rx);

        for path in ["/", "/assets/app.js", "/assets/styles.css"] {
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
        assert!(include_str!("../web/index.html").contains("id=\"camera-power-toggle\""));
        assert!(include_str!("../web/app.js").contains("/api/v1/camera/power"));
        assert!(include_str!("../web/index.html").contains("id=\"background-toggle\""));
        assert!(include_str!("../web/app.js").contains("/api/v1/video/background"));
        assert!(include_str!("../web/index.html").contains("data-output-mode"));
        assert!(include_str!("../web/app.js").contains("/api/v1/video/identity"));
    }

    #[tokio::test]
    async fn camera_power_control_stops_and_restores_the_pipeline() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        runtime
            .update(|state| {
                state.pipeline.enabled = true;
                state.pipeline.running = true;
            })
            .await;
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let pipeline = VideoPipelineControl::mock(runtime.clone());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = router_with_pipeline(
            config,
            runtime.clone(),
            PreviewHub::new(),
            camera,
            Some(pipeline),
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
        assert_eq!(power_events.len(), 2);
        assert_eq!(power_events[0].data["enabled"], false);
        assert_eq!(power_events[1].data["enabled"], true);
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
                    .body(Body::from(r#"{"enabled":true,"effect":"blur"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(preview.effects().background_enabled());
        assert_eq!(
            preview.effects().background_effect(),
            BackgroundEffect::Blur
        );
        assert!(!preview.effects().green_screen_enabled());
        let state = runtime.state().await;
        assert!(state.video_effects.background_enabled);
        assert_eq!(
            state.video_effects.background_effect,
            BackgroundEffect::Blur
        );
        assert!(!state.video_effects.green_screen_enabled);
        let events = runtime.recent_events().await;
        assert_eq!(events[0].kind, "video.effect.background");
        assert_eq!(events[0].data["enabled"], true);
        assert_eq!(events[0].data["effect"], "blur");
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
                    .body(Body::from(r#"{"identity":"stylized-3d"}"#))
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
                    .header("x-tarsier-avatar-engine", "stylized-3d")
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
    async fn perception_mask_is_published_with_frame_provenance() {
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

        let observation = json!({
            "frame_id": 42,
            "captured_at_ms": 123456,
            "face_detected": false,
            "hand_detected": false,
            "pose_detected": false,
            "gesture": null,
            "confidence": 0.0,
            "latency_ms": 10.0
        });
        let response = app
            .oneshot(
                Request::post("/api/v1/perception/observations")
                    .header("content-type", "application/json")
                    .body(Body::from(observation.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let effects = runtime.state().await.video_effects;
        assert!(effects.mask_available);
        assert_eq!(effects.mask_frame_id, Some(42));
        assert_eq!(
            (effects.mask_width, effects.mask_height),
            (Some(2), Some(2))
        );
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
