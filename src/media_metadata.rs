//! Portable capture settings. Values are daemon observations, not sensor EXIF telemetry.
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    model::{CameraImageControl, RuntimeState, unix_ms},
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
        embed_exif(
            &self.frame.bytes,
            &metadata,
            self.frame.captured_at_ms,
            standard_exif(&self.settings, self.dimensions),
        )
    }
}

/// Deliberately select settings, excluding serials, local paths, identities and errors.
/// Availability and readback status qualify values without per-control timing noise.
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
                "active": c.active,
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
            "hdr": {"enabled": camera.hdr, "readback_ok": camera.hdr_error.is_none()},
            "zoom": {"magnification": camera.zoom_magnification, "readback_ok": camera.zoom_error.is_none()},
            "attitude": {"yaw_degrees": camera.yaw_degrees, "pitch_degrees": camera.pitch_degrees,
                "roll_degrees": camera.roll_degrees, "source": camera.attitude_source},
            "tracking": camera.tracking,
            "face_tracking": {"enabled": camera.face_tracking.enabled, "speed_fraction": camera.face_tracking.speed_fraction},
            "auto_zoom": {"enabled": camera.face_tracking.auto_zoom.enabled,
                "calibrated": camera.face_tracking.auto_zoom.calibrated,
                "controlled_magnification": camera.face_tracking.auto_zoom.zoom_magnification,
                "target_face_size": camera.face_tracking.auto_zoom.target_face_size},
            "hands_tracking_enabled": camera.hands_tracking.enabled,
            "built_in_gestures": {"target_selection": camera.built_in_gestures.target_selection,
                "zoom": camera.built_in_gestures.zoom, "dynamic_zoom": camera.built_in_gestures.dynamic_zoom,
                "readback_ok": camera.built_in_gestures.error.is_none()},
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

// Each entry is (tag, TIFF type, count, bytes).
type Entry = (u16, u16, u32, Vec<u8>);

fn short(tag: u16, value: u16) -> Entry {
    (tag, 3, 1, value.to_le_bytes().to_vec())
}

