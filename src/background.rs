//! Shader packages are content; the supervised producer is a replaceable engine.
//! The core consumes complete latest frames without waiting on the producer.
use crate::{
    effects::VideoEffects,
    model::{AvatarEngine, BackgroundEffect, VideoOutputMode},
    runtime::Runtime,
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::watch,
};

pub fn default_plugin() -> String {
    "kelp".into()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub version: u32,
    pub id: String,
    pub name: String,
    pub shader: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub parameters: BTreeMap<String, serde_json::Value>,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn directory() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("tarsier/backgrounds")
}
fn validate(package: &Package) -> Result<()> {
    if package.version != 1
        || !valid_id(&package.id)
        || package.name.is_empty()
        || package.name.len() > 100
        || !(16..=1280).contains(&package.width)
        || !(16..=720).contains(&package.height)
        || !(1..=30).contains(&package.fps)
        || package.parameters.len() > 32
    {
        bail!("invalid background package metadata");
    }
    for (name, value) in &package.parameters {
        if name.is_empty()
            || name.len() > 64
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || matches!(name.as_str(), "time" | "resolution")
            || !value.is_number()
        {
            bail!("invalid shader parameter");
        }
    }
    Ok(())
}

pub fn load(id: &str) -> Result<(Package, String)> {
    if !valid_id(id) {
        bail!("invalid background plugin id");
    }
    let (manifest, source) = if id == "kelp" {
        (
            include_str!("../assets/backgrounds/kelp/manifest.json").to_owned(),
            include_str!("../assets/backgrounds/kelp/kelp.frag").to_owned(),
        )
    } else {
        let base = directory()
            .join(id)
            .canonicalize()
            .context("background plugin not found")?;
        let manifest_path = base.join("manifest.json");
        let manifest = read_bounded(&manifest_path, 16 * 1024)?;
        let package: Package = serde_json::from_str(&manifest)?;
        let shader = base.join(&package.shader).canonicalize()?;
        if !shader.starts_with(&base) {
            bail!("shader must remain inside its package");
        }
        (manifest, read_bounded(&shader, 128 * 1024)?)
    };
    let package: Package = serde_json::from_str(&manifest)?;
    validate(&package)?;
    if package.id != id {
        bail!("background plugin directory and id differ");
    }
    Ok((package, source))
}
fn read_bounded(path: &std::path::Path, limit: usize) -> Result<String> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let mut bytes = Vec::new();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        bail!("background package entries must be regular files");
    }
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("background package file too large");
    }
    Ok(String::from_utf8(bytes)?)
}
pub fn catalog() -> Vec<Package> {
    let mut packages = vec![load("kelp").expect("bundled background is valid").0];
    if let Ok(entries) = std::fs::read_dir(directory()) {
        for entry in entries.flatten().take(128) {
            if let Some(id) = entry.file_name().to_str() {
                if id != "kelp" {
                    if let Ok((package, _)) = load(id) {
                        packages.push(package);
                    }
                }
            }
        }
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages
}

#[derive(Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub pixels: Vec<u8>,
}
#[derive(Clone)]
pub struct Backgrounds(Arc<Mutex<Selection>>);
struct Selection {
    id: String,
    generation: u64,
    frame: Option<Arc<Frame>>,
}
impl Default for Backgrounds {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Selection {
            id: default_plugin(),
            generation: 0,
            frame: None,
        })))
    }
}
impl Backgrounds {
    pub fn selection(&self) -> (String, u64) {
        let s = self.0.lock().unwrap();
        (s.id.clone(), s.generation)
    }
    pub fn select(&self, id: &str) {
        let mut s = self.0.lock().unwrap();
        if s.id != id {
            s.id = id.into();
            s.generation += 1;
            s.frame = None;
        }
    }
    pub fn invalidate(&self) {
        let mut s = self.0.lock().unwrap();
        s.generation += 1;
        s.frame = None;
    }
    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.0.lock().unwrap().frame.clone()
    }
    pub(crate) fn publish(&self, generation: u64, frame: Frame) {
        let mut s = self.0.lock().unwrap();
        if s.generation == generation {
            s.frame = Some(Arc::new(frame));
        }
    }
}

