//! A process-owned recording source mixing the published microphone and desktop audio.
use anyhow::{Context, Result, bail};
use gstreamer::{self as gst, prelude::*};
use serde::{Deserialize, Serialize};
use std::{io::Write, os::unix::fs::OpenOptionsExt, process::Stdio, time::Duration};
use tokio::{
    process::Command,
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};

pub const SOURCE: &str = "tarsier_screencast";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub microphone_volume: u8,
    pub system_volume: u8,
    pub microphone_muted: bool,
    pub system_muted: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            microphone_volume: 70,
            system_volume: 70,
            microphone_muted: false,
            system_muted: false,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.microphone_volume > 100 || self.system_volume > 100 {
            bail!("Screencast volumes must be between 0 and 100");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct State {
    pub settings: Settings,
    pub running: bool,
    pub monitor: Option<String>,
    pub error: Option<String>,
}

async fn pactl(args: &[&str]) -> Result<String> {
    let output = timeout(
        Duration::from_secs(3),
        Command::new("pactl").args(args).kill_on_drop(true).output(),
    )
    .await
    .context("Audio server timed out")??;
    if !output.status.success() {
        bail!("Audio server unavailable");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn select_monitor(sinks: &[serde_json::Value], default: &str) -> Result<String> {
    let sink = sinks
        .iter()
        .find(|s| s["name"].as_str() == Some(default))
        .context("Default playback device is unavailable")?;
    let monitor = sink["monitor_source"]
        .as_str()
        .context("Playback device has no monitor")?;
    if default.starts_with("tarsier_") || monitor == SOURCE {
        bail!("Select a physical playback device to avoid an audio feedback loop");
    }
    Ok(monitor.to_owned())
}
async fn default_monitor() -> Result<String> {
    let default = pactl(&["get-default-sink"]).await?;
    let sinks: Vec<serde_json::Value> =
        serde_json::from_str(&pactl(&["-f", "json", "list", "sinks"]).await?)?;
    select_monitor(&sinks, &default)
}

struct Mixer {
    pipeline: gst::Pipeline,
    output: gstreamer_app::AppSink,
    microphone: gst::Element,
    system: gst::Element,
}
impl Drop for Mixer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
impl Mixer {
    fn new(microphone: &str, monitor: &str, settings: &Settings) -> Result<Self> {
        gst::init()?;
        // Device identifiers are properties, never interpolated into a pipeline description.
        let pipeline = gst::parse::launch(
            "audiomixer name=mix latency=40000000 ! audioconvert ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved ! appsink name=output sync=false max-buffers=8 drop=true \
             pulsesrc name=mic do-timestamp=true buffer-time=60000 latency-time=20000 ! audioconvert ! audioresample ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! volume name=mic_volume ! mix. \
             pulsesrc name=desktop do-timestamp=true buffer-time=60000 latency-time=20000 ! audioconvert ! audioresample ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! volume name=system_volume ! mix."
        )?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("Invalid audio pipeline"))?;
        pipeline
            .by_name("mic")
            .unwrap()
            .set_property("device", microphone);
        pipeline
            .by_name("desktop")
            .unwrap()
            .set_property("device", monitor);
        for name in ["mic", "desktop"] {
            pipeline
                .by_name(name)
                .unwrap()
                .set_property("client-name", "Tarsier Screencast");
        }
        Self::from_pipeline(pipeline, settings)
    }
    fn from_pipeline(pipeline: gst::Pipeline, settings: &Settings) -> Result<Self> {
        let mixer = Self {
            output: pipeline.by_name("output").unwrap().downcast().unwrap(),
            microphone: pipeline.by_name("mic_volume").unwrap(),
            system: pipeline.by_name("system_volume").unwrap(),
            pipeline,
        };
        mixer.configure(settings);
        mixer.pipeline.set_state(gst::State::Playing)?;
        Ok(mixer)
    }
    fn configure(&self, settings: &Settings) {
        self.microphone
            .set_property("volume", f64::from(settings.microphone_volume) / 100.0);
        self.microphone
            .set_property("mute", settings.microphone_muted);
        self.system
            .set_property("volume", f64::from(settings.system_volume) / 100.0);
        self.system.set_property("mute", settings.system_muted);
    }
    fn check(&self) -> Result<()> {
        for message in self.pipeline.bus().unwrap().iter() {
            match message.view() {
                gst::MessageView::Error(error) => bail!("Audio mixer: {}", error.error()),
                gst::MessageView::Eos(_) => bail!("Audio mixer stopped"),
                _ => {}
            }
        }
        Ok(())
    }
}

async fn publish(runtime: &crate::runtime::Runtime, microphone: &str) -> Result<()> {
    let sources: Vec<serde_json::Value> =
        serde_json::from_str(&pactl(&["-f", "json", "list", "sources"]).await?)?;
    if sources.iter().any(|s| s["name"] == SOURCE) {
        bail!("Tarsier Screencast already exists");
    }
    let pipe = crate::audio::Pipe::new()?;
    let args = format!(
        "{{ tunnel.mode=source tunnel.may-pause=false pipe.filename={} audio.format=S16LE audio.rate=48000 audio.channels=2 audio.position=[FL FR] stream.props={{ node.name={SOURCE} node.description=\"Tarsier Screencast\" node.virtual=true priority.session=0 }} }}",
        serde_json::to_string(&pipe.path)?
    );
    let mut child = Command::new("pw-cli")
        .args(["-m", "load-module", "libpipewire-module-pipe-tunnel", &args])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut writer = timeout(Duration::from_secs(4), async {
        loop {
            if child.try_wait()?.is_some() {
                bail!("PipeWire screencast source stopped");
            }
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&pipe.path)
            {
                Ok(writer) => return Ok::<_, anyhow::Error>(writer),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
    .await
    .context("Screencast source startup timed out")??;
    // PipeWire creates the FIFO before the PulseAudio compatibility server exposes the source.
    timeout(Duration::from_secs(4), async {
        loop {
            let sources: Vec<serde_json::Value> =
                serde_json::from_str(&pactl(&["-f", "json", "list", "sources"]).await?)?;
            if sources.iter().any(|s| s["name"] == SOURCE) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("Screencast source did not appear in the audio server")??;
    let mut mixer: Option<Mixer> = None;
    let mut monitor = None;
    let mut tick = interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut discovery: std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>> =
        Box::pin(default_monitor());
    let mut last_audio = std::time::Instant::now();
    let mut pending = Vec::new();
    loop {
        tokio::select! {
            target = &mut discovery => {
                discovery = Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    default_monitor().await
                });
                let state = runtime.state().await;
                let target = if !state.audio_virtual.enabled || !state.audio_virtual.running {
                    Err(anyhow::anyhow!("Turn on Tarsier Microphone to mix voice and system audio"))
                } else { target };
                match target {
                    Ok(target) => {
                        if monitor.as_ref() != Some(&target) || mixer.is_none() {
                            drop(mixer.take());
                            pending.clear();
                            mixer = Some(Mixer::new(microphone, &target, &state.audio_screencast.settings)?);
                            monitor = Some(target);
                            last_audio = std::time::Instant::now();
                        }
                    }
                    Err(error) => {
                        mixer = None;
                        pending.clear();
                        monitor = None;
                        runtime.update(|s| {
                            s.audio_screencast.running = false;
                            s.audio_screencast.monitor = None;
                            s.audio_screencast.error = Some(error.to_string());
                        }).await;
                    }
                }
            }
            _ = tick.tick() => {
                if child.try_wait()?.is_some() { bail!("PipeWire screencast source stopped"); }
                if let Some(mixer) = &mixer {
                    let state = runtime.state().await;
                    mixer.configure(&state.audio_screencast.settings);
                    mixer.check()?;
                    if pending.is_empty() {
                        if let Some(sample) = mixer.output.try_pull_sample(gst::ClockTime::ZERO) {
                            let buffer = sample.buffer().context("Missing mixed audio")?;
                            let map = buffer.map_readable()?;
                            pending.extend_from_slice(map.as_slice());
                            last_audio = std::time::Instant::now();
                            if !state.audio_screencast.running || state.audio_screencast.monitor != monitor {
                                runtime.update(|s| {
                                    s.audio_screencast.running = true;
                                    s.audio_screencast.monitor = monitor.clone();
                                    s.audio_screencast.error = None;
                                }).await;
                            }
                        } else if last_audio.elapsed() > Duration::from_secs(3) { bail!("Audio mixer stopped delivering samples"); }
                    }
                } else if pending.is_empty() { pending.resize(1920, 0); }
                if !pending.is_empty() {
                    match writer.write(&pending) {
                        Ok(n) => { pending.drain(..n); }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
    }
}

pub async fn run(
    runtime: crate::runtime::Runtime,
    config: crate::config::AudioConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut states = runtime.subscribe_state();
    loop {
        if *shutdown.borrow() {
            break;
        }
        let enabled =
            states.borrow().audio_screencast.settings.enabled && config.virtual_output_enabled;
        if !enabled {
            tokio::select! { _ = shutdown.changed() => break, _ = states.changed() => {} }
            continue;
        }
        let result = tokio::select! {
            _ = shutdown.changed() => break,
            _ = async { loop {
                if states.changed().await.is_err() || !states.borrow().audio_screencast.settings.enabled { break; }
            }} => Ok(()),
            result = publish(&runtime, &config.virtual_source) => result,
        };
        let error = result.err().map(|e| e.to_string());
        runtime
            .update(|s| {
                s.audio_screencast.running = false;
                s.audio_screencast.monitor = None;
                s.audio_screencast.error = error;
            })
            .await;
        tokio::select! { _ = shutdown.changed() => break, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "Requires a live PipeWire server and TARSIER_TEST_MICROPHONE"]
    async fn live_source_publishes_mutes_and_cleans_up() {
        use tokio::io::AsyncReadExt;
        let microphone =
            std::env::var("TARSIER_TEST_MICROPHONE").expect("Explicit test microphone");
        let runtime = crate::runtime::Runtime::new();
        runtime
            .update(|s| {
                s.audio_virtual.enabled = true;
                s.audio_virtual.running = true;
                s.audio_screencast.settings.enabled = true;
            })
            .await;
        let (stop, shutdown) = watch::channel(false);
        let config = crate::config::AudioConfig {
            virtual_source: microphone,
            ..Default::default()
        };
        let task = tokio::spawn(run(runtime.clone(), config, shutdown));
        let result = async {
            timeout(Duration::from_secs(10), async {
                while !runtime.state().await.audio_screencast.running {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .context("Mixer did not become ready")?;
            let mut recorder = Command::new("parec")
                .args([
                    "--raw",
                    "--format=s16le",
                    "--rate=48000",
                    "--channels=2",
                    "--latency-msec=20",
                    "--device",
                    SOURCE,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()?;
            let mut samples = vec![0u8; 19200];
            let output = recorder.stdout.as_mut().unwrap();
            timeout(Duration::from_secs(5), output.read_exact(&mut samples)).await??;
            runtime
                .update(|s| {
                    s.audio_screencast.settings.microphone_muted = true;
                    s.audio_screencast.settings.system_muted = true;
                })
                .await;
            // Drain two seconds, including any audio queued before mute.
            for _ in 0..20 {
                timeout(Duration::from_secs(3), output.read_exact(&mut samples)).await??;
            }
            // Float-to-integer conversion may add one least-significant bit of dither.
            if samples
                .chunks_exact(2)
                .any(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs() > 1)
            {
                bail!(
                    "Muted output was not silent (peak {})",
                    samples
                        .chunks_exact(2)
                        .map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs())
                        .max()
                        .unwrap_or(0)
                );
            }
            recorder.kill().await?;
            runtime
                .update(|s| s.audio_screencast.settings.enabled = false)
                .await;
            timeout(Duration::from_secs(5), async {
                loop {
                    let sources = pactl(&["list", "short", "sources"]).await?;
                    if !sources.contains(SOURCE) {
                        break Ok::<_, anyhow::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await??;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = stop.send(true);
        task.await.unwrap();
        assert!(
            result.is_ok(),
            "{result:?}; state: {:?}",
            runtime.state().await.audio_screencast
        );
    }

    #[test]
    fn monitor_selection_follows_default_and_rejects_feedback() {
        let sinks = serde_json::json!([
            {"name":"speakers", "monitor_source":"speakers.monitor"},
            {"name":"headphones", "monitor_source":"headphones.monitor"},
            {"name":"tarsier_loop", "monitor_source":"tarsier_loop.monitor"}
        ]);
        let sinks = sinks.as_array().unwrap();
        assert_eq!(
            select_monitor(sinks, "headphones").unwrap(),
            "headphones.monitor"
        );
        assert!(select_monitor(sinks, "missing").is_err());
        assert!(select_monitor(sinks, "tarsier_loop").is_err());
    }
    #[test]
    fn mixer_combines_channels_mutes_and_saturates_without_wrapping() {
        gst::init().unwrap();
        for (mic_muted, system_muted, expected) in [
            (false, false, 32767i16),
            (true, false, 18350),
            (false, true, 18350),
            (true, true, 0),
        ] {
            let pipeline = gst::parse::launch(
                "audiomixer name=mix ! audioconvert dithering=0 ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! appsink name=output sync=false \
                 appsrc name=mic format=time ! volume name=mic_volume ! mix. \
                 appsrc name=desktop format=time ! volume name=system_volume ! mix."
            ).unwrap().downcast::<gst::Pipeline>().unwrap();
            let mixer = Mixer::from_pipeline(pipeline, &Settings::default()).unwrap();
            mixer.configure(&Settings {
                microphone_muted: mic_muted,
                system_muted,
                ..Default::default()
            });
            for name in ["mic", "desktop"] {
                let source = mixer
                    .pipeline
                    .by_name(name)
                    .unwrap()
                    .downcast::<gstreamer_app::AppSrc>()
                    .unwrap();
                source.set_caps(Some(
                    &gst::Caps::builder("audio/x-raw")
                        .field("format", "F32LE")
                        .field("rate", 48000i32)
                        .field("channels", 2i32)
                        .field("layout", "interleaved")
                        .build(),
                ));
                let samples: Vec<u8> = (0..1920).flat_map(|_| 0.8f32.to_le_bytes()).collect();
                let mut buffer = gst::Buffer::from_mut_slice(samples);
                buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
                buffer
                    .get_mut()
                    .unwrap()
                    .set_duration(gst::ClockTime::from_mseconds(20));
                source.push_buffer(buffer).unwrap();
                source.end_of_stream().unwrap();
            }
            let sample = mixer
                .output
                .try_pull_sample(gst::ClockTime::from_seconds(3))
                .expect("Mixed samples");
            let bytes = sample.buffer().unwrap().map_readable().unwrap();
            assert!(!bytes.is_empty());
            for sample in bytes.chunks_exact(2) {
                let value = i16::from_le_bytes([sample[0], sample[1]]);
                assert!(
                    (i32::from(value) - i32::from(expected)).abs() <= 1,
                    "{value} != {expected}"
                );
            }
        }
    }

    #[test]
    fn settings_have_headroom_and_bounded_volumes() {
        let mut settings = Settings::default();
        assert!(!settings.enabled);
        assert!(settings.validate().is_ok());
        settings.system_volume = 101;
        assert!(settings.validate().is_err());
        assert!(!crate::config::AudioConfig::default().allows(SOURCE));
    }
}
