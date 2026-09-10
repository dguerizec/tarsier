//! Bounded, opt-in recording through the same read-only API as the UI.
use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Value, json};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, time::Duration};
use tokio::time::{Instant, MissedTickBehavior};

#[derive(Debug, Args)]
pub struct Options {
    /// New JSONL file to create; existing files are never overwritten.
    #[arg(short, long)]
    pub output: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8742")]
    pub url: String,
    /// Maximum recording duration in seconds; Ctrl-C stops earlier.
    #[arg(long, default_value = "600", value_parser = clap::value_parser!(u64).range(1..=86400))]
    pub duration: u64,
    /// Polling interval in seconds (worker samples refresh every two seconds).
    #[arg(long, default_value = "2", value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub interval: u64,
    /// Stop before the file exceeds this size in MiB.
    #[arg(long, default_value = "64", value_parser = clap::value_parser!(u64).range(1..=1024))]
    pub max_mib: u64,
}

fn endpoint(url: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(url).context("invalid daemon URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "Use an HTTP(S) daemon URL without credentials, query, or fragment; authenticate with TARSIER_API_TOKEN"
        );
    }
    url.set_path(&format!(
        "{}/api/v1/telemetry",
        url.path().trim_end_matches('/')
    ));
    Ok(url)
}

async fn sample(
    client: &reqwest::Client,
    url: &reqwest::Url,
    token: Option<&str>,
) -> std::result::Result<Value, (&'static str, bool)> {
    let mut request = client.get(url.clone());
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| ("connection_or_timeout", false))?;
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err(("authentication_required", true)),
        _ => return Err(("http_error", false)),
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ("response_read_failed", false))?
    {
        if body.len() + chunk.len() > 1024 * 1024 {
            return Err(("response_too_large", false));
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&body).map_err(|_| ("invalid_json", false))?;
    if !value.get("resources").is_some_and(Value::is_object) {
        return Err(("invalid_telemetry", false));
    }
    Ok(value)
}

struct Jsonl {
    file: std::fs::File,
    bytes: u64,
    limit: u64,
}
impl Jsonl {
    fn write(&mut self, value: &Value) -> Result<bool> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        if self.bytes + line.len() as u64 > self.limit {
            return Ok(false);
        }
        self.file
            .write_all(&line)
            .context("failed to write telemetry recording")?;
        self.file.flush()?;
        self.bytes += line.len() as u64;
        Ok(true)
    }
}

