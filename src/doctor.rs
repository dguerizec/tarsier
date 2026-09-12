//! Read-only diagnostics, with narrowly scoped, opt-in repairs.
use crate::{
    config::{Config, VideoSource},
    service::Installation,
};
use anyhow::{Context, Result, bail};
use std::{
    ffi::CString,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Default)]
struct Report {
    errors: usize,
}
impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        if !ok {
            self.errors += 1;
        }
        println!("[{}] {name}: {detail}", if ok { "OK" } else { "FAIL" });
    }
    fn warn(&self, name: &str, detail: impl std::fmt::Display) {
        println!("[WARN] {name}: {detail}");
    }
}

fn accessible(path: &Path, mode: i32) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // access checks permissions (including ACLs) without opening camera hardware.
    unsafe { nix::libc::access(path.as_ptr(), mode) == 0 }
}

fn find_executable(command: &str, directory: &Path, search_path: &str) -> Option<PathBuf> {
    let candidates = if command.contains('/') {
        vec![directory.join(command)]
    } else {
        std::env::split_paths(search_path)
            .map(|p| directory.join(p).join(command))
            .collect()
    };
    candidates
        .into_iter()
        .find(|p| p.is_file() && accessible(p, nix::libc::X_OK))
}

fn executable(command: &str, directory: &Path, search_path: &str) -> bool {
    find_executable(command, directory, search_path).is_some()
}

fn device_holders(device: &str) -> Vec<(u32, String)> {
    let Ok(metadata) = std::fs::metadata(device) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut holders = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        if fds.flatten().any(|fd| {
            std::fs::metadata(fd.path())
                .is_ok_and(|m| m.file_type().is_char_device() && m.rdev() == metadata.rdev())
        }) {
            holders.push((
                pid,
                std::fs::read_to_string(entry.path().join("comm"))
                    .unwrap_or_default()
                    .trim()
                    .into(),
            ));
        }
    }
    holders.sort_by_key(|p| p.0);
    holders
}

fn check_device(report: &mut Report, name: &str, device: &str) {
    let path = Path::new(device);
    let character = std::fs::metadata(path).is_ok_and(|m| m.file_type().is_char_device());
    report.check(
        name,
        character && accessible(path, nix::libc::R_OK | nix::libc::W_OK),
        format!("{device}; must exist as a character device with read/write access"),
    );
}

pub fn daemon_processes() -> Vec<(u32, PathBuf)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        if std::fs::read_to_string(path.join("comm"))
            .unwrap_or_default()
            .trim()
            != "tarsier"
        {
            continue;
        }
        let command = std::fs::read(path.join("cmdline")).unwrap_or_default();
        if command.split(|c| *c == 0).any(|arg| arg == b"serve") {
            found.push((
                pid,
                std::fs::read_link(path.join("exe")).unwrap_or_default(),
            ));
        }
    }
    found.sort_by_key(|p| p.0);
    found
}

