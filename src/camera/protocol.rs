use thiserror::Error;

use crate::model::BuiltInGesture;

pub const FRAME_SIZE: usize = 60;
pub const VENDOR_SELECTOR: u8 = 2;
pub const TRACKING_SELECTOR: u8 = 6;

pub const GIMBAL_RECEIVER: u8 = 0x03;
pub const GIM_SET_MOTOR: u16 = 0x00c3;
pub const AI_RECEIVER: u8 = 0x04;
pub const AI_GET_QUICK_STATUS: u16 = 0x0104;
pub const AI_GET_GIM_STATE: u16 = 0x6604;
pub const AI_SET_GIM_MOTOR_DEG: u16 = 0x6444;
pub const AI_SET_GESTURE_TARGET: u16 = 0x30c4;
pub const AI_SET_GESTURE_ZOOM: u16 = 0x3144;
pub const AI_SET_GESTURE_DYNAMIC_ZOOM: u16 = 0x3344;
pub const CAMERA_RECEIVER: u8 = 0x02;
pub const CAM_SET_DEV_STATUS: u16 = 0xa0c2;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame is too short: {0} bytes")]
    TooShort(usize),
    #[error("invalid frame magic 0x{0:02x}")]
    Magic(u8),
    #[error("header CRC mismatch")]
    HeaderCrc,
    #[error("payload is truncated")]
    TruncatedPayload,
    #[error("payload CRC mismatch")]
    PayloadCrc,
    #[error("payload exceeds the 44-byte frame capacity")]
    PayloadTooLarge,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedFrame {
    pub sequence: u16,
    pub command: u16,
    pub sender: u8,
    pub receiver: u8,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GimbalVector {
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AiGimbalState {
    pub euler: GimbalVector,
    pub motor: GimbalVector,
    pub velocity: GimbalVector,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AiGestureStatus {
    pub target_selection: bool,
    pub zoom: bool,
    pub dynamic_zoom: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraStatus {
    pub tracking: Option<bool>,
    pub zoom_percent: Option<u8>,
    pub hdr: Option<bool>,
    pub face_priority_auto_exposure: Option<bool>,
}

pub fn build_frame(
    sequence: u16,
    command: u16,
    receiver: u8,
    flags: u8,
    payload: &[u8],
) -> Result<[u8; FRAME_SIZE], FrameError> {
    if payload.len() > FRAME_SIZE - 16 {
        return Err(FrameError::PayloadTooLarge);
    }
    let mut frame = [0_u8; FRAME_SIZE];
    frame[0] = 0xaa;
    frame[1] = flags;
    frame[2..4].copy_from_slice(&sequence.to_le_bytes());
    frame[4..6].copy_from_slice(&12_u16.to_le_bytes());
    frame[8] = 0x0a;
    frame[9] = receiver;
    frame[10..12].copy_from_slice(&command.to_le_bytes());
    let header_crc = crc16_usb(&frame[..12]);
    frame[6..8].copy_from_slice(&header_crc.to_le_bytes());

    if !payload.is_empty() {
        frame[12..14].copy_from_slice(&(payload.len() as u16).to_le_bytes());
        frame[16..16 + payload.len()].copy_from_slice(payload);
        let payload_crc = crc16_usb(&frame[12..16 + payload.len()]);
        frame[14..16].copy_from_slice(&payload_crc.to_le_bytes());
    }
    Ok(frame)
}

pub fn parse_frame(buffer: &[u8]) -> Result<ParsedFrame, FrameError> {
    if buffer.len() < 12 {
        return Err(FrameError::TooShort(buffer.len()));
    }
    if buffer[0] != 0xaa {
        return Err(FrameError::Magic(buffer[0]));
    }
    let mut header = buffer[..12].to_vec();
    let received_header_crc = u16::from_le_bytes([header[6], header[7]]);
    header[6] = 0;
    header[7] = 0;
    if crc16_usb(&header) != received_header_crc {
        return Err(FrameError::HeaderCrc);
    }

    let payload_length = if buffer.len() >= 16 {
        u16::from_le_bytes([buffer[12], buffer[13]]) as usize
    } else {
        0
    };
    if payload_length > FRAME_SIZE - 16 || buffer.len() < 16 + payload_length {
        return Err(FrameError::TruncatedPayload);
    }
    let payload = if payload_length == 0 {
        Vec::new()
    } else {
        let mut segment = buffer[12..16 + payload_length].to_vec();
        let received_payload_crc = u16::from_le_bytes([segment[2], segment[3]]);
        segment[2] = 0;
        segment[3] = 0;
        if crc16_usb(&segment) != received_payload_crc {
            return Err(FrameError::PayloadCrc);
        }
        buffer[16..16 + payload_length].to_vec()
    };

    Ok(ParsedFrame {
        sequence: u16::from_le_bytes([buffer[2], buffer[3]]),
        command: u16::from_le_bytes([buffer[10], buffer[11]]),
        sender: buffer[8],
        receiver: buffer[9],
        payload,
    })
}

pub fn wake_frame(sequence: u16) -> [u8; FRAME_SIZE] {
    power_frame(sequence, true)
}

pub fn sleep_frame(sequence: u16) -> [u8; FRAME_SIZE] {
    power_frame(sequence, false)
}

fn power_frame(sequence: u16, enabled: bool) -> [u8; FRAME_SIZE] {
    build_frame(
        sequence,
        CAM_SET_DEV_STATUS,
        CAMERA_RECEIVER,
        0x25,
        &[u8::from(!enabled), 0, 0, 0],
    )
    .expect("camera power payload fits")
}

pub fn ai_gimbal_query(sequence: u16) -> [u8; FRAME_SIZE] {
    build_frame(sequence, AI_GET_GIM_STATE, AI_RECEIVER, 0x01, &[]).expect("empty query fits")
}

pub fn ai_status_query(sequence: u16) -> [u8; FRAME_SIZE] {
    build_frame(sequence, AI_GET_QUICK_STATUS, AI_RECEIVER, 0x01, &[]).expect("empty query fits")
}

pub fn recenter_frame(sequence: u16) -> [u8; FRAME_SIZE] {
    build_frame(sequence, GIM_SET_MOTOR, GIMBAL_RECEIVER, 0x25, &[0; 6])
        .expect("recenter payload fits")
}

pub fn move_frame(sequence: u16, yaw: f32, pitch: f32, roll: f32) -> [u8; FRAME_SIZE] {
    let mut payload = Vec::with_capacity(12);
    payload.extend_from_slice(&roll.to_le_bytes());
    payload.extend_from_slice(&pitch.to_le_bytes());
    payload.extend_from_slice(&yaw.to_le_bytes());
    build_frame(sequence, AI_SET_GIM_MOTOR_DEG, AI_RECEIVER, 0x25, &payload)
        .expect("move payload fits")
}

pub fn built_in_gesture_frame(
    sequence: u16,
    feature: BuiltInGesture,
    enabled: bool,
) -> [u8; FRAME_SIZE] {
    let command = match feature {
        BuiltInGesture::TargetSelection => AI_SET_GESTURE_TARGET,
        BuiltInGesture::Zoom => AI_SET_GESTURE_ZOOM,
        BuiltInGesture::DynamicZoom => AI_SET_GESTURE_DYNAMIC_ZOOM,
    };
    build_frame(sequence, command, AI_RECEIVER, 0x25, &[u8::from(enabled)])
        .expect("built-in gesture payload fits")
}

pub fn tracking_payload(enabled: bool) -> [u8; FRAME_SIZE] {
    let mut payload = [0_u8; FRAME_SIZE];
    payload[0] = 0x16;
    payload[1] = 0x02;
    payload[2] = if enabled { 0x02 } else { 0x00 };
    payload
}

pub fn hdr_payload(enabled: bool) -> [u8; FRAME_SIZE] {
    let mut payload = [0_u8; FRAME_SIZE];
    payload[0] = 0x01;
    payload[1] = 0x01;
    payload[2] = u8::from(enabled);
    payload
}

pub fn face_priority_auto_exposure_payload(enabled: bool) -> [u8; FRAME_SIZE] {
    let mut payload = [0_u8; FRAME_SIZE];
    payload[0] = 0x03;
    payload[1] = 0x01;
    payload[2] = u8::from(enabled);
    payload
}

pub fn decode_ai_gimbal_state(payload: &[u8]) -> Result<AiGimbalState, FrameError> {
    if payload.len() < 18 {
        return Err(FrameError::TooShort(payload.len()));
    }
    let value = |offset| i16::from_le_bytes([payload[offset], payload[offset + 1]]) as f32 / 10.0;
    Ok(AiGimbalState {
        euler: GimbalVector {
            roll: value(0),
            pitch: value(2),
            yaw: value(4),
        },
        motor: GimbalVector {
            roll: value(6),
            pitch: value(8),
            yaw: value(10),
        },
        velocity: GimbalVector {
            roll: value(12),
            pitch: value(14),
            yaw: value(16),
        },
    })
}

pub fn decode_ai_gesture_status(payload: &[u8]) -> Result<AiGestureStatus, FrameError> {
    if payload.len() < 6 {
        return Err(FrameError::TooShort(payload.len()));
    }
    Ok(AiGestureStatus {
        target_selection: payload[3] != 0,
        zoom: payload[4] != 0,
        dynamic_zoom: payload[5] != 0,
    })
}

pub fn decode_camera_status(block: &[u8]) -> Result<CameraStatus, FrameError> {
    const ZOOM_PERCENT_OFFSET: usize = 0x04;
    const HDR_OFFSET: usize = 0x06;
    const FACE_PRIORITY_AUTO_EXPOSURE_OFFSET: usize = 0x07;
    const AI_MODE_OFFSET: usize = 0x18;
    const AI_SUB_MODE_OFFSET: usize = 0x1c;
    if block.len() <= AI_SUB_MODE_OFFSET {
        return Err(FrameError::TooShort(block.len()));
    }
    let mode = (block[AI_MODE_OFFSET], block[AI_SUB_MODE_OFFSET]);
    let tracking = match mode {
        (0, 0) => Some(false),
        (1 | 3 | 4 | 5, 0) | (2, 0..=4) => Some(true),
        _ => None,
    };
    let zoom_percent =
        u16::from_le_bytes([block[ZOOM_PERCENT_OFFSET], block[ZOOM_PERCENT_OFFSET + 1]]);
    let flag = |offset| match block[offset] {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    Ok(CameraStatus {
        tracking,
        zoom_percent: (zoom_percent <= 100).then_some(zoom_percent as u8),
        hdr: flag(HDR_OFFSET),
        face_priority_auto_exposure: flag(FACE_PRIORITY_AUTO_EXPOSURE_OFFSET),
    })
}

fn crc16_usb(bytes: &[u8]) -> u16 {
    let mut crc = 0xffff_u16;
    for byte in bytes {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xa001
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xffff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_matches_hardware_capture() {
        let frame = wake_frame(0x000c);
        assert_eq!(
            &frame[..20],
            &hex("aa250c000c0089420a02c2a00400be0700000000")
        );
    }

    #[test]
    fn sleep_matches_libdev_command() {
        let frame = sleep_frame(0x0042);
        assert_eq!(
            &frame[..20],
            &hex("aa2542000c00ea630a02c2a00400bffb01000000")
        );
    }

    #[test]
    fn frame_round_trips_and_rejects_corruption() {
        let frame = build_frame(7, 0x0043, 0x03, 0x29, &[1, 2, 3, 4, 5, 6]).unwrap();
        let parsed = parse_frame(&frame).unwrap();
        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.command, 0x0043);
        assert_eq!(parsed.payload, [1, 2, 3, 4, 5, 6]);

        let mut corrupt = frame;
        corrupt[16] ^= 1;
        assert_eq!(parse_frame(&corrupt), Err(FrameError::PayloadCrc));
    }

    #[test]
    fn move_uses_roll_pitch_yaw_wire_order() {
        let frame = move_frame(1, 30.0, 20.0, 10.0);
        assert_eq!(f32::from_le_bytes(frame[16..20].try_into().unwrap()), 10.0);
        assert_eq!(f32::from_le_bytes(frame[20..24].try_into().unwrap()), 20.0);
        assert_eq!(f32::from_le_bytes(frame[24..28].try_into().unwrap()), 30.0);
    }

    #[test]
    fn built_in_gesture_commands_use_tiny_2_wire_opcodes() {
        for (feature, command) in [
            (BuiltInGesture::TargetSelection, AI_SET_GESTURE_TARGET),
            (BuiltInGesture::Zoom, AI_SET_GESTURE_ZOOM),
            (BuiltInGesture::DynamicZoom, AI_SET_GESTURE_DYNAMIC_ZOOM),
        ] {
            let disabled = parse_frame(&built_in_gesture_frame(7, feature, false)).unwrap();
            assert_eq!(disabled.receiver, AI_RECEIVER);
            assert_eq!(disabled.command, command);
            assert_eq!(disabled.payload, [0]);

            let enabled = parse_frame(&built_in_gesture_frame(8, feature, true)).unwrap();
            assert_eq!(enabled.command, command);
            assert_eq!(enabled.payload, [1]);
        }
    }

    #[test]
    fn ai_queries_use_the_libdev_wire_opcodes() {
        let gimbal = ai_gimbal_query(0x1234);
        let parsed = parse_frame(&gimbal).unwrap();
        assert_eq!(gimbal[1], 0x01);
        assert_eq!(parsed.sequence, 0x1234);
        assert_eq!(parsed.receiver, AI_RECEIVER);
        assert_eq!(parsed.command, AI_GET_GIM_STATE);
        assert!(parsed.payload.is_empty());

        let status = ai_status_query(0x5678);
        let parsed = parse_frame(&status).unwrap();
        assert_eq!(status[1], 0x01);
        assert_eq!(parsed.sequence, 0x5678);
        assert_eq!(parsed.receiver, AI_RECEIVER);
        assert_eq!(parsed.command, AI_GET_QUICK_STATUS);
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn decodes_libdev_ai_gimbal_state() {
        let payload = [
            29, 0, 38, 0, 0xb3, 0xf9, 0, 0, 76, 0, 0x85, 0x05, 0xff, 0xff, 1, 0, 8, 0, 0, 0, 0, 0,
            0, 0,
        ];
        let state = decode_ai_gimbal_state(&payload).unwrap();
        assert_eq!(
            state.euler,
            GimbalVector {
                roll: 2.9,
                pitch: 3.8,
                yaw: -161.3,
            }
        );
        assert_eq!(
            state.motor,
            GimbalVector {
                roll: 0.0,
                pitch: 7.6,
                yaw: 141.3,
            }
        );
        assert_eq!(
            state.velocity,
            GimbalVector {
                roll: -0.1,
                pitch: 0.1,
                yaw: 0.8,
            }
        );
    }

    #[test]
    fn decodes_libdev_ai_gesture_status() {
        let payload = [0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            decode_ai_gesture_status(&payload).unwrap(),
            AiGestureStatus {
                target_selection: true,
                zoom: false,
                dynamic_zoom: true,
            }
        );
    }

    #[test]
    fn encodes_hdr_as_libdev_selector_six_payload() {
        assert_eq!(&hdr_payload(false)[..3], &[0x01, 0x01, 0x00]);
        assert_eq!(&hdr_payload(true)[..3], &[0x01, 0x01, 0x01]);
    }

    #[test]
    fn encodes_face_priority_auto_exposure_as_selector_six_payload() {
        assert_eq!(
            &face_priority_auto_exposure_payload(false)[..3],
            &[0x03, 0x01, 0x00]
        );
        assert_eq!(
            &face_priority_auto_exposure_payload(true)[..3],
            &[0x03, 0x01, 0x01]
        );
    }

    #[test]
    fn decodes_tracking_zoom_and_hdr_from_selector_six_status() {
        let status = |mode, sub_mode, zoom_percent: u16, hdr, face_ae| {
            let mut block = [0_u8; FRAME_SIZE];
            block[0x04..0x06].copy_from_slice(&zoom_percent.to_le_bytes());
            block[0x06] = hdr;
            block[0x07] = face_ae;
            block[0x18] = mode;
            block[0x1c] = sub_mode;
            block
        };
        assert_eq!(
            decode_camera_status(&status(0, 0, 0_u16, 0, 0)).unwrap(),
            CameraStatus {
                tracking: Some(false),
                zoom_percent: Some(0),
                hdr: Some(false),
                face_priority_auto_exposure: Some(false),
            }
        );
        assert_eq!(
            decode_camera_status(&status(2, 0, 50_u16, 1, 1)).unwrap(),
            CameraStatus {
                tracking: Some(true),
                zoom_percent: Some(50),
                hdr: Some(true),
                face_priority_auto_exposure: Some(true),
            }
        );
        assert_eq!(
            decode_camera_status(&status(2, 4, 100_u16, 0, 0)).unwrap(),
            CameraStatus {
                tracking: Some(true),
                zoom_percent: Some(100),
                hdr: Some(false),
                face_priority_auto_exposure: Some(false),
            }
        );
        assert_eq!(
            decode_camera_status(&status(6, 0, 101_u16, 2, 2)).unwrap(),
            CameraStatus {
                tracking: None,
                zoom_percent: None,
                hdr: None,
                face_priority_auto_exposure: None,
            }
        );
        assert_eq!(
            decode_camera_status(&status(2, 9, 25_u16, 1, 1)).unwrap(),
            CameraStatus {
                tracking: None,
                zoom_percent: Some(25),
                hdr: Some(true),
                face_priority_auto_exposure: Some(true),
            }
        );
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
}
