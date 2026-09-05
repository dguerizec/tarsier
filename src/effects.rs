use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Result, bail};
use tokio::sync::watch;

use crate::model::unix_ms;

pub const MASK_MAX_AGE_MS: u64 = 750;
const MAX_MASK_PIXELS: usize = 1920 * 1080;

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
    }

    fn sample(&self, x: usize, y: usize, output_width: usize, output_height: usize) -> u8 {
        let mask_x = x.saturating_mul(self.width as usize) / output_width;
        let mask_y = y.saturating_mul(self.height as usize) / output_height;
        self.pixels[mask_y * self.width as usize + mask_x]
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
    green_screen_enabled: Arc<AtomicBool>,
    mask_tx: watch::Sender<Option<Arc<VideoMask>>>,
}

impl VideoEffects {
    pub fn new() -> Self {
        let (mask_tx, _) = watch::channel(None);
        Self {
            green_screen_enabled: Arc::new(AtomicBool::new(false)),
            mask_tx,
        }
    }

    pub fn green_screen_enabled(&self) -> bool {
        self.green_screen_enabled.load(Ordering::Relaxed)
    }

    pub fn set_green_screen_enabled(&self, enabled: bool) {
        self.green_screen_enabled.store(enabled, Ordering::Relaxed);
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

    pub fn apply_green_screen(
        &self,
        frame: &mut [u8],
        width: u32,
        height: u32,
        now_ms: u64,
    ) -> bool {
        if !self.green_screen_enabled() {
            return false;
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
        for y in 0..height {
            for x in 0..width {
                let offset = (y * width + x) * 4;
                let alpha = feather_alpha(mask.sample(x, y, width, height)) as u16;
                let background = 255 - alpha;
                frame[offset] = ((frame[offset] as u16 * alpha) / 255) as u8;
                frame[offset + 1] =
                    ((frame[offset + 1] as u16 * alpha + 255 * background) / 255) as u8;
                frame[offset + 2] = ((frame[offset + 2] as u16 * alpha) / 255) as u8;
            }
        }
        true
    }
}

impl Default for VideoEffects {
    fn default() -> Self {
        Self::new()
    }
}

fn feather_alpha(probability: u8) -> u8 {
    const BACKGROUND_LIMIT: u8 = 51;
    const FOREGROUND_LIMIT: u8 = 204;
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
        let mask = VideoMask::new(1, 100, 2, 1, vec![255, 0]).unwrap();
        let published_at_ms = mask.published_at_ms;
        effects.publish_mask(mask);
        let mut frame = [10, 20, 30, 255, 40, 50, 60, 255];

        assert!(effects.apply_green_screen(&mut frame, 2, 1, published_at_ms));
        assert_eq!(frame, [10, 20, 30, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn disabled_effect_passes_through_but_stale_mask_fails_closed() {
        let effects = VideoEffects::new();
        let mask = VideoMask::new(1, 100, 1, 1, vec![0]).unwrap();
        let published_at_ms = mask.published_at_ms;
        effects.publish_mask(mask);
        let mut frame = [10, 20, 30, 255];

        assert!(!effects.apply_green_screen(&mut frame, 1, 1, published_at_ms));
        assert_eq!(frame, [10, 20, 30, 255]);
        effects.set_green_screen_enabled(true);
        assert!(effects.apply_green_screen(
            &mut frame,
            1,
            1,
            published_at_ms + MASK_MAX_AGE_MS + 1,
        ));
        assert_eq!(frame, [0, 0, 0, 0]);
    }
}
