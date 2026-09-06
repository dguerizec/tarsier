//! Shared daemon capture and a process-owned PipeWire virtual microphone.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Write,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::runtime::Runtime;
use anyhow::{Context, Result, bail};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
    sync::{broadcast, oneshot, watch},
    task::JoinHandle,
    time::{MissedTickBehavior, interval, sleep, timeout},
};

pub const VIRTUAL_SOURCE: &str = "tarsier_microphone";
const BLOCK_BYTES: usize = 960 * 4;
const MAX_AGE: Duration = Duration::from_millis(120);

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct VirtualMicrophone {
    pub enabled: bool,
    pub source: Option<String>,
    pub muted: bool,
    pub running: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReservationStatus {
    #[default]
    Pending,
    Held,
    Released,
    Unavailable,
    Disconnected,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Reservation {
    pub status: ReservationStatus,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub muted: bool,
}

async fn all_sources() -> Result<Vec<Source>> {
    let output = timeout(
        Duration::from_secs(3),
        Command::new("pactl")
            .args(["-f", "json", "list", "sources"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Audio device discovery timed out")??;
    if !output.status.success() {
        bail!("Audio server unavailable");
    }
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    Ok(entries
        .into_iter()
        .filter(|v| v["properties"]["device.class"] != "monitor")
        .filter_map(|v| {
            Some(Source {
                id: v["name"].as_str()?.to_owned(),
                name: v["description"].as_str()?.to_owned(),
                muted: v["mute"].as_bool().unwrap_or(false),
            })
        })
        .collect())
}

pub async fn sources() -> Result<Vec<Source>> {
    Ok(all_sources()
        .await?
        .into_iter()
        .filter(|s| s.id != VIRTUAL_SOURCE)
        .collect())
}

#[derive(Debug, Serialize)]
pub struct Application {
    pub name: String,
    pub binary: Option<String>,
    pub pid: Option<String>,
    pub streams: usize,
    pub process: Option<ProcessIdentity>,
    pub next_signal: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
}

fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    if pid <= 1 || pid == std::process::id() {
        return None;
    }
    let path = format!("/proc/{pid}");
    if std::fs::metadata(&path).ok()?.uid() != std::fs::metadata("/proc/self").ok()?.uid() {
        return None;
    }
    let stat = std::fs::read_to_string(format!("{path}/stat")).ok()?;
    let start_ticks = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    Some(ProcessIdentity { pid, start_ticks })
}

fn signal_process(process: &ProcessIdentity, signal: i32) -> Result<()> {
    // A pidfd pins the process across exit/PID reuse between validation and signaling.
    let raw = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, process.pid, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    if process_identity(process.pid).as_ref() != Some(process) {
        bail!("The process changed or exited; refresh the application list");
    }
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<nix::libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct SourceApplications {
    pub available: bool,
    pub applications: Vec<Application>,
}

pub async fn applications(source: &str) -> Result<SourceApplications> {
    let output = timeout(
        Duration::from_secs(3),
        Command::new("pw-dump").kill_on_drop(true).output(),
    )
    .await
    .context("Audio application lookup timed out")??;
    if !output.status.success() {
        bail!("Could not inspect audio connections");
    }
    let graph: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    let mut result = connected_applications(&graph, source);
    result.applications.retain(|app| !is_own_capture(app));
    for app in &mut result.applications {
        app.process = app
            .pid
            .as_ref()
            .and_then(|pid| pid.parse().ok())
            .and_then(process_identity);
    }
    Ok(result)
}

fn connected_applications(graph: &[serde_json::Value], source: &str) -> SourceApplications {
    let nodes: HashMap<u64, &serde_json::Value> = graph
        .iter()
        .filter_map(|object| Some((object["id"].as_u64()?, object)))
        .collect();
    let Some(source_id) = graph
        .iter()
        .find(|object| {
            object["type"] == "PipeWire:Interface:Node"
                && object["info"]["props"]["node.name"] == source
        })
        .and_then(|object| object["id"].as_u64())
    else {
        return SourceApplications {
            available: false,
            applications: Vec::new(),
        };
    };
    // Stereo links share a node. Count each capture stream once, then group
    // streams belonging to the same client process without reading command lines.
    let connected: HashSet<u64> = graph
        .iter()
        .filter(|object| {
            object["type"] == "PipeWire:Interface:Link"
                && object["info"]["output-node-id"].as_u64() == Some(source_id)
        })
        .filter_map(|object| object["info"]["input-node-id"].as_u64())
        .collect();
    let mut applications = BTreeMap::<_, Application>::new();
    for id in connected {
        let Some(node) = nodes.get(&id) else {
            continue;
        };
        let props = &node["info"]["props"];
        let client_id = props["client.id"].as_u64();
        let client = client_id.and_then(|id| nodes.get(&id));
        let value = |key: &str| -> Option<String> {
            let field = if props[key].is_null() {
                client.map(|c| &c["info"]["props"][key])
            } else {
                Some(&props[key])
            }?;
            match field {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            }
        };
        let binary = value("application.process.binary");
        let pid = value("application.process.id");
        let name = value("application.name")
            .or_else(|| binary.clone())
            .or_else(|| value("node.description"))
            .or_else(|| value("node.name"))
            .unwrap_or_else(|| "Unknown application".into());
        let key = (
            name.clone(),
            pid.clone(),
            binary.clone(),
            if pid.is_none() {
                Some(client_id.unwrap_or(id))
            } else {
                None
            },
        );
        applications
            .entry(key)
            .or_insert(Application {
                name,
                binary,
                pid,
                streams: 0,
                process: None,
                next_signal: None,
            })
            .streams += 1;
    }
    SourceApplications {
        available: true,
        applications: applications.into_values().collect(),
    }
}

#[derive(Clone)]
struct Frame {
    captured: Instant,
    pcm: Arc<Vec<u8>>,
}

#[derive(Clone)]
enum Packet {
    Audio(Frame),
    Error(String),
}

#[derive(Clone, Default)]
pub struct AudioHub {
    channels: Arc<Mutex<HashMap<String, broadcast::Sender<Packet>>>>,
    terminated: Arc<Mutex<HashSet<ProcessIdentity>>>,
}

impl AudioHub {
    pub async fn inspect_applications(&self, source: &str) -> Result<SourceApplications> {
        let mut result = applications(source).await?;
        let mut terminated = self.terminated.lock().unwrap();
        terminated.retain(|process| process_identity(process.pid).as_ref() == Some(process));
        for app in &mut result.applications {
            app.next_signal = app
                .process
                .as_ref()
                .map(|process| if terminated.contains(process) { 9 } else { 15 });
        }
        Ok(result)
    }

    pub async fn kill_application(
        &self,
        source: &str,
        process: ProcessIdentity,
        signal: i32,
    ) -> Result<()> {
        if signal != 15 && signal != 9 {
            bail!("Only SIGTERM and SIGKILL are supported");
        }
        let connected = applications(source).await?;
        if !connected
            .applications
            .iter()
            .any(|app| app.process.as_ref() == Some(&process))
        {
            bail!("This process is no longer connected to the selected microphone");
        }
        let mut terminated = self.terminated.lock().unwrap();
        let expected = if terminated.contains(&process) { 9 } else { 15 };
        if signal != expected {
            bail!("Signal state changed; refresh the list before trying again");
        }
        signal_process(&process, signal)?;
        terminated.insert(process);
        Ok(())
    }

    fn channel(&self, source: &str) -> broadcast::Sender<Packet> {
        self.channels
            .lock()
            .unwrap()
            .entry(source.to_owned())
            .or_insert_with(|| broadcast::channel(16).0)
            .clone()
    }

    pub fn start(runtime: Runtime, shutdown: watch::Receiver<bool>) -> (Self, JoinHandle<()>) {
        let hub = Self::default();
        let task = tokio::spawn(hub.clone().supervise(runtime, shutdown));
        (hub, task)
    }

    async fn supervise(self, runtime: Runtime, mut shutdown: watch::Receiver<bool>) {
        let mut captures: HashMap<String, (bool, oneshot::Sender<()>, JoinHandle<()>)> =
            HashMap::new();
        let (inventory_tx, inventory) = watch::channel(None);
        let discovery = tokio::spawn(discover(inventory_tx, shutdown.clone()));
        let connections = tokio::spawn(monitor_connections(runtime.clone(), shutdown.clone()));
        let mut last_inventory = Vec::new();
        let mut output: Option<(oneshot::Sender<()>, JoinHandle<()>)> = None;
        let mut tick = interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = tick.tick() => {}
            }
            let discovered = inventory.borrow().clone();
            if let Some(sources) = discovered
                && sources != last_inventory
            {
                reconcile_reservations(&runtime, &sources).await;
                last_inventory = sources;
            }
            let state = runtime.state().await;
            let mut wanted: HashMap<String, bool> = last_inventory
                .iter()
                .filter_map(|source| {
                    let exclusive = !state.audio_released_sources.contains(&source.id);
                    (exclusive || state.audio_capture_sources.contains(&source.id))
                        .then(|| (source.id.clone(), exclusive))
                })
                .collect();
            if state.audio_virtual.enabled {
                // Meter the published source, including system-side mute/volume.
                wanted.insert(VIRTUAL_SOURCE.into(), false);
                if output.is_none() {
                    let (stop, stopped) = oneshot::channel();
                    let hub = self.clone();
                    let runtime = runtime.clone();
                    output = Some((
                        stop,
                        tokio::spawn(async move {
                            hub.virtual_output(runtime, stopped).await;
                        }),
                    ));
                }
            } else if let Some((stop, task)) = output.take() {
                let _ = stop.send(());
                let _ = task.await;
            }
            let removed: Vec<_> = captures
                .keys()
                .filter(|id| wanted.get(*id) != captures.get(*id).map(|entry| &entry.0))
                .cloned()
                .collect();
            for id in removed {
                let (_, stop, task) = captures.remove(&id).unwrap();
                let _ = stop.send(());
                let _ = task.await;
                let _ = self
                    .channel(&id)
                    .send(Packet::Error("Input changed".into()));
                if id != VIRTUAL_SOURCE && state.audio_released_sources.contains(&id) {
                    set_reservation(&runtime, &id, ReservationStatus::Released, None).await;
                }
            }
            for (id, exclusive) in wanted {
                if captures.contains_key(&id) {
                    continue;
                }
                if id != VIRTUAL_SOURCE {
                    set_reservation(
                        &runtime,
                        &id,
                        if exclusive {
                            ReservationStatus::Pending
                        } else {
                            ReservationStatus::Released
                        },
                        None,
                    )
                    .await;
                }
                let (stop, stopped) = oneshot::channel();
                let sender = self.channel(&id);
                let task = tokio::spawn(capture(
                    id.clone(),
                    sender,
                    stopped,
                    exclusive,
                    runtime.clone(),
                ));
                captures.insert(id, (exclusive, stop, task));
            }
        }
        if let Some((stop, task)) = output {
            let _ = stop.send(());
            let _ = task.await;
        }
        for (_, (_, stop, task)) in captures {
            let _ = stop.send(());
            let _ = task.await;
        }
        let _ = discovery.await;
        let _ = connections.await;
    }

    async fn virtual_output(self, runtime: Runtime, mut stop: oneshot::Receiver<()>) {
        loop {
            let result = tokio::select! {
                _ = &mut stop => break,
                result = self.publish(&runtime) => result,
            };
            let error = result.err().map(|e| e.to_string());
            runtime
                .update(|s| {
                    s.audio_virtual.running = false;
                    s.audio_virtual.error = error;
                })
                .await;
            tokio::select! { _ = &mut stop => break, _ = sleep(Duration::from_secs(2)) => {} }
        }
        runtime
            .update(|s| {
                s.audio_virtual.running = false;
                s.audio_virtual.error = None;
            })
            .await;
    }

    async fn publish(&self, runtime: &Runtime) -> Result<()> {
        if all_sources().await?.iter().any(|s| s.id == VIRTUAL_SOURCE) {
            bail!("A Tarsier virtual microphone already exists");
        }
        let pipe = Pipe::new()?;
        let args = format!(
            "{{ tunnel.mode=source tunnel.may-pause=false pipe.filename={} audio.format=S16LE audio.rate=48000 audio.channels=2 audio.position=[FL FR] stream.props={{ node.name={VIRTUAL_SOURCE} node.description=\"Tarsier Microphone\" node.virtual=true priority.session=0 }} }}",
            serde_json::to_string(&pipe.path)?
        );
        let mut child = Command::new("pw-cli")
            .args(["-m", "load-module", "libpipewire-module-pipe-tunnel", &args])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("Virtual microphone unavailable: install PipeWire tools")?;
        let writer = timeout(Duration::from_secs(4), async {
            loop {
                if let Some(status) = child.try_wait()? {
                    bail!("PipeWire virtual microphone exited: {status}");
                }
                match std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(nix::libc::O_NONBLOCK)
                    .open(&pipe.path)
                {
                    Ok(writer) => return Ok(writer),
                    Err(_) => sleep(Duration::from_millis(50)).await,
                }
            }
        })
        .await
        .context("PipeWire virtual microphone startup timed out")??;
        // The FIFO is created before the node is registered with PulseAudio.
        timeout(Duration::from_secs(4), async {
            loop {
                if all_sources().await?.iter().any(|s| s.id == VIRTUAL_SOURCE) {
                    return Ok::<_, anyhow::Error>(());
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("Virtual microphone did not appear in the audio server")??;
        let default_source = timeout(
            Duration::from_secs(3),
            Command::new("pactl")
                .args(["set-default-source", VIRTUAL_SOURCE])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("Setting the default microphone timed out")??;
        if !default_source.status.success() {
            bail!(
                "Could not set Tarsier Microphone as the system default: {}",
                String::from_utf8_lossy(&default_source.stderr).trim()
            );
        }
        runtime
            .update(|s| {
                s.audio_virtual.running = true;
                s.audio_virtual.error = None;
            })
            .await;
        let mut writer = writer;
        let mut selected = None;
        let mut input = None;
        let silence = vec![0; BLOCK_BYTES];
        let mut tick = interval(Duration::from_millis(20));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Some(status) = child.try_wait()? {
                bail!("PipeWire virtual microphone exited: {status}");
            }
            let state = runtime.state().await;
            let settings = state.audio_virtual;
            if selected != settings.source {
                selected = settings.source.clone();
                input = selected.as_ref().map(|id| self.channel(id).subscribe());
            }
            let frame = input.as_mut().and_then(next_frame);
            let allowed = settings.enabled
                && !settings.muted
                && selected
                    .as_ref()
                    .is_some_and(|id| state.audio_capture_sources.contains(id));
            let pcm = output_pcm(frame.as_ref(), allowed, &silence);
            // One block fits PIPE_BUF: nonblocking writes are atomic. A full pipe
            // drops this block instead of accumulating delayed microphone audio.
            match writer.write(pcm) {
                Ok(n) if n == pcm.len() => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Ok(_) => bail!("Incomplete virtual microphone PCM block"),
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub async fn stream(
        &self,
        socket: WebSocket,
        source: String,
        mut shutdown: watch::Receiver<bool>,
        mut states: watch::Receiver<crate::model::RuntimeState>,
    ) {
        let mut packets = self.channel(&source).subscribe();
        let (mut sender, mut receiver) = socket.split();
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                changed = states.changed() => {
                    if changed.is_err() { break; }
                    let state = states.borrow_and_update();
                    let enabled = if source == VIRTUAL_SOURCE { state.audio_virtual.enabled } else { state.audio_capture_sources.contains(&source) };
                    if !enabled { break; }
                },
                incoming = receiver.next() => match incoming {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                },
                packet = packets.recv() => {
                    let payload = match packet {
                        Ok(Packet::Audio(frame)) => serde_json::to_string(&measure(&frame.pcm)).unwrap(),
                        Ok(Packet::Error(error)) => serde_json::json!({"error": error}).to_string(),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    };
                    if !matches!(timeout(Duration::from_secs(1), sender.send(Message::Text(payload.into()))).await, Ok(Ok(()))) { break; }
                }
            }
        }
    }
}

fn next_frame(input: &mut broadcast::Receiver<Packet>) -> Option<Frame> {
    // Keep at most two blocks queued to absorb scheduling jitter without replaying old audio.
    while input.len() > 2 {
        let _ = input.try_recv();
    }
    match input.try_recv() {
        Ok(Packet::Audio(frame)) => Some(frame),
        _ => None,
    }
}

fn output_pcm<'a>(frame: Option<&'a Frame>, allowed: bool, silence: &'a [u8]) -> &'a [u8] {
    match frame {
        Some(frame) if allowed && frame.captured.elapsed() <= MAX_AGE => &frame.pcm,
        _ => silence,
    }
}

struct Pipe {
    path: PathBuf,
}
impl Pipe {
    fn new() -> Result<Self> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .context("XDG_RUNTIME_DIR is required for virtual audio")?;
        let dir = base.join(format!(
            "tarsier-audio-{}-{}",
            std::process::id(),
            crate::model::unix_ms()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self {
            path: dir.join("microphone.fifo"),
        })
    }
}
impl Drop for Pipe {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

async fn capture(
    source: String,
    sender: broadcast::Sender<Packet>,
    mut stop: oneshot::Receiver<()>,
    exclusive: bool,
    runtime: Runtime,
) {
    loop {
        let result = tokio::select! {
            _ = &mut stop => return,
            result = capture_once(&source, &sender, exclusive, &runtime) => result,
        };
        if exclusive {
            set_reservation(&runtime, &source, ReservationStatus::Unavailable,
                Some("Exclusive access unavailable; another application may be using this input. Retrying.".into())).await;
        }
        let _ = sender.send(Packet::Error(
            result
                .err()
                .map_or_else(|| "Capture stopped".into(), |e| e.to_string()),
        ));
        tokio::select! { _ = &mut stop => return, _ = sleep(Duration::from_secs(2)) => {} }
    }
}

async fn capture_once(
    source: &str,
    sender: &broadcast::Sender<Packet>,
    exclusive: bool,
    runtime: &Runtime,
) -> Result<()> {
    if !all_sources().await?.iter().any(|s| s.id == source) {
        bail!("Source disconnected · Waiting for the same microphone");
    }
    let mut command = Command::new("parec");
    command.args([
        "--raw",
        "--format=s16le",
        "--rate=48000",
        "--channels=2",
        "--latency-msec=20",
        "--client-name=Tarsier shared audio",
        "--property=node.dont-reconnect=true",
        "--property=node.dont-fallback=true",
        "--device",
        source,
    ]);
    if exclusive {
        command.arg("--property=node.exclusive=true");
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Audio capture unavailable: install parec")?;
    let mut stdout = child.stdout.take().expect("piped audio output");
    let mut acquired = false;
    loop {
        let mut pcm = vec![0; BLOCK_BYTES];
        timeout(Duration::from_secs(2), stdout.read_exact(&mut pcm))
            .await
            .context("Audio source stopped responding")??;
        if exclusive && !acquired {
            set_reservation(runtime, source, ReservationStatus::Held, None).await;
            acquired = true;
        }
        let _ = sender.send(Packet::Audio(Frame {
            captured: Instant::now(),
            pcm: Arc::new(pcm),
        }));
    }
}
async fn set_reservation(
    runtime: &Runtime,
    source: &str,
    status: ReservationStatus,
    error: Option<String>,
) {
    let reservation = Reservation { status, error };
    if runtime.state().await.audio_reservations.get(source) == Some(&reservation) {
        return;
    }
    runtime
        .update(|s| {
            s.audio_reservations.insert(source.to_owned(), reservation);
        })
        .await;
}

fn reservation_inventory(
    current: &BTreeMap<String, Reservation>,
    sources: &[Source],
    released: &[String],
) -> BTreeMap<String, Reservation> {
    let present: HashSet<_> = sources.iter().map(|s| s.id.as_str()).collect();
    let mut next = current.clone();
    for (id, reservation) in &mut next {
        if !present.contains(id.as_str()) {
            *reservation = Reservation {
                status: ReservationStatus::Disconnected,
                error: None,
            };
        }
    }
    for source in sources {
        let reservation = next.entry(source.id.clone()).or_default();
        if released.contains(&source.id) && reservation.status != ReservationStatus::Held {
            *reservation = Reservation {
                status: ReservationStatus::Released,
                error: None,
            };
        } else if reservation.status == ReservationStatus::Disconnected {
            *reservation = Reservation::default();
        }
    }
    next
}

async fn reconcile_reservations(runtime: &Runtime, sources: &[Source]) {
    runtime
        .update(|s| {
            s.audio_reservations =
                reservation_inventory(&s.audio_reservations, sources, &s.audio_released_sources);
        })
        .await;
}

fn is_own_capture(app: &Application) -> bool {
    let Some(pid) = app.pid.as_ref().and_then(|pid| pid.parse::<u32>().ok()) else {
        return false;
    };
    if pid == std::process::id() {
        return true;
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(") ")?
                .1
                .split_whitespace()
                .nth(1)?
                .parse::<u32>()
                .ok()
        })
        == Some(std::process::id())
}

fn busy_sources(graph: &[serde_json::Value]) -> Vec<String> {
    let mut busy: Vec<_> = graph
        .iter()
        .filter_map(|node| {
            let props = &node["info"]["props"];
            let name = props["node.name"].as_str()?;
            if node["type"] != "PipeWire:Interface:Node"
                || props["media.class"] != "Audio/Source"
                || name == VIRTUAL_SOURCE
            {
                return None;
            }
            connected_applications(graph, name)
                .applications
                .iter()
                .any(|app| !is_own_capture(app))
                .then(|| name.to_owned())
        })
        .collect();
    busy.sort();
    busy.dedup();
    busy
}

async fn refresh_connections(runtime: &Runtime) -> Result<()> {
    let output = timeout(
        Duration::from_secs(3),
        Command::new("pw-dump").kill_on_drop(true).output(),
    )
    .await??;
    if !output.status.success() {
        bail!("Could not inspect audio connections");
    }
    let graph: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    let busy = busy_sources(&graph);
    let output_applications = connected_applications(&graph, VIRTUAL_SOURCE)
        .applications
        .iter()
        .filter(|app| !is_own_capture(app))
        .count();
    let current = runtime.state().await;
    if current.audio_busy_sources != busy
        || current.audio_output_applications != output_applications
    {
        runtime
            .update(|state| {
                state.audio_released_sources.retain(|source| {
                    !state.audio_busy_sources.contains(source) || busy.contains(source)
                });
                state.audio_busy_sources = busy;
                state.audio_output_applications = output_applications;
            })
            .await;
    }
    Ok(())
}

async fn monitor_connections(runtime: Runtime, mut shutdown: watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        let child = Command::new("pw-link")
            .args(["--monitor", "--links", "--id"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut child) = child {
            let mut lines =
                BufReader::new(child.stdout.take().expect("piped link monitor")).lines();
            let mut tick = interval(Duration::from_millis(200));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut poll = interval(Duration::from_secs(5));
            poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut dirty = true;
            loop {
                tokio::select! {
                    _ = shutdown.changed() => return,
                    line = lines.next_line() => match line {
                        Ok(Some(_)) => { dirty = true; continue; },
                        _ => break,
                    },
                    _ = poll.tick() => dirty = true,
                    _ = tick.tick() => {},
                }
                if dirty {
                    dirty = false;
                    let _ = refresh_connections(&runtime).await;
                }
            }
        }
        // Keep the fallback scan working even when the monitor cannot start.
        let _ = refresh_connections(&runtime).await;
        tokio::select! { _ = shutdown.changed() => return, _ = sleep(Duration::from_secs(5)) => {} }
    }
}

async fn discover(
    inventory: watch::Sender<Option<Vec<Source>>>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        let mut command = Command::new("pactl");
        let child = command
            .arg("subscribe")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut child) = child {
            let mut lines =
                BufReader::new(child.stdout.take().expect("piped subscription")).lines();
            let mut poll = interval(Duration::from_secs(5));
            poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = poll.tick() => {},
                    line = lines.next_line() => match line {
                        Ok(Some(line)) if line.contains("on source #") && (line.contains("'new'") || line.contains("'remove'")) => {},
                        Ok(Some(_)) => continue,
                        _ => break,
                    }
                }
                if let Ok(sources) = sources().await {
                    inventory.send_replace(Some(sources));
                }
            }
        }
        tokio::select! { _ = shutdown.changed() => return, _ = sleep(Duration::from_secs(1)) => {} }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Level {
    min: f32,
    max: f32,
    peak: [f32; 2],
    rms: [f32; 2],
    clipped: bool,
}

fn measure(bytes: &[u8]) -> Level {
    let mut level = Level {
        min: 0.0,
        max: 0.0,
        peak: [0.0; 2],
        rms: [0.0; 2],
        clipped: false,
    };
    let frames = bytes.len() / 4;
    for frame in bytes.chunks_exact(4) {
        for channel in 0..2 {
            let sample =
                i16::from_le_bytes([frame[channel * 2], frame[channel * 2 + 1]]) as f32 / 32768.0;
            level.min = level.min.min(sample);
            level.max = level.max.max(sample);
            level.peak[channel] = level.peak[channel].max(sample.abs());
            level.rms[channel] += sample * sample;
            level.clipped |= sample.abs() >= 32767.0 / 32768.0;
        }
    }
    for rms in &mut level.rms {
        *rms = (*rms / frames.max(1) as f32).sqrt();
    }
    level
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_sources_excludes_own_capture_and_tracks_external_links() {
        use serde_json::json;
        let mut graph = vec![
            json!({"id":1,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"mic","media.class":"Audio/Source"}}}),
            json!({"id":2,"type":"PipeWire:Interface:Node","info":{"props":{"application.process.id":std::process::id()}}}),
            json!({"id":3,"type":"PipeWire:Interface:Node","info":{"props":{"application.name":"External recorder"}}}),
            json!({"id":4,"type":"PipeWire:Interface:Link","info":{"output-node-id":1,"input-node-id":2}}),
        ];
        assert!(busy_sources(&graph).is_empty());
        graph.push(json!({"id":5,"type":"PipeWire:Interface:Link","info":{"output-node-id":1,"input-node-id":3}}));
        assert_eq!(busy_sources(&graph), vec!["mic"]);
        graph.pop();
        assert!(busy_sources(&graph).is_empty());
    }

    #[test]
    fn signaling_rejects_stale_identity_and_terminates_owned_process() {
        assert!(process_identity(0).is_none());
        assert!(process_identity(std::process::id()).is_none());
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let identity = process_identity(child.id()).unwrap();
        let stale = ProcessIdentity {
            start_ticks: identity.start_ticks + 1,
            ..identity.clone()
        };
        let rejected = signal_process(&stale, 15).is_err();
        let alive = child.try_wait().unwrap().is_none();
        let sent = signal_process(&identity, 15);
        if sent.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        assert!(rejected && alive);
        assert!(sent.is_ok());
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(15));
    }

    #[test]
    fn application_lookup_follows_links_and_deduplicates_channels_and_processes() {
        use serde_json::json;
        let graph = vec![
            json!({"id":1,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"mic"}}}),
            json!({"id":9,"type":"PipeWire:Interface:Client","info":{"props":{"application.name":"Native recorder","application.process.id":123,"application.process.binary":"recorder"}}}),
            json!({"id":2,"type":"PipeWire:Interface:Node","info":{"props":{"client.id":9}}}),
            json!({"id":3,"type":"PipeWire:Interface:Node","info":{"props":{"client.id":9}}}),
            json!({"id":4,"type":"PipeWire:Interface:Node","info":{"props":{"application.name":"Unconnected app"}}}),
            json!({"id":10,"type":"PipeWire:Interface:Link","info":{"output-node-id":1,"input-node-id":2}}),
            json!({"id":11,"type":"PipeWire:Interface:Link","info":{"output-node-id":1,"input-node-id":2}}),
            json!({"id":12,"type":"PipeWire:Interface:Link","info":{"output-node-id":1,"input-node-id":3}}),
        ];
        let result = connected_applications(&graph, "mic");
        assert!(result.available);
        assert_eq!(result.applications.len(), 1);
        let app = &result.applications[0];
        assert_eq!(app.name, "Native recorder");
        assert_eq!(app.pid.as_deref(), Some("123"));
        assert_eq!(app.binary.as_deref(), Some("recorder"));
        assert_eq!(app.streams, 2);
        let missing = connected_applications(&graph, "missing");
        assert!(!missing.available);
        assert!(missing.applications.is_empty());
        let idle = connected_applications(&graph[..5], "mic");
        assert!(idle.available);
        assert!(idle.applications.is_empty());
    }

    #[test]
    fn new_inputs_are_reserved_and_release_survives_reconnection() {
        let source = Source {
            id: "mic".into(),
            name: "Microphone".into(),
            muted: false,
        };
        let initial = reservation_inventory(&BTreeMap::new(), std::slice::from_ref(&source), &[]);
        assert_eq!(initial["mic"].status, ReservationStatus::Pending);
        let released = vec!["mic".to_owned()];
        let shared = reservation_inventory(&initial, std::slice::from_ref(&source), &released);
        assert_eq!(shared["mic"].status, ReservationStatus::Released);
        let absent = reservation_inventory(&shared, &[], &released);
        assert_eq!(absent["mic"].status, ReservationStatus::Disconnected);
        let returned = reservation_inventory(&absent, std::slice::from_ref(&source), &released);
        assert_eq!(returned["mic"].status, ReservationStatus::Released);
        let next_boot = reservation_inventory(&BTreeMap::new(), &[source], &[]);
        assert_eq!(next_boot["mic"].status, ReservationStatus::Pending);
    }

    #[test]
    fn inventory_does_not_claim_release_before_the_holder_stops() {
        let source = Source {
            id: "mic".into(),
            name: "Microphone".into(),
            muted: false,
        };
        let current = BTreeMap::from([(
            "mic".into(),
            Reservation {
                status: ReservationStatus::Held,
                error: None,
            },
        )]);
        let next = reservation_inventory(&current, &[source], &["mic".into()]);
        assert_eq!(next["mic"].status, ReservationStatus::Held);
    }

    #[test]
    fn output_is_silent_when_muted_missing_or_stale() {
        let silence = vec![0; BLOCK_BYTES];
        let mut frame = Frame {
            captured: Instant::now(),
            pcm: Arc::new(vec![7; BLOCK_BYTES]),
        };
        assert_eq!(output_pcm(Some(&frame), true, &silence), &*frame.pcm);
        assert_eq!(output_pcm(Some(&frame), false, &silence), silence);
        assert_eq!(output_pcm(None, true, &silence), silence);
        frame.captured = Instant::now() - Duration::from_secs(1);
        assert_eq!(output_pcm(Some(&frame), true, &silence), silence);
    }

    #[test]
    fn slow_output_drops_backlog_and_does_not_repeat_samples() {
        let (sender, mut receiver) = broadcast::channel(16);
        for value in 1..=10 {
            assert!(
                sender
                    .send(Packet::Audio(Frame {
                        captured: Instant::now(),
                        pcm: Arc::new(vec![value; BLOCK_BYTES])
                    }))
                    .is_ok()
            );
        }
        assert_eq!(next_frame(&mut receiver).unwrap().pcm[0], 9);
        assert_eq!(next_frame(&mut receiver).unwrap().pcm[0], 10);
        assert!(next_frame(&mut receiver).is_none());
    }

    #[test]
    fn viewers_share_a_single_capture_channel() {
        let hub = AudioHub::default();
        let first = hub.channel("mic");
        let second = hub.channel("mic");
        assert!(first.same_channel(&second));
        assert!(!first.same_channel(&hub.channel("other")));
    }

    #[test]
    fn measures_stereo_without_phase_cancellation() {
        let samples: [i16; 4] = [16384, -16384, 16384, -16384];
        let bytes: Vec<u8> = samples.into_iter().flat_map(i16::to_le_bytes).collect();
        let level = measure(&bytes);
        assert_eq!(level.peak, [0.5, 0.5]);
        assert_eq!(level.rms, [0.5, 0.5]);
        assert_eq!((level.min, level.max), (-0.5, 0.5));
        assert!(!level.clipped);
    }

    #[test]
    fn handles_silence_and_full_scale() {
        assert_eq!(measure(&[0; 16]).rms, [0.0, 0.0]);
        assert!(measure(&[0, 128, 255, 127]).clipped);
        assert_eq!(measure(&[]).rms, [0.0, 0.0]);
    }
}
