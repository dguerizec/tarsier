use thiserror::Error;

pub const FRAME_SIZE: usize = 60;
pub const VENDOR_SELECTOR: u8 = 2;
pub const TRACKING_SELECTOR: u8 = 6;

pub const GIM_GET_STATE: u16 = 0x0043;
pub const GIMBAL_RECEIVER: u8 = 0x03;
pub const GIM_SET_MOTOR: u16 = 0x00c3;
pub const AI_RECEIVER: u8 = 0x04;
pub const AI_SET_GIM_MOTOR_DEG: u16 = 0x6444;
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
    build_frame(
        sequence,
        CAM_SET_DEV_STATUS,
        CAMERA_RECEIVER,
        0x25,
        &[0, 0, 0, 0],
    )
    .expect("wake payload fits")
}

pub fn gimbal_query(sequence: u16) -> [u8; FRAME_SIZE] {
    build_frame(sequence, GIM_GET_STATE, GIMBAL_RECEIVER, 0x01, &[]).expect("empty query fits")
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

pub fn tracking_payload(enabled: bool) -> [u8; FRAME_SIZE] {
    let mut payload = [0_u8; FRAME_SIZE];
    payload[0] = 0x16;
    payload[1] = 0x02;
    payload[2] = if enabled { 0x02 } else { 0x00 };
    payload
}

pub fn decode_gimbal_angles(payload: &[u8]) -> Result<(f32, f32, f32), FrameError> {
    if payload.len() < 6 {
        return Err(FrameError::TooShort(payload.len()));
    }
    let roll = i16::from_le_bytes([payload[0], payload[1]]) as f32 / 100.0;
    let pitch = i16::from_le_bytes([payload[2], payload[3]]) as f32 / 100.0;
    let yaw = i16::from_le_bytes([payload[4], payload[5]]) as f32 / 100.0;
    Ok((yaw, pitch, roll))
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
    fn decodes_known_gimbal_prefix() {
        let payload = [0xD3, 0xFD, 0xFF, 0xF3, 0x8F, 0xEF];
        let (yaw, pitch, roll) = decode_gimbal_angles(&payload).unwrap();
        assert!((roll - -5.57).abs() < 0.01);
        assert!((pitch - -30.73).abs() < 0.01);
        assert!((yaw - -42.09).abs() < 0.01);
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
}