pub async fn run(options: Options) -> Result<()> {
    let url = endpoint(&options.url)?;
    let token = std::env::var("TARSIER_API_TOKEN").ok();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&options.output)
        .context("cannot create recording (parent must exist and file must be new)")?;
    let mut output = Jsonl {
        file,
        bytes: 0,
        limit: options.max_mib * 1024 * 1024,
    };
    output.write(
        &json!({"schema_version":1,"kind":"start","recorded_at_ms":crate::model::unix_ms(),
        "interval_ms":options.interval * 1000,"duration_ms":options.duration * 1000,
        "pipeline_context":null,"pipeline_context_current":false}),
    )?;
    let mut ticker = tokio::time::interval(Duration::from_secs(options.interval));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let end = Instant::now() + Duration::from_secs(options.duration);
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let mut count = 0;
    let mut pipeline_context = Value::Null;
    let reason = loop {
        tokio::select! {
            biased;
            result = &mut interrupt => { result.context("failed to listen for Ctrl-C")?; break "interrupted"; },
            _ = tokio::time::sleep_until(end) => break "duration",
            _ = ticker.tick() => {},
        }
        let result = tokio::select! {
            biased;
            result = &mut interrupt => { result.context("failed to listen for Ctrl-C")?; break "interrupted"; },
            _ = tokio::time::sleep_until(end) => break "duration",
            result = sample(&client, &url, token.as_deref()) => result,
        };
        let at = crate::model::unix_ms();
        let context_current = if let Ok(data) = &result {
            pipeline_context = data.get("pipeline_context").cloned().unwrap_or(Value::Null);
            !pipeline_context.is_null()
        } else {
            false
        };
        let (mut record, fatal) = match result {
            Ok(data) => (
                json!({"schema_version":1,"kind":"sample","recorded_at_ms":at,"data":data}),
                false,
            ),
            Err((error, fatal)) => (
                json!({"schema_version":1,"kind":"error","recorded_at_ms":at,"error":error}),
                fatal,
            ),
        };
        record["pipeline_context"] = pipeline_context.clone();
        record["pipeline_context_current"] = json!(context_current);
        if !output.write(&record)? {
            break "size_limit";
        }
        if fatal {
            bail!("Authentication required: set TARSIER_API_TOKEN to a token with API access");
        }
        count += 1;
    };
    // The size cap takes precedence even over the optional final marker.
    output.write(&json!({"schema_version":1,"kind":"stop","recorded_at_ms":crate::model::unix_ms(),"reason":reason,"pipeline_context":pipeline_context,"pipeline_context_current":false}))?;
    eprintln!(
        "Recorded {count} samples/errors to {} ({} bytes; {reason})",
        options.output.display(),
        output.bytes
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};

    #[test]
    fn urls_cannot_embed_secrets() {
        assert!(endpoint("http://user:secret@localhost").is_err());
        assert!(endpoint("http://localhost?token=secret").is_err());
        assert!(endpoint("file:///tmp/data").is_err());
        assert_eq!(
            endpoint("http://localhost:8742/").unwrap().path(),
            "/api/v1/telemetry"
        );
    }

    #[tokio::test]
    async fn client_reads_samples_and_classifies_failures() {
        let app = Router::new()
            .route(
                "/ok",
                get(|| async { axum::Json(json!({"resources":{},"worker_stages":{"pid":42}})) }),
            )
            .route(
                "/auth",
                get(|| async { axum::http::StatusCode::UNAUTHORIZED }),
            )
            .route("/invalid", get(|| async { "not JSON" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let url = |path: &str| reqwest::Url::parse(&format!("http://{address}/{path}")).unwrap();
        assert_eq!(
            sample(&client, &url("ok"), None).await.unwrap()["worker_stages"]["pid"],
            42
        );
        assert_eq!(
            sample(&client, &url("auth"), None).await.unwrap_err(),
            ("authentication_required", true)
        );
        assert_eq!(
            sample(&client, &url("invalid"), None).await.unwrap_err(),
            ("invalid_json", false)
        );
        server.abort();
    }

    #[tokio::test]
    async fn recording_keeps_mode_changes_and_marks_context_during_outages() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/api/v1/telemetry",
            get(move || {
                let call = calls.fetch_add(1, Ordering::Relaxed);
                async move {
                    let status = if call == 1 {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        axum::http::StatusCode::OK
                    };
                    (
                        status,
                        axum::Json(json!({"resources":{}, "pipeline_context": {
                            "identity": if call == 0 { "camera" } else { "depth-map" },
                            "background": {"enabled":true,"selected_effect":"blur"}
                        }})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let path = std::env::temp_dir().join(format!(
            "tarsier-record-context-{}-{}.jsonl",
            std::process::id(),
            crate::model::unix_ms()
        ));
        let options = || Options {
            output: path.clone(),
            url: format!("http://{address}"),
            duration: 3,
            interval: 1,
            max_mib: 1,
        };
        run(options()).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 5);
        assert!(rows[0]["pipeline_context"].is_null());
        assert_eq!(rows[1]["pipeline_context"]["identity"], "camera");
        assert_eq!(rows[1]["pipeline_context_current"], true);
        assert_eq!(rows[2]["kind"], "error");
        assert_eq!(rows[2]["pipeline_context"]["identity"], "camera");
        assert_eq!(rows[2]["pipeline_context_current"], false);
        assert_eq!(rows[3]["pipeline_context"]["identity"], "depth-map");
        assert_eq!(rows[4]["reason"], "duration");
        assert!(run(options()).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::remove_file(path).unwrap();
        server.abort();
    }

    #[test]
    fn jsonl_size_limit_preserves_complete_lines() {
        let path = std::env::temp_dir().join(format!(
            "tarsier-jsonl-{}-{}",
            std::process::id(),
            crate::model::unix_ms()
        ));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let mut writer = Jsonl {
            file,
            bytes: 0,
            limit: 10,
        };
        assert!(writer.write(&json!({"a":1})).unwrap());
        assert!(!writer.write(&json!({"b":2})).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n");
        std::fs::remove_file(path).unwrap();
    }
}
