use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::Parser;
use reqwest::{Client, Method, Response};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Parser)]
#[command(
    name = "tarsier-mcp",
    version,
    about = "MCP gateway for the local Tarsier daemon"
)]
struct Args {
    /// Base URL of the loopback-only Tarsier HTTP API.
    #[arg(long, default_value = "http://127.0.0.1:8742")]
    daemon_url: String,
}

#[derive(Clone)]
struct TarsierGateway {
    client: Client,
    daemon_url: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MoveCameraParams {
    #[schemars(description = "Absolute yaw target in degrees within the daemon safety limits")]
    yaw: f32,
    #[schemars(description = "Absolute pitch target in degrees within the daemon safety limits")]
    pitch: f32,
    #[serde(default)]
    #[schemars(description = "Absolute roll target in degrees; defaults to zero")]
    roll: f32,
    #[serde(default)]
    #[schemars(
        description = "Optional absolute zoom from 1x to 4x; omit to leave zoom unchanged",
        range(min = 1.0, max = 4.0)
    )]
    zoom: Option<f32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ZoomParams {
    #[schemars(
        description = "Absolute zoom magnification from 1.0 (widest) to 4.0",
        range(min = 1.0, max = 4.0)
    )]
    magnification: f32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TrackingParams {
    #[schemars(description = "Whether this tracking or auto-zoom mode should be enabled")]
    enabled: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ScenarioParams {
    #[schemars(description = "Configured scenario identifier")]
    id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CameraPresetParams {
    #[schemars(description = "Configured camera preset identifier")]
    id: String,
}

impl TarsierGateway {
    fn new(daemon_url: String) -> anyhow::Result<Self> {
        let daemon_url = daemon_url.trim_end_matches('/').to_owned();
        let parsed = reqwest::Url::parse(&daemon_url)?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            anyhow::bail!("daemon URL must use HTTP or HTTPS");
        }
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(token) =
            std::env::var("TARSIER_MCP_TOKEN").or_else(|_| std::env::var("TARSIER_API_TOKEN"))
        {
            let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self { client, daemon_url })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/mcp{path}", self.daemon_url)
    }

    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> CallToolResult {
        let mut request = self.client.request(method, self.url(path));
        if let Some(body) = body {
            request = request.json(&body);
        }
        match request.send().await {
            Ok(response) => response_as_result(response).await,
            Err(error) => tool_error(format!("Tarsier daemon request failed: {error}")),
        }
    }
}

#[tool_router]
impl TarsierGateway {
    #[tool(
        description = "Read current camera, video pipeline, perception, and scenario state",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn get_state(&self) -> CallToolResult {
        self.request(Method::GET, "/api/v1/state", None).await
    }

    #[tool(
        description = "Read the latest known pan_degrees, tilt_degrees, and zoom_magnification, with sources, sample timestamps and telemetry errors. Unknown values are null. Last-commanded values are targets, not confirmation that the camera has reached them.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn get_camera_position(&self) -> CallToolResult {
        self.request(Method::GET, "/api/v1/camera/position", None)
            .await
    }

    #[tool(
        description = "Read effective Tarsier configuration, including camera safety limits",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn get_config(&self) -> CallToolResult {
        self.request(Method::GET, "/api/v1/config", None).await
    }

    #[tool(
        description = "List the configured Tarsier scenarios",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn list_scenarios(&self) -> CallToolResult {
        self.request(Method::GET, "/api/v1/scenarios", None).await
    }

    #[tool(
        description = "Read recent semantic and control events from Tarsier",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn recent_events(&self) -> CallToolResult {
        self.request(Method::GET, "/api/v1/events/recent", None)
            .await
    }

    #[tool(
        description = "List configured camera presets and their bounded orientations",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn list_camera_presets(&self) -> CallToolResult {
        self.request(Method::GET, "/api/v1/camera/presets", None)
            .await
    }

    #[tool(
        description = "Move to absolute yaw/pitch/roll angles, optionally setting absolute zoom (1x to 4x). Omitted zoom stays unchanged. Orientation and zoom are applied sequentially, not atomically.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn move_camera(
        &self,
        Parameters(params): Parameters<MoveCameraParams>,
    ) -> CallToolResult {
        self.request(
            Method::POST,
            "/api/v1/camera/move",
            Some(json!({"yaw": params.yaw, "pitch": params.pitch, "roll": params.roll, "zoom": params.zoom})),
        )
        .await
    }

    #[tool(
        description = "Enable or disable built-in camera tracking. Prefer this mode when smooth camera movement matters most. Enabling it disables Tarsier face tracking and auto zoom.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_camera_tracking(
        &self,
        Parameters(params): Parameters<TrackingParams>,
    ) -> CallToolResult {
        self.request(
            Method::POST,
            "/api/v1/camera/tracking",
            Some(json!({"enabled": params.enabled})),
        )
        .await
    }

    #[tool(
        description = "Enable or disable Tarsier face/shoulder tracking. Prefer this mode by default for framing. Enabling it disables built-in camera tracking; auto zoom must be requested separately.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_face_tracking(
        &self,
        Parameters(params): Parameters<TrackingParams>,
    ) -> CallToolResult {
        self.request(
            Method::POST,
            "/api/v1/camera/face-tracking",
            Some(json!({"enabled": params.enabled})),
        )
        .await
    }

    #[tool(
        description = "Enable or disable auto zoom for Tarsier face tracking. Enable face tracking first. Only enable zoom when the user requests it; calibration preserves the current face size.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_auto_zoom(
        &self,
        Parameters(params): Parameters<TrackingParams>,
    ) -> CallToolResult {
        self.request(
            Method::POST,
            "/api/v1/camera/auto-zoom",
            Some(json!({"enabled": params.enabled})),
        )
        .await
    }

    #[tool(
        description = "Set absolute camera zoom from 1x to 4x. Does not enable tracking or auto zoom. If auto zoom is already enabled, it recalibrates to the new framing.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_zoom(&self, Parameters(params): Parameters<ZoomParams>) -> CallToolResult {
        self.request(
            Method::POST,
            "/api/v1/camera/zoom",
            Some(json!({"magnification": params.magnification})),
        )
        .await
    }

    #[tool(
        description = "Recenter the camera gimbal",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn recenter_camera(&self) -> CallToolResult {
        self.request(Method::POST, "/api/v1/camera/actions/recenter", None)
            .await
    }

    #[tool(
        description = "Move the camera to one configured bounded preset",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn recall_camera_preset(
        &self,
        Parameters(params): Parameters<CameraPresetParams>,
    ) -> CallToolResult {
        if !valid_identifier(&params.id) {
            return tool_error(
                "Camera preset identifiers may contain only ASCII letters, digits, dots, underscores, and hyphens",
            );
        }
        self.request(
            Method::POST,
            &format!("/api/v1/camera/presets/{}/recall", params.id),
            None,
        )
        .await
    }

    #[tool(
        description = "Capture the latest camera frame as a JPEG image",
        annotations(
            read_only_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn take_snapshot(&self) -> CallToolResult {
        let response = match self
            .client
            .get(self.url("/api/v1/camera/snapshot"))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return tool_error(format!("Tarsier daemon request failed: {error}")),
        };
        if !response.status().is_success() {
            return response_as_result(response).await;
        }
        match response.bytes().await {
            Ok(bytes) => CallToolResult::success(vec![ContentBlock::image(
                BASE64.encode(bytes),
                "image/jpeg",
            )]),
            Err(error) => tool_error(format!("Failed to read the Tarsier snapshot: {error}")),
        }
    }

    #[tool(
        description = "Manually trigger one configured Tarsier scenario",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn trigger_scenario(
        &self,
        Parameters(params): Parameters<ScenarioParams>,
    ) -> CallToolResult {
        if !valid_identifier(&params.id) {
            return tool_error(
                "Scenario identifiers may contain only ASCII letters, digits, dots, underscores, and hyphens",
            );
        }
        self.request(
            Method::POST,
            &format!("/api/v1/scenarios/{}/trigger", params.id),
            None,
        )
        .await
    }
}