fn effective_config(installation: &Installation) -> Result<Config> {
    let mut config = Config::load(installation.config.as_deref())?;
    // Read persisted choices directly: loading the store would create or migrate files.
    match std::fs::read(&installation.settings) {
        Ok(bytes) => {
            let settings: crate::settings::UserSettings =
                serde_json::from_slice(&bytes).context("invalid persisted user settings")?;
            if let Some(camera) = settings.camera_device {
                crate::devices::apply_camera(&mut config, &camera);
            }
            if let Some(source) = settings.liveportrait_source {
                config.avatar.source_image = source;
            }
            if let Some(model) = settings.portrait3d_model {
                config.avatar.portrait_model = model;
            }
            if let Some(lan) = settings.network_lan_access {
                config.server.bind.set_ip(if lan {
                    std::net::Ipv4Addr::UNSPECIFIED.into()
                } else {
                    std::net::Ipv4Addr::LOCALHOST.into()
                });
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    config.validate()?;
    Ok(config)
}

pub async fn preflight(installation: &Installation, fix: bool) -> Result<usize> {
    let mut report = Report::default();
    let config = match effective_config(installation) {
        Ok(config) => {
            report.check(
                "Configuration",
                true,
                installation
                    .config
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or("built-in defaults".into()),
            );
            config
        }
        Err(error) => {
            report.check("Configuration", false, format!("{error:#}"));
            return Ok(report.errors);
        }
    };
    report.check(
        "Binary",
        accessible(&installation.executable, nix::libc::X_OK),
        installation.executable.display(),
    );
    report.check(
        "Working directory",
        installation.directory.is_dir(),
        installation.directory.display(),
    );
    if config.video.source == VideoSource::Camera {
        check_device(&mut report, "Physical camera", &config.video.input_device);
    }
    if config.video.loopback_enabled {
        report.check(
            "Loopback module",
            Path::new("/sys/module/v4l2loopback").exists(),
            "load v4l2loopback at boot if missing (requires administrator setup)",
        );
        let present = Path::new(&config.video.output_device).exists();
        if !present {
            report.check(
                "Loopback utility",
                executable(
                    "v4l2loopback-ctl",
                    &installation.directory,
                    &installation.search_path,
                ),
                "v4l2loopback-ctl must be installed to create a missing device",
            );
            report.check("Loopback control", accessible(Path::new("/dev/v4l2loopback"), nix::libc::R_OK | nix::libc::W_OK), "/dev/v4l2loopback requires read/write access; check video group membership and device rules");
            if fix {
                // Use the same creation path as normal daemon startup; never invoke sudo.
                let utility = find_executable(
                    "v4l2loopback-ctl",
                    &installation.directory,
                    &installation.search_path,
                )
                .unwrap_or_else(|| installation.directory.join(".missing-v4l2loopback-ctl"));
                match crate::pipeline::ensure_virtual_video_device(
                    &config.video.output_device,
                    utility
                        .to_str()
                        .context("loopback utility path must be UTF-8")?,
                )
                .await
                {
                    Ok(()) => println!("[FIXED] Created {}", config.video.output_device),
                    Err(error) => {
                        report.check("Create virtual camera", false, format!("{error:#}"))
                    }
                }
            }
        }
        check_device(&mut report, "Virtual camera", &config.video.output_device);
        if present
            && !executable(
                "v4l2loopback-ctl",
                &installation.directory,
                &installation.search_path,
            )
        {
            report.warn(
                "Loopback utility",
                "v4l2loopback-ctl is missing; automatic recreation after reboot will fail",
            );
        }
    }
    if let Err(error) = gstreamer::init() {
        report.check("GStreamer", false, error);
    } else {
        let mut elements = vec!["appsrc", "appsink", "videoconvert", "videoscale", "jpegenc"];
        if config.video.source == VideoSource::Camera {
            elements.push("v4l2src");
        }
        if config.video.loopback_enabled {
            elements.push("v4l2sink");
        }
        for element in elements {
            report.check(
                "GStreamer element",
                gstreamer::ElementFactory::find(element).is_some(),
                element,
            );
        }
    }
    if config.perception.enabled && config.perception.supervise_worker {
        report.check(
            "Perception launcher",
            executable("uv", &installation.directory, &installation.search_path),
            "uv must be on the service PATH",
        );
        let project = installation
            .directory
            .join(&config.perception.worker_project);
        report.check(
            "Perception project",
            project.join("pyproject.toml").is_file(),
            project.display(),
        );
        report.warn(
            "Perception runtime",
            "model loading and inference are not exercised by doctor",
        );
    }
    for command in ["pactl", "pw-cli", "pw-link", "parec"] {
        report.check(
            "Audio utility",
            executable(command, &installation.directory, &installation.search_path),
            command,
        );
    }
    if let Some(command) = config.audio.voice_worker.first() {
        report.check(
            "Voice worker",
            executable(command, &installation.directory, &installation.search_path),
            command,
        );
    }
    if config.avatar.enabled {
        report.check(
            "Avatar source",
            installation
                .directory
                .join(&config.avatar.source_image)
                .is_file(),
            config.avatar.source_image.display(),
        );
    }
    Ok(report.errors)
}

pub async fn run(config: Option<PathBuf>, fix: bool) -> Result<()> {
    let path = crate::service::unit_path()?;
    let mut report = Report::default();
    let installed = match crate::service::read_installation(&path) {
        Ok(value) => value,
        Err(error) => {
            report.warn("Service definition", format!("{error:#}"));
            None
        }
    };
    let mut installation = installed.clone().unwrap_or(Installation {
        executable: std::env::current_exe()?,
        directory: std::env::current_dir()?,
        config: None,
        settings: std::path::absolute(crate::settings::default_path()?)?,
        auth: std::path::absolute(crate::auth::default_path()?)?,
        search_path: std::env::var("PATH").unwrap_or_default(),
    });
    if let Some(config) = config {
        installation.config = Some(std::fs::canonicalize(config)?);
        installation.directory = std::env::current_dir()?;
    }
    println!(
        "Tarsier doctor{}",
        if fix {
            " (safe repairs enabled)"
        } else {
            " (read-only)"
        }
    );
    report.errors += preflight(&installation, fix).await?;
    let mut main_pid = 0;
    match crate::service::state().await {
        Ok(state) => {
            let loaded = state
                .get("LoadState")
                .map(String::as_str)
                .unwrap_or("unknown");
            let active = state
                .get("ActiveState")
                .map(String::as_str)
                .unwrap_or("unknown");
            let enabled = state
                .get("UnitFileState")
                .map(String::as_str)
                .unwrap_or("unknown");
            println!("[INFO] Service: {loaded}, {active}, {enabled}");
            println!(
                "[INFO] Service binary: {}",
                state.get("ExecStart").map(String::as_str).unwrap_or("none")
            );
            if installed.is_none() || enabled != "enabled" {
                report.warn("Autostart", "no enabled managed installation; use tarsier install with the intended binary/configuration");
            }
            main_pid = state
                .get("MainPID")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            if active == "failed" || state.get("NRestarts").is_some_and(|s| s != "0") {
                report.warn(
                    "Service restarts",
                    "inspect journalctl --user -u tarsier.service for recent failures",
                );
            }
        }
        Err(error) => report.check("User service manager", false, format!("{error:#}")),
    }
    let processes = daemon_processes();
    for (pid, binary) in &processes {
        println!(
            "[INFO] Daemon PID {pid}: {}{}",
            binary.display(),
            if *pid == main_pid {
                " (managed service)"
            } else {
                " (outside service)"
            }
        );
    }
    report.check(
        "Daemon instances",
        processes.len() <= 1,
        format!(
            "{} visible; process visibility may be restricted",
            processes.len()
        ),
    );
    if let Ok(config) = effective_config(&installation) {
        if config.video.source == VideoSource::Camera {
            println!(
                "[INFO] Physical camera open handles (partial process visibility): {:?}",
                device_holders(&config.video.input_device)
            );
        }
        // Merely connecting to HTTP does not acquire camera capture or toggle output.
        let mut address = config.server.bind;
        if address.ip().is_unspecified() {
            address.set_ip(if address.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()?;
        let mut request = client.get(format!("http://{address}/api/v1/health"));
        if let Ok(token) = std::env::var("TARSIER_API_TOKEN") {
            request = request.bearer_auth(token);
        }
        match request.send().await {
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => report.warn("API", format!("{address} responds but requires authentication; set TARSIER_API_TOKEN for a health check")),
            Ok(response) if response.status().is_success() => {
                let body = response.json::<serde_json::Value>().await.unwrap_or_default();
                report.check("API", body.get("status").and_then(|s| s.as_str()) == Some("ok") && body.get("started_at_ms").is_some(), format!("{address}; status={}, version={}", body.get("status").and_then(|s| s.as_str()).unwrap_or("unknown"), body.get("version").and_then(|s| s.as_str()).unwrap_or("unknown")));
            }
            Ok(response) => report.check("API", false, format!("port {address} returned {}; inspect its owner", response.status())),
            Err(error) if processes.is_empty() => report.warn("API", format!("daemon is not running at {address}: {error}")),
            Err(error) => report.check("API", false, error),
        }
        if config.video.loopback_enabled && Path::new(&config.video.output_device).exists() {
            match crate::video_clients::Monitor::default()
                .fresh_applications(config.video.output_device)
                .await
            {
                Ok(snapshot) => println!(
                    "[INFO] Virtual camera clients: {}",
                    serde_json::to_string(&snapshot)?
                ),
                Err(error) => report.warn("Virtual camera clients", error),
            }
        }
    }
    // Login and linger are intentionally reported, never changed by --fix.
    let uid = std::fs::metadata("/proc/self")?.uid().to_string();
    let mut command = tokio::process::Command::new("loginctl");
    if let Ok(Ok(output)) = tokio::time::timeout(
        Duration::from_secs(3),
        command
            .args(["show-user", &uid, "--property=Linger"])
            .kill_on_drop(true)
            .output(),
    )
    .await
        && output.status.success()
    {
        println!(
            "[INFO] {} (yes: user services can start before login)",
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    if report.errors > 0 {
        bail!(
            "doctor found {} failed checks; --fix only creates a missing virtual camera",
            report.errors
        );
    }
    println!("No failed checks. Warnings above describe limits or optional setup.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_checks_do_not_need_to_open_devices() {
        assert!(accessible(
            Path::new("/dev/null"),
            nix::libc::R_OK | nix::libc::W_OK
        ));
        assert!(!accessible(
            Path::new("/nonexistent/tarsier-device"),
            nix::libc::R_OK
        ));
        assert!(!executable(
            "does-not-exist-tarsier",
            Path::new("/"),
            "/usr/bin"
        ));
        assert!(executable("true", Path::new("/"), "/usr/bin:/bin"));
    }
}
