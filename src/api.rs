use std::{convert::Infallible, sync::Arc};

use axum::{
    Json, Router,
    body::Body,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;

use crate::{
    camera::CameraHandle,
    config::{CameraPresetConfig, Config, ScenarioConfig},
    model::{CameraAttitudeSource, PerceptionObservation, ScenarioActivation, unix_ms},
    pipeline::PreviewHub,
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
}

pub fn router(
    config: Config,
    runtime: Runtime,
    preview: PreviewHub,
    camera: Option<CameraHandle>,
) -> Router {
    let state = ApiState {
        stabilizer: Arc::new(Mutex::new(OpenPalmStabilizer::new(&config.perception))),
        face_presence: Arc::new(Mutex::new(FacePresenceStabilizer::new(&config.perception))),
        config,
        runtime,
        preview,
        camera,
    };
    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(app_js))
        .route("/assets/styles.css", get(styles_css))
        .route("/api/v1/health", get(health))
        .route("/api/v1/state", get(current_state))
        .route("/api/v1/camera/state", get(camera_state))
        .route("/api/v1/camera/move", post(move_camera))
        .route("/api/v1/camera/tracking", post(set_tracking))
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
        .route("/api/v1/preview.mjpeg", get(preview_mjpeg))
        .route("/api/v1/camera/snapshot", get(snapshot))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}

async fn styles_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/styles.css"),
    )
}

async fn health(State(state): State<ApiState>) -> Json<Value> {
    let snapshot = state.runtime.state().await;
    Json(json!({
        "status": if snapshot.pipeline.error.is_some()
            || snapshot.camera.error.is_some()
            || snapshot.perception.error.is_some()
        {
            "degraded"
        } else {
            "ok"
        },
        "version": snapshot.version,
        "uptime_ms": unix_ms().saturating_sub(snapshot.started_at_ms),
    }))
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

async fn move_camera(
    State(state): State<ApiState>,
    Json(request): Json<MoveCameraRequest>,
) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
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

async fn set_tracking(
    State(state): State<ApiState>,
    Json(request): Json<TrackingRequest>,
) -> Response {
    set_tracking_inner(&state, request.enabled).await
}

async fn camera_action(
    State(state): State<ApiState>,
    axum::extract::Path(action): axum::extract::Path<String>,
) -> Response {
    let Some(camera) = state.camera.clone() else {
        return camera_unavailable();
    };
    match action.as_str() {
        "recenter" => match camera.recenter().await {
            Ok(()) => {
                record_commanded_attitude(&state, 0.0, 0.0, 0.0).await;
                record_camera_command(&state, "camera.recenter", Value::Null).await;
                StatusCode::ACCEPTED.into_response()
            }
            Err(error) => command_error(error),
        },
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
    match camera.set_tracking(enabled).await {
        Ok(()) => {
            state
                .runtime
                .update(|runtime| runtime.camera.tracking = Some(enabled))
                .await;
            record_camera_command(state, "camera.tracking", json!({"enabled": enabled})).await;
            StatusCode::ACCEPTED.into_response()
        }
        Err(error) => command_error(error),
    }
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
    let mut receiver = state.preview.subscribe();
    let stream = async_stream::stream! {
        loop {
            if receiver.changed().await.is_err() {
                break;
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
    if observation.confidence.is_nan() || !(0.0..=1.0).contains(&observation.confidence) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "confidence must be between 0 and 1"})),
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
            runtime.perception.hand_detected = observation.hand_detected;
            runtime.perception.gesture = observation.gesture.clone();
            runtime.perception.confidence = Some(observation.confidence);
            runtime.perception.sample_at_ms = Some(observation.captured_at_ms);
            runtime.perception.latency_ms = observation.latency_ms;
        })
        .await;

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
    websocket.on_upgrade(move |socket| stream_events(socket, state.runtime))
}

async fn stream_events(socket: WebSocket, runtime: Runtime) {
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
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::*;
    use crate::{camera, config::CameraAdapter};

    #[tokio::test]
    async fn preset_recall_uses_camera_owner_and_emits_an_event() {
        let mut config = Config::default();
        config.camera.adapter = CameraAdapter::Mock;
        config.perception.enabled = false;
        let runtime = Runtime::new();
        let camera = camera::start(config.camera.clone(), runtime.clone())
            .await
            .unwrap();
        let app = router(config, runtime.clone(), PreviewHub::new(), camera);

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
}
