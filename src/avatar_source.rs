use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use image::{ImageDecoder, ImageFormat, ImageReader};

pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;

/// Decode within fixed limits and use the same centered square as LivePortrait.
pub fn normalize(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() > MAX_UPLOAD_BYTES {
        bail!("portrait is too large");
    }
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    if !matches!(reader.format(), Some(ImageFormat::Png | ImageFormat::Jpeg)) {
        bail!("portrait must be PNG or JPEG");
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    if decoder.total_bytes() > 128 * 1024 * 1024 {
        bail!("decoded portrait is too large");
    }
    let orientation = decoder.orientation()?;
    let mut image = image::DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let size = image.width().min(image.height());
    if size < 256 {
        bail!("portrait must be at least 256 pixels on each side");
    }
    let image = image
        .crop_imm(
            (image.width() - size) / 2,
            (image.height() - size) / 2,
            size,
            size,
        )
        .resize_exact(512, 512, image::imageops::FilterType::Lanczos3);
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image.to_rgb8()).write_to(&mut output, ImageFormat::Png)?;
    Ok(output.into_inner())
}

#[derive(Clone, serde::Serialize)]
pub struct LivePortraitState {
    pub source: PathBuf,
    pub revision: u64,
    pub active_revision: u64,
    pub error: Option<String>,
}

impl LivePortraitState {
    pub fn new(source: PathBuf) -> Self {
        Self {
            source,
            revision: 0,
            active_revision: 0,
            error: None,
        }
    }

    pub fn select(&mut self, source: PathBuf) {
        self.source = source;
        self.revision += 1;
        self.error = None;
    }

    /// Keep accepting the old portrait until the first complete new frame arrives.
    pub fn accept_frame(&mut self, revision: u64) -> bool {
        if revision == self.revision {
            self.active_revision = revision;
            self.error = None;
            true
        } else {
            revision == self.active_revision
        }
    }
}

#[derive(Clone, serde::Serialize)]
pub struct Portrait {
    pub id: String,
    pub name: String,
    pub selected: bool,
    #[serde(skip)]
    pub path: PathBuf,
}

pub fn catalog(directories: &[PathBuf], current: &Path) -> Result<Vec<Portrait>> {
    use sha2::{Digest, Sha256};
    let current = current.canonicalize().ok();
    let mut paths = Vec::new();
    for directory in directories {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                paths.push(entry.path());
            }
        }
    }
    if let Some(current) = &current {
        paths.push(current.clone());
    }
    let mut portraits = Vec::new();
    for path in paths {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "png" | "jpg" | "jpeg") {
            continue;
        }
        let path = path.canonicalize()?;
        if portraits
            .iter()
            .any(|portrait: &Portrait| portrait.path == path)
        {
            continue;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        portraits.push(Portrait {
            id: format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes())),
            name,
            selected: current.as_ref() == Some(&path),
            path,
        });
    }
    portraits.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(portraits)
}

pub fn thumbnail(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((MAX_UPLOAD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    normalize(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_gate_keeps_old_frames_until_new_frame_and_rejects_obsolete_work() {
        let mut state = LivePortraitState::new("old.png".into());
        state.select("first.png".into());
        assert!(state.accept_frame(0));
        state.select("latest.png".into());
        assert!(!state.accept_frame(1));
        assert!(state.accept_frame(0));
        state.error = Some("failed preparation".into());
        assert!(state.accept_frame(0));
        assert!(state.error.is_some());
        assert!(state.accept_frame(2));
        assert!(state.error.is_none());
        assert!(!state.accept_frame(0));
        assert_eq!(state.active_revision, 2);
    }

    #[test]
    fn catalog_filters_deduplicates_and_marks_current_portrait() {
        let directory =
            std::env::temp_dir().join(format!("tarsier-catalog-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(directory.join("nested")).unwrap();
        for name in ["b.JPG", "a.png", "profile.json", "nested/hidden.png"] {
            std::fs::write(directory.join(name), b"fixture").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.png", directory.join("link.png")).unwrap();
        let portraits = catalog(&[directory.clone()], &directory.join("a.png")).unwrap();
        assert_eq!(portraits.len(), 2);
        assert_eq!(portraits[0].name, "a.png");
        assert!(portraits[0].selected);
        assert!(!portraits[1].selected);
        let again = catalog(&[directory.clone()], &directory.join("a.png")).unwrap();
        assert_eq!(portraits[0].id, again[0].id);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn normalizes_bundled_portrait_and_rejects_invalid_inputs() {
        let source = include_bytes!("../assets/avatars/liveportrait-default.png");
        let normalized = normalize(source).unwrap();
        let decoded = image::load_from_memory(&normalized).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (512, 512));
        let mut jpeg = Cursor::new(Vec::new());
        decoded.write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
        assert!(normalize(&jpeg.into_inner()).is_ok());
        assert!(normalize(b"not an image").is_err());
        assert!(normalize(&source[..100]).is_err());
        let mut tiny = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(255, 256)
            .write_to(&mut tiny, ImageFormat::Png)
            .unwrap();
        assert!(normalize(&tiny.into_inner()).is_err());
    }

    #[test]
    fn rejects_oversized_geometry_before_decoding_pixels() {
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(4097, 256)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        assert!(normalize(&encoded.into_inner()).is_err());
        assert!(normalize(&vec![0; MAX_UPLOAD_BYTES + 1]).is_err());
    }
}
