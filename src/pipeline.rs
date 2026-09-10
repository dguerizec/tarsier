use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use serde_json::json;
use tokio::sync::watch;

use crate::{
    config::{VideoConfig, VideoSource},
    effects::VideoEffects,
    media_metadata::CapturedImage,
    model::unix_ms,
    runtime::Runtime,
};

#[derive(Clone)]
pub struct PreviewHub {
    output_tx: watch::Sender<Option<Bytes>>,
    perception_tx: watch::Sender<Option<PerceptionFrame>>,
    photo_tx: watch::Sender<Option<CapturedImage>>,
    snapshot_tx: watch::Sender<Option<CapturedImage>>,
    effects: VideoEffects,
    virtual_frame: Arc<Mutex<Option<(Instant, gst::Buffer)>>>,
    recording_tx: watch::Sender<Option<gst::Buffer>>,
    output_muted: Arc<AtomicBool>,
    replacement: Arc<Mutex<Replacement>>,
}

#[derive(Default)]
struct Replacement {
    info: ReplacementInfo,
    media: Option<crate::mute_media::Media>,
}

#[derive(Clone, Default, serde::Serialize)]
pub struct ReplacementInfo {
    pub selection: Option<crate::mute_media::Selection>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct PerceptionFrame {
    pub bytes: Bytes,
    pub frame_id: u64,
    pub captured_at_ms: u64,
}

struct CaptureClock {
    started_at_ms: AtomicU64,
    fps: u32,
    first_pts_ns: Mutex<Option<u64>>,
}

impl CaptureClock {
    fn new(started_at_ms: u64, fps: u32) -> Self {
        Self {
            started_at_ms: AtomicU64::new(started_at_ms),
            fps,
            first_pts_ns: Mutex::new(None),
        }
    }

    fn captured_at_ms(&self, pts_ns: u64) -> u64 {
        self.started_at_ms.load(Ordering::Relaxed).saturating_add(pts_ns / 1_000_000)
    }

    fn frame_id(&self, pts_ns: u64) -> u64 {
        let first_pts_ns = *self.first_pts_ns.lock().unwrap().get_or_insert(pts_ns);
        let elapsed_ns = pts_ns.saturating_sub(first_pts_ns) as u128;
        let rounded_frames = (elapsed_ns * self.fps as u128 + 500_000_000) / 1_000_000_000;
        u64::try_from(rounded_frames)
            .unwrap_or(u64::MAX - 1)
            .saturating_add(1)
    }
}

impl PreviewHub {
    pub fn new() -> Self {
        let (output_tx, _) = watch::channel(None);
        let (perception_tx, _) = watch::channel(None);
        Self {
            photo_tx: watch::channel(None).0,
            snapshot_tx: watch::channel(None).0,
            output_tx,
            perception_tx,
            effects: VideoEffects::new(),
            virtual_frame: Arc::default(),
            recording_tx: watch::channel(None).0,
            output_muted: Arc::new(AtomicBool::new(false)),
            replacement: Arc::default(),
        }
    }

    pub fn set_replacement(
        &self,
        selection: Option<crate::mute_media::Selection>,
        media: Option<crate::mute_media::Media>,
        error: Option<String>,
    ) {
        *self.replacement.lock().unwrap() = Replacement {
            info: ReplacementInfo { selection, error },
            media,
        };
    }

    pub fn replacement_info(&self) -> ReplacementInfo {
        self.replacement.lock().unwrap().info.clone()
    }

    fn replacement_frame(&self, active: bool) -> Option<gst::Buffer> {
        let mut replacement = self.replacement.lock().unwrap();
        let result = replacement.media.as_mut()?.frame(active);
        match result {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!(%error, "mute media failed; keeping black output fallback");
                replacement.info.error = Some(error.to_string());
                replacement.media = None;
                None
            }
        }
    }

    pub fn set_output_muted(&self, muted: bool) {
        self.output_muted.store(muted, Ordering::Relaxed);
    }

    pub fn output_muted(&self) -> bool {
        self.output_muted.load(Ordering::Relaxed)
    }

