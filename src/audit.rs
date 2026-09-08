//! Persistent, private JSONL audit records. Never record credentials or bodies.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Instant,
};

static AUDIT: OnceLock<Mutex<Journal>> = OnceLock::new();

struct Journal {
    directory: PathBuf,
    session: String,
    sequence: u64,
}

impl Journal {
    fn new(directory: PathBuf) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        Ok(Self {
            directory,
            session: format!("{}-{}", crate::model::unix_ms(), std::process::id()),
            sequence: 0,
        })
    }

    fn append(&mut self, event: &str, data: Value) -> Result<()> {
        let now = chrono::Utc::now();
        let path = self
            .directory
            .join(format!("audit-{}.jsonl", now.format("%Y-%m-%d")));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)?;
        self.sequence += 1;
        let mut bytes = serde_json::to_vec(&json!({
            "timestamp": now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "pid": std::process::id(), "session": self.session,
            "sequence": self.sequence, "event": event, "data": data,
        }))?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        Ok(())
    }
}

pub fn init() -> Result<()> {
    let root = std::env::var_os("XDG_STATE_HOME")
        .filter(|p| Path::new(p).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .context("Set HOME or an absolute XDG_STATE_HOME for audit logging")?;
    let mut journal = Journal::new(root.join("tarsier"))?;
    journal.append(
        "daemon.starting",
        json!({"version": env!("CARGO_PKG_VERSION")}),
    )?;
    AUDIT
        .set(Mutex::new(journal))
        .map_err(|_| anyhow::anyhow!("audit logger already initialized"))?;
    Ok(())
}

pub fn record(event: &str, data: Value) {
    let Some(journal) = AUDIT.get() else {
        return;
    };
    let result = journal
        .lock()
        .map_err(|_| anyhow::anyhow!("audit lock poisoned"))
        .and_then(|mut journal| journal.append(event, data));
    if let Err(error) = result {
        tracing::error!(%error, event, "could not persist audit record");
    }
}

/// Record even a cancelled HTTP request, without retaining its headers or body.
pub struct RequestLog {
    id: u64,
    start: Instant,
    completed: bool,
}

impl RequestLog {
    pub fn begin(method: &str, path: &str, peer: Option<String>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        record(
            "http.request",
            json!({"request_id": id, "method": method, "path": path, "peer": peer}),
        );
        Self {
            id,
            start: Instant::now(),
            completed: false,
        }
    }
    pub fn finish(mut self, status: u16) {
        record(
            "http.response",
            json!({"request_id": self.id, "status": status, "duration_ms": self.start.elapsed().as_millis()}),
        );
        self.completed = true;
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        if !self.completed {
            record(
                "http.cancelled",
                json!({"request_id": self.id, "duration_ms": self.start.elapsed().as_millis()}),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn journal_appends_across_sessions_with_private_files_and_parseable_lines() {
        let dir = std::env::temp_dir().join(format!(
            "tarsier-audit-{}-{}",
            std::process::id(),
            crate::model::unix_ms()
        ));
        let mut journal = Journal::new(dir.clone()).unwrap();
        journal
            .append("camera.power.requested", json!({"enabled": false}))
            .unwrap();
        drop(journal);
        Journal::new(dir.clone())
            .unwrap()
            .append("daemon.starting", json!({}))
            .unwrap();
        let path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let text = fs::read_to_string(&path).unwrap();
        let rows: Vec<Value> = text
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["data"]["enabled"], false);
        assert_eq!(rows[1]["event"], "daemon.starting");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
