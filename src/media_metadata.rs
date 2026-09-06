//! Portable capture settings. Values are daemon observations, not sensor EXIF telemetry.
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    model::{RuntimeState, unix_ms},
    pipeline::PerceptionFrame,
};

#[derive(Clone)]
pub(crate) struct CapturedImage {
    pub frame: PerceptionFrame,
    settings: RuntimeState,
    observed_at_ms: u64,
    dimensions: (u32, u32),
}

impl CapturedImage {
    pub fn new(frame: PerceptionFrame, settings: RuntimeState, dimensions: (u32, u32)) -> Self {
        Self {
            frame,
            settings,
            observed_at_ms: unix_ms(),
            dimensions,
        }
    }

    pub fn jpeg(&self) -> Result<Vec<u8>> {
        let metadata = settings_metadata(
            &self.settings,
            self.observed_at_ms,
            "jpeg-publication",
            Some((self.frame.frame_id, self.frame.captured_at_ms)),
            self.dimensions,
        );
        embed_exif(&self.frame.bytes, &metadata, self.frame.captured_at_ms)
    }
}

/// Deliberately select settings, excluding serials, local paths, identities and errors.
/// Sample times and availability distinguish cached readback from live sensor values.
pub(crate) fn settings_metadata(
    state: &RuntimeState,
    observed_at_ms: u64,
    scope: &str,
    frame: Option<(u64, u64)>,
    dimensions: (u32, u32),
) -> Value {
    let camera = &state.camera;
    let effects = &state.video_effects;
    let controls: Vec<_> = camera
        .image_settings
        .controls
        .iter()
        .map(|c| {
            json!({
                "control": c.control, "value": c.value, "available": c.available,
                "active": c.active, "sample_at_ms": c.sample_at_ms,
                "readback_ok": c.error.is_none(),
            })
        })
        .collect();
    json!({
        "schema": "tarsier.capture-settings/v1",
        "software": format!("Tarsier {}", state.version),
        "scope": scope,
        "settings_observed_at_ms": observed_at_ms,
        "settings_basis": "last-known-daemon-state",
        "frame": frame.map(|(id, at)| json!({"id": id, "captured_at_ms": at})),
        "output": {"width": dimensions.0, "height": dimensions.1, "source": state.pipeline.source, "observed_fps": state.pipeline.fps},
        "camera": {
            "adapter": camera.adapter, "available": camera.available,
            "powered_on": camera.powered_on,
            "hdr": {"enabled": camera.hdr, "sample_at_ms": camera.hdr_sample_at_ms, "readback_ok": camera.hdr_error.is_none()},
            "zoom": {"magnification": camera.zoom_magnification, "sample_at_ms": camera.zoom_sample_at_ms, "readback_ok": camera.zoom_error.is_none()},
            "attitude": {"yaw_degrees": camera.yaw_degrees, "pitch_degrees": camera.pitch_degrees,
                "roll_degrees": camera.roll_degrees, "source": camera.attitude_source, "sample_at_ms": camera.sample_at_ms},
            "tracking": camera.tracking,
            "face_tracking": {"enabled": camera.face_tracking.enabled, "speed_fraction": camera.face_tracking.speed_fraction},
            "auto_zoom": {"enabled": camera.face_tracking.auto_zoom.enabled,
                "calibrated": camera.face_tracking.auto_zoom.calibrated,
                "controlled_magnification": camera.face_tracking.auto_zoom.zoom_magnification,
                "target_face_size": camera.face_tracking.auto_zoom.target_face_size},
            "hands_tracking_enabled": camera.hands_tracking.enabled,
            "built_in_gestures": camera.built_in_gestures,
            "image_controls": controls,
        },
        "effects": {
            "transform": effects.transform, "output_mode": effects.output_mode,
            "avatar_engine": effects.avatar_engine,
            "background_enabled": effects.background_enabled,
            "background_effect": effects.background_effect,
            "depth_near": effects.depth_near, "depth_far": effects.depth_far,
        },
    })
}

// Each entry is (tag, TIFF type, count, bytes). Only fixed capture tags are emitted.
type Entry = (u16, u16, u32, Vec<u8>);

fn ascii(tag: u16, text: &str) -> Entry {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    (tag, 2, bytes.len() as u32, bytes)
}