    pub(crate) fn subscribe_recording(&self) -> watch::Receiver<Option<gst::Buffer>> {
        self.recording_tx.subscribe()
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Bytes>> {
        self.output_tx.subscribe()
    }

    pub(crate) fn subscribe_perception(&self) -> watch::Receiver<Option<PerceptionFrame>> {
        self.perception_tx.subscribe()
    }

    pub(crate) fn latest(&self) -> Option<CapturedImage> {
        self.snapshot_tx.borrow().clone()
    }

    pub(crate) fn latest_photo(&self) -> Option<CapturedImage> {
        self.photo_tx
            .borrow()
            .clone()
            .filter(|frame| unix_ms().saturating_sub(frame.frame.captured_at_ms) <= 1000)
    }

    pub(crate) fn publish_photo(&self, frame: CapturedImage) {
        self.photo_tx.send_replace(Some(frame));
    }

    pub fn effects(&self) -> &VideoEffects {
        &self.effects
    }

    fn publish_output(&self, frame: CapturedImage) {
        self.output_tx.send_replace(Some(frame.frame.bytes.clone()));
        self.snapshot_tx.send_replace(Some(frame));
    }

    fn publish_perception(&self, frame: PerceptionFrame) {
        self.perception_tx.send_replace(Some(frame));
    }

    fn clear(&self) {
        *self.virtual_frame.lock().unwrap() = None;
        self.photo_tx.send_replace(None);
        self.snapshot_tx.send_replace(None);
        self.output_tx.send_replace(None);
        self.perception_tx.send_replace(None);
        self.effects.clear_mask();
        self.effects.clear_avatar();
        self.effects.clear_depth();
    }
}

pub struct VideoPipeline {
    config: Arc<Mutex<VideoConfig>>,
    source_changed_at_ms: Arc<AtomicU64>,
    supervisor_running: Arc<AtomicBool>,
    desired_running: Arc<AtomicBool>,
    desired_reserved: Arc<AtomicBool>,
    runtime: Runtime,
    supervisor: Option<JoinHandle<()>>,
    _virtual_output: Option<VirtualVideoOutput>,
}

#[derive(Clone)]
pub struct VideoPipelineControl {
    config: Arc<Mutex<VideoConfig>>,
    source_changed_at_ms: Arc<AtomicU64>,
    desired_running: Arc<AtomicBool>,
    desired_reserved: Arc<AtomicBool>,
    runtime: Runtime,
    #[cfg(test)]
    immediate: bool,
}

struct ActivePipeline {
    pipeline: gst::Pipeline,
    running: Arc<AtomicBool>,
    frame_count: Arc<AtomicU64>,
    last_frame_at_ms: Arc<AtomicU64>,
    capture_clock: Arc<CaptureClock>,
}

// Own the virtual device independently of the physical capture pipeline.
struct VirtualVideoOutput {
    pipeline: gst::Pipeline,
    running: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
}

impl VirtualVideoOutput {
    fn start(
        config: &VideoConfig,
        preview: PreviewHub,
        capture_enabled: Arc<AtomicBool>,
        runtime: Runtime,
        sink: &str,
    ) -> Result<Self> {
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=frames is-live=true format=time block=false max-buffers=2 leaky-type=downstream ! \
             video/x-raw,format=BGRx,width={},height={},framerate={}/1,interlace-mode=progressive ! videoconvert ! \
             video/x-raw,format=YUY2 ! {sink}",
            config.width, config.height, config.fps
        ))?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("invalid virtual video pipeline"))?;
        let source = pipeline
            .by_name("frames")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        let mut output = Self {
            pipeline,
            running: Arc::new(AtomicBool::new(true)),
            writer: None,
        };
        output
            .pipeline
            .set_state(gst::State::Playing)
            .context("failed to start virtual video output")?;
        let running = Arc::clone(&output.running);
        let black = gst::Buffer::from_slice(vec![
            0u8;
            config.width as usize * config.height as usize * 4
        ]);
        let period = Duration::from_secs_f64(1.0 / config.fps as f64);
        let bus = output
            .pipeline
            .bus()
            .context("virtual video bus is missing")?;
        let handle = tokio::runtime::Handle::current();
        output.writer = Some(
            std::thread::Builder::new()
                .name("tarsier-virtual-video".into())
                .spawn(move || {
                    let started = Instant::now();
                    let mut deadline = started;
                    while running.load(Ordering::Relaxed) {
                        let mut recording_frame = if capture_enabled.load(Ordering::Relaxed) {
                            preview
                                .virtual_frame
                                .lock()
                                .unwrap()
                                .as_ref()
                                .filter(|(at, _)| at.elapsed() <= Duration::from_millis(500))
                                .map(|(_, buffer)| buffer.clone())
                                .unwrap_or_else(|| black.clone())
                        } else {
                            black.clone()
                        };
                        // Read mute after acquiring the frame: a source switch must
                        // never pair its new image with a pre-switch unmuted state.
                        let muted = preview.output_muted();
                        let replacement = preview.replacement_frame(muted);
                        let buffer = recording_frame.make_mut();
                        buffer.set_pts(gst::ClockTime::from_nseconds(
                            started.elapsed().as_nanos() as u64
                        ));
                        buffer.set_dts(None);
                        buffer
                            .set_duration(gst::ClockTime::from_nseconds(period.as_nanos() as u64));
                        // Record processed capture independently of the conference output mute.
                        let mut frame = if muted {
                            replacement.unwrap_or_else(|| black.clone())
                        } else {
                            recording_frame.clone()
                        };
                        let pts = recording_frame.pts();
                        let duration = recording_frame.duration();
                        let output_buffer = frame.make_mut();
                        output_buffer.set_pts(pts);
                        output_buffer.set_dts(None);
                        output_buffer.set_duration(duration);
                        preview.recording_tx.send_replace(Some(recording_frame));
                        let push_error = source.push_buffer(frame).err();
                        let bus_error = bus.pop_filtered(&[gst::MessageType::Error]);
                        if push_error.is_some() || bus_error.is_some() {
                            let error = format!(
                                "virtual video output failed: {push_error:?} {bus_error:?}"
                            );
                            tracing::error!(%error);
                            handle.spawn({
                                let runtime = runtime.clone();
                                async move {
                                    runtime
                                        .update(|state| state.pipeline.error = Some(error))
                                        .await;
                                }
                            });
                            break;
                        }
                        deadline += period;
                        let now = Instant::now();
                        if deadline > now {
                            std::thread::sleep(deadline - now);
                        } else {
                            deadline = now;
                        }
                    }
                })?,
        );
        output
            .pipeline
            .state(gst::ClockTime::from_seconds(3))
            .0
            .context("virtual video output failed to reach Playing")?;
        Ok(output)
    }
}

