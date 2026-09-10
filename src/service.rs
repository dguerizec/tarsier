//! A single, explicitly replaceable systemd user installation.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

pub const UNIT: &str = "tarsier.service";
const MARKER: &str = "# Tarsier installation v1: ";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installation {
    pub executable: PathBuf,
    pub directory: PathBuf,
    pub config: Option<PathBuf>,
    pub settings: PathBuf,
    pub auth: PathBuf,
    pub search_path: String,
}

pub fn unit_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("set HOME or XDG_CONFIG_HOME to locate the user service directory")?;
    if !base.is_absolute() {
        bail!("XDG_CONFIG_HOME must be absolute");
    }
    Ok(base.join("systemd/user").join(UNIT))
}

fn quote(value: &str, exec: bool) -> Result<String> {
    if value.chars().any(char::is_control) {
        bail!("service values must not contain control characters");
    }
    let value = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    Ok(format!(
        "\"{}\"",
        if exec {
            value.replace('$', "$$")
        } else {
            value
        }
    ))
}

fn path_quote(path: &Path, exec: bool) -> Result<String> {
    if !path.is_absolute() {
        bail!("service paths must be absolute: {}", path.display());
    }
    quote(path.to_str().context("service paths must be UTF-8")?, exec)
}

impl Installation {
    pub fn render(&self) -> Result<String> {
        // WorkingDirectory is a single raw path, unlike ExecStart's quoted words.
        path_quote(&self.directory, false)?;
        let directory = self
            .directory
            .to_str()
            .context("working directory must be UTF-8")?;
        if directory.ends_with(char::is_whitespace) {
            bail!("working directory must not end with whitespace");
        }
        let directory = directory.replace('%', "%%");
        let mut command = format!("{} serve", path_quote(&self.executable, true)?);
        if let Some(config) = &self.config {
            command += &format!(" --config {}", path_quote(config, true)?);
        }
        let settings = quote(
            &format!("TARSIER_USER_SETTINGS_PATH={}", self.settings.display()),
            false,
        )?;
        let auth = quote(&format!("TARSIER_AUTH_PATH={}", self.auth.display()), false)?;
        let search_path = quote(&format!("PATH={}", self.search_path), false)?;
        Ok(format!(
            "{MARKER}{}\n[Unit]\nDescription=Tarsier camera and audio daemon\nStartLimitIntervalSec=0\n\n[Service]\nType=simple\nWorkingDirectory={}\nExecStart={command}\nEnvironment={settings}\nEnvironment={auth}\nEnvironment={search_path}\nRestart=always\nRestartSec=2s\nTimeoutStopSec=30s\n\n[Install]\nWantedBy=default.target\n",
            serde_json::to_string(self)?,
            directory
        ))
    }
}

pub fn read_installation(path: &Path) -> Result<Option<Installation>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let metadata = text
                .lines()
                .next()
                .and_then(|l| l.strip_prefix(MARKER))
                .context("service is not managed by tarsier install")?;
            let installation: Installation = serde_json::from_str(metadata)?;
            if installation.render()? != text {
                bail!(
                    "managed service was edited externally; refusing to overwrite or remove it silently"
                );
            }
            Ok(Some(installation))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub async fn systemctl(args: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(45),
        tokio::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("systemctl timed out; inspect the service before retrying")?
    .context("cannot execute systemctl")?;
    if !output.status.success() {
        bail!(
            "systemctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub async fn state() -> Result<BTreeMap<String, String>> {
    let text = systemctl(&[
        "show",
        UNIT,
        "--property=LoadState,ActiveState,UnitFileState,FragmentPath,MainPID,ExecStart,NRestarts",
    ])
    .await?;
    Ok(text
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.into(), v.into())))
        .collect())
}

fn active(state: &BTreeMap<String, String>) -> bool {
    matches!(
        state.get("ActiveState").map(String::as_str),
        Some("active" | "activating" | "reloading" | "deactivating")
    )
}

fn check_replacement(
    old: Option<&Installation>,
    desired: &Installation,
    unmanaged: bool,
    replace: bool,
) -> Result<()> {
    if (unmanaged || old.is_some_and(|old| old != desired)) && !replace {
        bail!(
            "a different or unmanaged Tarsier service exists; inspect it and use --replace to replace it explicitly"
        );
    }
    Ok(())
}

