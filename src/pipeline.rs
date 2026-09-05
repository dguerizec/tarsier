use std::{
    path::Path,
    sync::{
        Arc,
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
    perception_tx: watch::Sender<Option<Bytes>>,
    effects: VideoEffects,
}

impl PreviewHub {
    pub fn new() -> Self {
        let (output_tx, _) = watch::channel(None);
        let (perception_tx, _) = watch::channel(None);
        Self {
            output_tx,
            perception_tx,
            effects: VideoEffects::new(),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Bytes>> {
        self.output_tx.subscribe()
    }

    pub fn subscribe_perception(&self) -> watch::Receiver<Option<Bytes>> {
        self.perception_tx.subscribe()
    }

    pub fn latest(&self) -> Option<Bytes> {
        self.output_tx.borrow().clone()
    }

    pub fn effects(&self) -> &VideoEffects {
        &self.effects
    }

    fn publish_output(&self, frame: Bytes) {
        self.output_tx.send_replace(Some(frame));
    }

    fn publish_perception(&self, frame: Bytes) {
        self.perception_tx.send_replace(Some(frame));
    }

    fn clear(&self) {
        self.output_tx.send_replace(None);
        self.perception_tx.send_replace(None);
        self.effects.clear_mask();
    }
}

pub struct VideoPipeline {
    supervisor_running: Arc<AtomicBool>,
    supervisor: Option<JoinHandle<()>>,
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
        let supervisor = spawn_supervisor(
            pipeline,
            config,
            runtime,
            preview,
            Arc::clone(&supervisor_running),
        )?;
        Ok(Self {
            supervisor_running,
            supervisor: Some(supervisor),
        })
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

        perception_sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let preview = preview.clone();
                    move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        preview.publish_perception(Bytes::copy_from_slice(map.as_slice()));
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        );

        let effects = preview.effects().clone();
        let width = config.width;
        let height = config.height;
        effect_processor
            .static_pad("src")
            .context("effect processor source pad is missing")?
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if !effects.green_screen_enabled() {
                    return gst::PadProbeReturn::Ok;
                }
                let Some(buffer) = info.buffer_mut() else {
                    return gst::PadProbeReturn::Drop;
                };
                let buffer = buffer.make_mut();
                let Ok(mut map) = buffer.map_writable() else {
                    return gst::PadProbeReturn::Drop;
                };
                effects.apply_green_screen(map.as_mut_slice(), width, height, unix_ms());
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
    Failed(String),
    Eos,
}

fn spawn_supervisor(
    initial: ActivePipeline,
    config: VideoConfig,
    runtime: Runtime,
    preview: PreviewHub,
    supervisor_running: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let tokio_handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("tarsier-video-supervisor".into())
        .spawn(move || {
            let mut active = Some(initial);
            while supervisor_running.load(Ordering::Relaxed) {
                let exit = wait_for_pipeline_exit(
                    active.as_ref().expect("active pipeline is present"),
                    &supervisor_running,
                );
                if matches!(exit, PipelineExit::Shutdown) {
                    break;
                }

                let detail = match exit {
                    PipelineExit::Failed(detail) => detail,
                    PipelineExit::Eos => "video pipeline reached end of stream".to_owned(),
                    PipelineExit::Shutdown => unreachable!(),
                };
                active
                    .as_ref()
                    .expect("active pipeline is present")
                    .running
                    .store(false, Ordering::Relaxed);
                preview.clear();
                tokio_handle.block_on(async {
                    runtime
                        .update(|state| {
                            state.pipeline.running = false;
                            state.pipeline.fps = 0.0;
                            state.pipeline.error = Some(detail.clone());
                            state.video_effects.mask_available = false;
                            state.video_effects.mask_frame_id = None;
                            state.video_effects.mask_width = None;
                            state.video_effects.mask_height = None;
                            state.video_effects.mask_captured_at_ms = None;
                            state.video_effects.mask_published_at_ms = None;
                        })
                        .await;
                    runtime
                        .emit(
                            "pipeline.failed",
                            "video-supervisor",
                            None,
                            json!({"error": detail}),
                        )
                        .await;
                });
                drop(active.take());

                while supervisor_running.load(Ordering::Relaxed) {
                    if !wait_for_retry(
                        &supervisor_running,
                        Duration::from_millis(config.restart_delay_ms),
                    ) {
                        break;
                    }
                    if config.source == VideoSource::Camera
                        && !Path::new(&config.input_device).exists()
                    {
                        continue;
                    }
                    match tokio_handle.block_on(ActivePipeline::start(
                        &config,
                        runtime.clone(),
                        preview.clone(),
                        true,
                    )) {
                        Ok(next) => {
                            let restart_count = tokio_handle
                                .block_on(async { runtime.state().await.pipeline.restart_count });
                            tokio_handle.block_on(runtime.emit(
                                "pipeline.restarted",
                                "video-supervisor",
                                None,
                                json!({"restart_count": restart_count}),
                            ));
                            tracing::info!(restart_count, "video pipeline restarted");
                            active = Some(next);
                            break;
                        }
                        Err(error) => {
                            let detail = format!("video pipeline restart failed: {error:#}");
                            tracing::warn!(%detail);
                            tokio_handle.block_on(runtime.update(|state| {
                                state.pipeline.running = false;
                                state.pipeline.fps = 0.0;
                                state.pipeline.error = Some(detail);
                            }));
                        }
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
) -> PipelineExit {
    let Some(bus) = active.pipeline.bus() else {
        return PipelineExit::Failed("video pipeline has no GStreamer bus".into());
    };
    while supervisor_running.load(Ordering::Relaxed) {
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

fn wait_for_retry(running: &AtomicBool, delay: Duration) -> bool {
    let deadline = std::time::Instant::now() + delay;
    while running.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
        std::thread::sleep(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(Duration::from_millis(100)),
        );
    }
    running.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let supervisor = spawn_supervisor(
            initial,
            config,
            runtime.clone(),
            preview.clone(),
            Arc::clone(&supervisor_running),
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
        assert_eq!(description.matches("leaky=downstream").count(), 4);
    }
}
