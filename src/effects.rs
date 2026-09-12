use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use tokio::sync::watch;

use crate::model::{BackgroundEffect, VideoOutputMode, unix_ms};

pub const MASK_MAX_AGE_MS: u64 = 200;
const MASK_SYNC_WAIT_MS: u64 = 150;
const MASK_BUFFER_CAPACITY: usize = 16;
pub const AVATAR_MAX_AGE_MS: u64 = 500;
pub const DEPTH_MAX_AGE_MS: u64 = 500;
const MAX_MASK_PIXELS: usize = 1920 * 1080;
pub const MAX_AVATAR_FRAME_BYTES: usize = 1920 * 1080 * 4;
pub const MAX_DEPTH_FRAME_BYTES: usize = 1920 * 1080 * size_of::<f32>();
const BLUR_DOWNSAMPLE: usize = 8;
const BLUR_RADIUS: usize = 3;
const PIXEL_PARTY_BLOCK_SIZE: usize = 16;
const PIXEL_PARTY_CYCLE_MS: u64 = 24_000;
const PIXEL_PARTY_SCENE_WIDTH: usize = 80;
const PIXEL_PARTY_SCENE_HEIGHT: usize = 45;
const PIXEL_PARTY_SCENE_BYTES: usize = PIXEL_PARTY_SCENE_WIDTH * PIXEL_PARTY_SCENE_HEIGHT * 3;
static PIXEL_PARTY_SCENE: &[u8; PIXEL_PARTY_SCENE_BYTES] =
    include_bytes!("../assets/backgrounds/pixel-party.bgr");

#[derive(Clone, Debug)]
pub struct VideoMask {
    pub frame_id: u64,
    pub captured_at_ms: u64,
    pub published_at_ms: u64,
    pub width: u32,
    pub height: u32,
    pixels: Arc<[u8]>,
    coverage: bool,
}

impl VideoMask {
    pub fn new(
        frame_id: u64,
        captured_at_ms: u64,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> Result<Self> {
        let expected_len = mask_len(width, height)?;
        if pixels.len() != expected_len {
            bail!(
                "video mask contains {} bytes, expected {expected_len}",
                pixels.len()
            );
        }
        Ok(Self {
            frame_id,
            captured_at_ms,
            published_at_ms: unix_ms(),
            width,
            height,
            pixels: pixels.into(),
            coverage: false,
        })
    }

    fn alpha(&self, index: usize) -> u8 {
        if self.coverage {
            self.pixels[index]
        } else {
            feather_alpha(self.pixels[index])
        }
    }

    fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.published_at_ms) <= MASK_MAX_AGE_MS
            && now_ms.saturating_sub(self.captured_at_ms) <= MASK_MAX_AGE_MS
    }
}

#[derive(Clone, Debug)]
pub struct AvatarFrame {
    pub frame_id: u64,
    pub captured_at_ms: u64,
    pub published_at_ms: u64,
    pub width: u32,
    pub height: u32,
    pixels: Arc<[u8]>,
    mask: Option<VideoMask>,
}

impl AvatarFrame {
    pub fn new(
        frame_id: u64,
        captured_at_ms: u64,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> Result<Self> {
        let expected_len = avatar_frame_len(width, height)?;
        if pixels.len() != expected_len {
            bail!(
                "avatar frame contains {} bytes, expected {expected_len}",
                pixels.len()
            );
        }
        Ok(Self {
            frame_id,
            captured_at_ms,
            published_at_ms: unix_ms(),
            width,
            height,
            pixels: pixels.into(),
            mask: None,
        })
    }

    pub fn with_alpha(mut self, enabled: bool) -> Self {
        if enabled {
            self.mask = Some(VideoMask {
                coverage: true,
                frame_id: self.frame_id,
                captured_at_ms: self.captured_at_ms,
                published_at_ms: self.published_at_ms,
                width: self.width,
                height: self.height,
                pixels: self
                    .pixels
                    .chunks_exact(4)
                    .map(|p| p[3])
                    .collect::<Vec<_>>()
                    .into(),
            });
        }
        self
    }

    fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.published_at_ms) <= AVATAR_MAX_AGE_MS
            && now_ms.saturating_sub(self.captured_at_ms) <= AVATAR_MAX_AGE_MS
    }
}

#[derive(Clone, Debug)]
pub struct DepthFrame {
    pub frame_id: u64,
    pub captured_at_ms: u64,
    pub published_at_ms: u64,
    pub width: u32,
    pub height: u32,
    pub far: f32,
    pub near: f32,
    values: Arc<[f32]>,
}

impl DepthFrame {
    pub fn new(
        frame_id: u64,
        captured_at_ms: u64,
        width: u32,
        height: u32,
        far: f32,
        near: f32,
        bytes: &[u8],
    ) -> Result<Self> {
        let expected_values = mask_len(width, height)?;
        let expected_bytes = expected_values
            .checked_mul(size_of::<f32>())
            .filter(|length| *length <= MAX_DEPTH_FRAME_BYTES)
            .ok_or_else(|| anyhow::anyhow!("depth frame exceeds the maximum supported size"))?;
        if bytes.len() != expected_bytes {
            bail!(
                "depth frame contains {} bytes, expected {expected_bytes}",
                bytes.len()
            );
        }
        if !far.is_finite() || !near.is_finite() || near <= far {
            bail!("depth visualization bounds must be finite and near must exceed far");
        }
        let values = bytes
            .chunks_exact(size_of::<f32>())
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four-byte depth chunk")))
            .collect::<Vec<_>>();
        if values.iter().any(|value| !value.is_finite()) {
            bail!("depth frame values must be finite");
        }
        Ok(Self {
            frame_id,
            captured_at_ms,
            published_at_ms: unix_ms(),
            width,
            height,
            far,
            near,
            values: values.into(),
        })
    }

    pub fn values(&self) -> &[f32] {
        &self.values
    }

    fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.published_at_ms) <= DEPTH_MAX_AGE_MS
            && now_ms.saturating_sub(self.captured_at_ms) <= DEPTH_MAX_AGE_MS
    }
}

fn avatar_frame_len(width: u32, height: u32) -> Result<usize> {
    let pixel_count = mask_len(width, height)?;
    pixel_count
        .checked_mul(4)
        .filter(|length| *length <= MAX_AVATAR_FRAME_BYTES)
        .ok_or_else(|| anyhow::anyhow!("avatar frame exceeds the maximum supported size"))
}

fn mask_len(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 {
        bail!("video mask dimensions must be greater than zero");
    }
    let len = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow::anyhow!("video mask dimensions overflow"))?;
    if len > MAX_MASK_PIXELS {
        bail!("video mask exceeds the maximum supported size");
    }
    Ok(len)
}

#[derive(Clone)]
pub struct VideoEffects {
    pub backgrounds: crate::background::Backgrounds,
    transform: Arc<AtomicU8>,
    transform_scratch: Arc<Mutex<crate::video_transform::TransformScratch>>,
    output_mode: Arc<AtomicU8>,
    background_enabled: Arc<AtomicBool>,
    background_effect: Arc<AtomicU8>,
    masks: Arc<(Mutex<MaskStore>, Condvar)>,
    avatar_tx: watch::Sender<Option<Arc<AvatarFrame>>>,
    depth_tx: watch::Sender<Option<Arc<DepthFrame>>>,
    scratch: Arc<Mutex<EffectScratch>>,
    held_output: Arc<Mutex<HeldOutput>>,
}