/// Export only direct semantic mappings. UVC brightness/gain and processing
/// sliders have no calibrated mapping to EXIF APEX/ISO or categorical levels.
fn standard_exif(state: &RuntimeState, dimensions: (u32, u32)) -> Vec<Entry> {
    let mut entries = vec![
        (0xa002, 4, 1, dimensions.0.to_le_bytes().to_vec()),
        (0xa003, 4, 1, dimensions.1.to_le_bytes().to_vec()),
    ];
    let camera = &state.camera;
    if state.pipeline.source != "camera" || !camera.available || camera.error.is_some() {
        return entries;
    }
    let value = |control| {
        camera
            .image_settings
            .controls
            .iter()
            .find(|c| c.control == control && c.available && c.active && c.error.is_none())
            .and_then(|c| c.value)
    };
    if let Some(mode @ 0..=3) = value(CameraImageControl::AutoExposure) {
        // V4L2: auto, manual, shutter priority, aperture priority.
        let program = [2, 1, 4, 3][mode as usize];
        entries.push(short(0x8822, program));
        entries.push(short(0xa402, u16::from(mode == 1)));
        // Auto exposure readback can be a cached manual setting, not exposure telemetry.
        if matches!(mode, 1 | 2)
            && let Some(time) = value(CameraImageControl::ExposureTimeAbsolute).filter(|v| *v > 0)
        {
            let mut rational = (time as u32).to_le_bytes().to_vec();
            rational.extend_from_slice(&10_000u32.to_le_bytes());
            entries.push((0x829a, 5, 1, rational));
        }
    }
    if let Some(automatic @ 0..=1) = value(CameraImageControl::WhiteBalanceAutomatic) {
        entries.push(short(0xa403, u16::from(automatic == 0)));
    }
    entries
}

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
fn embed_exif(
    jpeg: &[u8],
    metadata: &Value,
    captured_at_ms: u64,
    mut camera_entries: Vec<Entry>,
) -> Result<Vec<u8>> {
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
    camera_entries.extend([
        (0x9000, 7, 4, b"0231".to_vec()),
        ascii(0x9003, &at.format("%Y:%m:%d %H:%M:%S").to_string()),
        ascii(0x9011, "+00:00"),
        (0x9286, 7, comment.len() as u32, comment),
        ascii(0x9291, &format!("{:03}", captured_at_ms % 1000)),
    ]);
    camera_entries.sort_by_key(|entry| entry.0);
    write_ifd(&mut tiff, &camera_entries);
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
        let bytes = embed_exif(JPEG, &metadata, 1_788_696_123_456, vec![]).unwrap();
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
        assert!(!metadata.to_string().contains("sample_at_ms"));
        let video = settings_metadata(&settings, 987654, "recording-start", None, (1280, 720));
        assert!(video["frame"].is_null());
        assert!(!video.to_string().contains("sample_at_ms"));
        assert_eq!(video["camera"]["hdr"]["enabled"], false);
    }

    fn camera_settings(mode: i32) -> RuntimeState {
        use crate::model::{CameraImageControlKind, CameraImageControlState};
        let mut state = RuntimeState::default();
        state.pipeline.source = "camera".into();
        state.camera.available = true;
        for (control, value) in [
            (CameraImageControl::AutoExposure, mode),
            (CameraImageControl::ExposureTimeAbsolute, 19),
            (CameraImageControl::WhiteBalanceAutomatic, 0),
        ] {
            state.camera.image_settings.upsert(CameraImageControlState {
                control,
                kind: CameraImageControlKind::Integer,
                available: true,
                active: true,
                read_only: false,
                value: Some(value),
                minimum: None,
                maximum: None,
                step: None,
                default_value: None,
                options: vec![],
                sample_at_ms: Some(123),
                error: None,
            });
        }
        state
    }

    fn parse_camera(state: &RuntimeState) -> exif::Exif {
        let bytes = embed_exif(JPEG, &json!({}), 0, standard_exif(state, (3840, 2160))).unwrap();
        Reader::new()
            .read_from_container(&mut std::io::Cursor::new(bytes))
            .unwrap()
    }

    #[test]
    fn standard_exif_maps_exposure_modes_units_white_balance_and_dimensions() {
        for (mode, program) in [(0, 2), (1, 1), (2, 4), (3, 3)] {
            let state = camera_settings(mode);
            let parsed = parse_camera(&state);
            for (tag, expected) in [
                (Tag::ExposureProgram, program),
                (Tag::ExposureMode, u32::from(mode == 1)),
                (Tag::WhiteBalance, 1),
                (Tag::PixelXDimension, 3840),
                (Tag::PixelYDimension, 2160),
            ] {
                assert_eq!(
                    parsed
                        .get_field(tag, In::PRIMARY)
                        .unwrap()
                        .value
                        .get_uint(0),
                    Some(expected)
                );
            }
            let exposure = parsed.get_field(Tag::ExposureTime, In::PRIMARY);
            if matches!(mode, 1 | 2) {
                let exif::Value::Rational(values) = &exposure.unwrap().value else {
                    panic!("Exposure must be rational")
                };
                assert_eq!((values[0].num, values[0].denom), (19, 10_000));
            } else {
                assert!(
                    exposure.is_none(),
                    "Auto mode must not export cached manual exposure"
                );
            }
            for tag in [
                Tag::BrightnessValue,
                Tag::PhotographicSensitivity,
                Tag::Contrast,
                Tag::Saturation,
                Tag::Sharpness,
            ] {
                assert!(parsed.get_field(tag, In::PRIMARY).is_none());
            }
        }
        let mut state = camera_settings(1);
        state.camera.image_settings.controls[2].value = Some(1);
        assert_eq!(
            parse_camera(&state)
                .get_field(Tag::WhiteBalance, In::PRIMARY)
                .unwrap()
                .value
                .get_uint(0),
            Some(0)
        );
    }

    #[test]
    fn standard_exif_omits_unavailable_failed_invalid_and_synthetic_camera_values() {
        for variant in 0..7 {
            let mut state = camera_settings(1);
            match variant {
                0 => state.camera.available = false,
                1 => state.pipeline.source = "test".into(),
                2 => state.camera.error = Some("Disconnected".into()),
                3 => state
                    .camera
                    .image_settings
                    .controls
                    .iter_mut()
                    .for_each(|c| c.active = false),
                4 => state
                    .camera
                    .image_settings
                    .controls
                    .iter_mut()
                    .for_each(|c| c.available = false),
                5 => state
                    .camera
                    .image_settings
                    .controls
                    .iter_mut()
                    .for_each(|c| c.error = Some("Read failed".into())),
                _ => state
                    .camera
                    .image_settings
                    .controls
                    .iter_mut()
                    .for_each(|c| c.value = Some(-1)),
            }
            let parsed = parse_camera(&state);
            for tag in [
                Tag::ExposureTime,
                Tag::ExposureMode,
                Tag::ExposureProgram,
                Tag::WhiteBalance,
            ] {
                assert!(parsed.get_field(tag, In::PRIMARY).is_none());
            }
        }
        for value in [None, Some(0), Some(-1)] {
            let mut state = camera_settings(1);
            state.camera.image_settings.controls[1].value = value;
            assert!(
                parse_camera(&state)
                    .get_field(Tag::ExposureTime, In::PRIMARY)
                    .is_none()
            );
        }
    }

    #[test]
    fn malformed_duplicate_and_oversized_exif_are_rejected() {
        for invalid in [
            b"not JPEG".as_slice(),
            b"\xff\xd8\xff",
            b"\xff\xd8\xff\xe0\xff\xff",
        ] {
            assert!(embed_exif(invalid, &json!({}), 0, vec![]).is_err());
        }
        let tagged = embed_exif(JPEG, &json!({}), 0, vec![]).unwrap();
        assert!(embed_exif(&tagged, &json!({}), 0, vec![]).is_err());
        assert!(embed_exif(JPEG, &json!({"text": "x".repeat(65_536)}), 0, vec![]).is_err());
    }
}