struct Reader {
    file: File,
    width: usize,
    height: usize,
    stride: usize,
    sequence: u64,
}
impl Reader {
    fn read(&mut self) -> Result<Option<Frame>> {
        use std::os::unix::fs::FileExt as _;
        if let Err(error) = FileExt::try_lock_shared(&self.file) {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
        let result = (|| {
            let mut header = [0; 64];
            self.file.read_exact_at(&mut header, 0)?;
            if header == [0; 64] {
                return Ok(None);
            }
            let u32_at = |i| u32::from_le_bytes(header[i..i + 4].try_into().unwrap()) as usize;
            let u64_at = |i| u64::from_le_bytes(header[i..i + 8].try_into().unwrap());
            let size = self.stride * self.height;
            if &header[..8] != b"TARSFRM1"
                || u32_at(8) != self.width
                || u32_at(12) != self.height
                || u32_at(16) != self.stride
                || u32_at(20) != 0
                || u64_at(48) != size as u64
            {
                bail!("invalid shared background frame header");
            }
            let sequence = u64_at(24);
            if sequence <= self.sequence {
                return Ok(None);
            }
            let mut pixels = vec![0; size];
            self.file.read_exact_at(&mut pixels, 64)?;
            self.sequence = sequence;
            Ok(Some(Frame {
                width: self.width,
                height: self.height,
                stride: self.stride,
                pixels,
            }))
        })();
        FileExt::unlock(&self.file)?;
        result
    }
}
struct Worker {
    child: Child,
    reader: Reader,
    generation: u64,
    last_frame: Instant,
}
impl Worker {
    async fn start(id: &str, generation: u64) -> Result<(Self, String)> {
        let (package, source) = load(id)?;
        let python = std::env::var_os("TARSIER_BACKGROUND_PYTHON")
            .unwrap_or_else(|| "worker/.venv/bin/python".into());
        let mut command = Command::new(python);
        crate::perception::close_inherited_file_descriptors(&mut command);
        let mut child = command
            .args(["-u", "-c", include_str!("../tools/background_worker.py")])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("start shader worker")?;
        let mut request = serde_json::to_value(&package)?;
        request["source"] = source.into();
        let mut payload = serde_json::to_vec(&request)?;
        payload.push(b'\n');
        let mut stdin = child.stdin.take().unwrap();
        tokio::time::timeout(Duration::from_secs(10), stdin.write_all(&payload)).await??;
        drop(stdin);
        let mut line = String::new();
        let stdout = child.stdout.take().unwrap();
        tokio::time::timeout(
            Duration::from_secs(15),
            BufReader::new(stdout.take(4096)).read_line(&mut line),
        )
        .await??;
        #[derive(Deserialize)]
        struct Hello {
            pid: u32,
            fd: u32,
            renderer: String,
        }
        let hello: Hello = serde_json::from_str(&line).context("shader worker handshake")?;
        if Some(hello.pid) != child.id() {
            bail!("shader worker PID mismatch");
        }
        let file = File::open(format!("/proc/{}/fd/{}", hello.pid, hello.fd))?;
        let width = package.width as usize;
        let height = package.height as usize;
        let stride = (width * 3 + 3) & !3;
        if file.metadata()?.len() != (64 + stride * height) as u64 {
            bail!("unexpected background memory size");
        }
        tracing::info!(
            plugin = id,
            pid = hello.pid,
            renderer = hello.renderer,
            "background shader worker started"
        );
        Ok((
            Self {
                child,
                reader: Reader {
                    file,
                    width,
                    height,
                    stride,
                    sequence: 0,
                },
                generation,
                last_frame: Instant::now(),
            },
            hello.renderer,
        ))
    }
    async fn stop(mut self) {
        let _ = self.child.kill().await;
    }
}

pub fn start(
    effects: VideoEffects,
    runtime: Runtime,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut worker: Option<Worker> = None;
        let mut retry = Instant::now();
        let mut previous_generation = None;
        let mut interval = tokio::time::interval(Duration::from_millis(33));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = interval.tick() => {}, _ = shutdown.changed() => break }
            if *shutdown.borrow() {
                break;
            }
            let snapshot = runtime.state().await;
            let needed = snapshot.pipeline.running
                && effects.background_enabled()
                && effects.background_effect() == BackgroundEffect::Shader
                && (effects.output_mode() == VideoOutputMode::Camera
                    || (effects.output_mode() == VideoOutputMode::ComicAvatar
                        && snapshot.video_effects.avatar_engine == Some(AvatarEngine::Portrait3d)));
            let (id, generation) = effects.backgrounds.selection();
            if previous_generation != Some(generation) {
                retry = Instant::now();
                previous_generation = Some(generation);
            }
            if worker
                .as_ref()
                .is_some_and(|w| !needed || w.generation != generation)
            {
                worker.take().unwrap().stop().await;
                retry = Instant::now();
                runtime
                    .update(|s| {
                        s.video_effects.background_worker_pid = None;
                        s.video_effects.background_ready = false;
                    })
                    .await;
            }
            if !needed {
                continue;
            }
            if worker.is_none() && Instant::now() >= retry {
                // Cancelling startup drops the child with kill-on-drop, including
                // a compiler that has not reached the IPC handshake yet.
                let changed = async {
                    loop {
                        tokio::time::sleep(Duration::from_millis(33)).await;
                        if effects.backgrounds.selection().1 != generation
                            || !effects.background_enabled()
                            || !runtime.state().await.pipeline.running
                        {
                            break;
                        }
                    }
                };
                let result = tokio::select! {
                    result = Worker::start(&id, generation) => Some(result),
                    _ = shutdown.changed() => break,
                    _ = changed => None,
                };
                let Some(result) = result else {
                    continue;
                };
                match result {
                    Ok((new, renderer)) => {
                        let pid = new.child.id();
                        worker = Some(new);
                        runtime
                            .update(|s| {
                                s.video_effects.background_worker_pid = pid;
                                s.video_effects.background_renderer = Some(renderer);
                                s.video_effects.background_error = None;
                            })
                            .await;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "background worker failed to start");
                        runtime
                            .update(|s| {
                                s.video_effects.background_error = Some(error.to_string());
                            })
                            .await;
                        retry = Instant::now() + Duration::from_secs(5);
                    }
                }
            }
            if let Some(active) = worker.as_mut() {
                let result = active.reader.read();
                let invalid = result.is_err();
                let exited = active.child.try_wait().ok().flatten().is_some();
                match result {
                    Ok(Some(frame)) => {
                        effects.backgrounds.publish(active.generation, frame);
                        active.last_frame = Instant::now();
                        if !snapshot.video_effects.background_ready {
                            runtime
                                .update(|s| {
                                    if effects.backgrounds.selection().1 == active.generation {
                                        s.video_effects.background_ready = true;
                                    }
                                })
                                .await;
                        }
                    }
                    Err(ref error) => {
                        tracing::warn!(%error, "background frame rejected");
                    }
                    _ => {}
                }
                if exited || invalid || active.last_frame.elapsed() > Duration::from_secs(5) {
                    worker.take().unwrap().stop().await;
                    runtime.update(|s| { s.video_effects.background_worker_pid = None; s.video_effects.background_error = Some("Background renderer stopped; retaining the last complete frame and retrying".into()); }).await;
                    retry = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        if let Some(worker) = worker {
            worker.stop().await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packages_and_generation_are_bounded() {
        let (package, source) = load("kelp").unwrap();
        assert!(source.starts_with("#version 330"));
        assert_eq!(package.id, "kelp");
        assert!(load("../escape").is_err());
        let mut invalid = package;
        invalid.width = u32::MAX;
        assert!(validate(&invalid).is_err());
        let store = Backgrounds::default();
        let generation = store.selection().1;
        store.invalidate();
        store.publish(
            generation,
            Frame {
                width: 1,
                height: 1,
                stride: 3,
                pixels: vec![1, 2, 3],
            },
        );
        assert!(store.latest().is_none());
    }
    #[test]
    fn reader_skips_contention_and_rejects_wrong_dimensions() {
        let producer = crate::shared_frames::SharedFrames::new(4, 2).unwrap();
        let path = producer
            .source()
            .strip_prefix("shm://")
            .unwrap()
            .split('#')
            .next()
            .unwrap();
        let mut reader = Reader {
            file: File::open(path).unwrap(),
            width: 4,
            height: 2,
            stride: 12,
            sequence: 0,
        };
        let bytes: Vec<_> = (0..24).collect();
        producer.publish(&bytes, 3, 123, 0).unwrap();
        assert_eq!(reader.read().unwrap().unwrap().pixels, bytes);
        assert!(reader.read().unwrap().is_none());
        let lock = File::open(path).unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        assert!(reader.read().unwrap().is_none());
        FileExt::unlock(&lock).unwrap();
        reader.width = 3;
        assert!(reader.read().is_err());
    }
    #[tokio::test]
    #[ignore = "requires the local OpenGL/EGL worker environment"]
    async fn gpu_worker_publishes_animated_frames_and_stops() {
        let (mut worker, renderer) = Worker::start("kelp", 1).await.unwrap();
        eprintln!("renderer: {renderer}");
        let mut frames = Vec::new();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            if let Some(frame) = worker.reader.read().unwrap() {
                frames.push(frame);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(frames.len() >= 2, "worker did not animate");
        assert_ne!(
            frames.first().unwrap().pixels,
            frames.last().unwrap().pixels
        );
        eprintln!(
            "{} frames in {:.2}s",
            frames.len(),
            started.elapsed().as_secs_f32()
        );
        if let Ok(path) = std::env::var("TARSIER_BACKGROUND_CAPTURE_PATH") {
            let frame = frames.last().unwrap();
            let rgb: Vec<u8> = frame
                .pixels
                .chunks_exact(3)
                .flat_map(|bgr| [bgr[2], bgr[1], bgr[0]])
                .collect();
            image::save_buffer(
                path,
                &rgb,
                frame.width as u32,
                frame.height as u32,
                image::ColorType::Rgb8,
            )
            .unwrap();
        }
        let pid = worker.child.id().unwrap();
        worker.stop().await;
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
    #[tokio::test]
    #[ignore = "requires the local OpenGL/EGL worker environment"]
    async fn gpu_supervisor_restarts_and_releases_inactive_worker() {
        async fn wait_for(
            runtime: &Runtime,
            predicate: impl Fn(&crate::model::RuntimeState) -> bool,
        ) -> crate::model::RuntimeState {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let state = runtime.state().await;
                    if predicate(&state) {
                        return state;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("background supervisor transition timed out")
        }
        let effects = VideoEffects::new();
        effects.set_background(true, BackgroundEffect::Shader);
        let runtime = Runtime::new();
        runtime.update(|state| state.pipeline.running = true).await;
        let (shutdown, receiver) = watch::channel(false);
        let task = start(effects.clone(), runtime.clone(), receiver);
        let state = wait_for(&runtime, |s| s.video_effects.background_ready).await;
        let pid = state.video_effects.background_worker_pid.unwrap();
        assert!(effects.backgrounds.latest().is_some());
        // Kill only the producer created by this test, then require a new producer.
        unsafe {
            nix::libc::kill(pid as i32, nix::libc::SIGKILL);
        }
        let state = wait_for(&runtime, |s| {
            s.video_effects
                .background_worker_pid
                .is_some_and(|p| p != pid)
        })
        .await;
        assert!(
            effects.backgrounds.latest().is_some(),
            "crash discarded the held background"
        );
        let second_pid = state.video_effects.background_worker_pid.unwrap();
        runtime.update(|s| s.pipeline.running = false).await;
        wait_for(&runtime, |s| {
            s.video_effects.background_worker_pid.is_none()
        })
        .await;
        assert!(!std::path::Path::new(&format!("/proc/{second_pid}")).exists());
        runtime.update(|s| s.pipeline.running = true).await;
        wait_for(&runtime, |s| s.video_effects.background_ready).await;
        effects.set_background(false, BackgroundEffect::Shader);
        wait_for(&runtime, |s| {
            s.video_effects.background_worker_pid.is_none()
        })
        .await;
        shutdown.send(true).unwrap();
        task.await.unwrap();
    }
}