// Stores only successfully processed pixels, before the presentation transform.
#[derive(Default)]
struct HeldOutput {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

#[derive(Default)]
struct MaskStore {
    latest: Option<Arc<VideoMask>>,
    pending: VecDeque<Arc<VideoMask>>,
}

#[derive(Default)]
struct EffectScratch {
    reduced: Vec<u8>,
    temporary: Vec<u8>,
}

impl VideoEffects {
    pub fn new() -> Self {
        let (avatar_tx, _) = watch::channel(None);
        let (depth_tx, _) = watch::channel(None);
        Self {
            backgrounds: Default::default(),
            output_mode: Arc::new(AtomicU8::new(output_mode_code(VideoOutputMode::Camera))),
            transform: Arc::new(AtomicU8::new(0)),
            transform_scratch: Arc::new(Mutex::new(Default::default())),
            background_enabled: Arc::new(AtomicBool::new(false)),
            background_effect: Arc::new(AtomicU8::new(effect_code(BackgroundEffect::GreenScreen))),
            masks: Arc::new((Mutex::new(MaskStore::default()), Condvar::new())),
            avatar_tx,
            depth_tx,
            scratch: Arc::new(Mutex::new(EffectScratch::default())),
            held_output: Arc::new(Mutex::new(HeldOutput::default())),
        }
    }

    pub fn output_mode(&self) -> VideoOutputMode {
        output_mode_from_code(self.output_mode.load(Ordering::Relaxed))
    }

    pub fn set_output_mode(&self, mode: VideoOutputMode) {
        let mut held = self
            .held_output
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *held = HeldOutput::default();
        if mode != self.output_mode() {
            self.backgrounds.invalidate();
            self.clear_mask();
        }
        self.output_mode
            .store(output_mode_code(mode), Ordering::Relaxed);
    }

    pub fn processing_enabled(&self) -> bool {
        self.output_mode() != VideoOutputMode::Camera
            || self.background_enabled()
            || self.transform.load(Ordering::Relaxed) != 0
    }

    pub fn set_transform(&self, transform: crate::video_transform::VideoTransform) {
        self.transform.store(transform.code(), Ordering::Relaxed);
    }

    pub fn transform(&self) -> crate::video_transform::VideoTransform {
        crate::video_transform::VideoTransform::from_code(self.transform.load(Ordering::Relaxed))
    }

    pub fn background_enabled(&self) -> bool {
        self.background_enabled.load(Ordering::Relaxed)
    }

    pub fn background_effect(&self) -> BackgroundEffect {
        effect_from_code(self.background_effect.load(Ordering::Relaxed))
    }

