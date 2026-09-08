//! Inspect driver capture activity and local processes holding the virtual device open.
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
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
    /// Driver-reported capture state, independent of process visibility.
    /// None means unavailable or unsupported, never a confirmed idle device.
    pub capture_active: Option<bool>,
    pub applications: Vec<Application>,
}

impl Snapshot {
    pub fn connected(&self) -> bool {
        self.available && (self.capture_active == Some(true) || !self.applications.is_empty())
    }
}

#[derive(Clone, Default)]
pub struct Monitor(Arc<Mutex<Option<(Instant, String, Snapshot)>>>);

impl Monitor {
    pub async fn applications(&self, device: String) -> Result<Snapshot> {
        self.inspect(device, false).await
    }

    /// Bypass the display cache before accepting a camera command.
    pub async fn fresh_applications(&self, device: String) -> Result<Snapshot> {
        self.inspect(device, true).await
    }

    async fn inspect(&self, device: String, fresh: bool) -> Result<Snapshot> {
        let mut cache = self.0.lock().await;
        if let Some((at, cached_device, snapshot)) = &*cache
            && !fresh
            && cached_device == &device
            && at.elapsed() < Duration::from_secs(2)
        {
            return Ok(snapshot.clone());
        }
        let path = device.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            let mut snapshot = scan(
                Path::new("/proc"),
                Path::new(&path),
                std::process::id(),
                fs::metadata("/proc/self")?.uid(),
            )?;
            if snapshot.available {
                snapshot.capture_active = driver_capture_active(Path::new(&path)).ok();
            }
            anyhow::Ok(snapshot)
        })
        .await??;
        *cache = Some((Instant::now(), device, snapshot.clone()));
        Ok(snapshot)
    }
}

// Linux videodev2.h ABI and v4l2loopback's private client-usage event.
const CLIENT_USAGE_EVENT: u32 = 0x0800_0000 + 0x08e0_0000 + 1;

#[repr(C)]
#[derive(Default)]
struct EventSubscription {
    kind: u32,
    id: u32,
    flags: u32,
    reserved: [u32; 5],
}

#[repr(C)]
union EventData {
    bytes: [u8; 64],
    // The kernel union includes a signed 64-bit control value.
    alignment: i64,
}

#[repr(C)]
struct VideoEvent {
    kind: u32,
    data: EventData,
    pending: u32,
    sequence: u32,
    timestamp: nix::libc::timespec,
    id: u32,
    reserved: [u32; 8],
}

nix::ioctl_write_ptr!(subscribe_event, b'V', 90, EventSubscription);
nix::ioctl_read!(dequeue_event, b'V', 89, VideoEvent);

fn driver_capture_active(device: &Path) -> Result<bool> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(device)
        .context("Could not open the virtual camera for usage events")?;
    let subscription = EventSubscription {
        kind: CLIENT_USAGE_EVENT,
        flags: 1, // V4L2_EVENT_SUB_FL_SEND_INITIAL: include readers already streaming.
        ..EventSubscription::default()
    };
    // SAFETY: These repr(C) buffers match the Linux V4L2 ABI and the file remains
    // open throughout both ioctls. All-zero bytes are valid for VideoEvent.
    let mut event: VideoEvent = unsafe { std::mem::zeroed() };
    unsafe { subscribe_event(file.as_raw_fd(), &subscription) }
        .context("Virtual camera does not provide client-usage events")?;
    unsafe { dequeue_event(file.as_raw_fd(), &mut event) }
        .context("Virtual camera did not provide its initial capture state")?;
    anyhow::ensure!(
        event.kind == CLIENT_USAGE_EVENT,
        "Unexpected virtual camera event"
    );
    // SAFETY: The private event payload is a native-endian u32 count in data[0..4].
    let count = u32::from_ne_bytes(unsafe { event.data.bytes[..4].try_into().unwrap() });
    // v4l2loopback 0.15.3 reports a boolean, not an exact process count.
    Ok(count != 0)
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
        capture_active: None,
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
    fn unsupported_usage_events_are_not_reported_as_idle() {
        assert!(driver_capture_active(Path::new("/dev/null")).is_err());
    }

    #[tokio::test]
    async fn command_scan_bypasses_recent_display_cache() {
        let monitor = Monitor::default();
        let device = String::new();
        *monitor.0.lock().await = Some((
            Instant::now(),
            device.clone(),
            Snapshot {
                available: true,
                partial: false,
                capture_active: Some(false),
                applications: vec![],
            },
        ));
        assert!(
            monitor
                .applications(device.clone())
                .await
                .unwrap()
                .available
        );
        assert!(!monitor.fresh_applications(device).await.unwrap().available);
    }

    #[test]
    fn occupied_device_blocks_mcp_commands() {
        // The API uses this scan result before executing a gateway mutation.
        let snapshot = Snapshot {
            available: true,
            partial: false,
            capture_active: Some(false),
            applications: vec![Application {
                pid: 20,
                name: "Conference".into(),
                binary: None,
            }],
        };
        assert_eq!(
            crate::api::mcp_usage_rejection(Ok(snapshot))
                .unwrap()
                .status(),
            axum::http::StatusCode::CONFLICT,
        );
    }
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

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn only_confirmed_clients_allow_manual_unmute() {
        let mut snapshot = Snapshot { available: true, partial: true, capture_active: None, applications: vec![] };
        assert!(!snapshot.connected());
        snapshot.capture_active = Some(false);
        assert!(!snapshot.connected());
        snapshot.capture_active = Some(true);
        assert!(snapshot.connected());
        snapshot.capture_active = Some(false);
        snapshot.applications.push(Application { pid: 42, name: "Conference".into(), binary: None });
        assert!(snapshot.connected());
        snapshot.applications.clear();
        assert!(!snapshot.connected());
    }
}