impl Drop for VirtualVideoOutput {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

pub(crate) async fn ensure_virtual_video_device(device: &str, utility: &str) -> Result<()> {
    if Path::new(device).try_exists()? {
        return Ok(());
    }
    tracing::info!(device, "creating missing virtual camera");
    let mut command = tokio::process::Command::new(utility);
    command
        .args(["add", "-n", "Tarsier Camera", "-x", "1", device])
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .with_context(|| format!("timed out creating virtual camera {device}"))?
        .with_context(|| {
            format!("failed to create virtual camera {device}: install v4l2loopback-ctl and ensure it is executable")
        })?;
    // udev assigns ownership and ACLs asynchronously after device creation.
    // Another starter may also have created it while the command was running.
    if output.status.success() || Path::new(device).try_exists()? {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(device) {
                Ok(_) => return Ok(()),
                Err(error) if Instant::now() >= deadline => {
                    return Err(error).with_context(|| {
                        format!("virtual camera {device} was created but is not accessible; check device permissions")
                    });
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }
    bail!(
        "failed to create virtual camera {device} ({}): {}. Ensure the v4l2loopback module is loaded and you have access to /dev/v4l2loopback",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
}

impl VideoPipeline {
    pub async fn start(config: VideoConfig, runtime: Runtime, preview: PreviewHub) -> Result<Self> {
        gst::init().context("failed to initialize GStreamer")?;
        validate_path(&config.input_device)?;
        validate_path(&config.output_device)?;
        let supervisor_running = Arc::new(AtomicBool::new(true));
        let desired_running = Arc::new(AtomicBool::new(true));
        let desired_reserved = Arc::new(AtomicBool::new(false));
        let virtual_output = if config.loopback_enabled {
            ensure_virtual_video_device(&config.output_device, "v4l2loopback-ctl").await?;
            Some(VirtualVideoOutput::start(
                &config,
                preview.clone(),
                Arc::clone(&desired_running),
                runtime.clone(),
                &format!("v4l2sink device=\"{}\" sync=false", config.output_device),
            )?)
        } else {
            None
        };
        let pipeline =
            ActivePipeline::start(&config, runtime.clone(), preview.clone(), false).await?;
        let config = Arc::new(Mutex::new(config));
        let supervisor = spawn_supervisor(
            pipeline,
            Arc::clone(&config),
            runtime.clone(),
            preview,
            Arc::clone(&supervisor_running),
            Arc::clone(&desired_running),
            Arc::clone(&desired_reserved),
        )?;
        Ok(Self {
            source_changed_at_ms: Arc::new(AtomicU64::new(0)),
            config,
            supervisor_running,
            desired_running,
            desired_reserved,
            runtime,
            supervisor: Some(supervisor),
            _virtual_output: virtual_output,
        })
    }

    pub fn control(&self) -> VideoPipelineControl {
        VideoPipelineControl {
            source_changed_at_ms: Arc::clone(&self.source_changed_at_ms),
            config: Arc::clone(&self.config),
            desired_running: Arc::clone(&self.desired_running),
            desired_reserved: Arc::clone(&self.desired_reserved),
            runtime: self.runtime.clone(),
            #[cfg(test)]
            immediate: false,
        }
    }
}

impl VideoPipelineControl {
    pub fn accepts_frame(&self, captured_at_ms: u64) -> bool {
        self.desired_running.load(Ordering::Relaxed)
            && captured_at_ms >= self.source_changed_at_ms.load(Ordering::Relaxed)
    }

    pub fn camera_source(&self) -> String {
        let config = self.config.lock().unwrap();
        if config.source == VideoSource::Camera {
            config.input_device.clone()
        } else {
            String::new()
        }
    }

    // The capture must be stopped first; the virtual output has its own lifetime.
    pub async fn select_source(&self, camera: &str) -> Result<()> {
        if self.desired_running.load(Ordering::Relaxed) {
            anyhow::bail!("stop capture before selecting a camera");
        }
        if !camera.is_empty() {
            validate_path(camera)?;
        }
        {
            let mut config = self.config.lock().unwrap();
            config.source = if camera.is_empty() {
                VideoSource::Test
            } else {
                VideoSource::Camera
            };
            if !camera.is_empty() {
                config.input_device = camera.into();
            }
        }
        self.source_changed_at_ms
            .store(unix_ms(), Ordering::Relaxed);
        self.runtime
            .update(|state| {
                state.pipeline.source = if camera.is_empty() { "test" } else { "camera" }.into();
                state.pipeline.input_device = (!camera.is_empty()).then(|| camera.to_owned());
            })
            .await;
        Ok(())
    }

    pub async fn wait_for_frame(&self) -> Result<()> {
        #[cfg(test)]
        if self.immediate {
            return Ok(());
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let pipeline = self.runtime.state().await.pipeline;
                if pipeline.running && pipeline.last_frame_at_ms.is_some() {
                    return Ok(());
                }
                if let Some(error) = pipeline.error {
                    anyhow::bail!("{error}");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("timed out waiting for frames from the selected camera")?
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<()> {
        self.set_capture(enabled, false).await
    }

    pub async fn stop_reserved(&self) -> Result<()> {
        self.source_changed_at_ms.store(unix_ms(), Ordering::Relaxed);
        self.set_capture(false, true).await
    }

    async fn set_capture(&self, enabled: bool, reserved: bool) -> Result<()> {
        self.desired_reserved.store(reserved, Ordering::Relaxed);
        self.desired_running.store(enabled, Ordering::Relaxed);
        if enabled {
            self.runtime
                .update(|state| {
                    state.pipeline.enabled = true;
                    state.pipeline.error = None;
                })
                .await;
        }

        #[cfg(test)]
        if self.immediate {
            self.runtime
                .update(|state| {
                    state.pipeline.enabled = enabled;
                    state.pipeline.running = enabled;
                    state.pipeline.camera_reserved = reserved;
                    state.pipeline.error = None;
                })
                .await;
            return Ok(());
        }

        let wait = async {
            loop {
                let pipeline = self.runtime.state().await.pipeline;
                let reached_target = if enabled {
                    pipeline.enabled && pipeline.running
                } else {
                    !pipeline.enabled && !pipeline.running && pipeline.camera_reserved == reserved
                };
                if reached_target {
                    return Ok(());
                }
                if let Some(error) = pipeline.error.filter(|_| enabled || reserved) {
                    bail!("{error}");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .map_err(|_| {
                let action = if enabled { "start" } else { "stop" };
                anyhow::anyhow!("timed out waiting for the video pipeline to {action}")
            })?
    }

    #[cfg(test)]
    pub(crate) fn mock(runtime: Runtime) -> Self {
        Self {
            source_changed_at_ms: Arc::new(AtomicU64::new(0)),
            config: Arc::new(Mutex::new(VideoConfig::default())),
            desired_running: Arc::new(AtomicBool::new(true)),
            desired_reserved: Arc::new(AtomicBool::new(false)),
            runtime,
            immediate: true,
        }
    }
}

impl ActivePipeline {
    async fn start(
        config: &VideoConfig,
        runtime: Runtime,
        preview: PreviewHub,
        restarted: bool,
    ) -> Result<Self> {
        let description = pipeline_description(config);
        tracing::debug!(%description, "building GStreamer pipeline");
        let element = gst::parse::launch(&description).context("invalid GStreamer pipeline")?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("GStreamer description did not produce a pipeline"))?;
        let app_sink = pipeline
            .by_name("preview")
            .context("preview appsink is missing")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("preview element is not an appsink"))?;
        let perception_sink = pipeline
            .by_name("perception_preview")
            .context("perception preview appsink is missing")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("perception preview element is not an appsink"))?;
        let effect_processor = pipeline
            .by_name("effect_processor")
            .context("effect processor is missing")?;

        if let Some(sink) = pipeline.by_name("loopback_bridge") {
            let sink = sink
                .downcast::<gst_app::AppSink>()
                .map_err(|_| anyhow::anyhow!("invalid loopback bridge"))?;
            let frames = Arc::clone(&preview.virtual_frame);
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                        *frames.lock().unwrap() = Some((Instant::now(), buffer));
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }

        let capture_clock = Arc::new(CaptureClock::new(unix_ms(), config.fps));

        let photo_sink = pipeline
            .by_name("photo")
            .context("photo appsink is missing")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("photo element is not an appsink"))?;
        photo_sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let preview = preview.clone();
                    let capture_clock = Arc::clone(&capture_clock);
                    let settings = runtime.subscribe_state();
                    let dimensions = (config.width, config.height);
                    move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let pts = buffer.pts().ok_or(gst::FlowError::Error)?.nseconds();
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        preview.publish_photo(CapturedImage::new(
                            PerceptionFrame {
                                bytes: Bytes::copy_from_slice(map.as_slice()),
                                frame_id: capture_clock.frame_id(pts),
                                captured_at_ms: capture_clock.captured_at_ms(pts),
                            },
                            settings.borrow().clone(),
                            dimensions,
                        ));
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        );

        perception_sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let preview = preview.clone();
                    let capture_clock = Arc::clone(&capture_clock);
                    move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let pts_ns = buffer
                            .pts()
                            .map(|pts| pts.nseconds())
                            .ok_or(gst::FlowError::Error)?;
                        let frame_id = capture_clock.frame_id(pts_ns);
                        let captured_at_ms = capture_clock.captured_at_ms(pts_ns);
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        preview.publish_perception(PerceptionFrame {
                            bytes: Bytes::copy_from_slice(map.as_slice()),
                            frame_id,
                            captured_at_ms,
                        });
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        );

        let effects = preview.effects().clone();
        let effect_clock = Arc::clone(&capture_clock);
        let width = config.width;
        let height = config.height;
        effect_processor
            .static_pad("src")
            .context("effect processor source pad is missing")?
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if !effects.processing_enabled() {
                    return gst::PadProbeReturn::Ok;
                }
                let Some(buffer) = info.buffer_mut() else {
                    return gst::PadProbeReturn::Drop;
                };
                let buffer = buffer.make_mut();
                let frame_id = buffer
                    .pts()
                    .map(|pts| effect_clock.frame_id(pts.nseconds()));
                let Ok(mut map) = buffer.map_writable() else {
                    return gst::PadProbeReturn::Drop;
                };
                effects.apply_output_for_frame(
                    map.as_mut_slice(),
                    width,
                    height,
                    unix_ms(),
                    frame_id,
                );
                gst::PadProbeReturn::Ok
            });

        let existing_frame_count = runtime.state().await.pipeline.frame_count;
        let frame_count = Arc::new(AtomicU64::new(existing_frame_count));
        let last_frame_at_ms = Arc::new(AtomicU64::new(0));
        app_sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let preview = preview.clone();
                    let frame_count = Arc::clone(&frame_count);
                    let last_frame_at_ms = Arc::clone(&last_frame_at_ms);
                    let capture_clock = Arc::clone(&capture_clock);
                    let settings = runtime.subscribe_state();
                    let dimensions = (config.preview_width, config.preview_height);
                    move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        let pts = buffer.pts().ok_or(gst::FlowError::Error)?.nseconds();
                        preview.publish_output(CapturedImage::new(
                            PerceptionFrame {
                                bytes: Bytes::copy_from_slice(map.as_slice()),
                                frame_id: capture_clock.frame_id(pts),
                                captured_at_ms: capture_clock.captured_at_ms(pts),
                            },
                            settings.borrow().clone(),
                            dimensions,
                        ));
                        frame_count.fetch_add(1, Ordering::Relaxed);
                        last_frame_at_ms.store(unix_ms(), Ordering::Relaxed);
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        );

        pipeline
            .set_state(gst::State::Playing)
            .context("failed to start GStreamer pipeline")?;

        let running = Arc::new(AtomicBool::new(true));
        runtime
            .update(|state| {
                state.pipeline.enabled = true;
                state.pipeline.running = true;
                state.pipeline.camera_reserved = false;
                state.pipeline.source = match config.source {
                    VideoSource::Camera => "camera",
                    VideoSource::Test => "test",
                }
                .into();
                state.pipeline.input_device =
                    (config.source == VideoSource::Camera).then(|| config.input_device.clone());
                state.pipeline.output_device = config
                    .loopback_enabled
                    .then(|| config.output_device.clone());
                state.pipeline.width = config.width;
                state.pipeline.height = config.height;
                state.pipeline.fps = 0.0;
                state.pipeline.last_frame_at_ms = None;
                if restarted {
                    state.pipeline.restart_count = state.pipeline.restart_count.saturating_add(1);
                }
                state.pipeline.error = None;
            })
            .await;

        spawn_telemetry(
            runtime.clone(),
            Arc::clone(&running),
            frame_count.clone(),
            last_frame_at_ms.clone(),
        );
        Ok(Self { pipeline, running, frame_count, last_frame_at_ms, capture_clock })
    }

    fn reserve(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Ready)
            .context("failed to stop capture for reservation")?;
        let (result, current, _) = self.pipeline.state(gst::ClockTime::from_seconds(5));
        result.context("camera did not stop")?;
        anyhow::ensure!(current == gst::State::Ready, "camera did not reach READY");
        if let Some(source) = self.pipeline.by_name("physical_camera") {
            crate::pipeline_reservation::set_reserved(source.property::<i32>("device-fd"), true)?;
        }
        Ok(())
    }