#[tool_handler(
    name = "tarsier",
    version = "0.1.0",
    instructions = "Inspect and safely control the local Tarsier camera daemon. Camera movement is bounded by daemon configuration, and every mutation is recorded as an event. Prefer set_face_tracking for framing, or set_camera_tracking when smoother movement is requested. Do not switch modes automatically. Enable set_auto_zoom only when requested, after face tracking. Mutations are blocked while another application uses the virtual camera; read-only tools remain available."
)]
impl ServerHandler for TarsierGateway {}

async fn response_as_result(response: Response) -> CallToolResult {
    let status = response.status();
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => return tool_error(format!("Failed to read the Tarsier response: {error}")),
    };
    if !status.is_success() {
        let detail = String::from_utf8_lossy(&body);
        return tool_error(format!("Tarsier returned HTTP {status}: {detail}"));
    }
    if body.is_empty() {
        return CallToolResult::structured(json!({"accepted": true}));
    }
    match serde_json::from_slice(&body) {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => tool_error(format!("Tarsier returned invalid JSON: {error}")),
    }
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

fn valid_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= 64
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let service = TarsierGateway::new(args.daemon_url)?.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::{Json, Router, routing::get};
    use rmcp::model::CallToolRequestParams;
    use serde_json::json;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;

    async fn mock_daemon() -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/mcp/api/v1/state",
            get(|| async { Json(json!({"camera": {"available": true}})) }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    #[test]
    fn scenario_identifiers_cannot_escape_the_api_path() {
        assert!(valid_identifier("open-palm-demo"));
        assert!(valid_identifier("room.camera_1"));
        assert!(!valid_identifier("../config"));
        assert!(!valid_identifier("contains/slash"));
        assert!(!valid_identifier(""));
    }

    #[tokio::test]
    async fn mcp_negotiates_lists_tools_and_proxies_state() {
        let (daemon_url, daemon_task) = mock_daemon().await;
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let gateway = TarsierGateway::new(daemon_url).unwrap();
        let server_task = tokio::spawn(async move {
            gateway.serve(server_transport).await?.waiting().await?;
            anyhow::Ok(())
        });

        let client = ().serve(client_transport).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 15);
        for name in [
            "set_camera_tracking",
            "set_face_tracking",
            "set_auto_zoom",
            "set_zoom",
            "get_camera_position",
        ] {
            assert!(tools.iter().any(|tool| tool.name == name));
        }
        assert!(!tools.iter().any(|tool| tool.name == "set_tracking"));
        assert!(tools.iter().any(|tool| tool.name == "get_state"));

        let result = client
            .call_tool(CallToolRequestParams::new("get_state"))
            .await
            .unwrap();
        assert_eq!(
            result.structured_content,
            Some(json!({"camera": {"available": true}}))
        );

        client.cancel().await.unwrap();
        server_task.await.unwrap().unwrap();
        daemon_task.abort();
    }

    #[tokio::test]
    async fn all_mutating_tools_identify_mcp_and_propagate_usage_rejection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let app = Router::new().fallback(move |request: axum::extract::Request| {
            let seen = seen.clone();
            async move {
                assert_eq!(request.method(), axum::http::Method::POST);
                assert!(request.uri().path().starts_with("/mcp/api/v1/"));
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (
                    axum::http::StatusCode::CONFLICT,
                    Json(json!({"error": "Virtual camera in use"})),
                )
            }
        });
        let daemon = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let gateway = TarsierGateway::new(format!("http://{address}")).unwrap();
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            gateway
                .serve(server_transport)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let client = ().serve(client_transport).await.unwrap();
        for (name, arguments) in [
            ("move_camera", json!({"yaw": 0, "pitch": 0})),
            ("set_zoom", json!({"magnification": 2.0})),
            ("recenter_camera", json!({})),
            ("recall_camera_preset", json!({"id": "test"})),
            ("set_camera_tracking", json!({"enabled": true})),
            ("set_camera_tracking", json!({"enabled": false})),
            ("set_face_tracking", json!({"enabled": true})),
            ("set_face_tracking", json!({"enabled": false})),
            ("set_auto_zoom", json!({"enabled": true})),
            ("set_auto_zoom", json!({"enabled": false})),
            ("trigger_scenario", json!({"id": "test"})),
        ] {
            let result = client
                .call_tool(
                    CallToolRequestParams::new(name)
                        .with_arguments(arguments.as_object().unwrap().clone()),
                )
                .await
                .unwrap();
            assert_eq!(result.is_error, Some(true), "{name}");
            assert!(
                serde_json::to_string(&result)
                    .unwrap()
                    .contains("409 Conflict")
            );
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 11);
        client.cancel().await.unwrap();
        server.await.unwrap();
        daemon.abort();
    }
}
