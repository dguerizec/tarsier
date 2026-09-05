use serde::{Deserialize, Serialize};

/// Clockwise rotation followed by a horizontal mirror in the output image.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VideoTransform {
    pub rotation: u16,
    pub mirror: bool,
}

impl VideoTransform {
    pub fn valid(self) -> bool {
        matches!(self.rotation, 0 | 90 | 180 | 270)
    }

    pub fn code(self) -> u8 {
        (self.rotation / 90) as u8 | (u8::from(self.mirror) << 2)
    }

    pub fn from_code(code: u8) -> Self {
        Self {
            rotation: u16::from(code & 3) * 90,
            mirror: code & 4 != 0,
        }
    }
}

#[derive(Default)]
pub struct TransformScratch {
    key: Option<(u32, u32, VideoTransform)>,
    indices: Vec<Option<usize>>,
    source: Vec<u8>,
}

impl TransformScratch {
    /// Fit the rotated image into the stable output canvas without cropping.
    pub fn apply(&mut self, frame: &mut [u8], width: u32, height: u32, transform: VideoTransform) {
        let (w, h) = (width as usize, height as usize);
        if w == 0 || h == 0 || frame.len() < w * h * 4 {
            frame.fill(0);
            return;
        }
        if self.key != Some((width, height, transform)) {
            let sideways = !transform.rotation.is_multiple_of(180);
            let (rw, rh) = if sideways { (h, w) } else { (w, h) };
            let scale = (w as f64 / rw as f64).min(h as f64 / rh as f64);
            let (dw, dh) = (
                (rw as f64 * scale).round() as usize,
                (rh as f64 * scale).round() as usize,
            );
            let (ox, oy) = ((w - dw) / 2, (h - dh) / 2);
            self.indices.clear();
            for y in 0..h {
                for x in 0..w {
                    let x = if transform.mirror { w - 1 - x } else { x };
                    if x < ox || x >= ox + dw || y < oy || y >= oy + dh {
                        self.indices.push(None);
                        continue;
                    }
                    let rx = (x - ox) * rw / dw;
                    let ry = (y - oy) * rh / dh;
                    let (sx, sy) = match transform.rotation {
                        90 => (ry, h - 1 - rx),
                        180 => (w - 1 - rx, h - 1 - ry),
                        270 => (w - 1 - ry, rx),
                        _ => (rx, ry),
                    };
                    self.indices.push(Some((sy * w + sx) * 4));
                }
            }
            self.key = Some((width, height, transform));
        }
        self.source.resize(w * h * 4, 0);
        self.source.copy_from_slice(&frame[..w * h * 4]);
        for (pixel, index) in frame.chunks_exact_mut(4).zip(&self.indices) {
            if let Some(index) = index {
                pixel.copy_from_slice(&self.source[*index..*index + 4]);
            } else {
                pixel.fill(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotations_and_mirror_preserve_pixel_order() {
        for (rotation, expected) in [
            (0, [1, 2, 3, 4]),
            (90, [3, 1, 4, 2]),
            (180, [4, 3, 2, 1]),
            (270, [2, 4, 1, 3]),
        ] {
            for mirror in [false, true] {
                let transform = VideoTransform { rotation, mirror };
                assert_eq!(VideoTransform::from_code(transform.code()), transform);
                let mut frame = [1, 2, 3, 4]
                    .into_iter()
                    .flat_map(|v| [v; 4])
                    .collect::<Vec<_>>();
                TransformScratch::default().apply(&mut frame, 2, 2, transform);
                let mut expected = expected;
                if mirror {
                    expected.swap(0, 1);
                    expected.swap(2, 3);
                }
                assert_eq!(
                    frame.chunks_exact(4).map(|p| p[0]).collect::<Vec<_>>(),
                    expected
                );
            }
        }
    }

    #[test]
    fn portrait_fits_inside_landscape_and_clears_borders() {
        let mut frame = vec![255; 8 * 4 * 4];
        TransformScratch::default().apply(
            &mut frame,
            8,
            4,
            VideoTransform {
                rotation: 90,
                mirror: false,
            },
        );
        for row in frame.chunks_exact(32) {
            assert!(row[..12].iter().all(|v| *v == 0));
            assert!(row[12..20].iter().all(|v| *v == 255));
            assert!(row[20..].iter().all(|v| *v == 0));
        }
    }
}
