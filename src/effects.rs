use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use anyhow::{Result, bail};
use tokio::sync::watch;

use crate::model::{BackgroundEffect, unix_ms};

pub const MASK_MAX_AGE_MS: u64 = 200;
const MAX_MASK_PIXELS: usize = 1920 * 1080;
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
    background_enabled: Arc<AtomicBool>,
    background_effect: Arc<AtomicU8>,
    mask_tx: watch::Sender<Option<Arc<VideoMask>>>,
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
        Self {
            background_enabled: Arc::new(AtomicBool::new(false)),
            background_effect: Arc::new(AtomicU8::new(effect_code(BackgroundEffect::GreenScreen))),
            mask_tx,
            scratch: Arc::new(Mutex::new(EffectScratch::default())),
        }
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