    fn release_reservation(&self) -> Result<()> {
        if let Some(source) = self.pipeline.by_name("physical_camera") {
            crate::pipeline_reservation::set_reserved(source.property::<i32>("device-fd"), false)?;
        }
        Ok(())
    }

    async fn resume(&mut self, runtime: Runtime) -> Result<()> {
        self.release_reservation()?;
        self.capture_clock.started_at_ms.store(unix_ms(), Ordering::Relaxed);
        *self.capture_clock.first_pts_ns.lock().unwrap() = None;
        self.last_frame_at_ms.store(0, Ordering::Relaxed);
        self.pipeline.set_state(gst::State::Playing).context("failed to resume camera")?;
        self.running = Arc::new(AtomicBool::new(true));
        runtime.update(|state| {
            state.pipeline.enabled = true;
            state.pipeline.running = true;
            state.pipeline.camera_reserved = false;
            state.pipeline.error = None;
        }).await;
        spawn_telemetry(runtime, self.running.clone(), self.frame_count.clone(), self.last_frame_at_ms.clone());
        Ok(())
    }
}

impl Drop for ActivePipeline {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Err(error) = self.pipeline.set_state(gst::State::Null) {
            tracing::warn!(?error, "failed to stop GStreamer pipeline cleanly");
        }
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        self.supervisor_running.store(false, Ordering::Relaxed);
        if let Some(supervisor) = self.supervisor.take()
            && supervisor.join().is_err()
        {
            tracing::warn!("video supervisor thread panicked during shutdown");
        }
    }
}

fn validate_path(path: &str) -> Result<()> {
    if path.contains(['"', '\'', '\\', '\n', '\r']) {
        bail!("video device path contains unsupported characters");
    }
    Ok(())
}

