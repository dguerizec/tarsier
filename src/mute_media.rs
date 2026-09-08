use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use image::{ImageDecoder, ImageReader};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_IMAGE: &[u8] = include_bytes!("../web/mute-default.png");

pub fn default_selection() -> Option<Selection> {
    Some(Selection {
        filename: format!("{:x}.png", Sha256::digest(DEFAULT_IMAGE)),
        name: "Tarsier".into(),
        kind: Kind::Image,
    })
}

pub const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Image,
    Video,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Selection {
    pub filename: String,
    pub name: String,
    pub kind: Kind,
}

impl Selection {
    pub fn path(&self, directory: &Path) -> Result<PathBuf> {
        let (hash, extension) = self
            .filename
            .split_once('.')
            .context("invalid media filename")?;
        if hash.len() != 64
            || !hash.bytes().all(|c| c.is_ascii_hexdigit())
            || !matches!(extension, "png" | "jpg" | "mp4" | "webm")
        {
            bail!("invalid media filename");
        }
        Ok(directory.join(&self.filename))
    }
}

pub struct Media {
    content: Content,
}
enum Content {
    Image(gst::Buffer),
    Video(Clip),
}
struct Clip {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    first: gst::Buffer,
    last: gst::Buffer,
    playing: bool,
}

impl Drop for Clip {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Media {
    pub fn open(selection: &Selection, directory: &Path, width: u32, height: u32) -> Result<Self> {
        gst::init()?;
        let path = selection.path(directory)?;
        let content = match selection.kind {
            Kind::Image => {
                let bytes = if Some(selection) == default_selection().as_ref() {
                    DEFAULT_IMAGE.to_vec()
                } else {
                    fs::read(path)?
                };
                let mut reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(8192);
                limits.max_image_height = Some(8192);
                limits.max_alloc = Some(128 * 1024 * 1024);
                reader.limits(limits);
                let mut decoder = reader.into_decoder()?;
                if decoder.total_bytes() > 128 * 1024 * 1024 {
                    bail!("image is too large");
                }
                let orientation = decoder.orientation()?;
                let mut image = image::DynamicImage::from_decoder(decoder)?;
                image.apply_orientation(orientation);
                let image = image
                    .resize(width, height, image::imageops::FilterType::Triangle)
                    .to_rgba8();
                let mut pixels = vec![0; width as usize * height as usize * 4];
                let x0 = (width - image.width()) / 2;
                let y0 = (height - image.height()) / 2;
                for (x, y, pixel) in image.enumerate_pixels() {
                    let offset = (((y + y0) * width + x + x0) * 4) as usize;
                    for (channel, source) in [2, 1, 0].into_iter().enumerate() {
                        pixels[offset + channel] =
                            (pixel[source] as u16 * pixel[3] as u16 / 255) as u8;
                    }
                }
                Content::Image(gst::Buffer::from_slice(pixels))
            }
            Kind::Video => {
                let pipeline = gst::parse::launch(&format!(
                    "uridecodebin name=media ! queue ! videoconvert ! videoscale add-borders=true ! \
                     video/x-raw,format=BGRx,width={width},height={height},pixel-aspect-ratio=1/1 ! \
                     appsink name=frames max-buffers=2 drop=true sync=true"
                ))?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("invalid replacement video pipeline"))?;
                pipeline.by_name("media").unwrap().set_property(
                    "uri",
                    reqwest::Url::from_file_path(fs::canonicalize(path)?)
                        .map_err(|_| anyhow::anyhow!("invalid media path"))?
                        .as_str(),
                );
                let sink = pipeline
                    .by_name("frames")
                    .unwrap()
                    .downcast::<gst_app::AppSink>()
                    .unwrap();
                let mut clip = Clip {
                    pipeline,
                    sink,
                    first: gst::Buffer::new(),
                    last: gst::Buffer::new(),
                    playing: false,
                };
                clip.pipeline.set_state(gst::State::Paused)?;
                let sample = clip.sink.try_pull_preroll(gst::ClockTime::from_seconds(10))
                    .context("video could not be decoded; use an MP4 or WebM with a supported video codec")?;
                clip.first = sample.buffer().context("video has no frame")?.to_owned();
                clip.last = clip.first.clone();
                Content::Video(clip)
            }
        };
        Ok(Self { content })
    }

    // The virtual writer owns playback timing. Never expose live capture on failure.
    pub fn frame(&mut self, active: bool) -> Result<Option<gst::Buffer>> {
        match &mut self.content {
            Content::Image(buffer) => Ok(active.then(|| buffer.clone())),
            Content::Video(clip) => {
                if active != clip.playing {
                    if active {
                        clip.pipeline.set_state(gst::State::Playing)?;
                    } else {
                        clip.pipeline.set_state(gst::State::Paused)?;
                        clip.pipeline.seek_simple(
                            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                            gst::ClockTime::ZERO,
                        )?;
                        clip.last = clip.first.clone();
                    }
                    clip.playing = active;
                }
                if !active {
                    return Ok(None);
                }
                let bus = clip.pipeline.bus().unwrap();
                if let Some(error) = bus.pop_filtered(&[gst::MessageType::Error]) {
                    bail!("replacement video playback failed: {error:?}");
                }
                if let Some(sample) = clip.sink.try_pull_sample(gst::ClockTime::ZERO) {
                    clip.last = sample.buffer().context("video has no frame")?.to_owned();
                } else if clip.sink.is_eos() {
                    clip.pipeline.seek_simple(
                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                        gst::ClockTime::ZERO,
                    )?;
                }
                Ok(Some(clip.last.clone()))
            }
        }
    }
}

