//! Bounded, process-isolated voice conversion. Final mute remains in audio output.
use crate::runtime::Runtime;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, timeout},
};

const BLOCK_BYTES: usize = 3840;
const CHUNK_BYTES: usize = BLOCK_BYTES * 8;
const MAX_AGE: Duration = Duration::from_millis(650);

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct VoiceState {
    pub enabled: bool,
    pub pitch: i32,
    pub ready: bool,
    pub inference_ms: Option<f32>,
    pub pipeline_ms: Option<u64>,
    pub dropped_chunks: u64,
    pub error: Option<String>,
}

struct Request {
    pcm: Vec<u8>,
    captured: Instant,
    generation: u64,
    pitch: i32,
    reset: bool,
}
struct Response {
    pcm: Vec<u8>,
    captured: Instant,
    generation: u64,
    inference_ms: f32,
}

pub struct Bridge {
    input: mpsc::Sender<Request>,
    output: mpsc::Receiver<Response>,
    task: JoinHandle<()>,
    pending: Vec<u8>,
    captured: Option<Instant>,
    playback: VecDeque<(Vec<u8>, Instant)>,
    generation: u64,
    reset_worker: bool,
    pub dropped: u64,
    pub inference_ms: Option<f32>,
    pub pipeline_ms: Option<u64>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Bridge {
    pub fn new(command: Vec<String>, runtime: Runtime) -> Self {
        let (input, requests) = mpsc::channel(1);
        let (responses, output) = mpsc::channel(1);
        let task = tokio::spawn(supervise(command, requests, responses, runtime));
        Self {
            input,
            output,
            task,
            pending: Vec::with_capacity(CHUNK_BYTES),
            captured: None,
            playback: VecDeque::new(),
            generation: 0,
            reset_worker: true,
            dropped: 0,
            inference_ms: None,
            pipeline_ms: None,
        }
    }

    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending.clear();
        self.captured = None;
        self.playback.clear();
        self.reset_worker = true;
        self.pipeline_ms = None;
    }

    pub fn process(&mut self, pcm: &[u8], captured: Instant, pitch: i32) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            self.captured = Some(captured);
        }
        self.pending.extend_from_slice(pcm);
        if self.pending.len() == CHUNK_BYTES {
            let request = Request {
                pcm: std::mem::replace(&mut self.pending, Vec::with_capacity(CHUNK_BYTES)),
                captured: self.captured.take().unwrap(),
                generation: self.generation,
                pitch,
                reset: self.reset_worker,
            };
            if self.input.try_send(request).is_err() {
                self.dropped += 1;
                self.reset_worker = true;
            } else {
                self.reset_worker = false;
            }
        }
        while let Ok(response) = self.output.try_recv() {
            if response.generation != self.generation || response.captured.elapsed() > MAX_AGE {
                self.dropped += 1;
                continue;
            }
            if self.playback.len() > 8 {
                self.playback.clear();
                self.dropped += 1;
            }
            self.inference_ms = Some(response.inference_ms);
            for (i, pcm) in response.pcm.chunks_exact(BLOCK_BYTES).enumerate() {
                self.playback.push_back((
                    pcm.to_vec(),
                    response.captured + Duration::from_millis(i as u64 * 20),
                ));
            }
        }
        while let Some((pcm, captured)) = self.playback.pop_front() {
            if captured.elapsed() <= MAX_AGE {
                self.pipeline_ms = Some(captured.elapsed().as_millis() as u64);
                return Some(pcm);
            }
            self.dropped += 1;
        }
        None
    }
}

async fn supervise(
    command: Vec<String>,
    mut requests: mpsc::Receiver<Request>,
    responses: mpsc::Sender<Response>,
    runtime: Runtime,
) {
    loop {
        let result = run(&command, &mut requests, &responses, &runtime).await;
        let error = result.err().map(|error| error.to_string());
        runtime
            .update(|s| {
                s.audio_voice.ready = false;
                s.audio_voice.error = error;
            })
            .await;
        if requests.is_closed() {
            return;
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn run(
    command: &[String],
    requests: &mut mpsc::Receiver<Request>,
    responses: &mpsc::Sender<Response>,
    runtime: &Runtime,
) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    let program = command.first().context("No voice worker configured")?;
    let mut child = Command::new(program)
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("Could not start voice conversion worker")?;
    let mut writer = child.stdin.take().unwrap();
    let mut reader = child.stdout.take().unwrap();
    let mut magic = [0; 4];
    timeout(Duration::from_secs(60), reader.read_exact(&mut magic))
        .await
        .context("Voice model startup timed out")??;
    if &magic != b"RVC1" {
        bail!("Invalid voice worker handshake");
    }
    runtime
        .update(|s| {
            s.audio_voice.ready = true;
            s.audio_voice.error = None;
        })
        .await;
    let mut reset = true;
    while let Some(request) = requests.recv().await {
        if request.captured.elapsed() > MAX_AGE {
            reset = true;
            continue;
        }
        let mut header = request.pitch.to_le_bytes().to_vec();
        header.extend_from_slice(&u32::from(reset || request.reset).to_le_bytes());
        let mut output = vec![0; CHUNK_BYTES + 4];
        timeout(Duration::from_secs(3), async {
            writer.write_all(&header).await?;
            writer.write_all(&request.pcm).await?;
            reader.read_exact(&mut output).await?;
            Ok::<_, std::io::Error>(())
        })
        .await
        .context("Voice inference timed out")??;
        reset = false;
        let inference_ms = f32::from_le_bytes(output[..4].try_into().unwrap());
        if !inference_ms.is_finite() || inference_ms < 0.0 {
            bail!("Invalid voice timing response");
        }
        let response = Response {
            pcm: output[4..].to_vec(),
            captured: request.captured,
            generation: request.generation,
            inference_ms,
        };
        if responses.try_send(response).is_err() {
            reset = true;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reset_discards_inflight_results_and_stale_audio() {
        let (input, _requests) = mpsc::channel(1);
        let (responses, output) = mpsc::channel(1);
        let mut bridge = Bridge {
            input,
            output,
            task: tokio::spawn(std::future::pending()),
            pending: vec![],
            captured: None,
            playback: VecDeque::new(),
            generation: 0,
            reset_worker: false,
            dropped: 0,
            inference_ms: None,
            pipeline_ms: None,
        };
        responses
            .send(Response {
                pcm: vec![1; CHUNK_BYTES],
                captured: Instant::now(),
                generation: 0,
                inference_ms: 1.0,
            })
            .await
            .unwrap();
        bridge.reset();
        assert!(
            bridge
                .process(&[0; BLOCK_BYTES], Instant::now(), 0)
                .is_none()
        );
        responses
            .send(Response {
                pcm: vec![1; CHUNK_BYTES],
                captured: Instant::now() - Duration::from_secs(1),
                generation: 1,
                inference_ms: 1.0,
            })
            .await
            .unwrap();
        assert!(
            bridge
                .process(&[0; BLOCK_BYTES], Instant::now(), 0)
                .is_none()
        );
        assert_eq!(bridge.dropped, 2);
        responses
            .send(Response {
                pcm: vec![2; CHUNK_BYTES],
                captured: Instant::now(),
                generation: 1,
                inference_ms: 1.0,
            })
            .await
            .unwrap();
        assert_eq!(
            bridge
                .process(&[0; BLOCK_BYTES], Instant::now(), 0)
                .unwrap(),
            vec![2; BLOCK_BYTES]
        );
        bridge.reset();
        assert!(
            bridge
                .process(&[0; BLOCK_BYTES], Instant::now(), 0)
                .is_none()
        );
    }
}