fn pipeline_description(config: &VideoConfig) -> String {
    let source = match config.source {
        VideoSource::Camera => format!(
            "v4l2src name=physical_camera device=\"{}\" do-timestamp=true ! image/jpeg,width={},height={},framerate={}/1 ! jpegdec ! videoconvert",
            config.input_device, config.width, config.height, config.fps
        ),
        VideoSource::Test => format!(
            "videotestsrc is-live=true pattern=ball ! video/x-raw,width={},height={},framerate={}/1 ! videoconvert",
            config.width, config.height, config.fps
        ),
    };
    let mut branches = format!(
        "{source} ! tee name=camera_input \
         camera_input. ! queue leaky=downstream max-size-buffers=2 ! videoscale ! videoconvert ! \
         video/x-raw,width={},height={} ! jpegenc quality={} ! \
         appsink name=perception_preview max-buffers=1 drop=true sync=false \
         camera_input. ! queue name=effect_alignment leaky=downstream max-size-buffers=3 min-threshold-buffers=2 ! videoconvert ! \
         video/x-raw,format=BGRx,width={},height={},framerate={}/1 ! \
         identity name=effect_processor ! tee name=stream \
         stream. ! queue leaky=downstream max-size-buffers=2 ! videoscale ! videoconvert ! \
         video/x-raw,width={},height={} ! jpegenc quality={} ! \
         appsink name=preview max-buffers=1 drop=true sync=false",
        config.preview_width,
        config.preview_height,
        config.preview_quality,
        config.width,
        config.height,
        config.fps,
        config.preview_width,
        config.preview_height,
        config.preview_quality
    );
    branches.push_str(
        " stream. ! queue leaky=downstream max-size-buffers=1 ! videoconvert ! jpegenc quality=95 ! \
         appsink name=photo max-buffers=1 drop=true sync=false",
    );
    if config.loopback_enabled {
        branches.push_str(
            " stream. ! queue leaky=downstream max-size-buffers=2 ! \
             appsink name=loopback_bridge max-buffers=1 drop=true sync=false",
        );
    }
    branches
}

fn spawn_telemetry(
    runtime: Runtime,
    running: Arc<AtomicBool>,
    frame_count: Arc<AtomicU64>,
    last_frame_at_ms: Arc<AtomicU64>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut previous_count = frame_count.load(Ordering::Relaxed);
        let mut previous_at = tokio::time::Instant::now();
        loop {
            interval.tick().await;
            if !running.load(Ordering::Relaxed) {
                break;
            }
            let now = tokio::time::Instant::now();
            let count = frame_count.load(Ordering::Relaxed);
            let elapsed = now.duration_since(previous_at).as_secs_f32().max(0.001);
            let fps = count.saturating_sub(previous_count) as f32 / elapsed;
            let last_frame = last_frame_at_ms.load(Ordering::Relaxed);
            runtime
                .update(|state| {
                    state.pipeline.frame_count = count;
                    state.pipeline.fps = fps;
                    state.pipeline.last_frame_at_ms = (last_frame > 0).then_some(last_frame);
                })
                .await;
            previous_count = count;
            previous_at = now;
        }
    });
}

enum PipelineExit {
    Shutdown,
    Disabled,
    Failed(String),
    Eos,
}

fn spawn_supervisor(
    initial: ActivePipeline,
    config: Arc<Mutex<VideoConfig>>,
    runtime: Runtime,
    preview: PreviewHub,
    supervisor_running: Arc<AtomicBool>,
    desired_running: Arc<AtomicBool>,
    desired_reserved: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let tokio_handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("tarsier-video-supervisor".into())
        .spawn(move || {
            let mut active = Some(initial);
            let mut retry_after_failure = false;
            let mut disabled_reported = false;
            while supervisor_running.load(Ordering::Relaxed) {
                if let Some(current) = active.as_mut() {
                    let mut exit =
                        wait_for_pipeline_exit(current, &supervisor_running, &desired_running);
                    current.running.store(false, Ordering::Relaxed);
                    if matches!(exit, PipelineExit::Disabled) && desired_reserved.load(Ordering::Relaxed) {
                        let reservation = current.reserve();
                        preview.clear();
                        tokio_handle.block_on(runtime.update(|state| {
                            state.pipeline.enabled = false;
                            state.pipeline.running = false;
                            state.pipeline.camera_reserved = reservation.is_ok();
                            state.pipeline.fps = 0.0;
                            state.pipeline.last_frame_at_ms = None;
                            state.pipeline.error = reservation.err().map(|error| format!("camera reservation failed: {error:#}"));
                            clear_runtime_effect_frames(state);
                        }));
                        while supervisor_running.load(Ordering::Relaxed)
                            && !desired_running.load(Ordering::Relaxed)
                            && desired_reserved.load(Ordering::Relaxed)
                        {
                            std::thread::sleep(Duration::from_millis(25));
                        }
                        if supervisor_running.load(Ordering::Relaxed) && desired_running.load(Ordering::Relaxed) {
                            match tokio_handle.block_on(current.resume(runtime.clone())) {
                                Ok(()) => continue,
                                Err(error) => {
                                    exit = PipelineExit::Failed(format!("camera resume failed: {error:#}"));
                                }
                            }
                        }
                        let _ = current.release_reservation();
                    }
                    drop(active.take());
                    tokio_handle.block_on(runtime.update(|state| state.pipeline.camera_reserved = false));
                    preview.clear();
                    match exit {
                        PipelineExit::Shutdown => break,
                        PipelineExit::Disabled => {
                            tokio_handle.block_on(runtime.update(|state| {
                                state.pipeline.enabled = false;
                                state.pipeline.running = false;
                                state.pipeline.fps = 0.0;
                                state.pipeline.last_frame_at_ms = None;
                                state.pipeline.error = None;
                                clear_runtime_effect_frames(state);
                            }));
                            tokio_handle.block_on(runtime.emit(
                                "pipeline.disabled",
                                "video-supervisor",
                                None,
                                json!({}),
                            ));
                            retry_after_failure = false;
                            disabled_reported = true;
                        }
                        PipelineExit::Failed(detail) => {
                            tokio_handle.block_on(runtime.update(|state| {
                                state.pipeline.running = false;
                                state.pipeline.fps = 0.0;
                                state.pipeline.error = Some(detail.clone());
                                clear_runtime_effect_frames(state);
                            }));
                            tokio_handle.block_on(runtime.emit(
                                "pipeline.failed",
                                "video-supervisor",
                                None,
                                json!({"error": detail}),
                            ));
                            retry_after_failure = true;
                            disabled_reported = false;
                        }
                        PipelineExit::Eos => {
                            let detail = "video pipeline reached end of stream".to_owned();
                            tokio_handle.block_on(runtime.update(|state| {
                                state.pipeline.running = false;
                                state.pipeline.fps = 0.0;
                                state.pipeline.error = Some(detail.clone());
                                clear_runtime_effect_frames(state);
                            }));
                            tokio_handle.block_on(runtime.emit(
                                "pipeline.failed",
                                "video-supervisor",
                                None,
                                json!({"error": detail}),
                            ));
                            retry_after_failure = true;
                            disabled_reported = false;
                        }
                    }
                }

                if !supervisor_running.load(Ordering::Relaxed) {
                    break;
                }

                if !desired_running.load(Ordering::Relaxed) {
                    if !disabled_reported {
                        tokio_handle.block_on(runtime.update(|state| {
                            state.pipeline.enabled = false;
                            state.pipeline.running = false;
                            state.pipeline.fps = 0.0;
                            state.pipeline.last_frame_at_ms = None;
                            state.pipeline.error = None;
                            clear_runtime_effect_frames(state);
                        }));
                        tokio_handle.block_on(runtime.emit(
                            "pipeline.disabled",
                            "video-supervisor",
                            None,
                            json!({}),
                        ));
                        retry_after_failure = false;
                        disabled_reported = true;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }

                disabled_reported = false;
                let config = config.lock().unwrap().clone();
                if retry_after_failure
                    && !wait_for_retry(
                        &supervisor_running,
                        &desired_running,
                        Duration::from_millis(config.restart_delay_ms),
                    )
                {
                    continue;
                }
                if config.source == VideoSource::Camera && !Path::new(&config.input_device).exists()
                {
                    retry_after_failure = true;
                    continue;
                }
                let restarted = retry_after_failure;
                match tokio_handle.block_on(ActivePipeline::start(
                    &config,
                    runtime.clone(),
                    preview.clone(),
                    restarted,
                )) {
                    Ok(next) => {
                        active = Some(next);
                        retry_after_failure = false;
                        if restarted {
                            let restart_count = tokio_handle
                                .block_on(async { runtime.state().await.pipeline.restart_count });
                            tokio_handle.block_on(runtime.emit(
                                "pipeline.restarted",
                                "video-supervisor",
                                None,
                                json!({"restart_count": restart_count}),
                            ));
                            tracing::info!(restart_count, "video pipeline restarted");
                        } else {
                            tokio_handle.block_on(runtime.emit(
                                "pipeline.enabled",
                                "video-supervisor",
                                None,
                                json!({}),
                            ));
                        }
                    }
                    Err(error) => {
                        let detail = format!("video pipeline start failed: {error:#}");
                        tracing::warn!(%detail);
                        tokio_handle.block_on(runtime.update(|state| {
                            state.pipeline.enabled = true;
                            state.pipeline.running = false;
                            state.pipeline.fps = 0.0;
                            state.pipeline.error = Some(detail.clone());
                        }));
                        retry_after_failure = true;
                    }
                }
            }
            preview.clear();
            drop(active.take());
        })
        .context("failed to spawn video supervisor")
}

fn wait_for_pipeline_exit(
    active: &ActivePipeline,
    supervisor_running: &AtomicBool,
    desired_running: &AtomicBool,
) -> PipelineExit {
    let Some(bus) = active.pipeline.bus() else {
        return PipelineExit::Failed("video pipeline has no GStreamer bus".into());
    };
    while supervisor_running.load(Ordering::Relaxed) {
        if !desired_running.load(Ordering::Relaxed) {
            return PipelineExit::Disabled;
        }
        let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) else {
            continue;
        };
        match message.view() {
            gst::MessageView::Error(error) => {
                return PipelineExit::Failed(format!(
                    "{}{}",
                    error.error(),
                    error
                        .debug()
                        .map(|debug| format!(" ({debug})"))
                        .unwrap_or_default()
                ));
            }
            gst::MessageView::Eos(..) => return PipelineExit::Eos,
            _ => {}
        }
    }
    PipelineExit::Shutdown
}

