//! Stable API audio device for satellites; physical routing stays inside Tarsier.
use crate::{
    audio::{AudioHub, Frame, Packet, next_frame, output_pcm},
    runtime::Runtime,
};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval},
};

pub const DEVICE: &str = "tarsier_satellites";
const BLOCK_BYTES: usize = 960 * 4;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub source: Option<String>,
    pub muted: bool,
}

pub async fn route(hub: AudioHub, runtime: Runtime, mut shutdown: watch::Receiver<bool>) {
    let output = hub.channel(DEVICE);
    let silence = vec![0; BLOCK_BYTES];
    let mut selected = None;
    let mut input = None;
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = tick.tick() => {}
        }
        let state = runtime.state().await;
        let settings = state.audio_satellite;
        let source = settings.source.filter(|id| id != DEVICE);
        if selected != source {
            selected = source;
            input = selected.as_ref().map(|id| hub.subscribe_raw(id));
        }
        // Drain while muted/disabled so unmuting never replays buffered input.
        let frame = input.as_mut().and_then(next_frame);
        if !settings.enabled {
            continue;
        }
        let allowed = !settings.muted
            && selected
                .as_ref()
                .is_some_and(|id| state.audio_capture_sources.contains(id));
        let pcm = output_pcm(frame.as_ref(), allowed, &silence).to_vec();
        // A fresh clock and silence keep the device alive through source switches,
        // disabled capture and unplugging. No physical source identity is exported.
        let _ = output.send(Packet::Audio(Frame {
            captured: Instant::now(),
            pcm: Arc::new(pcm),
        }));
    }
}