    pub fn set_background(&self, enabled: bool, effect: BackgroundEffect) {
        let mut held = self
            .held_output
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if enabled != self.background_enabled() || effect != self.background_effect() {
            *held = HeldOutput::default();
            self.backgrounds.invalidate();
            self.clear_mask();
        }
        self.background_effect
            .store(effect_code(effect), Ordering::Relaxed);
        self.background_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn set_background_plugin(&self, id: &str) {
        let mut held = self.held_output.lock().unwrap_or_else(|e| e.into_inner());
        if self.backgrounds.selection().0 != id {
            self.backgrounds.select(id);
            *held = HeldOutput::default();
            self.clear_mask();
        }
    }

    #[cfg(test)]
    pub fn green_screen_enabled(&self) -> bool {
        self.background_enabled() && self.background_effect() == BackgroundEffect::GreenScreen
    }

    pub fn set_green_screen_enabled(&self, enabled: bool) {
        self.set_background(enabled, BackgroundEffect::GreenScreen);
    }

    pub fn publish_mask(&self, mask: VideoMask) {
        let mask = Arc::new(mask);
        let (store, available) = &*self.masks;
        let Ok(mut store) = store.lock() else {
            return;
        };
        store.latest = Some(Arc::clone(&mask));
        if let Some(index) = store
            .pending
            .iter()
            .position(|candidate| candidate.frame_id == mask.frame_id)
        {
            store.pending[index] = mask;
        } else {
            store.pending.push_back(mask);
            while store.pending.len() > MASK_BUFFER_CAPACITY {
                store.pending.pop_front();
            }
        }
        available.notify_all();
    }

    pub fn latest_mask(&self) -> Option<Arc<VideoMask>> {
        self.masks.0.lock().ok()?.latest.clone()
    }

    pub fn clear_mask(&self) {
        let (store, available) = &*self.masks;
        if let Ok(mut store) = store.lock() {
            *store = MaskStore::default();
            available.notify_all();
        }
    }

    pub fn publish_avatar(&self, frame: AvatarFrame) {
        self.avatar_tx.send_replace(Some(Arc::new(frame)));
    }

    pub fn latest_avatar(&self) -> Option<Arc<AvatarFrame>> {
        self.avatar_tx.borrow().clone()
    }

    pub fn clear_avatar(&self) {
        self.avatar_tx.send_replace(None);
    }

    pub fn publish_depth(&self, frame: DepthFrame) {
        self.depth_tx.send_replace(Some(Arc::new(frame)));
    }

    pub fn latest_depth(&self) -> Option<Arc<DepthFrame>> {
        self.depth_tx.borrow().clone()
    }

    pub fn clear_depth(&self) {
        self.depth_tx.send_replace(None);
    }

    #[cfg(test)]
    pub fn apply_output(&self, frame: &mut [u8], width: u32, height: u32, now_ms: u64) -> bool {
        self.apply_output_for_frame(frame, width, height, now_ms, None)
    }

    pub fn apply_output_for_frame(
        &self,
        frame: &mut [u8],
        width: u32,
        height: u32,
        now_ms: u64,
        frame_id: Option<u64>,
    ) -> bool {
        // Serialize cache invalidation with processing so a settings change cannot
        // retain a frame produced for the previous identity or background effect.
        let mut held = self
            .held_output
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if held.width != width || held.height != height || held.pixels.len() != frame.len() {
            *held = HeldOutput::default();
        }
        let processed = match self.output_mode() {
            VideoOutputMode::Camera => {
                self.apply_background_for_frame(frame, width, height, now_ms, frame_id)
            }
            VideoOutputMode::ComicAvatar => {
                let expected_len = avatar_frame_len(width, height).ok();
                let avatar = self.latest_avatar().filter(|avatar| {
                    avatar.is_fresh(now_ms) && avatar.width == width && avatar.height == height
                });
                match (expected_len, avatar) {
                    (Some(expected_len), Some(avatar)) if frame.len() >= expected_len
                        && (avatar.mask.is_none() || !self.background_enabled() || self.background_effect() != BackgroundEffect::Shader || self.backgrounds.latest().is_some()) => {
                        frame[..expected_len].copy_from_slice(&avatar.pixels);
                        if let Some(mask) = &avatar.mask {
                            let background = self.background_effect();
                            if self.background_enabled() && background != BackgroundEffect::Blur {
                                match background {
                                    BackgroundEffect::GreenScreen => apply_green_screen(
                                        frame,
                                        width as usize,
                                        height as usize,
                                        mask,
                                    ),
                                    BackgroundEffect::PixelParty => {
                                        let mut scratch =
                                            self.scratch.lock().unwrap_or_else(|e| e.into_inner());
                                        apply_pixel_party(
                                            frame,
                                            width as usize,
                                            height as usize,
                                            mask,
                                            now_ms,
                                            &mut scratch,
                                        );
                                    }
                                    BackgroundEffect::Shader => {
                                        if let Some(background) = self.backgrounds.latest() {
                                            apply_shader_background(frame, width as usize, height as usize, mask, &background);
                                        }
                                    }
                                    BackgroundEffect::Blur => unreachable!(),
                                }
                            } else {
                                // A synthetic neutral backdrop also remains neutral when blurred.
                                // Never composite the camera beneath an avatar identity.
                                for (pixel, alpha) in
                                    frame.chunks_exact_mut(4).zip(mask.pixels.iter())
                                {
                                    for (channel, background) in [33u16, 28, 26].iter().enumerate()
                                    {
                                        pixel[channel] = ((pixel[channel] as u16 * *alpha as u16
                                            + background * (255 - *alpha as u16))
                                            / 255)
                                            as u8;
                                    }
                                }
                            }
                            for pixel in frame.chunks_exact_mut(4) {
                                pixel[3] = 0;
                            }
                        }
                        Some(true)
                    }
                    _ => None,
                }
            }
            VideoOutputMode::DepthMap => {
                let expected_len = avatar_frame_len(width, height).ok();
                let depth = self.latest_depth().filter(|depth| depth.is_fresh(now_ms));
                match (expected_len, depth) {
                    (Some(expected_len), Some(depth)) if frame.len() >= expected_len => {
                        render_depth_map(&mut frame[..expected_len], width, height, &depth);
                        Some(true)
                    }
                    _ => None,
                }
            }
        };
        let changed = match processed {
            Some(true) => {
                held.width = width;
                held.height = height;
                held.pixels.resize(frame.len(), 0);
                held.pixels.copy_from_slice(frame);
                true
            }
            Some(false) => {
                *held = HeldOutput::default();
                false
            }
            None => {
                if held.pixels.len() == frame.len() {
                    frame.copy_from_slice(&held.pixels);
                } else {
                    frame.fill(0);
                }
                true
            }
        };
        drop(held);
        let code = self.transform.load(Ordering::Relaxed);
        if code != 0 {
            self.transform_scratch
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .apply(
                    frame,
                    width,
                    height,
                    crate::video_transform::VideoTransform::from_code(code),
                );
        }
        changed || code != 0
    }

    #[cfg(test)]
    pub fn apply_background(&self, frame: &mut [u8], width: u32, height: u32, now_ms: u64) -> bool {
        self.apply_output_for_frame(frame, width, height, now_ms, None)
    }

    fn apply_background_for_frame(
        &self,
        frame: &mut [u8],
        width: u32,
        height: u32,
        now_ms: u64,
        frame_id: Option<u64>,
    ) -> Option<bool> {
        if !self.background_enabled() {
            return Some(false);
        }
        if width == 0 || height == 0 {
            frame.fill(0);
            return None;
        }
        let pixel_count = match (width as usize).checked_mul(height as usize) {
            Some(pixel_count) => pixel_count,
            None => {
                frame.fill(0);
                return None;
            }
        };
        let expected_len = match pixel_count.checked_mul(4) {
            Some(expected_len) => expected_len,
            None => {
                frame.fill(0);
                return None;
            }
        };
        if frame.len() < expected_len {
            frame.fill(0);
            return None;
        }
        let mask = match frame_id {
            Some(frame_id) => self.wait_for_mask(frame_id),
            None => self.latest_mask().filter(|mask| mask.is_fresh(now_ms)),
        };
        let Some(mask) = mask else {
            frame[..expected_len].fill(0);
            return None;
        };

        let width = width as usize;
        let height = height as usize;
        match self.background_effect() {
            BackgroundEffect::Shader => {
                let Some(background) = self.backgrounds.latest() else {
                    frame[..expected_len].fill(0);
                    return None;
                };
                apply_shader_background(frame, width, height, &mask, &background);
            }
            BackgroundEffect::GreenScreen => {
                apply_green_screen(frame, width, height, &mask);
            }
            BackgroundEffect::Blur => {
                let Ok(mut scratch) = self.scratch.lock() else {
                    frame[..expected_len].fill(0);
                    return None;
                };
                apply_background_blur(frame, width, height, &mask, &mut scratch);
            }
            BackgroundEffect::PixelParty => {
                let Ok(mut scratch) = self.scratch.lock() else {
                    frame[..expected_len].fill(0);
                    return None;
                };
                apply_pixel_party(frame, width, height, &mask, now_ms, &mut scratch);
            }
        }
        Some(true)
    }

    fn wait_for_mask(&self, frame_id: u64) -> Option<Arc<VideoMask>> {
        let deadline = Instant::now() + Duration::from_millis(MASK_SYNC_WAIT_MS);
        let (store, available) = &*self.masks;
        let mut store = store.lock().ok()?;
        loop {
            let now_ms = unix_ms();
            store.pending.retain(|mask| mask.is_fresh(now_ms));
            if let Some(index) = store
                .pending
                .iter()
                .position(|mask| mask.frame_id == frame_id)
            {
                return store.pending.remove(index);
            }
            // Perception uses latest-frame queues and may skip this source frame.
            // Once a newer mask is available, holding the last safe output is
            // preferable to stalling the video branch for an obsolete mask.
            // Always prefer an exact match above, even with out-of-order delivery.
            if store.pending.iter().any(|mask| mask.frame_id > frame_id) {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next_store, result) = available.wait_timeout(store, remaining).ok()?;
            store = next_store;
            if result.timed_out() {
                return None;
            }
        }
    }
}

fn effect_code(effect: BackgroundEffect) -> u8 {
    match effect {
        BackgroundEffect::GreenScreen => 0,
        BackgroundEffect::Blur => 1,
        BackgroundEffect::PixelParty => 2,
        BackgroundEffect::Shader => 3,
    }
}

fn effect_from_code(code: u8) -> BackgroundEffect {
    match code {
        1 => BackgroundEffect::Blur,
        2 => BackgroundEffect::PixelParty,
        3 => BackgroundEffect::Shader,
        _ => BackgroundEffect::GreenScreen,
    }
}

fn output_mode_code(mode: VideoOutputMode) -> u8 {
    match mode {
        VideoOutputMode::Camera => 0,
        VideoOutputMode::ComicAvatar => 1,
        VideoOutputMode::DepthMap => 2,
    }
}

fn output_mode_from_code(code: u8) -> VideoOutputMode {
    match code {
        1 => VideoOutputMode::ComicAvatar,
        2 => VideoOutputMode::DepthMap,
        _ => VideoOutputMode::Camera,
    }
}

fn render_depth_map(frame: &mut [u8], width: u32, height: u32, depth: &DepthFrame) {
    let width = width as usize;
    let height = height as usize;
    let depth_width = depth.width as usize;
    let depth_height = depth.height as usize;
    let range = depth.near - depth.far;
    for y in 0..height {
        let depth_y = y * depth_height / height;
        for x in 0..width {
            let depth_x = x * depth_width / width;
            let proximity = ((depth.values[depth_y * depth_width + depth_x] - depth.far) / range)
                .clamp(0.0, 1.0);
            let [blue, green, red] = depth_color(proximity);
            let offset = (y * width + x) * 4;
            frame[offset] = blue;
            frame[offset + 1] = green;
            frame[offset + 2] = red;
            frame[offset + 3] = 255;
        }
    }
}

fn depth_color(proximity: f32) -> [u8; 3] {
    // BGR anchors form a monotonic far-to-near progression suitable for video.
    const COLORS: [[u8; 3]; 6] = [
        [72, 16, 24],
        [196, 52, 35],
        [216, 180, 36],
        [58, 232, 134],
        [16, 158, 245],
        [20, 28, 166],
    ];
    let position = proximity.clamp(0.0, 1.0) * (COLORS.len() - 1) as f32;
    let left = (position.floor() as usize).min(COLORS.len() - 1);
    let right = (left + 1).min(COLORS.len() - 1);
    let fraction = position - left as f32;
    let mut color = [0; 3];
    for channel in 0..3 {
        color[channel] = (COLORS[left][channel] as f32 * (1.0 - fraction)
            + COLORS[right][channel] as f32 * fraction)
            .round() as u8;
    }
    color
}

fn apply_green_screen(frame: &mut [u8], width: usize, height: usize, mask: &VideoMask) {
    let mask_width = mask.width as usize;
    let mask_height = mask.height as usize;
    let mut mask_y = 0;
    let mut mask_y_error = 0;
    for y in 0..height {
        let mask_row = mask_y * mask_width;
        let mut mask_x = 0;
        let mut mask_x_error = 0;
        for x in 0..width {
            let offset = (y * width + x) * 4;
            let alpha = mask.alpha(mask_row + mask_x) as u16;
            match alpha {
                0 => {
                    frame[offset] = 0;
                    frame[offset + 1] = 255;
                    frame[offset + 2] = 0;
                }
                255 => {}
                _ => {
                    let background = 255 - alpha;
                    frame[offset] = ((frame[offset] as u16 * alpha) / 255) as u8;
                    frame[offset + 1] =
                        ((frame[offset + 1] as u16 * alpha + 255 * background) / 255) as u8;
                    frame[offset + 2] = ((frame[offset + 2] as u16 * alpha) / 255) as u8;
                }
            }

            mask_x_error += mask_width;
            while mask_x_error >= width {
                mask_x_error -= width;
                mask_x = (mask_x + 1).min(mask_width - 1);
            }
        }
        mask_y_error += mask_height;
        while mask_y_error >= height {
            mask_y_error -= height;
            mask_y = (mask_y + 1).min(mask_height - 1);
        }
    }
}

fn apply_background_blur(
    frame: &mut [u8],
    width: usize,
    height: usize,
    mask: &VideoMask,
    scratch: &mut EffectScratch,
) {
    let reduced_width = width.div_ceil(BLUR_DOWNSAMPLE);
    let reduced_height = height.div_ceil(BLUR_DOWNSAMPLE);
    let reduced_len = reduced_width * reduced_height * 3;
    let EffectScratch { reduced, temporary } = scratch;
    reduced.resize(reduced_len, 0);
    temporary.resize(reduced_len, 0);

    for reduced_y in 0..reduced_height {
        let source_y = (reduced_y * BLUR_DOWNSAMPLE + BLUR_DOWNSAMPLE / 2).min(height - 1);
        for reduced_x in 0..reduced_width {
            let source_x = (reduced_x * BLUR_DOWNSAMPLE + BLUR_DOWNSAMPLE / 2).min(width - 1);
            let source_offset = (source_y * width + source_x) * 4;
            let target_offset = (reduced_y * reduced_width + reduced_x) * 3;
            reduced[target_offset..target_offset + 3]
                .copy_from_slice(&frame[source_offset..source_offset + 3]);
        }
    }

    box_blur_horizontal(
        reduced,
        temporary,
        reduced_width,
        reduced_height,
        BLUR_RADIUS,
    );
    box_blur_vertical(
        temporary,
        reduced,
        reduced_width,
        reduced_height,
        BLUR_RADIUS,
    );

    let mask_width = mask.width as usize;
    let mask_height = mask.height as usize;
    let mut mask_y = 0;
    let mut mask_y_error = 0;
    for y in 0..height {
        let reduced_y = y / BLUR_DOWNSAMPLE;
        let mask_row = mask_y * mask_width;
        let mut mask_x = 0;
        let mut mask_x_error = 0;
        for x in 0..width {
            let offset = (y * width + x) * 4;
            let reduced_offset = (reduced_y * reduced_width + x / BLUR_DOWNSAMPLE) * 3;
            let alpha = mask.alpha(mask_row + mask_x) as u16;
            match alpha {
                0 => frame[offset..offset + 3]
                    .copy_from_slice(&reduced[reduced_offset..reduced_offset + 3]),
                255 => {}
                _ => {
                    let background = 255 - alpha;
                    frame[offset] = ((frame[offset] as u16 * alpha
                        + reduced[reduced_offset] as u16 * background)
                        / 255) as u8;
                    frame[offset + 1] = ((frame[offset + 1] as u16 * alpha
                        + reduced[reduced_offset + 1] as u16 * background)
                        / 255) as u8;
                    frame[offset + 2] = ((frame[offset + 2] as u16 * alpha
                        + reduced[reduced_offset + 2] as u16 * background)
                        / 255) as u8;
                }
            }

            mask_x_error += mask_width;
            while mask_x_error >= width {
                mask_x_error -= width;
                mask_x = (mask_x + 1).min(mask_width - 1);
            }
        }
        mask_y_error += mask_height;
        while mask_y_error >= height {
            mask_y_error -= height;
            mask_y = (mask_y + 1).min(mask_height - 1);
        }
    }
}

fn apply_pixel_party(
    frame: &mut [u8],
    width: usize,
    height: usize,
    mask: &VideoMask,
    now_ms: u64,
    scratch: &mut EffectScratch,
) {
    let reduced_width = width.div_ceil(PIXEL_PARTY_BLOCK_SIZE);
    let reduced_height = height.div_ceil(PIXEL_PARTY_BLOCK_SIZE);
    let reduced_len = reduced_width * reduced_height * 3;
    scratch.reduced.resize(reduced_len, 0);

    for reduced_y in 0..reduced_height {
        for reduced_x in 0..reduced_width {
            let target_offset = (reduced_y * reduced_width + reduced_x) * 3;
            let color =
                pixel_party_color(reduced_x, reduced_y, reduced_width, reduced_height, now_ms);
            scratch.reduced[target_offset..target_offset + 3].copy_from_slice(&color);
        }
    }

    let mask_width = mask.width as usize;
    let mask_height = mask.height as usize;
    let mut mask_y = 0;
    let mut mask_y_error = 0;
    for y in 0..height {
        let reduced_y = y / PIXEL_PARTY_BLOCK_SIZE;
        let mask_row = mask_y * mask_width;
        let mut mask_x = 0;
        let mut mask_x_error = 0;
        for x in 0..width {
            let offset = (y * width + x) * 4;
            let reduced_offset = (reduced_y * reduced_width + x / PIXEL_PARTY_BLOCK_SIZE) * 3;
            let alpha = mask.alpha(mask_row + mask_x) as u16;
            match alpha {
                0 => frame[offset..offset + 3]
                    .copy_from_slice(&scratch.reduced[reduced_offset..reduced_offset + 3]),
                255 => {}
                _ => {
                    let background = 255 - alpha;
                    for channel in 0..3 {
                        frame[offset + channel] = ((frame[offset + channel] as u16 * alpha
                            + scratch.reduced[reduced_offset + channel] as u16 * background)
                            / 255) as u8;
                    }
                }
            }

            mask_x_error += mask_width;
            while mask_x_error >= width {
                mask_x_error -= width;
                mask_x = (mask_x + 1).min(mask_width - 1);
            }
        }
        mask_y_error += mask_height;
        while mask_y_error >= height {
            mask_y_error -= height;
            mask_y = (mask_y + 1).min(mask_height - 1);
        }
    }
}

fn pixel_party_color(
    block_x: usize,
    block_y: usize,
    block_width: usize,
    block_height: usize,
    now_ms: u64,
) -> [u8; 3] {
    let x = (block_x as f32 + 0.5) / block_width.max(1) as f32;
    let y = (block_y as f32 + 0.5) / block_height.max(1) as f32;
    let scene_x = block_x * PIXEL_PARTY_SCENE_WIDTH / block_width.max(1);
    let scene_y = block_y * PIXEL_PARTY_SCENE_HEIGHT / block_height.max(1);
    let scene_offset = (scene_y.min(PIXEL_PARTY_SCENE_HEIGHT - 1) * PIXEL_PARTY_SCENE_WIDTH
        + scene_x.min(PIXEL_PARTY_SCENE_WIDTH - 1))
        * 3;
    // Frames and the bundled scene use BGR channel order.
    let mut color = [
        PIXEL_PARTY_SCENE[scene_offset],
        PIXEL_PARTY_SCENE[scene_offset + 1],
        PIXEL_PARTY_SCENE[scene_offset + 2],
    ];
    let phase = (now_ms % PIXEL_PARTY_CYCLE_MS) as f32 / PIXEL_PARTY_CYCLE_MS as f32
        * std::f32::consts::TAU;

    // The scene supplies the art direction. Animation stays local and restrained:
    // the monitor and practical lamp breathe, while a few shelf highlights twinkle.
    let monitor_weight = pixel_party_radial_weight(x, y, 0.27, 0.44, 0.25, 0.28);
    let monitor_pulse = (phase + x * 2.5).sin().mul_add(0.5, 0.5);
    color = scale_pixel_party_color(color, 1.0 + monitor_weight * (monitor_pulse - 0.5) * 0.08);

    let lamp_weight = pixel_party_radial_weight(x, y, 0.80, 0.43, 0.13, 0.24);
    let lamp_pulse = (phase * 0.72 + 0.8).sin().mul_add(0.5, 0.5);
    color = scale_pixel_party_color(color, 1.0 + lamp_weight * (lamp_pulse - 0.5) * 0.07);

    let luminance = (color[0] as u16 + color[1] as u16 + color[2] as u16) / 3;
    let highlight_seed = pixel_party_hash(scene_x, scene_y);
    if x > 0.56 && y < 0.64 && luminance > 92 && highlight_seed > 0.86 {
        let twinkle = (phase * 0.55 + highlight_seed * std::f32::consts::TAU).sin();
        color = scale_pixel_party_color(color, 1.0 + twinkle * 0.035);
    }
    color
}

fn pixel_party_radial_weight(
    x: f32,
    y: f32,
    center_x: f32,
    center_y: f32,
    radius_x: f32,
    radius_y: f32,
) -> f32 {
    let distance =
        (((x - center_x) / radius_x).powi(2) + ((y - center_y) / radius_y).powi(2)).sqrt();
    (1.0 - distance).clamp(0.0, 1.0)
}

fn scale_pixel_party_color(color: [u8; 3], amount: f32) -> [u8; 3] {
    std::array::from_fn(|channel| (color[channel] as f32 * amount).round().clamp(0.0, 255.0) as u8)
}

fn pixel_party_hash(x: usize, y: usize) -> f32 {
    let mut value = (x as u32)
        .wrapping_mul(374_761_393)
        .wrapping_add((y as u32).wrapping_mul(668_265_263));
    value = (value ^ (value >> 13)).wrapping_mul(1_274_126_177);
    ((value ^ (value >> 16)) & 0xffff) as f32 / u16::MAX as f32
}

fn box_blur_horizontal(
    source: &[u8],
    target: &mut [u8],
    width: usize,
    height: usize,
    radius: usize,
) {
    let window = (radius * 2 + 1) as u32;
    for y in 0..height {
        for channel in 0..3 {
            let mut sum = 0_u32;
            for offset in 0..=radius * 2 {
                let x = offset.saturating_sub(radius).min(width - 1);
                sum += source[(y * width + x) * 3 + channel] as u32;
            }
            for x in 0..width {
                target[(y * width + x) * 3 + channel] = (sum / window) as u8;
                if x + 1 < width {
                    let remove_x = x.saturating_sub(radius);
                    let add_x = (x + radius + 1).min(width - 1);
                    sum -= source[(y * width + remove_x) * 3 + channel] as u32;
                    sum += source[(y * width + add_x) * 3 + channel] as u32;
                }
            }
        }
    }
}

fn box_blur_vertical(source: &[u8], target: &mut [u8], width: usize, height: usize, radius: usize) {
    let window = (radius * 2 + 1) as u32;
    for x in 0..width {
        for channel in 0..3 {
            let mut sum = 0_u32;
            for offset in 0..=radius * 2 {
                let y = offset.saturating_sub(radius).min(height - 1);
                sum += source[(y * width + x) * 3 + channel] as u32;
            }
            for y in 0..height {
                target[(y * width + x) * 3 + channel] = (sum / window) as u8;
                if y + 1 < height {
                    let remove_y = y.saturating_sub(radius);
                    let add_y = (y + radius + 1).min(height - 1);
                    sum -= source[(remove_y * width + x) * 3 + channel] as u32;
                    sum += source[(add_y * width + x) * 3 + channel] as u32;
                }
            }
        }
    }
}

impl Default for VideoEffects {
    fn default() -> Self {
        Self::new()
    }
}

fn feather_alpha(probability: u8) -> u8 {
    const BACKGROUND_LIMIT: u8 = 107;
    const FOREGROUND_LIMIT: u8 = 148;
    match probability {
        ..=BACKGROUND_LIMIT => 0,
        FOREGROUND_LIMIT.. => 255,
        probability => {
            let numerator = u16::from(probability - BACKGROUND_LIMIT) * 255;
            (numerator / u16::from(FOREGROUND_LIMIT - BACKGROUND_LIMIT)) as u8
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatar_alpha_composites_background_without_camera_or_perception_mask() {
        let effects = VideoEffects::default();
        effects.set_output_mode(VideoOutputMode::ComicAvatar);
        let now = unix_ms();
        effects.publish_avatar(
            AvatarFrame::new(
                7,
                now,
                3,
                1,
                vec![10, 20, 30, 255, 80, 90, 100, 0, 100, 100, 100, 128],
            )
            .unwrap()
            .with_alpha(true),
        );
        effects.set_background(true, BackgroundEffect::GreenScreen);
        let mut frame = vec![213; 12];
        effects.apply_output(&mut frame, 3, 1, now);
        assert_eq!(&frame[..4], &[10, 20, 30, 0]);
        assert_eq!(&frame[4..8], &[0, 255, 0, 0]);
        assert_eq!(&frame[8..12], &[50, 177, 50, 0]);
        effects.set_background(false, BackgroundEffect::GreenScreen);
        effects.apply_output(&mut frame, 3, 1, now);
        assert_eq!(&frame[4..8], &[33, 28, 26, 0]);
        effects.set_background(true, BackgroundEffect::PixelParty);
        effects.apply_output(&mut frame, 3, 1, now);
        assert_eq!(&frame[..4], &[10, 20, 30, 0]);
        assert_ne!(&frame[4..8], &[213; 4]);
        effects.clear_avatar();
        effects.set_background(true, BackgroundEffect::GreenScreen);
        frame.fill(213);
        effects.apply_output(&mut frame, 3, 1, now);
        assert_eq!(frame, vec![0; 12]);
    }

    #[test]
    fn validates_mask_dimensions_and_length() {
        assert!(VideoMask::new(1, 100, 0, 1, vec![]).is_err());
        assert!(VideoMask::new(1, 100, 2, 2, vec![0; 3]).is_err());
        assert!(VideoMask::new(1, 100, 2, 2, vec![0; 4]).is_ok());
    }

    #[test]
    fn validates_avatar_dimensions_and_length() {
        assert!(AvatarFrame::new(1, 100, 0, 1, vec![]).is_err());
        assert!(AvatarFrame::new(1, 100, 2, 2, vec![0; 15]).is_err());
        assert!(AvatarFrame::new(1, 100, 2, 2, vec![0; 16]).is_ok());
    }

    #[test]
    fn validates_and_preserves_relative_depth_values() {
        let bytes = [0.25_f32, 1.5, 2.75, 4.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let depth = DepthFrame::new(7, 100, 2, 2, 0.25, 4.0, &bytes).unwrap();

        assert_eq!(depth.values(), &[0.25, 1.5, 2.75, 4.0]);
        assert!(DepthFrame::new(7, 100, 2, 2, 0.25, 4.0, &bytes[..12]).is_err());
        assert!(DepthFrame::new(7, 100, 2, 2, 4.0, 0.25, &bytes).is_err());

        let mut invalid = bytes;
        invalid[..4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(DepthFrame::new(7, 100, 2, 2, 0.25, 4.0, &invalid).is_err());
    }

    #[test]
    fn comic_avatar_replaces_the_entire_camera_frame() {
        let effects = VideoEffects::new();
        effects.set_output_mode(VideoOutputMode::ComicAvatar);
        let captured_at_ms = unix_ms();
        let avatar =
            AvatarFrame::new(7, captured_at_ms, 2, 1, vec![10, 20, 30, 0, 40, 50, 60, 0]).unwrap();
        let published_at_ms = avatar.published_at_ms;
        effects.publish_avatar(avatar);
        let mut frame = [255; 8];

        assert!(effects.apply_output(&mut frame, 2, 1, published_at_ms));
        assert_eq!(frame, [10, 20, 30, 0, 40, 50, 60, 0]);
    }

    #[test]
    fn comic_avatar_fails_closed_when_the_frame_is_stale_or_mismatched() {
        let effects = VideoEffects::new();
        effects.set_output_mode(VideoOutputMode::ComicAvatar);
        let captured_at_ms = unix_ms();
        let avatar = AvatarFrame::new(7, captured_at_ms, 1, 1, vec![1, 2, 3, 0]).unwrap();
        let published_at_ms = avatar.published_at_ms;
        effects.publish_avatar(avatar);

        let mut mismatched = [255; 8];
        assert!(effects.apply_output(&mut mismatched, 2, 1, published_at_ms));
        assert_eq!(mismatched, [0; 8]);

        let mut stale = [255; 4];
        assert!(effects.apply_output(&mut stale, 1, 1, published_at_ms + AVATAR_MAX_AGE_MS + 1,));
        assert_eq!(stale, [0; 4]);
    }

    #[test]
    fn depth_map_colorizes_relative_values_and_holds_when_stale() {
        let effects = VideoEffects::new();
        effects.set_output_mode(VideoOutputMode::DepthMap);
        let captured_at_ms = unix_ms();
        let bytes = [0.0_f32, 1.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let depth = DepthFrame::new(8, captured_at_ms, 2, 1, 0.0, 1.0, &bytes).unwrap();
        let published_at_ms = depth.published_at_ms;
        effects.publish_depth(depth);

        let mut frame = [0; 8];
        assert!(effects.apply_output(&mut frame, 2, 1, published_at_ms));
        assert_ne!(&frame[..3], &frame[4..7]);
        assert_eq!(frame[3], 255);
        assert_eq!(frame[7], 255);

        let mut stale = [255; 8];
        assert!(effects.apply_output(&mut stale, 2, 1, published_at_ms + DEPTH_MAX_AGE_MS + 1,));
        assert_eq!(stale, frame);
    }

    #[test]
    fn green_screen_keeps_foreground_and_replaces_background() {
        let effects = VideoEffects::new();
        effects.set_green_screen_enabled(true);
        let captured_at_ms = unix_ms();
        let mask = VideoMask::new(1, captured_at_ms, 2, 1, vec![0, 255]).unwrap();
        let published_at_ms = mask.published_at_ms;
        effects.publish_mask(mask);
        let mut frame = [
            10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ];

        assert!(effects.apply_background(&mut frame, 4, 1, published_at_ms));
        assert_eq!(
            frame,
            [
                0, 255, 0, 255, 0, 255, 0, 255, 70, 80, 90, 255, 100, 110, 120, 255,
            ]
        );
    }

    #[test]
    fn green_screen_pairs_a_camera_frame_with_its_exact_mask() {
        let effects = VideoEffects::new();
        effects.set_green_screen_enabled(true);
        let captured_at_ms = unix_ms();
        effects.publish_mask(VideoMask::new(10, captured_at_ms, 2, 1, vec![255, 0]).unwrap());
        effects.publish_mask(VideoMask::new(20, captured_at_ms, 2, 1, vec![0, 255]).unwrap());
        let mut frame = [10, 20, 30, 255, 40, 50, 60, 255];

        assert!(effects.apply_output_for_frame(&mut frame, 2, 1, captured_at_ms, Some(10),));
        assert_eq!(frame, [10, 20, 30, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn effect_transitions_clear_masks_and_fail_closed_until_matching_frame() {
        let effects = VideoEffects::new();
        let now = unix_ms();
        effects.set_background(true, BackgroundEffect::GreenScreen);
        effects.publish_mask(VideoMask::new(1, now, 1, 1, vec![255]).unwrap());
        effects.set_background(false, BackgroundEffect::GreenScreen);
        assert!(effects.latest_mask().is_none());
        effects.set_background(true, BackgroundEffect::GreenScreen);
        let mut frame = [99; 4];
        effects.apply_output_for_frame(&mut frame, 1, 1, now, Some(2));
        assert_eq!(frame, [0; 4]);
        // A late result from before the transition cannot unmask a new frame.
        effects.publish_mask(VideoMask::new(1, now, 1, 1, vec![255]).unwrap());
        frame.fill(99);
        effects.apply_output_for_frame(&mut frame, 1, 1, now, Some(2));
        assert_eq!(frame, [0; 4]);
        let now = unix_ms();
        effects.publish_mask(VideoMask::new(2, now, 1, 1, vec![255]).unwrap());
        frame.fill(99);
        effects.apply_output_for_frame(&mut frame, 1, 1, now, Some(2));
        assert_eq!(frame, [99; 4]);
        effects.set_output_mode(VideoOutputMode::DepthMap);
        assert!(effects.latest_mask().is_none());
        effects.publish_mask(VideoMask::new(3, now, 1, 1, vec![255]).unwrap());
        effects.set_output_mode(VideoOutputMode::Camera);
        assert!(effects.latest_mask().is_none());
    }

    #[test]
    fn disabled_effect_passes_through_but_stale_mask_fails_closed() {
        let effects = VideoEffects::new();
        let captured_at_ms = unix_ms();
        let mask = VideoMask::new(1, captured_at_ms, 1, 1, vec![0]).unwrap();
        let published_at_ms = mask.published_at_ms;
        effects.publish_mask(mask);
        let mut frame = [10, 20, 30, 255];

        assert!(!effects.apply_background(&mut frame, 1, 1, published_at_ms));
        assert_eq!(frame, [10, 20, 30, 255]);
        effects.set_green_screen_enabled(true);
        assert!(effects.apply_background(&mut frame, 1, 1, published_at_ms + MASK_MAX_AGE_MS + 1,));
        assert_eq!(frame, [0, 0, 0, 0]);

        frame = [10, 20, 30, 255];
        effects.set_background(true, BackgroundEffect::Blur);
        assert!(effects.apply_background(&mut frame, 1, 1, published_at_ms + MASK_MAX_AGE_MS + 1,));
        assert_eq!(frame, [0, 0, 0, 0]);

        frame = [10, 20, 30, 255];
        effects.set_background(true, BackgroundEffect::PixelParty);
        assert!(effects.apply_background(&mut frame, 1, 1, published_at_ms + MASK_MAX_AGE_MS + 1,));
        assert_eq!(frame, [0, 0, 0, 0]);
    }

    #[test]
    fn background_holds_processed_pixels_on_timeout_and_resumes_with_a_matching_mask() {
        for effect in [
            BackgroundEffect::GreenScreen,
            BackgroundEffect::Blur,
            BackgroundEffect::PixelParty,
        ] {
            let effects = VideoEffects::new();
            effects.set_background(true, effect);
            let now = unix_ms();
            effects.publish_mask(VideoMask::new(1, now, 2, 1, vec![255, 0]).unwrap());
            let mut frame = [10, 20, 30, 255, 40, 50, 60, 255];
            effects.apply_output_for_frame(&mut frame, 2, 1, now, Some(1));
            let processed = frame;

            frame.fill(99);
            effects.apply_output_for_frame(&mut frame, 2, 1, now, Some(2));
            assert_eq!(frame, processed, "{effect:?}");
            effects.clear_mask();
            frame.fill(88);
            effects.apply_output(&mut frame, 2, 1, now + 10_000);
            assert_eq!(frame, processed, "{effect:?}");

            let now = unix_ms();
            effects.publish_mask(VideoMask::new(3, now, 2, 1, vec![255, 0]).unwrap());
            frame.fill(77);
            effects.apply_output_for_frame(&mut frame, 2, 1, now, Some(3));
            assert_eq!(&frame[..4], &[77; 4]);
        }
    }

    #[test]
    fn avatar_holds_without_reapplying_transform_and_resumes() {
        let effects = VideoEffects::new();
        effects.set_output_mode(VideoOutputMode::ComicAvatar);
        effects.set_transform(crate::video_transform::VideoTransform {
            rotation: 0,
            mirror: true,
        });
        let now = unix_ms();
        effects.publish_avatar(
            AvatarFrame::new(1, now, 2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]).unwrap(),
        );
        let mut frame = [99; 8];
        effects.apply_output(&mut frame, 2, 1, now);
        assert_eq!(frame, [4, 5, 6, 255, 1, 2, 3, 255]);
        let processed = frame;
        for time in [now + AVATAR_MAX_AGE_MS + 1, now + 10_000] {
            frame.fill(99);
            effects.apply_output(&mut frame, 2, 1, time);
            assert_eq!(frame, processed);
        }
        effects.clear_avatar();
        frame.fill(99);
        effects.apply_output(&mut frame, 2, 1, now);
        assert_eq!(frame, processed);
        effects.publish_avatar(AvatarFrame::new(2, now, 2, 1, vec![7; 8]).unwrap());
        effects.apply_output(&mut frame, 2, 1, now);
        assert_eq!(frame, [7; 8]);
    }

    #[test]
    fn held_frame_is_invalidated_by_settings_and_dimensions() {
        let effects = VideoEffects::new();
        effects.set_background(true, BackgroundEffect::GreenScreen);
        let now = unix_ms();
        effects.publish_mask(VideoMask::new(1, now, 2, 1, vec![0, 255]).unwrap());
        let mut frame = [99; 8];
        effects.apply_output(&mut frame, 2, 1, now);
        effects.clear_mask();
        effects.set_background(true, BackgroundEffect::Blur);
        effects.apply_output(&mut frame, 2, 1, now);
        assert_eq!(frame, [0; 8]);

        effects.set_output_mode(VideoOutputMode::ComicAvatar);
        effects.publish_avatar(AvatarFrame::new(1, now, 2, 1, vec![7; 8]).unwrap());
        effects.apply_output(&mut frame, 2, 1, now);
        effects.clear_avatar();
        // Even equal byte lengths cannot reuse an image with different dimensions.
        effects.apply_output(&mut frame, 1, 2, now);
        assert_eq!(frame, [0; 8]);
        effects.publish_avatar(AvatarFrame::new(2, now, 2, 1, vec![7; 8]).unwrap());
        effects.apply_output(&mut frame, 2, 1, now);
        // Identity switches may keep the same output mode (another avatar engine).
        effects.clear_avatar();
        effects.set_output_mode(VideoOutputMode::ComicAvatar);
        effects.apply_output(&mut frame, 2, 1, now);
        assert_eq!(frame, [0; 8]);
        effects.set_output_mode(VideoOutputMode::Camera);
        effects.set_background(false, BackgroundEffect::Blur);
        frame.fill(42);
        assert!(!effects.apply_output(&mut frame, 2, 1, now));
        assert_eq!(frame, [42; 8]);
    }

    #[test]
    fn background_effect_selection_is_exclusive() {
        let effects = VideoEffects::new();
        effects.set_background(true, BackgroundEffect::Blur);

        assert!(effects.background_enabled());
        assert_eq!(effects.background_effect(), BackgroundEffect::Blur);
        assert!(!effects.green_screen_enabled());

        effects.set_background(true, BackgroundEffect::PixelParty);
        assert_eq!(effects.background_effect(), BackgroundEffect::PixelParty);
        assert!(!effects.green_screen_enabled());

        effects.set_green_screen_enabled(true);
        assert_eq!(effects.background_effect(), BackgroundEffect::GreenScreen);
        assert!(effects.green_screen_enabled());
    }

    #[test]
    fn blur_keeps_foreground_and_softens_background() {
        let effects = VideoEffects::new();
        effects.set_background(true, BackgroundEffect::Blur);
        let captured_at_ms = unix_ms();
        let mask_pixels = (0..16 * 8)
            .map(|index| if index == 2 * 16 + 3 { 255 } else { 0 })
            .collect();
        let mask = VideoMask::new(1, captured_at_ms, 16, 8, mask_pixels).unwrap();
        let published_at_ms = mask.published_at_ms;
        effects.publish_mask(mask);
        let mut frame = Vec::with_capacity(16 * 8 * 4);
        for index in 0..16 * 8 {
            let value = if index % 16 < 8 { 0 } else { 240 };
            frame.extend_from_slice(&[value, value, value, 255]);
        }
        let foreground_offset = (2 * 16 + 3) * 4;
        frame[foreground_offset..foreground_offset + 3].copy_from_slice(&[7, 17, 27]);

        assert!(effects.apply_background(&mut frame, 16, 8, published_at_ms));
        assert_eq!(
            &frame[foreground_offset..foreground_offset + 3],
            &[7, 17, 27]
        );
        assert_ne!(&frame[0..3], &[0, 0, 0]);
        assert_eq!(frame[3], 255);
    }

    #[test]
    fn blur_fails_closed_for_empty_dimensions() {
        let effects = VideoEffects::new();
        effects.set_background(true, BackgroundEffect::Blur);
        let mut frame = [255; 4];

        assert!(effects.apply_background(&mut frame, 0, 1, unix_ms()));
        assert_eq!(frame, [0; 4]);
    }

    #[test]
    fn pixel_party_keeps_foreground_crisp_and_replaces_background() {
        let effects = VideoEffects::new();
        effects.set_background(true, BackgroundEffect::PixelParty);
        let captured_at_ms = unix_ms();
        let mut mask_pixels = vec![0; 48 * 24];
        let foreground_index = 10 * 48 + 30;
        mask_pixels[foreground_index] = 255;
        let mask = VideoMask::new(1, captured_at_ms, 48, 24, mask_pixels).unwrap();
        let published_at_ms = mask.published_at_ms;
        effects.publish_mask(mask);
        let mut frame = Vec::with_capacity(48 * 24 * 4);
        for y in 0..24 {
            for x in 0..48 {
                frame.extend_from_slice(&[x as u8 * 3, y as u8 * 7, (x + y) as u8 * 2, 255]);
            }
        }
        let foreground_offset = foreground_index * 4;
        frame[foreground_offset..foreground_offset + 3].copy_from_slice(&[7, 17, 27]);
        let mut alternate_source = vec![250; 48 * 24 * 4];
        for alpha in alternate_source[3..].iter_mut().step_by(4) {
            *alpha = 255;
        }
        alternate_source[foreground_offset..foreground_offset + 3].copy_from_slice(&[7, 17, 27]);

        assert!(effects.apply_background(&mut frame, 48, 24, published_at_ms));
        assert!(effects.apply_background(&mut alternate_source, 48, 24, published_at_ms,));
        assert_eq!(
            &frame[foreground_offset..foreground_offset + 3],
            &[7, 17, 27]
        );
        assert_eq!(frame, alternate_source);
        assert_eq!(&frame[0..3], &frame[(8 * 48 + 8) * 4..(8 * 48 + 8) * 4 + 3]);
        assert_ne!(&frame[0..3], &frame[24 * 4..24 * 4 + 3]);
    }

    #[test]
    fn pixel_party_studio_animates_and_loops() {
        let start = pixel_party_color(4, 3, 12, 8, 0);

        assert_eq!(start, pixel_party_color(4, 3, 12, 8, PIXEL_PARTY_CYCLE_MS));
        assert_ne!(
            start,
            pixel_party_color(4, 3, 12, 8, PIXEL_PARTY_CYCLE_MS / 4)
        );
        assert_ne!(start, pixel_party_color(5, 3, 12, 8, 0));
    }
}

fn apply_shader_background(frame: &mut [u8], width: usize, height: usize, mask: &VideoMask, background: &crate::background::Frame) {
    for y in 0..height {
        let mask_row = y * mask.height as usize / height * mask.width as usize;
        let background_row = y * background.height / height * background.stride;
        for x in 0..width {
            let alpha = mask.alpha(mask_row + x * mask.width as usize / width) as u32;
            let source = background_row + x * background.width / width * 3;
            let offset = (y * width + x) * 4;
            for channel in 0..3 {
                frame[offset + channel] = ((frame[offset + channel] as u32 * alpha
                    + background.pixels[source + channel] as u32 * (255 - alpha)) / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod shader_tests {
    use super::*;
    #[test]
    fn baked_avatar_background_does_not_wait_for_shader() {
        let effects = VideoEffects::new();
        effects.set_background(true, BackgroundEffect::Shader);
        effects.set_output_mode(VideoOutputMode::ComicAvatar);
        let now = unix_ms();
        effects.publish_avatar(AvatarFrame::new(1, now, 2, 1, vec![70; 8]).unwrap());
        let mut frame = vec![200; 8];
        effects.apply_output(&mut frame, 2, 1, now);
        assert_eq!(frame, vec![70; 8]);
    }

    #[test]
    fn skipped_mask_reuses_safe_output_without_waiting_for_timeout() {
        let effects = VideoEffects::new();
        effects.set_background(true, BackgroundEffect::PixelParty);
        let now = unix_ms();
        effects.publish_mask(VideoMask::new(10, now, 2, 1, vec![255, 0]).unwrap());
        let mut frame = vec![90; 8];
        effects.apply_output_for_frame(&mut frame, 2, 1, now, Some(10));
        let held = frame.clone();
        // Frame 11 was dropped upstream; the next completed mask is for 12.
        effects.publish_mask(VideoMask::new(12, now, 2, 1, vec![255, 0]).unwrap());
        frame.fill(200);
        let started = Instant::now();
        effects.apply_output_for_frame(&mut frame, 2, 1, now, Some(11));
        assert!(started.elapsed() < Duration::from_millis(75));
        assert_eq!(frame, held);
        frame.fill(200);
        effects.apply_output_for_frame(&mut frame, 2, 1, now, Some(12));
        assert_eq!(&frame[..4], &[200; 4]);
    }

    #[test]
    fn exact_mask_wins_when_a_newer_mask_is_also_buffered() {
        let effects = VideoEffects::new();
        let now = unix_ms();
        effects.publish_mask(VideoMask::new(12, now, 1, 1, vec![0]).unwrap());
        effects.publish_mask(VideoMask::new(11, now, 1, 1, vec![255]).unwrap());
        assert_eq!(effects.wait_for_mask(11).unwrap().frame_id, 11);
        assert_eq!(effects.wait_for_mask(12).unwrap().frame_id, 12);
    }

    #[test]
    fn shader_composition_preserves_subject_and_holds_on_missing_mask() {
        let effects = VideoEffects::new();
        effects.set_background(true, BackgroundEffect::Shader);
        let now = unix_ms();
        let mut frame = vec![90; 8];
        effects.publish_mask(VideoMask::new(1, now, 2, 1, vec![255, 0]).unwrap());
        effects.apply_output(&mut frame, 2, 1, now);
        assert_eq!(frame, vec![0; 8]);
        let generation = effects.backgrounds.selection().1;
        effects.backgrounds.publish(generation, crate::background::Frame { width: 1, height: 1, stride: 3, pixels: vec![10, 20, 30] });
        frame.fill(90);
        effects.apply_output(&mut frame, 2, 1, now);
        assert_eq!(&frame[..4], &[90; 4]);
        assert_eq!(&frame[4..7], &[10, 20, 30]);
        let expected = frame.clone();
        effects.clear_mask();
        frame.fill(200);
        effects.apply_output(&mut frame, 2, 1, now);
        assert_eq!(frame, expected);
        effects.set_background_plugin("another-plugin");
        frame.fill(200);
        effects.apply_output(&mut frame, 2, 1, now);
        assert_eq!(frame, vec![0; 8]);
    }
}
