use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use anyhow::{Result, bail};
use tokio::sync::watch;

use crate::model::{BackgroundEffect, VideoOutputMode, unix_ms};

pub const MASK_MAX_AGE_MS: u64 = 200;
pub const AVATAR_MAX_AGE_MS: u64 = 500;
pub const DEPTH_MAX_AGE_MS: u64 = 500;
const MAX_MASK_PIXELS: usize = 1920 * 1080;
pub const MAX_AVATAR_FRAME_BYTES: usize = 1920 * 1080 * 4;
pub const MAX_DEPTH_FRAME_BYTES: usize = 1920 * 1080 * size_of::<f32>();
const BLUR_DOWNSAMPLE: usize = 8;
const BLUR_RADIUS: usize = 3;

#[derive(Clone, Debug)]
pub struct VideoMask {
    pub frame_id: u64,
    pub captured_at_ms: u64,
    pub published_at_ms: u64,
    pub width: u32,
    pub height: u32,
    pixels: Arc<[u8]>,
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
        })
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
        })
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
    output_mode: Arc<AtomicU8>,
    background_enabled: Arc<AtomicBool>,
    background_effect: Arc<AtomicU8>,
    mask_tx: watch::Sender<Option<Arc<VideoMask>>>,
    avatar_tx: watch::Sender<Option<Arc<AvatarFrame>>>,
    depth_tx: watch::Sender<Option<Arc<DepthFrame>>>,
    scratch: Arc<Mutex<EffectScratch>>,
}

#[derive(Default)]
struct EffectScratch {
    reduced: Vec<u8>,
    temporary: Vec<u8>,
}

impl VideoEffects {
    pub fn new() -> Self {
        let (mask_tx, _) = watch::channel(None);
        let (avatar_tx, _) = watch::channel(None);
        let (depth_tx, _) = watch::channel(None);
        Self {
            output_mode: Arc::new(AtomicU8::new(output_mode_code(VideoOutputMode::Camera))),
            background_enabled: Arc::new(AtomicBool::new(false)),
            background_effect: Arc::new(AtomicU8::new(effect_code(BackgroundEffect::GreenScreen))),
            mask_tx,
            avatar_tx,
            depth_tx,
            scratch: Arc::new(Mutex::new(EffectScratch::default())),
        }
    }

    pub fn output_mode(&self) -> VideoOutputMode {
        output_mode_from_code(self.output_mode.load(Ordering::Relaxed))
    }

    pub fn set_output_mode(&self, mode: VideoOutputMode) {
        self.output_mode
            .store(output_mode_code(mode), Ordering::Relaxed);
    }

    pub fn processing_enabled(&self) -> bool {
        self.output_mode() != VideoOutputMode::Camera || self.background_enabled()
    }

    pub fn background_enabled(&self) -> bool {
        self.background_enabled.load(Ordering::Relaxed)
    }

    pub fn background_effect(&self) -> BackgroundEffect {
        effect_from_code(self.background_effect.load(Ordering::Relaxed))
    }

    pub fn set_background(&self, enabled: bool, effect: BackgroundEffect) {
        self.background_effect
            .store(effect_code(effect), Ordering::Relaxed);
        self.background_enabled.store(enabled, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn green_screen_enabled(&self) -> bool {
        self.background_enabled() && self.background_effect() == BackgroundEffect::GreenScreen
    }

    pub fn set_green_screen_enabled(&self, enabled: bool) {
        self.set_background(enabled, BackgroundEffect::GreenScreen);
    }

    pub fn publish_mask(&self, mask: VideoMask) {
        self.mask_tx.send_replace(Some(Arc::new(mask)));
    }

    pub fn latest_mask(&self) -> Option<Arc<VideoMask>> {
        self.mask_tx.borrow().clone()
    }

    pub fn clear_mask(&self) {
        self.mask_tx.send_replace(None);
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

    pub fn apply_output(&self, frame: &mut [u8], width: u32, height: u32, now_ms: u64) -> bool {
        match self.output_mode() {
            VideoOutputMode::Camera => self.apply_background(frame, width, height, now_ms),
            VideoOutputMode::ComicAvatar => {
                let expected_len = avatar_frame_len(width, height).ok();
                let avatar = self.latest_avatar().filter(|avatar| {
                    avatar.is_fresh(now_ms) && avatar.width == width && avatar.height == height
                });
                match (expected_len, avatar) {
                    (Some(expected_len), Some(avatar)) if frame.len() >= expected_len => {
                        frame[..expected_len].copy_from_slice(&avatar.pixels);
                    }
                    _ => frame.fill(0),
                }
                true
            }
            VideoOutputMode::DepthMap => {
                let expected_len = avatar_frame_len(width, height).ok();
                let depth = self.latest_depth().filter(|depth| depth.is_fresh(now_ms));
                match (expected_len, depth) {
                    (Some(expected_len), Some(depth)) if frame.len() >= expected_len => {
                        render_depth_map(&mut frame[..expected_len], width, height, &depth);
                    }
                    _ => frame.fill(0),
                }
                true
            }
        }
    }

    pub fn apply_background(&self, frame: &mut [u8], width: u32, height: u32, now_ms: u64) -> bool {
        if !self.background_enabled() {
            return false;
        }
        if width == 0 || height == 0 {
            frame.fill(0);
            return true;
        }
        let pixel_count = match (width as usize).checked_mul(height as usize) {
            Some(pixel_count) => pixel_count,
            None => {
                frame.fill(0);
                return true;
            }
        };
        let expected_len = match pixel_count.checked_mul(4) {
            Some(expected_len) => expected_len,
            None => {
                frame.fill(0);
                return true;
            }
        };
        if frame.len() < expected_len {
            frame.fill(0);
            return true;
        }
        let Some(mask) = self.latest_mask().filter(|mask| mask.is_fresh(now_ms)) else {
            frame[..expected_len].fill(0);
            return true;
        };

        let width = width as usize;
        let height = height as usize;
        match self.background_effect() {
            BackgroundEffect::GreenScreen => {
                apply_green_screen(frame, width, height, &mask);
            }
            BackgroundEffect::Blur => {
                let Ok(mut scratch) = self.scratch.lock() else {
                    frame[..expected_len].fill(0);
                    return true;
                };
                apply_background_blur(frame, width, height, &mask, &mut scratch);
            }
        }
        true
    }
}

fn effect_code(effect: BackgroundEffect) -> u8 {
    match effect {
        BackgroundEffect::GreenScreen => 0,
        BackgroundEffect::Blur => 1,
    }
}

fn effect_from_code(code: u8) -> BackgroundEffect {
    match code {
        1 => BackgroundEffect::Blur,
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
            let alpha = feather_alpha(mask.pixels[mask_row + mask_x]) as u16;
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
            let alpha = feather_alpha(mask.pixels[mask_row + mask_x]) as u16;
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
    fn depth_map_colorizes_relative_values_and_fails_closed_when_stale() {
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
        assert_eq!(stale, [0; 8]);
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
    }

    #[test]
    fn background_effect_selection_is_exclusive() {
        let effects = VideoEffects::new();
        effects.set_background(true, BackgroundEffect::Blur);

        assert!(effects.background_enabled());
        assert_eq!(effects.background_effect(), BackgroundEffect::Blur);
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
}
