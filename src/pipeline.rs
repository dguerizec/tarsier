use std::{
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
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
    model::unix_ms,
    runtime::Runtime,
};

#[derive(Clone)]
pub struct PreviewHub {
    output_tx: watch::Sender<Option<Bytes>>,
    perception_tx: watch::Sender<Option<PerceptionFrame>>,
    photo_tx: watch::Sender<Option<PerceptionFrame>>,
    effects: VideoEffects,
}

#[derive(Clone, Debug)]
pub(crate) struct PerceptionFrame {
    pub bytes: Bytes,
    pub frame_id: u64,
    pub captured_at_ms: u64,
}

struct CaptureClock {
    started_at_ms: u64,
    fps: u32,
    first_pts_ns: OnceLock<u64>,
}

impl CaptureClock {
    fn new(started_at_ms: u64, fps: u32) -> Self {
        Self {
            started_at_ms,
            fps,
            first_pts_ns: OnceLock::new(),
        }
    }

    fn captured_at_ms(&self, pts_ns: u64) -> u64 {
        self.started_at_ms.saturating_add(pts_ns / 1_000_000)
    }

    fn frame_id(&self, pts_ns: u64) -> u64 {
        let first_pts_ns = *self.first_pts_ns.get_or_init(|| pts_ns);
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
            output_tx,
            perception_tx,
            effects: VideoEffects::new(),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Bytes>> {
        self.output_tx.subscribe()
    }

    pub(crate) fn subscribe_perception(&self) -> watch::Receiver<Option<PerceptionFrame>> {
        self.perception_tx.subscribe()
    }

    pub fn latest(&self) -> Option<Bytes> {
        self.output_tx.borrow().clone()
    }

    pub(crate) fn latest_photo(&self) -> Option<PerceptionFrame> {
        self.photo_tx
            .borrow()
            .clone()
            .filter(|frame| unix_ms().saturating_sub(frame.captured_at_ms) <= 1000)
    }

    pub(crate) fn publish_photo(&self, frame: PerceptionFrame) {
        self.photo_tx.send_replace(Some(frame));
    }

    pub fn effects(&self) -> &VideoEffects {
        &self.effects
    }

    fn publish_output(&self, frame: Bytes) {
        self.output_tx.send_replace(Some(frame));
    }

    fn publish_perception(&self, frame: PerceptionFrame) {
        self.perception_tx.send_replace(Some(frame));
    }

    fn clear(&self) {
        self.photo_tx.send_replace(None);
        self.output_tx.send_replace(None);
        self.perception_tx.send_replace(None);
        self.effects.clear_mask();
        self.effects.clear_avatar();
        self.effects.clear_depth();
    }
}

pub struct VideoPipeline {
    supervisor_running: Arc<AtomicBool>,
    desired_running: Arc<AtomicBool>,
    runtime: Runtime,
    supervisor: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct VideoPipelineControl {
    desired_running: Arc<AtomicBool>,
    runtime: Runtime,
    #[cfg(test)]
    immediate: bool,
}

struct ActivePipeline {
    pipeline: gst::Pipeline,
    running: Arc<AtomicBool>,
}

impl VideoPipeline {
    pub async fn start(config: VideoConfig, runtime: Runtime, preview: PreviewHub) -> Result<Self> {
        gst::init().context("failed to initialize GStreamer")?;
        validate_path(&config.input_device)?;
        validate_path(&config.output_device)?;
        let pipeline =
            ActivePipeline::start(&config, runtime.clone(), preview.clone(), false).await?;
        let supervisor_running = Arc::new(AtomicBool::new(true));
        let desired_running = Arc::new(AtomicBool::new(true));
        let supervisor = spawn_supervisor(
            pipeline,
            config,
            runtime.clone(),
            preview,
            Arc::clone(&supervisor_running),
            Arc::clone(&desired_running),
        )?;
        Ok(Self {
            supervisor_running,
            desired_running,
            runtime,
            supervisor: Some(supervisor),
        })
    }

    pub fn control(&self) -> VideoPipelineControl {
        VideoPipelineControl {
            desired_running: Arc::clone(&self.desired_running),
            runtime: self.runtime.clone(),
            #[cfg(test)]
            immediate: false,
        }
    }
}

impl VideoPipelineControl {
    pub async fn set_enabled(&self, enabled: bool) -> Result<()> {
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
                    !pipeline.enabled && !pipeline.running
                };
                if reached_target {
                    return Ok(());
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
            desired_running: Arc::new(AtomicBool::new(true)),
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
                    move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let pts = buffer.pts().ok_or(gst::FlowError::Error)?.nseconds();
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        preview.publish_photo(PerceptionFrame {
                            bytes: Bytes::copy_from_slice(map.as_slice()),
                            frame_id: capture_clock.frame_id(pts),
                            captured_at_ms: capture_clock.captured_at_ms(pts),
                        });
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
                    move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        preview.publish_output(Bytes::copy_from_slice(map.as_slice()));
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
            frame_count,
            last_frame_at_ms,
        );
        Ok(Self { pipeline, running })
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
            "v4l2src device=\"{}\" do-timestamp=true ! image/jpeg,width={},height={},framerate={}/1 ! jpegdec ! videoconvert",
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
        branches.push_str(&format!(
            " stream. ! queue leaky=downstream max-size-buffers=2 ! videoconvert ! \
             video/x-raw,format=YUY2,width={},height={},framerate={}/1 ! \
             v4l2sink device=\"{}\" sync=false",
            config.width, config.height, config.fps, config.output_device
        ));
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
    config: VideoConfig,
    runtime: Runtime,
    preview: PreviewHub,
    supervisor_running: Arc<AtomicBool>,
    desired_running: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let tokio_handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("tarsier-video-supervisor".into())
        .spawn(move || {
            let mut active = Some(initial);
            let mut retry_after_failure = false;
            let mut disabled_reported = false;
            while supervisor_running.load(Ordering::Relaxed) {
                if let Some(current) = active.as_ref() {
                    let exit =
                        wait_for_pipeline_exit(current, &supervisor_running, &desired_running);
                    current.running.store(false, Ordering::Relaxed);
                    preview.clear();
                    drop(active.take());
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
            config,
            runtime.clone(),
            preview.clone(),
            Arc::clone(&supervisor_running),
            desired_running,
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
        assert!(description.contains("v4l2src device=\"/dev/video0\""));
        assert!(description.contains("v4l2sink device=\"/dev/video42\""));
        assert_eq!(description.matches("leaky=downstream").count(), 5);
    }
}