pub fn upload(
    directory: &Path,
    name: &str,
    bytes: &[u8],
    width: u32,
    height: u32,
) -> Result<(Selection, Media)> {
    use std::{
        io::Write,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    };
    if bytes.is_empty() || bytes.len() > MAX_UPLOAD_BYTES {
        bail!("choose a file up to 100 MB");
    }
    let (extension, kind) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("png", Kind::Image)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        ("jpg", Kind::Image)
    } else if bytes.get(4..8) == Some(b"ftyp") {
        ("mp4", Kind::Video)
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        ("webm", Kind::Video)
    } else {
        bail!("choose a PNG, JPEG, MP4 or WebM file");
    };
    let filename = format!("{:x}.{extension}", Sha256::digest(bytes));
    let selection = Selection {
        filename,
        name: name.chars().filter(|c| !c.is_control()).take(150).collect(),
        kind,
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let path = selection.path(directory)?;
    let created = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(error.into());
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    match Media::open(&selection, directory, width, height) {
        Ok(media) => Ok((selection, media)),
        Err(error) => {
            if created {
                let _ = fs::remove_file(path);
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        time::{Duration, Instant},
    };

    #[test]
    fn default_mute_image_is_available_without_a_local_file() {
        let selected = default_selection().unwrap();
        let mut media = Media::open(&selected, Path::new("/nonexistent/tarsier"), 640, 360).unwrap();
        let frame = media.frame(true).unwrap().unwrap();
        assert_eq!(frame.size(), 640 * 360 * 4);
        assert!(frame.map_readable().unwrap().as_slice().iter().any(|v| *v > 128));
        assert!(media.frame(false).unwrap().is_none());
    }

    #[test]
    fn image_replacement_fits_without_cropping_and_reloads() {
        let directory = std::env::temp_dir().join(format!(
            "tarsier-mute-image-{}-{}",
            std::process::id(),
            crate::model::unix_ms()
        ));
        let image = image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let (selection, mut media) = upload(&directory, "away.png", bytes.get_ref(), 4, 4).unwrap();
        assert!(media.frame(false).unwrap().is_none());
        let frame = media.frame(true).unwrap().unwrap();
        let pixels = frame.map_readable().unwrap();
        assert_eq!(&pixels.as_slice()[..16], &[0; 16]);
        assert_eq!(&pixels.as_slice()[16..20], &[0, 0, 255, 0]);
        let mut restored = Media::open(&selection, &directory, 8, 8).unwrap();
        assert_eq!(restored.frame(true).unwrap().unwrap().size(), 8 * 8 * 4);
        assert!(upload(&directory, "bad.png", b"not a picture", 4, 4).is_err());
        let invalid = Selection {
            filename: "../../private.png".into(),
            ..selection
        };
        assert!(invalid.path(&directory).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn replacement_video_loops_and_pauses_when_not_muted() {
        gst::init().unwrap();
        let directory = std::env::temp_dir().join(format!(
            "tarsier-mute-video-{}-{}",
            std::process::id(),
            crate::model::unix_ms()
        ));
        fs::create_dir_all(&directory).unwrap();
        let fixture = directory.join("fixture.webm");
        let encoder = gst::parse::launch("webmmux name=mux ! filesink name=file videotestsrc num-buffers=5 pattern=ball ! video/x-raw,width=64,height=48,framerate=10/1 ! vp8enc deadline=1 ! queue ! mux.video_0 audiotestsrc num-buffers=24 samplesperbuffer=1024 ! audio/x-raw,rate=48000 ! audioconvert ! vorbisenc ! queue ! mux.audio_0")
            .unwrap().downcast::<gst::Pipeline>().unwrap();
        encoder
            .by_name("file")
            .unwrap()
            .set_property("location", fixture.to_str().unwrap());
        encoder.set_state(gst::State::Playing).unwrap();
        let message = encoder
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        encoder.set_state(gst::State::Null).unwrap();
        assert!(
            matches!(message.view(), gst::MessageView::Eos(_)),
            "{message:?}"
        );
        let (_, mut media) = upload(
            &directory,
            "away.webm",
            &fs::read(&fixture).unwrap(),
            64,
            48,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut near_end = false;
        let mut looped = false;
        while Instant::now() < deadline {
            let frame = media.frame(true).unwrap().unwrap();
            assert_eq!(frame.size(), 64 * 48 * 4);
            let pts = frame.pts().unwrap().mseconds();
            if pts >= 300 {
                near_end = true;
            }
            if near_end && pts < 100 {
                looped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(looped, "the clip must restart instead of freezing at EOS");
        assert!(media.frame(false).unwrap().is_none());
        assert_eq!(
            media.frame(true).unwrap().unwrap().pts().unwrap(),
            gst::ClockTime::ZERO
        );
        drop(media);
        fs::remove_dir_all(directory).unwrap();
    }
}
