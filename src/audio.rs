//! On-demand audio metering through the user's PipeWire/PulseAudio server.
use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::{io::AsyncReadExt, process::Command, sync::watch, time::timeout};

#[derive(Serialize)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub muted: bool,
}

pub async fn sources() -> Result<Vec<Source>> {
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

#[derive(Debug, Serialize)]
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

pub async fn stream(
    socket: WebSocket,
    source: String,
    mut shutdown: watch::Receiver<bool>,
    mut states: watch::Receiver<crate::model::RuntimeState>,
) {
    if !states.borrow().audio_capture_sources.contains(&source) {
        return;
    }
    let (mut sender, mut receiver) = socket.split();
    let mut child = match Command::new("parec")
        .args([
            "--raw",
            "--format=s16le",
            "--rate=48000",
            "--channels=2",
            "--latency-msec=50",
            "--client-name=Tarsier audio meter",
            "--property=node.dont-reconnect=true",
            "--device",
            &source,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            let _ = sender
                .send(Message::Text(
                    r#"{"error":"Audio capture unavailable: install parec"}"#.into(),
                ))
                .await;
            return;
        }
    };
    let mut stdout = child.stdout.take().expect("piped audio output");
    // Fifty milliseconds of stereo PCM. Only amplitude summaries leave the daemon.
    let mut buffer = vec![0; 2400 * 4];
    let mut filled = 0;
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            changed = states.changed() => {
                if changed.is_err() || !states.borrow_and_update().audio_capture_sources.contains(&source) {
                    break;
                }
            },
            incoming = receiver.next() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
            read = timeout(Duration::from_secs(3), stdout.read(&mut buffer[filled..])) => {
                if !matches!(read, Ok(Ok(count)) if count > 0) {
                    let _ = timeout(Duration::from_secs(1), sender.send(Message::Text(
                        r#"{"error":"Capture stopped: source disconnected or unavailable"}"#.into()))).await;
                    break;
                }
                filled += read.unwrap().unwrap();
                if filled < buffer.len() {
                    continue;
                }
                filled = 0;
                let message = Message::Text(serde_json::to_string(&measure(&buffer)).unwrap().into());
                if !matches!(timeout(Duration::from_secs(1), sender.send(message)).await, Ok(Ok(()))) {
                    break;
                }
            }
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

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
