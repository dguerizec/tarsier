//! Optional authentication shared by HTTP clients, browser sessions, and the local CLI.
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use fs2::FileExt;
use futures_util::StreamExt;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

const COOKIE: &str = "tarsier_session";
const SESSION_SECONDS: u64 = 12 * 60 * 60;

#[derive(Default, Serialize, Deserialize)]
struct Credentials {
    password: Option<String>,
    #[serde(default)]
    tokens: Vec<Token>,
    #[serde(default)]
    sessions: HashMap<String, Session>,
}
#[derive(Serialize, Deserialize)]
struct Token {
    id: String,
    name: String,
    hash: String,
    #[serde(default = "default_destinations")]
    destinations: Vec<TokenDestination>,
    created_at_ms: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TokenDestination {
    Api,
    Mcp,
}
fn default_destinations() -> Vec<TokenDestination> {
    vec![TokenDestination::Api]
}

#[derive(Serialize, Deserialize)]
struct Session {
    password_hash: String,
    expires_at_ms: u64,
}
#[derive(Clone)]
pub struct Auth {
    path: PathBuf,
    login_gate: Arc<Mutex<Instant>>,
    worker_hash: String,
}

pub fn default_path() -> Result<PathBuf> {
    Ok(std::env::var_os("TARSIER_AUTH_PATH")
        .map(PathBuf::from)
        .unwrap_or(crate::settings::default_path()?.with_file_name("auth.json")))
}
fn secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn password_hash(password: &str) -> Result<String> {
    if password.len() < 12 || password.len() > 1024 {
        bail!("Use a password between 12 and 1024 bytes");
    }
    Argon2::default()
        .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
        .map(|hash| hash.to_string())
        .map_err(|_| anyhow::anyhow!("Password hashing failed"))
}
fn verify(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

impl Auth {
    pub fn new(path: PathBuf, worker_token: &str) -> Result<Self> {
        let auth = Self {
            path,
            login_gate: Arc::new(Mutex::new(Instant::now())),
            worker_hash: digest(worker_token),
        };
        auth.transaction(|_| Ok(()))?;
        Ok(auth)
    }
    /// The lock and atomic rename also serialize edits from a separate CLI process.
    fn transaction<T>(&self, f: impl FnOnce(&mut Credentials) -> Result<T>) -> Result<T> {
        let parent = self
            .path
            .parent()
            .context("Auth path requires a parent directory")?;
        fs::create_dir_all(parent)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.path.with_extension("lock"))?;
        lock.lock_exclusive()?;
        let mut data: Credentials = match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("Invalid authentication file")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Credentials::default(),
            Err(error) => return Err(error.into()),
        };
        if let Some(hash) = &data.password {
            PasswordHash::new(hash).map_err(|_| anyhow::anyhow!("Invalid password hash"))?;
        }
        let before = serde_json::to_vec(&data)?;
        let result = f(&mut data)?;
        let after = serde_json::to_vec(&data)?;
        if before != after {
            let temp = self.path.with_extension(format!("{}.tmp", secret()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&after)?;
            file.sync_all()?;
            fs::rename(&temp, &self.path)?;
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(result)
    }
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Credentials) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let auth = self.clone();
        tokio::task::spawn_blocking(move || auth.transaction(f)).await?
    }
    pub fn reset_password(&self, password: &str) -> Result<()> {
        let hash = password_hash(password)?;
        self.transaction(|data| {
            data.password = Some(hash);
            data.sessions.clear();
            Ok(())
        })
    }
    pub fn disable(&self) -> Result<()> {
        self.transaction(|data| {
            data.password = None;
            data.sessions.clear();
            data.tokens.clear();
            Ok(())
        })
    }
    async fn master(&self, headers: &HeaderMap, hash: &str) -> bool {
        let Some(cookie) = session_cookie(headers) else {
            return false;
        };
        let token_hash = digest(cookie);
        let password_hash = hash.to_owned();
        self.run(move |data| {
            Ok(data.password.as_ref() == Some(&password_hash)
                && data.sessions.get(&token_hash).is_some_and(|session| {
                    session.expires_at_ms > crate::model::unix_ms()
                        && session.password_hash == password_hash
                }))
        })
        .await
        .unwrap_or(false)
    }
    pub async fn allowed(&self, headers: &HeaderMap, path: &str, method: &str) -> bool {
        let Ok((password, tokens)) = self
            .run(|data| {
                Ok((
                    data.password.clone(),
                    data.tokens
                        .iter()
                        .map(|t| (t.hash.clone(), t.destinations.clone()))
                        .collect::<Vec<_>>(),
                ))
            })
            .await
        else {
            return false;
        };
        let Some(password) = password else {
            return true;
        };
        if let Some(token) = bearer(headers) {
            let hash = digest(token);
            if hash == self.worker_hash {
                return (matches!(path, "/api/v1/video/identity" | "/api/v1/perception/demand") && method == "GET")
                    || (path == "/api/v1/video/liveportrait/status" && method == "POST")
                    || matches!(
                        path,
                        "/api/v1/perception/input.mjpeg"
                            | "/api/v1/perception/observations"
                            | "/api/v1/perception/mask"
                            | "/api/v1/perception/telemetry"
                            | "/api/v1/avatar/frame"
                            | "/api/v1/depth/frame"
                    );
            }
            let destination = if path.starts_with("/mcp/") {
                TokenDestination::Mcp
            } else {
                TokenDestination::Api
            };
            return tokens.iter().any(|(stored, destinations)| {
                stored == &hash && destinations.contains(&destination)
            });
        }
        !path.starts_with("/mcp/") && self.master(headers, &password).await
    }
    async fn session(&self, password_hash: String) -> Result<String> {
        self.run(move |data| {
            if data.password.as_ref() != Some(&password_hash) {
                bail!("Password changed during sign in");
            }
            let now = crate::model::unix_ms();
            data.sessions
                .retain(|_, session| session.expires_at_ms > now);
            if data.sessions.len() >= 128 {
                data.sessions.clear();
            }
            let token = secret();
            data.sessions.insert(
                digest(&token),
                Session {
                    password_hash,
                    expires_at_ms: now + SESSION_SECONDS * 1000,
                },
            );
            Ok(token)
        })
        .await
    }
    async fn session_response(&self, password_hash: String, headers: &HeaderMap) -> Response {
        match self.session(password_hash).await {
            Ok(token) => cookie_response(&token, headers),
            Err(_) => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Authentication storage unavailable",
            ),
        }
    }
}
pub fn worker_token() -> String {
    secret()
}
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| part.trim().strip_prefix(&format!("{COOKIE}=")))
}
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}
fn local_host(headers: &HeaderMap) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(url) = format!("http://{host}").parse::<reqwest::Url>() else {
        return false;
    };
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(url) = origin.to_str().unwrap_or("").parse::<reqwest::Url>() else {
        return false;
    };
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let Ok(expected) = format!("{}://{host}", url.scheme()).parse::<reqwest::Url>() else {
        return false;
    };
    matches!(url.scheme(), "http" | "https") && url.origin() == expected.origin()
}
pub async fn guard(State(auth): State<Auth>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let headers = request.headers().clone();
    let method = request.method().as_str().to_owned();
    let telemetry_read = path == "/api/v1/telemetry" && method == "GET";
    if telemetry_read && !request.extensions().get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| peer.0.ip().to_canonical().is_loopback())
    {
        return error(StatusCode::FORBIDDEN, "Telemetry is only available over loopback");
    }
    let public = telemetry_read
        || path == "/login"
        || path.starts_with("/assets/")
        || path == "/api/v1/auth/status"
        || path == "/api/v1/auth/login";
    if !same_origin(&headers)
        || headers
            .get("sec-fetch-site")
            .is_some_and(|h| h == "cross-site")
    {
        return error(StatusCode::FORBIDDEN, "Cross-origin access is not allowed");
    }
    if !public && !auth.allowed(&headers, &path, &method).await {
        if path == "/" || path == "/settings" {
            return Redirect::to("/login").into_response();
        }
        return error(StatusCode::UNAUTHORIZED, "Authentication required");
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("x-frame-options", "DENY".parse().unwrap());
    response
        .headers_mut()
        .insert("referrer-policy", "no-referrer".parse().unwrap());
    // Recheck long-lived media streams after activation, logout, expiry, or revocation.
    if (path.ends_with(".mjpeg")
        || path.starts_with("/api/v1/camera/photos/")
        || path.starts_with("/api/v1/video/recordings/"))
        && response.status().is_success()
    {
        let (parts, body) = response.into_parts();
        let stream = async_stream::stream! {
            let mut stream = body.into_data_stream();
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = tick.tick() => { if !auth.allowed(&headers, &path, &method).await { break; } }
                    chunk = stream.next() => { match chunk { Some(chunk) => yield chunk, None => break } }
                }
            }
        };
        response = Response::from_parts(parts, Body::from_stream(stream));
    }
    response
}