fn clear_runtime_effect_frames(state: &mut crate::model::RuntimeState) {
    state.video_effects.mask_available = false;
    state.video_effects.mask_frame_id = None;
    state.video_effects.mask_width = None;
    state.video_effects.mask_height = None;
    state.video_effects.mask_captured_at_ms = None;
    state.video_effects.mask_published_at_ms = None;
    state.video_effects.depth_available = false;
    state.video_effects.depth_frame_id = None;
    state.video_effects.depth_width = None;
    state.video_effects.depth_height = None;
    state.video_effects.depth_far = None;
    state.video_effects.depth_near = None;
    state.video_effects.depth_captured_at_ms = None;
    state.video_effects.depth_published_at_ms = None;
}

fn wait_for_retry(running: &AtomicBool, desired_running: &AtomicBool, delay: Duration) -> bool {
    let deadline = std::time::Instant::now() + delay;
    while running.load(Ordering::Relaxed)
        && desired_running.load(Ordering::Relaxed)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(Duration::from_millis(100)),
        );
    }
    running.load(Ordering::Relaxed) && desired_running.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn virtual_device_existing_path_does_not_require_utility() {
        ensure_virtual_video_device("/dev/null", "/nonexistent/tarsier-loopback-ctl")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn virtual_device_creation_failure_explains_permissions() {
        let error = ensure_virtual_video_device("/nonexistent/tarsier-video", "/bin/false")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("/dev/v4l2loopback"));
        assert!(error.to_string().contains("exit status: 1"));
    }

    #[tokio::test]
    async fn virtual_device_creation_requires_device_even_after_success() {
        assert!(
            ensure_virtual_video_device("/nonexistent/tarsier-video", "/bin/true")
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_selection_keeps_the_same_virtual_output_stream() {
        gst::init().unwrap();
        let config = VideoConfig {
            source: VideoSource::Test,
            width: 64,
            height: 48,
            preview_width: 64,
            preview_height: 48,
            loopback_enabled: true,
            ..VideoConfig::default()
        };
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let enabled = Arc::new(AtomicBool::new(true));
        let output = VirtualVideoOutput::start(
            &config,
            preview.clone(),
            enabled.clone(),
            runtime.clone(),
            "appsink name=consumer max-buffers=1 drop=true sync=false",
        )
        .unwrap();
        let consumer = output
            .pipeline
            .by_name("consumer")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        let initial = ActivePipeline::start(&config, runtime.clone(), preview.clone(), false)
            .await
            .unwrap();
        let config = Arc::new(Mutex::new(config));
        let running = Arc::new(AtomicBool::new(true));
        let reserved = Arc::new(AtomicBool::new(false));
        let supervisor = spawn_supervisor(
            initial,
            config.clone(),
            runtime.clone(),
            preview.clone(),
            running.clone(),
            enabled.clone(),
            reserved.clone(),
        )
        .unwrap();
        let pipeline = VideoPipeline {
            source_changed_at_ms: Arc::new(AtomicU64::new(0)),
            config,
            supervisor_running: running,
            desired_running: enabled,
            desired_reserved: reserved,
            runtime: runtime.clone(),
            supervisor: Some(supervisor),
            _virtual_output: Some(output),
        };
        let control = pipeline.control();
        let mut last_pts = None;
        for _ in 0..2 {
            control.set_enabled(false).await.unwrap();
            control
                .select_source("/dev/tarsier-test-unavailable-camera")
                .await
                .unwrap();
            assert_eq!(
                control.camera_source(),
                "/dev/tarsier-test-unavailable-camera"
            );
            let sample = consumer
                .try_pull_sample(gst::ClockTime::from_seconds(1))
                .unwrap();
            let pts = sample.buffer().unwrap().pts().unwrap();
            if let Some(previous) = last_pts {
                assert!(pts > previous);
            }
            last_pts = Some(pts);
            control.select_source("").await.unwrap();
            assert!(!control.accepts_frame(1));
            assert!(!control.accepts_frame(unix_ms()));
            control.set_enabled(true).await.unwrap();
            assert!(control.accepts_frame(unix_ms()));
            assert_eq!(control.camera_source(), "");
            tokio::time::timeout(Duration::from_secs(3), async {
                while preview.latest().is_none() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(runtime.state().await.pipeline.source, "test");
        }
        // Virtual output ownership stays in the original pipeline throughout.
        assert!(
            pipeline
                ._virtual_output
                .as_ref()
                .unwrap()
                .running
                .load(Ordering::Relaxed)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reserved_capture_stops_frames_and_resumes_without_rebuilding() {
        let config = VideoConfig {
            source: VideoSource::Test,
            loopback_enabled: false,
            width: 64, height: 48, preview_width: 64, preview_height: 48,
            ..VideoConfig::default()
        };
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let pipeline = VideoPipeline::start(config, runtime.clone(), preview.clone()).await.unwrap();
        let control = pipeline.control();
        for _ in 0..3 {
            control.wait_for_frame().await.unwrap();
            control.stop_reserved().await.unwrap();
            let stopped = runtime.state().await.pipeline;
            assert!(stopped.camera_reserved);
            assert!(!stopped.running);
            assert!(!control.accepts_frame(unix_ms()));
            assert!(preview.latest().is_none());
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(runtime.state().await.pipeline.frame_count, stopped.frame_count);
            control.set_enabled(true).await.unwrap();
            control.wait_for_frame().await.unwrap();
            assert!(!runtime.state().await.pipeline.camera_reserved);
            assert_eq!(runtime.state().await.pipeline.restart_count, 0);
            assert!(preview.latest().unwrap().frame.captured_at_ms.abs_diff(unix_ms()) < 1000);
        }
        control.stop_reserved().await.unwrap();
        control.set_enabled(false).await.unwrap();
        assert!(!runtime.state().await.pipeline.camera_reserved);
        control.select_source("").await.unwrap();
        control.set_enabled(true).await.unwrap();
        control.wait_for_frame().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn virtual_video_keeps_streaming_black_across_capture_power_cycles() {
        gst::init().unwrap();
        let config = VideoConfig {
            source: VideoSource::Test,
            width: 64,
            height: 48,
            preview_width: 64,
            preview_height: 48,
            ..VideoConfig::default()
        };
        let preview = PreviewHub::new();
        preview.set_output_muted(true);
        let enabled = Arc::new(AtomicBool::new(true));
        let output = VirtualVideoOutput::start(
            &config,
            preview.clone(),
            Arc::clone(&enabled),
            Runtime::new(),
            "appsink name=consumer max-buffers=1 drop=true sync=false",
        )
        .unwrap();
        let consumer = output
            .pipeline
            .by_name("consumer")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        let mut last_pts = None;
        let mut read_until = |black: bool| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let sample = consumer
                    .try_pull_sample(gst::ClockTime::from_seconds(1))
                    .expect("the connected consumer must keep receiving frames");
                let buffer = sample.buffer().unwrap();
                let pts = buffer.pts().unwrap();
                if let Some(previous) = last_pts {
                    assert!(pts > previous);
                }
                last_pts = Some(pts);
                let map = buffer.map_readable().unwrap();
                let is_black = map
                    .as_slice()
                    .chunks_exact(4)
                    .all(|pixel| pixel == [16, 128, 16, 128]);
                if is_black == black {
                    break;
                }
                assert!(Instant::now() < deadline, "unexpected virtual video pixels");
            }
        };
        let runtime = Runtime::new();
        let capture = ActivePipeline::start(&config, runtime.clone(), preview.clone(), false)
            .await
            .unwrap();
        read_until(true);
        preview.set_output_muted(false);
        read_until(false);
        for _ in 0..2 {
            let before = preview.virtual_frame.lock().unwrap().as_ref().unwrap().0;
            preview.set_output_muted(true);
            for _ in 0..4 {
                read_until(true);
            }
            // Muting the virtual device must not stop or blacken capture/preview.
            let frames = preview.virtual_frame.lock().unwrap();
            let (at, frame) = frames.as_ref().unwrap();
            assert!(*at > before);
            assert!(
                frame
                    .map_readable()
                    .unwrap()
                    .as_slice()
                    .iter()
                    .any(|byte| *byte != 0)
            );
            drop(frames);
            assert!(enabled.load(Ordering::Relaxed));
            preview.set_output_muted(false);
            read_until(false);
        }
        preview.set_output_muted(true);
        enabled.store(false, Ordering::Relaxed);
        drop(capture);
        preview.clear();
        assert!(preview.output_muted());
        for _ in 0..5 {
            read_until(true);
        }
        enabled.store(true, Ordering::Relaxed);
        // Wake starts with black, never the previous capture's last frame.
        read_until(true);
        let capture = ActivePipeline::start(&config, runtime, preview.clone(), true)
            .await
            .unwrap();
        read_until(true);
        preview.set_output_muted(false);
        read_until(false);
        // A failed capture also falls back to black once its last frame expires.
        drop(capture);
        tokio::time::sleep(Duration::from_millis(550)).await;
        read_until(true);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn muted_output_uses_replacement_and_returns_to_black_when_removed() {
        gst::init().unwrap();
        let config = VideoConfig {
            width: 4,
            height: 4,
            ..VideoConfig::default()
        };
        let directory = std::env::temp_dir().join(format!(
            "tarsier-mute-stream-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let mut image = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut image, image::ImageFormat::Png)
            .unwrap();
        let (selection, media) =
            crate::mute_media::upload(&directory, "away.png", image.get_ref(), 4, 4).unwrap();
        let preview = PreviewHub::new();
        preview.set_replacement(Some(selection), Some(media), None);
        preview.set_output_muted(true);
        let captured = gst::Buffer::from_slice([0, 255, 0, 0].repeat(16));
        *preview.virtual_frame.lock().unwrap() = Some((Instant::now(), captured));
        let mut frames = preview.subscribe_recording();
        let enabled = Arc::new(AtomicBool::new(true));
        let output = VirtualVideoOutput::start(
            &config,
            preview.clone(),
            enabled.clone(),
            Runtime::new(),
            "appsink name=consumer max-buffers=1 drop=true sync=false",
        ).unwrap();
        let consumer = output.pipeline.by_name("consumer").unwrap()
            .downcast::<gst_app::AppSink>().unwrap();
        let read_output = || {
            consumer.try_pull_sample(gst::ClockTime::from_seconds(1)).unwrap()
                .buffer().unwrap().map_readable().unwrap().as_slice().to_vec()
        };
        let replacement_pixels = read_output();
        // YUY2 conversion can round differently across GStreamer versions.
        assert!(replacement_pixels.chunks_exact(4).all(|pixel|
            (60..=100).contains(&pixel[0]) && (70..=110).contains(&pixel[1])
                && (60..=100).contains(&pixel[2]) && pixel[3] >= 220
        ), "unexpected replacement pixels: {replacement_pixels:?}");
        frames.changed().await.unwrap();
        assert_eq!(frames.borrow_and_update().as_ref().unwrap().map_readable().unwrap().as_slice(), [0, 255, 0, 0].repeat(16));
        // Stopped capture records black, even while a replacement remains on the output.
        enabled.store(false, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                frames.changed().await.unwrap();
                if frames.borrow_and_update().as_ref().unwrap().map_readable().unwrap().as_slice().iter().all(|byte| *byte == 0) {
                    break;
                }
            }
        }).await.unwrap();
        assert_eq!(read_output(), replacement_pixels);
        preview.set_replacement(None, None, None);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !read_output().chunks_exact(4).all(|pixel| pixel == [16, 128, 16, 128]) {
            assert!(Instant::now() < deadline);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recording_receives_final_frames_and_privacy_black_without_a_device_reader() {
        gst::init().unwrap();
        let config = VideoConfig {
            width: 64,
            height: 48,
            ..VideoConfig::default()
        };
        let preview = PreviewHub::new();
        let mut frames = preview.subscribe_recording();
        let pixels = [40, 80, 120, 0].repeat(64 * 48);
        *preview.virtual_frame.lock().unwrap() =
            Some((Instant::now(), gst::Buffer::from_slice(pixels.clone())));
        let enabled = Arc::new(AtomicBool::new(true));
        let _output = VirtualVideoOutput::start(
            &config,
            preview,
            enabled.clone(),
            Runtime::new(),
            "fakesink sync=false",
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), frames.changed())
            .await
            .unwrap()
            .unwrap();
        let frame = frames.borrow_and_update().clone().unwrap();
        assert_eq!(frame.map_readable().unwrap().as_slice(), pixels);
        enabled.store(false, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                frames.changed().await.unwrap();
                let frame = frames.borrow_and_update().clone().unwrap();
                if frame
                    .map_readable()
                    .unwrap()
                    .as_slice()
                    .iter()
                    .all(|byte| *byte == 0)
                {
                    break;
                }
            }
        })
        .await
        .expect("recording must receive black when capture is disabled");
    }

    #[test]
    fn capture_clock_maps_source_pts_to_one_wall_clock_timeline() {
        let clock = CaptureClock::new(1_725_000_000_000, 30);
        let first = clock.captured_at_ms(5_000_000_000);

        assert_eq!(first, 1_725_000_005_000);
        assert_eq!(clock.captured_at_ms(5_033_333_333), first + 33);
        assert_eq!(clock.captured_at_ms(5_066_666_666), first + 66);
        assert_eq!(clock.frame_id(5_000_000_000), 1);
        assert_eq!(clock.frame_id(5_033_333_333), 2);
        assert_eq!(clock.frame_id(5_066_666_666), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn supervisor_restarts_a_pipeline_after_eos() {
        gst::init().unwrap();
        let config = VideoConfig {
            source: VideoSource::Test,
            loopback_enabled: false,
            restart_delay_ms: 50,
            ..VideoConfig::default()
        };
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let initial = ActivePipeline::start(&config, runtime.clone(), preview.clone(), false)
            .await
            .unwrap();
        assert!(initial.pipeline.send_event(gst::event::Eos::new()));
        let supervisor_running = Arc::new(AtomicBool::new(true));
        let desired_running = Arc::new(AtomicBool::new(true));
        let supervisor = spawn_supervisor(
            initial,
            Arc::new(Mutex::new(config)),
            runtime.clone(),
            preview.clone(),
            Arc::clone(&supervisor_running),
            desired_running,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let state = runtime.state().await;
                if state.pipeline.running
                    && state.pipeline.restart_count == 1
                    && preview.latest().is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("supervisor should rebuild the test pipeline");

        supervisor_running.store(false, Ordering::Relaxed);
        supervisor.join().unwrap();
        let events = runtime.recent_events().await;
        assert!(events.iter().any(|event| event.kind == "pipeline.failed"));
        assert!(
            events
                .iter()
                .any(|event| event.kind == "pipeline.restarted")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pipeline_control_releases_and_restarts_the_active_pipeline() {
        let config = VideoConfig {
            source: VideoSource::Test,
            loopback_enabled: false,
            restart_delay_ms: 50,
            ..VideoConfig::default()
        };
        let runtime = Runtime::new();
        let preview = PreviewHub::new();
        let pipeline = VideoPipeline::start(config, runtime.clone(), preview.clone())
            .await
            .unwrap();
        let control = pipeline.control();

        control.set_enabled(false).await.unwrap();
        let state = runtime.state().await;
        assert!(!state.pipeline.enabled);
        assert!(!state.pipeline.running);
        assert!(preview.latest().is_none());

        control.set_enabled(true).await.unwrap();
        let state = runtime.state().await;
        assert!(state.pipeline.enabled);
        assert!(state.pipeline.running);

        drop(pipeline);
        let events = runtime.recent_events().await;
        assert!(events.iter().any(|event| event.kind == "pipeline.disabled"));
        assert!(events.iter().any(|event| event.kind == "pipeline.enabled"));
    }

    #[test]
    fn synthetic_pipeline_has_independent_preview_branch() {
        let config = VideoConfig {
            source: VideoSource::Test,
            loopback_enabled: false,
            ..VideoConfig::default()
        };
        let description = pipeline_description(&config);
        assert!(description.contains("videotestsrc"));
        assert!(description.contains("appsink name=perception_preview"));
        assert!(description.contains("name=effect_alignment"));
        assert!(description.contains("min-threshold-buffers=2"));
        assert!(description.contains("identity name=effect_processor"));
        assert!(description.contains("appsink name=preview"));
        assert!(!description.contains("v4l2sink"));
    }

    #[test]
    fn camera_pipeline_keeps_loopback_branch_leaky() {
        let description = pipeline_description(&VideoConfig::default());
        assert!(description.contains("v4l2src name=physical_camera device=\"/dev/video0\""));
        assert!(description.contains("appsink name=loopback_bridge"));
        assert!(!description.contains("v4l2sink"));
        assert_eq!(description.matches("leaky=downstream").count(), 5);
    }
}