fn write_ifd(tiff: &mut Vec<u8>, entries: &[Entry]) -> usize {
    let start = tiff.len();
    tiff.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    tiff.resize(start + 2 + entries.len() * 12 + 4, 0);
    for (i, (tag, kind, count, bytes)) in entries.iter().enumerate() {
        let entry = start + 2 + i * 12;
        tiff[entry..entry + 2].copy_from_slice(&tag.to_le_bytes());
        tiff[entry + 2..entry + 4].copy_from_slice(&kind.to_le_bytes());
        tiff[entry + 4..entry + 8].copy_from_slice(&count.to_le_bytes());
        if bytes.len() <= 4 {
            tiff[entry + 8..entry + 8 + bytes.len()].copy_from_slice(bytes);
        } else {
            if !tiff.len().is_multiple_of(2) {
                tiff.push(0);
            }
            let offset = tiff.len() as u32;
            tiff[entry + 8..entry + 12].copy_from_slice(&offset.to_le_bytes());
            tiff.extend_from_slice(bytes);
        }
    }
    start
}

/// Insert APP1 without touching the JPEG's compressed pixels or other metadata.
/// The input is an internally encoded JPEG; existing EXIF is rejected to avoid ambiguity.
fn embed_exif(jpeg: &[u8], metadata: &Value, captured_at_ms: u64) -> Result<Vec<u8>> {
    ensure!(jpeg.starts_with(&[0xff, 0xd8]), "Invalid JPEG header");
    let mut offset = 2;
    let mut insert_at = 2;
    loop {
        ensure!(jpeg.get(offset) == Some(&0xff), "Invalid JPEG marker");
        let marker_start = offset;
        while jpeg.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *jpeg
            .get(offset)
            .ok_or_else(|| anyhow::anyhow!("Truncated JPEG marker"))?;
        offset += 1;
        if marker == 0xda || marker == 0xd9 {
            break;
        }
        ensure!(
            !matches!(marker, 0 | 0xd8 | 0xd0..=0xd7 | 1),
            "Unexpected JPEG marker"
        );
        let length = jpeg
            .get(offset..offset + 2)
            .ok_or_else(|| anyhow::anyhow!("Truncated JPEG segment"))?;
        let length = u16::from_be_bytes([length[0], length[1]]) as usize;
        ensure!(
            length >= 2 && offset + length <= jpeg.len(),
            "Invalid JPEG segment length"
        );
        if marker == 0xe1 && jpeg[offset + 2..offset + length].starts_with(b"Exif\0\0") {
            bail!("JPEG already contains EXIF");
        }
        // Keep the JFIF APP0 segment first, as required by JFIF readers.
        if marker == 0xe0 && marker_start == 2 {
            insert_at = offset + length;
        }
        offset += length;
    }
    let at = chrono::DateTime::from_timestamp_millis(i64::try_from(captured_at_ms)?)
        .ok_or_else(|| anyhow::anyhow!("Invalid capture timestamp"))?;
    let mut comment = b"ASCII\0\0\0".to_vec();
    // ASCII JSON remains lossless for Unicode through JSON escapes.
    for character in serde_json::to_string(metadata)?.chars() {
        if character.is_ascii() {
            comment.push(character as u8);
        } else {
            for unit in character.encode_utf16(&mut [0; 2]) {
                comment.extend_from_slice(format!("\\u{unit:04x}").as_bytes());
            }
        }
    }
    ensure!(
        comment.len() < 60_000,
        "Capture settings exceed the EXIF size limit"
    );
    let mut tiff = b"II\x2a\0\x08\0\0\0".to_vec();
    let root = write_ifd(
        &mut tiff,
        &[
            (0x0112, 3, 1, 1u16.to_le_bytes().to_vec()),
            ascii(0x0131, &format!("Tarsier {}", env!("CARGO_PKG_VERSION"))),
            (0x8769, 4, 1, vec![0; 4]),
        ],
    );
    if !tiff.len().is_multiple_of(2) {
        tiff.push(0);
    }
    let exif_offset = tiff.len() as u32;
    tiff[root + 2 + 2 * 12 + 8..root + 2 + 2 * 12 + 12].copy_from_slice(&exif_offset.to_le_bytes());
    write_ifd(
        &mut tiff,
        &[
            (0x9000, 7, 4, b"0231".to_vec()),
            ascii(0x9003, &at.format("%Y:%m:%d %H:%M:%S").to_string()),
            ascii(0x9011, "+00:00"),
            (0x9286, 7, comment.len() as u32, comment),
            ascii(0x9291, &format!("{:03}", captured_at_ms % 1000)),
        ],
    );
    let length = u16::try_from(tiff.len() + 8)?;
    let mut result = Vec::with_capacity(jpeg.len() + tiff.len() + 10);
    result.extend_from_slice(&jpeg[..insert_at]);
    result.extend_from_slice(&[0xff, 0xe1]);
    result.extend_from_slice(&length.to_be_bytes());
    result.extend_from_slice(b"Exif\0\0");
    result.extend_from_slice(&tiff);
    result.extend_from_slice(&jpeg[insert_at..]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use exif::{In, Reader, Tag};

    // A JFIF segment followed by a scan marker; compressed bytes must remain untouched.
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x07JFIF\0\xff\xda\x00\x02pixels\xff\xd9";

    fn read_comment(jpeg: &[u8]) -> Value {
        let parsed = Reader::new()
            .read_from_container(&mut std::io::Cursor::new(jpeg))
            .unwrap();
        let field = parsed.get_field(Tag::UserComment, In::PRIMARY).unwrap();
        let exif::Value::Undefined(bytes, _) = &field.value else {
            panic!("EXIF comment is not undefined")
        };
        assert_eq!(&bytes[..8], b"ASCII\0\0\0");
        assert!(bytes.is_ascii());
        serde_json::from_slice(&bytes[8..]).unwrap()
    }

    #[test]
    fn exif_round_trips_unicode_and_preserves_compressed_bytes() {
        let metadata =
            json!({"schema": "tarsier.capture-settings/v1", "text": "é 🐒", "zoom": 1.5});
        let bytes = embed_exif(JPEG, &metadata, 1_788_696_123_456).unwrap();
        assert_eq!(read_comment(&bytes), metadata);
        assert_eq!(&bytes[..11], &JPEG[..11]);
        let segment_length = u16::from_be_bytes([bytes[13], bytes[14]]) as usize;
        assert_eq!(&bytes[13 + segment_length..], &JPEG[11..]);
        let parsed = Reader::new()
            .read_from_container(&mut std::io::Cursor::new(bytes))
            .unwrap();
        assert_eq!(
            parsed
                .get_field(Tag::Orientation, In::PRIMARY)
                .unwrap()
                .value
                .get_uint(0),
            Some(1)
        );
        assert!(
            parsed
                .get_field(Tag::DateTimeOriginal, In::PRIMARY)
                .is_some()
        );
        assert!(
            parsed
                .get_field(Tag::OffsetTimeOriginal, In::PRIMARY)
                .is_some()
        );
        assert!(
            parsed
                .get_field(Tag::SubSecTimeOriginal, In::PRIMARY)
                .is_some()
        );
        // Raw UVC exposure settings are not claimed as measured photographic exposure.
        assert!(parsed.get_field(Tag::ExposureTime, In::PRIMARY).is_none());
    }

    #[test]
    fn capture_freezes_settings_and_omits_private_runtime_data() {
        let mut settings = RuntimeState::default();
        settings.camera.hdr = Some(true);
        settings.camera.serial = Some("private-serial".into());
        settings.pipeline.input_device = Some("/private/path".into());
        settings.camera.error = Some("private-error".into());
        let capture = CapturedImage::new(
            PerceptionFrame {
                bytes: Bytes::from_static(JPEG),
                frame_id: 42,
                captured_at_ms: 123456,
            },
            settings.clone(),
            (1920, 1080),
        );
        settings.camera.hdr = Some(false);
        let metadata = read_comment(&capture.jpeg().unwrap());
        assert_eq!(metadata["camera"]["hdr"]["enabled"], true);
        assert_eq!(metadata["frame"]["id"], 42);
        assert_eq!(metadata["frame"]["captured_at_ms"], 123456);
        assert_eq!(metadata["output"]["width"], 1920);
        assert_eq!(metadata["scope"], "jpeg-publication");
        assert!(!metadata.to_string().contains("private"));
        let video = settings_metadata(&settings, 987654, "recording-start", None, (1280, 720));
        assert!(video["frame"].is_null());
        assert_eq!(video["camera"]["hdr"]["enabled"], false);
    }

    #[test]
    fn malformed_duplicate_and_oversized_exif_are_rejected() {
        for invalid in [
            b"not JPEG".as_slice(),
            b"\xff\xd8\xff",
            b"\xff\xd8\xff\xe0\xff\xff",
        ] {
            assert!(embed_exif(invalid, &json!({}), 0).is_err());
        }
        let tagged = embed_exif(JPEG, &json!({}), 0).unwrap();
        assert!(embed_exif(&tagged, &json!({}), 0).is_err());
        assert!(embed_exif(JPEG, &json!({"text": "x".repeat(65_536)}), 0).is_err());
    }
}