pub fn routes(auth: Auth) -> Router {
    Router::new()
        .route(
            "/login",
            get(|| async { axum::response::Html(include_str!("../web/login.html")) }),
        )
        .route("/api/v1/auth/status", get(status))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/password", post(change_password))
        .route("/api/v1/auth/tokens", get(tokens).post(create_token))
        .route("/api/v1/auth/tokens/{id}/revoke", post(revoke_token))
        .layer(axum::extract::DefaultBodyLimit::max(8192))
        .with_state(auth)
}
async fn status(State(auth): State<Auth>, headers: HeaderMap) -> Response {
    match auth.run(|data| Ok(data.password.clone())).await {
        Ok(hash) => Json(json!({"enabled": hash.is_some(), "admin": if let Some(hash) = hash { auth.master(&headers, &hash).await } else { false }})).into_response(),
        Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "Authentication storage unavailable"),
    }
}
#[derive(Deserialize)]
struct PasswordRequest {
    password: String,
    #[serde(default)]
    current_password: String,
}
fn browser_write(headers: &HeaderMap) -> bool {
    headers.get("x-tarsier-request").is_some_and(|v| v == "1") && same_origin(headers)
}
fn cookie_response(token: &str, headers: &HeaderMap) -> Response {
    let secure = headers
        .get(header::ORIGIN)
        .is_some_and(|h| h.to_str().unwrap_or("").starts_with("https://"));
    (
        [(
            header::SET_COOKIE,
            format!(
                "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_SECONDS}{}",
                if secure { "; Secure" } else { "" }
            ),
        )],
        Json(json!({"ok": true})),
    )
        .into_response()
}
async fn login(
    State(auth): State<Auth>,
    headers: HeaderMap,
    Json(input): Json<PasswordRequest>,
) -> Response {
    if !browser_write(&headers) {
        return error(StatusCode::FORBIDDEN, "Same-origin request required");
    }
    let mut gate = auth.login_gate.lock().await;
    if *gate > Instant::now() {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Please wait before trying again",
        );
    }
    *gate = Instant::now() + Duration::from_secs(1);
    drop(gate);
    let result = auth
        .run(move |data| {
            let hash = data
                .password
                .clone()
                .context("No master password configured")?;
            if !verify(&input.password, &hash) {
                bail!("Invalid password");
            }
            Ok(hash)
        })
        .await;
    match result {
        Ok(hash) => auth.session_response(hash, &headers).await,
        Err(_) => error(
            StatusCode::UNAUTHORIZED,
            "Invalid password or authentication unavailable",
        ),
    }
}
async fn logout(State(auth): State<Auth>, headers: HeaderMap) -> Response {
    if !browser_write(&headers) {
        return error(StatusCode::FORBIDDEN, "Same-origin request required");
    }
    if let Some(token) = session_cookie(&headers) {
        let token_hash = digest(token);
        if auth
            .run(move |data| {
                data.sessions.remove(&token_hash);
                Ok(())
            })
            .await
            .is_err()
        {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Authentication storage unavailable",
            );
        }
    }
    (
        [(
            header::SET_COOKIE,
            format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
        )],
        Json(json!({"ok": true})),
    )
        .into_response()
}
async fn admin(auth: &Auth, headers: &HeaderMap) -> Option<String> {
    if bearer(headers).is_some() {
        return None;
    }
    match auth.run(|data| Ok(data.password.clone())).await {
        Ok(Some(hash)) if auth.master(headers, &hash).await => Some(hash),
        _ => None,
    }
}
async fn change_password(
    State(auth): State<Auth>,
    peer: Option<axum::Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Json(input): Json<PasswordRequest>,
) -> Response {
    if !browser_write(&headers) {
        return error(StatusCode::FORBIDDEN, "Same-origin request required");
    }
    let current = match auth.run(|data| Ok(data.password.clone())).await {
        Ok(value) => value,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Authentication storage unavailable",
            );
        }
    };
    if let Some(hash) = &current {
        if !auth.master(&headers, hash).await {
            return error(StatusCode::FORBIDDEN, "Master password session required");
        }
    } else if !peer.is_some_and(|peer| peer.0.0.ip().is_loopback()) || !local_host(&headers) {
        return error(
            StatusCode::FORBIDDEN,
            "Set the first master password on the camera computer or with the CLI",
        );
    }
    let result = auth
        .run(move |data| {
            if data.password != current {
                bail!("Credentials changed; reload and try again");
            }
            if let Some(hash) = &data.password
                && !verify(&input.current_password, hash)
            {
                bail!("Current password is incorrect");
            }
            data.sessions.clear();
            data.password = if input.password.is_empty() {
                None
            } else {
                Some(password_hash(&input.password)?)
            };
            if data.password.is_none() {
                data.tokens.clear();
            }
            Ok(data.password.clone())
        })
        .await;
    match result {
        Ok(hash) => match hash {
            Some(hash) => auth.session_response(hash, &headers).await,
            None => logout(State(auth), headers).await,
        },
        Err(err) => error(StatusCode::BAD_REQUEST, &err.to_string()),
    }
}
async fn tokens(State(auth): State<Auth>, headers: HeaderMap) -> Response {
    let Some(version) = admin(&auth, &headers).await else {
        return error(StatusCode::FORBIDDEN, "Master password session required");
    };
    match auth
        .run(move |data| {
            if data.password.as_ref() != Some(&version) {
                bail!("Credentials changed; sign in again");
            }
            Ok(data
                .tokens
                .iter()
                .map(|t| json!({"id": t.id, "name": t.name, "created_at_ms": t.created_at_ms, "destinations": t.destinations}))
                .collect::<Vec<_>>())
        })
        .await
    {
        Ok(tokens) => Json(json!({"tokens": tokens})).into_response(),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Authentication storage unavailable",
        ),
    }
}
#[derive(Deserialize)]
struct TokenRequest {
    name: String,
    #[serde(default = "default_destinations")]
    destinations: Vec<TokenDestination>,
}
async fn create_token(
    State(auth): State<Auth>,
    headers: HeaderMap,
    Json(input): Json<TokenRequest>,
) -> Response {
    if !browser_write(&headers) {
        return error(StatusCode::FORBIDDEN, "Same-origin request required");
    }
    let Some(version) = admin(&auth, &headers).await else {
        return error(StatusCode::FORBIDDEN, "Master password session required");
    };
    if input.destinations.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "Select at least one token destination",
        );
    }
    if input.name.trim().is_empty() || input.name.len() > 80 {
        return error(
            StatusCode::BAD_REQUEST,
            "Use a token name between 1 and 80 bytes",
        );
    }
    let result = auth
        .run(move |data| {
            if data.password.as_ref() != Some(&version) {
                bail!("Credentials changed; sign in again");
            }
            if data.tokens.len() >= 100 {
                bail!("Token limit reached");
            }
            let token = secret();
            let id = secret();
            data.tokens.push(Token {
                id: id.clone(),
                name: input.name.trim().into(),
                hash: digest(&token),
                destinations: input.destinations.clone(),
                created_at_ms: crate::model::unix_ms(),
            });
            Ok(json!({"id": id, "token": token, "destinations": input.destinations}))
        })
        .await;
    match result {
        Ok(value) => (StatusCode::CREATED, Json(value)).into_response(),
        Err(err) => error(StatusCode::BAD_REQUEST, &err.to_string()),
    }
}
async fn revoke_token(
    State(auth): State<Auth>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    if !browser_write(&headers) {
        return error(StatusCode::FORBIDDEN, "Same-origin request required");
    }
    let Some(version) = admin(&auth, &headers).await else {
        return error(StatusCode::FORBIDDEN, "Master password session required");
    };
    match auth
        .run(move |data| {
            if data.password.as_ref() != Some(&version) {
                bail!("Credentials changed; sign in again");
            }
            data.tokens.retain(|token| token.id != id);
            Ok(())
        })
        .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Authentication storage unavailable",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn token_destinations_are_enforced_and_cannot_administer_tokens() {
        let fixture = Fixture::new();
        let cookie = fixture.setup().await;
        for (destinations, api, mcp) in [
            (json!(["api"]), true, false),
            (json!(["mcp"]), false, true),
            (json!(["api", "mcp"]), true, true),
        ] {
            let created = value(
                fixture
                    .request(
                        "POST",
                        "/api/v1/auth/tokens",
                        Some(&cookie),
                        None,
                        json!({"name": "Scoped client", "destinations": destinations}),
                    )
                    .await,
            )
            .await;
            let token = created["token"].as_str().unwrap();
            assert_eq!(created["destinations"], destinations);
            for (path, permitted) in [("/api/v1/state", api), ("/mcp/api/v1/state", mcp)] {
                let response = fixture
                    .request("GET", path, None, Some(token), json!(null))
                    .await;
                assert_eq!(
                    response.status(),
                    if permitted {
                        StatusCode::OK
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                );
            }
            for session in [None, Some(cookie.as_str())] {
                let response = fixture
                    .request(
                        "POST",
                        "/api/v1/auth/tokens",
                        session,
                        Some(token),
                        json!({"name": "Escalation", "destinations": ["api", "mcp"]}),
                    )
                    .await;
                assert!(!response.status().is_success());
            }
            let update = fixture
                .request(
                    "POST",
                    &format!(
                        "/api/v1/auth/tokens/{}/destinations",
                        created["id"].as_str().unwrap()
                    ),
                    Some(&cookie),
                    None,
                    json!({"destinations": ["api", "mcp"]}),
                )
                .await;
            assert_eq!(update.status(), StatusCode::NOT_FOUND);
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
            headers.insert("x-tarsier-client", "mcp".parse().unwrap());
            assert_eq!(
                fixture.auth.allowed(&headers, "/api/v1/state", "GET").await,
                api
            );
            assert_eq!(
                fixture
                    .auth
                    .allowed(&headers, "/mcp/api/v1/state", "GET")
                    .await,
                mcp
            );
        }
        for destinations in [json!([]), json!(["unknown"])] {
            let response = fixture
                .request(
                    "POST",
                    "/api/v1/auth/tokens",
                    Some(&cookie),
                    None,
                    json!({"name": "Invalid", "destinations": destinations}),
                )
                .await;
            assert!(response.status().is_client_error());
        }
        let listing = value(
            fixture
                .request(
                    "GET",
                    "/api/v1/auth/tokens",
                    Some(&cookie),
                    None,
                    json!(null),
                )
                .await,
        )
        .await;
        assert_eq!(listing["tokens"].as_array().unwrap().len(), 3);
        assert_eq!(listing["tokens"][1]["destinations"], json!(["mcp"]));
        assert_eq!(
            fixture
                .request("GET", "/mcp/api/v1/state", Some(&cookie), None, json!(null))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn legacy_tokens_default_to_api_only() {
        let fixture = Fixture::new();
        fixture.setup().await;
        let mut data: serde_json::Value =
            serde_json::from_slice(&fs::read(&fixture.auth.path).unwrap()).unwrap();
        data["tokens"] = json!([{"id": "legacy", "name": "Legacy", "hash": digest("legacy-token"), "created_at_ms": 0}]);
        fs::write(&fixture.auth.path, serde_json::to_vec(&data).unwrap()).unwrap();
        assert_eq!(
            fixture
                .request(
                    "GET",
                    "/api/v1/state",
                    None,
                    Some("legacy-token"),
                    json!(null)
                )
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            fixture
                .request(
                    "GET",
                    "/mcp/api/v1/state",
                    None,
                    Some("legacy-token"),
                    json!(null)
                )
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    struct Fixture {
        auth: Auth,
        directory: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!("tarsier-auth-{}", secret()));
            Self {
                auth: Auth::new(directory.join("auth.json"), "internal-worker").unwrap(),
                directory,
            }
        }
        fn router(&self) -> Router {
            let (shutdown, receiver) = tokio::sync::watch::channel(false);
            // Keep the sender alive while constructing a router without hardware.
            let router = crate::api::router_with_controls(
                crate::config::Config::default(),
                crate::runtime::Runtime::new(),
                crate::pipeline::PreviewHub::new(),
                None,
                crate::api::ApiOptions {
                    auth: Some(self.auth.clone()),
                    ..Default::default()
                },
                receiver,
            );
            drop(shutdown);
            router
        }
        async fn request(
            &self,
            method: &str,
            path: &str,
            cookie: Option<&str>,
            token: Option<&str>,
            body: serde_json::Value,
        ) -> Response {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "127.0.0.1:8742")
                .header("origin", "http://127.0.0.1:8742")
                .header("content-type", "application/json")
                .header("x-tarsier-request", "1")
                .extension(ConnectInfo(
                    "127.0.0.1:30000".parse::<SocketAddr>().unwrap(),
                ));
            if let Some(cookie) = cookie {
                request = request.header("cookie", cookie);
            }
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            self.router()
                .oneshot(request.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap()
        }
        async fn setup(&self) -> String {
            let response = self
                .request(
                    "POST",
                    "/api/v1/auth/password",
                    None,
                    None,
                    json!({"password":"correct horse battery staple"}),
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
    async fn value(response: Response) -> serde_json::Value {
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn telemetry_reads_are_local_only_and_worker_writes_remain_protected() {
        let fixture = Fixture::new();
        fixture.setup().await;
        let response = fixture.request("GET", "/api/v1/telemetry", None, None, json!(null)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let data = value(response).await;
        assert!(data["resources"].is_object());
        assert!(data["pipeline_context"].is_object());
        for (peer, expected) in [
            (Some("[::1]:30000"), StatusCode::OK),
            (Some("[::ffff:127.0.0.1]:30000"), StatusCode::OK),
            (Some("192.0.2.10:30000"), StatusCode::FORBIDDEN),
            (None, StatusCode::FORBIDDEN),
        ] {
            let mut request = Request::builder().uri("/api/v1/telemetry")
                .header("host", "localhost:8742")
                .header("x-forwarded-for", "127.0.0.1")
                .header("forwarded", "for=127.0.0.1");
            if let Some(peer) = peer {
                request = request.extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
            }
            let response = fixture.router().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), expected, "peer {peer:?}");
        }
        for path in ["/api/v1/telemetry", "/api/v1/perception/telemetry", "/api/v1/perception/observations"] {
            assert_eq!(fixture.request("POST", path, None, None, json!({})).await.status(), StatusCode::UNAUTHORIZED);
        }
        assert_eq!(fixture.request("GET", "/api/v1/state", None, None, json!(null)).await.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn optional_auth_protects_every_operator_surface_and_requires_master_for_tokens() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", None, None, json!(null))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            fixture
                .request(
                    "POST",
                    "/api/v1/auth/tokens",
                    None,
                    None,
                    json!({"name":"deck"})
                )
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let cookie = fixture.setup().await;
        for path in [
            "/api/v1/state",
            "/api/v1/health",
            "/api/v1/config",
            "/api/v1/video/applications",
            "/api/v1/events",
            "/api/v1/preview.mjpeg",
            "/api/v1/camera/snapshot",
            "/api/v1/camera/photos/example.jpg",
            "/api/v1/video/recordings/example.mp4",
            "/api/v1/perception/input.mjpeg",
            "/api/v1/audio/sources",
        ] {
            assert_eq!(
                fixture
                    .request("GET", path, None, None, json!(null))
                    .await
                    .status(),
                StatusCode::UNAUTHORIZED,
                "{path}"
            );
        }
        assert_eq!(
            fixture
                .request(
                    "POST",
                    "/api/v1/camera/actions/recenter",
                    None,
                    None,
                    json!({})
                )
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            fixture
                .request("GET", "/settings", None, None, json!(null))
                .await
                .status(),
            StatusCode::SEE_OTHER
        );
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", Some(&cookie), None, json!(null))
                .await
                .status(),
            StatusCode::OK
        );
        let token = value(
            fixture
                .request(
                    "POST",
                    "/api/v1/auth/tokens",
                    Some(&cookie),
                    None,
                    json!({"name":"Stream Deck"}),
                )
                .await,
        )
        .await;
        let secret = token["token"].as_str().unwrap();
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", None, Some(secret), json!(null))
                .await
                .status(),
            StatusCode::OK
        );
        for (method, path, body) in [
            ("GET", "/api/v1/auth/tokens", json!(null)),
            ("POST", "/api/v1/auth/tokens", json!({"name":"intruder"})),
            (
                "POST",
                "/api/v1/auth/password",
                json!({"password":"another password"}),
            ),
        ] {
            assert_eq!(
                fixture
                    .request(method, path, None, Some(secret), body)
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        let stored = fs::read_to_string(&fixture.auth.path).unwrap();
        assert!(!stored.contains(secret));
        assert!(!stored.contains("correct horse battery staple"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&fixture.auth.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let listing = value(
            fixture
                .request(
                    "GET",
                    "/api/v1/auth/tokens",
                    Some(&cookie),
                    None,
                    json!(null),
                )
                .await,
        )
        .await;
        assert_eq!(listing["tokens"][0]["name"], "Stream Deck");
        assert!(listing["tokens"][0].get("hash").is_none());
        assert!(listing["tokens"][0].get("token").is_none());
        let revoke = format!(
            "/api/v1/auth/tokens/{}/revoke",
            token["id"].as_str().unwrap()
        );
        assert_eq!(
            fixture
                .request("POST", &revoke, Some(&cookie), None, json!({}))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", None, Some(secret), json!(null))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn cli_reset_invalidates_sessions_preserves_tokens_and_disable_revokes_them() {
        let fixture = Fixture::new();
        let cookie = fixture.setup().await;
        let token = value(
            fixture
                .request(
                    "POST",
                    "/api/v1/auth/tokens",
                    Some(&cookie),
                    None,
                    json!({"name":"MCP"}),
                )
                .await,
        )
        .await;
        let token = token["token"].as_str().unwrap();
        let cli = Auth::new(fixture.auth.path.clone(), "unused").unwrap();
        cli.reset_password("new master password").unwrap();
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", Some(&cookie), None, json!(null))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", None, Some(token), json!(null))
                .await
                .status(),
            StatusCode::OK
        );
        let response = fixture
            .request(
                "POST",
                "/api/v1/auth/login",
                None,
                None,
                json!({"password":"new master password"}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert!(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("HttpOnly; SameSite=Strict")
        );
        assert_eq!(
            fixture
                .request(
                    "POST",
                    "/api/v1/auth/logout",
                    Some(&cookie),
                    None,
                    json!({})
                )
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", Some(&cookie), None, json!(null))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        cli.disable().unwrap();
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", None, None, json!(null))
                .await
                .status(),
            StatusCode::OK
        );
        cli.reset_password("newer master password").unwrap();
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", None, Some(token), json!(null))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn rejects_cross_origin_remote_bootstrap_invalid_password_and_corrupt_storage() {
        let fixture = Fixture::new();
        for (origin, peer) in [
            ("http://evil.example", "127.0.0.1:3000"),
            ("http://127.0.0.1:8742", "192.168.1.2:3000"),
        ] {
            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/auth/password")
                .header("host", "127.0.0.1:8742")
                .header("origin", origin)
                .header("content-type", "application/json")
                .header("x-tarsier-request", "1")
                .extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()))
                .body(Body::from(r#"{"password":"correct horse battery staple"}"#))
                .unwrap();
            assert_eq!(
                fixture.router().oneshot(request).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
        let cookie = fixture.setup().await;
        assert_eq!(
            fixture
                .request(
                    "POST",
                    "/api/v1/auth/password",
                    Some(&cookie),
                    None,
                    json!({"password":"replacement password", "current_password":"wrong"})
                )
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            fixture
                .request(
                    "POST",
                    "/api/v1/auth/login",
                    None,
                    None,
                    json!({"password":"wrong"})
                )
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            fixture
                .request(
                    "POST",
                    "/api/v1/auth/login",
                    None,
                    None,
                    json!({"password":"wrong"})
                )
                .await
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        fs::write(&fixture.auth.path, b"broken").unwrap();
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", Some(&cookie), None, json!(null))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(Auth::new(fixture.auth.path.clone(), "worker").is_err());
    }

    #[tokio::test]
    async fn browser_session_survives_restart_and_logout_stays_revoked() {
        use std::os::unix::fs::PermissionsExt;

        let mut fixture = Fixture::new();
        let cookie = fixture.setup().await;
        let token = cookie.split_once('=').unwrap().1;
        let stored = fs::read_to_string(&fixture.auth.path).unwrap();
        assert!(!stored.contains(token));
        assert!(stored.contains(&digest(token)));
        assert_eq!(
            fs::metadata(&fixture.auth.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let expires_before = fixture
            .auth
            .transaction(|data| Ok(data.sessions[&digest(token)].expires_at_ms))
            .unwrap();

        fixture.auth = Auth::new(fixture.auth.path.clone(), "new-worker").unwrap();
        assert_eq!(
            fixture
                .request("GET", "/", Some(&cookie), None, json!(null))
                .await
                .status(),
            StatusCode::OK
        );
        let expires_after = fixture
            .auth
            .transaction(|data| Ok(data.sessions[&digest(token)].expires_at_ms))
            .unwrap();
        assert_eq!(expires_before, expires_after);
        assert_eq!(
            fixture
                .request(
                    "POST",
                    "/api/v1/auth/logout",
                    Some(&cookie),
                    None,
                    json!({})
                )
                .await
                .status(),
            StatusCode::OK
        );

        fixture.auth = Auth::new(fixture.auth.path.clone(), "third-worker").unwrap();
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", Some(&cookie), None, json!(null))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn worker_credential_is_scoped_and_sessions_expire() {
        let fixture = Fixture::new();
        let cookie = fixture.setup().await;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer internal-worker".parse().unwrap(),
        );
        for path in [
            "/api/v1/perception/input.mjpeg",
            "/api/v1/perception/observations",
            "/api/v1/perception/telemetry",
            "/api/v1/perception/mask",
            "/api/v1/avatar/frame",
            "/api/v1/depth/frame",
            "/api/v1/video/identity",
            "/api/v1/perception/demand",
        ] {
            assert!(fixture.auth.allowed(&headers, path, "GET").await);
        }
        assert!(
            !fixture
                .auth
                .allowed(&headers, "/api/v1/video/identity", "POST")
                .await
        );
        assert!(
            fixture
                .auth
                .allowed(&headers, "/api/v1/video/liveportrait/status", "POST")
                .await
        );
        assert!(
            !fixture
                .auth
                .allowed(&headers, "/api/v1/video/liveportrait/source", "POST")
                .await
        );
        assert!(!fixture.auth.allowed(&headers, "/api/v1/state", "GET").await);
        assert!(
            !fixture
                .auth
                .allowed(&headers, "/api/v1/camera/power", "POST")
                .await
        );
        fixture
            .auth
            .transaction(|data| {
                for session in data.sessions.values_mut() {
                    session.expires_at_ms = crate::model::unix_ms();
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            fixture
                .request("GET", "/api/v1/state", Some(&cookie), None, json!(null))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
}