pub async fn install(config: Option<PathBuf>, now: bool, replace: bool) -> Result<()> {
    let config = config
        .map(std::fs::canonicalize)
        .transpose()
        .context("cannot resolve configuration path")?;
    crate::config::Config::load(config.as_deref())?.validate()?;
    let desired = Installation {
        executable: std::env::current_exe()?.canonicalize()?,
        directory: std::env::current_dir()?.canonicalize()?,
        config,
        settings: std::path::absolute(crate::settings::default_path()?)?,
        auth: std::path::absolute(crate::auth::default_path()?)?,
        search_path: std::env::var("PATH").context("PATH is missing")?,
    };
    let text = desired.render()?;
    let path = unit_path()?;
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!(
            "{} is a symlink; inspect and remove it manually before installation",
            path.display()
        );
    }
    let old = read_installation(&path);
    let state = state().await?;
    let fragment = state.get("FragmentPath").map(String::as_str).unwrap_or("");
    let foreign = !fragment.is_empty() && fragment != path.to_string_lossy();
    check_replacement(
        old.as_ref().ok().and_then(|i| i.as_ref()),
        &desired,
        old.is_err() || foreign,
        replace,
    )?;
    let transient = fragment.contains("/systemd/transient/");
    if transient && !now {
        bail!(
            "the current service is transient; use --replace --now to migrate it without leaving two service definitions"
        );
    }
    if foreign && !transient {
        bail!("service is loaded from {fragment}; remove that definition before installing here");
    }
    if now {
        let main_pid = state
            .get("MainPID")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let others: Vec<_> = crate::doctor::daemon_processes()
            .into_iter()
            .filter(|(pid, _)| *pid != main_pid)
            .collect();
        if !others.is_empty() {
            bail!(
                "another Tarsier daemon is running outside this service: {others:?}; stop it before using --now"
            );
        }
    }
    let failures = crate::doctor::preflight(&desired, false).await?;
    if failures > 0 {
        println!(
            "[WARN] {failures} prerequisite checks failed; installation can proceed, but resolve them with tarsier doctor before relying on the service."
        );
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    // Write completely before replacing the unit so an interrupted write cannot truncate it.
    let temporary = path.with_extension(format!("service.{}.tmp", std::process::id()));
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    if transient {
        systemctl(&["stop", UNIT]).await?;
    }
    std::fs::rename(&temporary, &path)?;
    systemctl(&["daemon-reload"]).await?;
    systemctl(&["enable", UNIT]).await?;
    if now {
        systemctl(&["restart", UNIT]).await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let state = self::state().await?;
        if !active(&state) || state.get("NRestarts").is_some_and(|s| s != "0") {
            bail!(
                "service installed but startup failed or restarted; inspect journalctl --user -u tarsier.service"
            );
        }
    }
    println!(
        "Installed and enabled {}\nBinary: {}\nWorking directory: {}",
        path.display(),
        desired.executable.display(),
        desired.directory.display()
    );
    println!(
        "{}",
        if now {
            "Service started. Run tarsier doctor for runtime diagnostics."
        } else {
            "The running process was not restarted. Use systemctl --user restart tarsier.service to apply now."
        }
    );
    println!(
        "Starts with the user manager (normally at login). Boot without login requires loginctl enable-linger for this user."
    );
    Ok(())
}

pub async fn uninstall(now: bool) -> Result<()> {
    let path = unit_path()?;
    if read_installation(&path)?.is_none() {
        println!("No managed Tarsier installation found.");
        return Ok(());
    }
    if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
        bail!("refusing to remove a symlinked unit");
    }
    let state = state().await?;
    if let Some(fragment) = state.get("FragmentPath")
        && !fragment.is_empty()
        && fragment != &path.to_string_lossy()
    {
        bail!(
            "a different service definition is loaded from {fragment}; refusing to stop or disable it"
        );
    }
    if active(&state) && !now {
        bail!("service is running; use uninstall --now to stop it explicitly");
    }
    if now {
        systemctl(&["stop", UNIT]).await?;
    }
    systemctl(&["disable", UNIT]).await?;
    std::fs::remove_file(&path)?;
    systemctl(&["daemon-reload"]).await?;
    println!(
        "Removed the Tarsier service. Binary, configuration, settings and media were preserved."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn installation() -> Installation {
        Installation {
            executable: "/tmp/a b%$c/tarsier".into(),
            directory: "/tmp/a b%$c".into(),
            config: Some("/tmp/a b/config.toml".into()),
            settings: "/tmp/settings.json".into(),
            auth: "/tmp/auth.json".into(),
            search_path: "/usr/bin".into(),
        }
    }
    #[test]
    fn service_escapes_specifiers_variables_and_spaces() {
        let unit = installation().render().unwrap();
        assert!(unit.contains(
            "ExecStart=\"/tmp/a b%%$$c/tarsier\" serve --config \"/tmp/a b/config.toml\""
        ));
        assert!(unit.contains("WorkingDirectory=/tmp/a b%%$c\n"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(quote("a\nb", true).is_err());
        assert!(path_quote(Path::new("relative"), true).is_err());
    }
    #[test]
    fn installation_replacement_is_explicit_and_idempotent() {
        let desired = installation();
        assert!(check_replacement(Some(&desired), &desired, false, false).is_ok());
        let mut other = desired.clone();
        other.executable = "/tmp/release/tarsier".into();
        assert!(check_replacement(Some(&other), &desired, false, false).is_err());
        assert!(check_replacement(None, &desired, true, false).is_err());
        assert!(check_replacement(Some(&other), &desired, false, true).is_ok());
    }
}
