//! Inspect local processes holding the virtual V4L2 device open.
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Serialize)]
pub struct Application {
    pid: u32,
    name: String,
    binary: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub available: bool,
    pub partial: bool,
    pub applications: Vec<Application>,
}

#[derive(Clone, Default)]
pub struct Monitor(Arc<Mutex<Option<(Instant, String, Snapshot)>>>);

impl Monitor {
    pub async fn applications(&self, device: String) -> Result<Snapshot> {
        let mut cache = self.0.lock().await;
        if let Some((at, cached_device, snapshot)) = &*cache
            && cached_device == &device
            && at.elapsed() < Duration::from_secs(2)
        {
            return Ok(snapshot.clone());
        }
        let path = device.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            scan(
                Path::new("/proc"),
                Path::new(&path),
                std::process::id(),
                fs::metadata("/proc/self")?.uid(),
            )
        })
        .await??;
        *cache = Some((Instant::now(), device, snapshot.clone()));
        Ok(snapshot)
    }
}

fn belongs_to_daemon(mut pid: u32, daemon: u32, parents: &HashMap<u32, u32>) -> bool {
    let mut seen = HashSet::new();
    while seen.insert(pid) {
        if pid == daemon {
            return true;
        }
        let Some(parent) = parents.get(&pid) else {
            break;
        };
        pid = *parent;
    }
    false
}

fn scan(proc: &Path, device: &Path, daemon: u32, uid: u32) -> Result<Snapshot> {
    let mut result = Snapshot {
        available: false,
        partial: false,
        applications: vec![],
    };
    let metadata = match fs::metadata(device) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(error) => return Err(error).context("Could not inspect the virtual camera"),
    };
    if !metadata.file_type().is_char_device() {
        return Ok(result);
    }
    result.available = true;
    let mut parents = HashMap::new();
    let mut candidates = Vec::new();
    for entry in fs::read_dir(proc)
        .context("Could not inspect local processes")?
        .flatten()
    {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let path = entry.path();
        if let Ok(stat) = fs::read_to_string(path.join("stat"))
            && let Some((_, fields)) = stat.rsplit_once(") ")
            && let Some(parent) = fields
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u32>().ok())
        {
            parents.insert(pid, parent);
        }
        if fs::metadata(&path).is_ok_and(|metadata| metadata.uid() == uid) {
            candidates.push((pid, path));
        }
    }
    for (pid, path) in candidates {
        if belongs_to_daemon(pid, daemon, &parents) {
            continue;
        }
        let fds = match fs::read_dir(path.join("fd")) {
            Ok(fds) => fds,
            Err(error) => {
                result.partial |= error.kind() == std::io::ErrorKind::PermissionDenied;
                continue;
            }
        };
        let opened = fds.flatten().any(|fd| {
            fs::metadata(fd.path())
                .is_ok_and(|m| m.file_type().is_char_device() && m.rdev() == metadata.rdev())
        });
        if !opened {
            continue;
        }
        let binary = fs::read_link(path.join("exe"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        let name = fs::read_to_string(path.join("comm"))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .or_else(|| binary.clone())
            .unwrap_or_else(|| format!("Process {pid}"));
        result.applications.push(Application { pid, name, binary });
    }
    result
        .applications
        .sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn device_scan_deduplicates_handles_and_excludes_daemon_descendants() {
        let root =
            std::env::temp_dir().join(format!("tarsier-video-clients-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for (pid, parent, device) in [
            (10, 1, "/dev/null"),
            (11, 10, "/dev/null"),
            (12, 11, "/dev/null"),
            (20, 1, "/dev/null"),
            (30, 1, "/dev/zero"),
        ] {
            let path = root.join(pid.to_string());
            fs::create_dir_all(path.join("fd")).unwrap();
            fs::write(
                path.join("stat"),
                format!("{pid} (process with spaces) S {parent} 0"),
            )
            .unwrap();
            fs::write(path.join("comm"), "Test reader\n").unwrap();
            for fd in [3, 4] {
                std::os::unix::fs::symlink(device, path.join("fd").join(fd.to_string())).unwrap();
            }
        }
        let snapshot = scan(
            &root,
            Path::new("/dev/null"),
            10,
            fs::metadata(&root).unwrap().uid(),
        )
        .unwrap();
        fs::remove_dir_all(root).unwrap();
        assert!(snapshot.available);
        assert!(!snapshot.partial);
        assert_eq!(snapshot.applications.len(), 1);
        assert_eq!(snapshot.applications[0].pid, 20);
        assert_eq!(snapshot.applications[0].name, "Test reader");
    }
}
