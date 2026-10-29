//! The innermost layer: 0xAA appliance frames, and the CRC-8 their command
//! payloads carry.
//!
//! ```text
//! 0xAA  len  0xAC  .. .. .. .. ..  ver  frame_type  <payload>  checksum
//! ```
//!
//! `len` counts the 10-byte header and the payload, not the checksum.

use alloc::vec::Vec;

pub const HEADER_LEN: usize = 10;
pub const AIR_CONDITIONER: u8 = 0xAC;

pub const CONTROL: u8 = 0x02;
pub const QUERY: u8 = 0x03;

/// msmart's CRC-8 table; not a standard polynomial, so the table is the spec.
const CRC8: [u8; 256] = [
    0x00, 0x5E, 0xBC, 0xE2, 0x61, 0x3F, 0xDD, 0x83, 0xC2, 0x9C, 0x7E, 0x20, 0xA3, 0xFD, 0x1F, 0x41,
    0x9D, 0xC3, 0x21, 0x7F, 0xFC, 0xA2, 0x40, 0x1E, 0x5F, 0x01, 0xE3, 0xBD, 0x3E, 0x60, 0x82, 0xDC,
    0x23, 0x7D, 0x9F, 0xC1, 0x42, 0x1C, 0xFE, 0xA0, 0xE1, 0xBF, 0x5D, 0x03, 0x80, 0xDE, 0x3C, 0x62,
    0xBE, 0xE0, 0x02, 0x5C, 0xDF, 0x81, 0x63, 0x3D, 0x7C, 0x22, 0xC0, 0x9E, 0x1D, 0x43, 0xA1, 0xFF,
    0x46, 0x18, 0xFA, 0xA4, 0x27, 0x79, 0x9B, 0xC5, 0x84, 0xDA, 0x38, 0x66, 0xE5, 0xBB, 0x59, 0x07,
    0xDB, 0x85, 0x67, 0x39, 0xBA, 0xE4, 0x06, 0x58, 0x19, 0x47, 0xA5, 0xFB, 0x78, 0x26, 0xC4, 0x9A,
    0x65, 0x3B, 0xD9, 0x87, 0x04, 0x5A, 0xB8, 0xE6, 0xA7, 0xF9, 0x1B, 0x45, 0xC6, 0x98, 0x7A, 0x24,
    0xF8, 0xA6, 0x44, 0x1A, 0x99, 0xC7, 0x25, 0x7B, 0x3A, 0x64, 0x86, 0xD8, 0x5B, 0x05, 0xE7, 0xB9,
    0x8C, 0xD2, 0x30, 0x6E, 0xED, 0xB3, 0x51, 0x0F, 0x4E, 0x10, 0xF2, 0xAC, 0x2F, 0x71, 0x93, 0xCD,
    0x11, 0x4F, 0xAD, 0xF3, 0x70, 0x2E, 0xCC, 0x92, 0xD3, 0x8D, 0x6F, 0x31, 0xB2, 0xEC, 0x0E, 0x50,
    0xAF, 0xF1, 0x13, 0x4D, 0xCE, 0x90, 0x72, 0x2C, 0x6D, 0x33, 0xD1, 0x8F, 0x0C, 0x52, 0xB0, 0xEE,
    0x32, 0x6C, 0x8E, 0xD0, 0x53, 0x0D, 0xEF, 0xB1, 0xF0, 0xAE, 0x4C, 0x12, 0x91, 0xCF, 0x2D, 0x73,
    0xCA, 0x94, 0x76, 0x28, 0xAB, 0xF5, 0x17, 0x49, 0x08, 0x56, 0xB4, 0xEA, 0x69, 0x37, 0xD5, 0x8B,
    0x57, 0x09, 0xEB, 0xB5, 0x36, 0x68, 0x8A, 0xD4, 0x95, 0xCB, 0x29, 0x77, 0xF4, 0xAA, 0x48, 0x16,
    0xE9, 0xB7, 0x55, 0x0B, 0x88, 0xD6, 0x34, 0x6A, 0x2B, 0x75, 0x97, 0xC9, 0x4A, 0x14, 0xF6, 0xA8,
    0x74, 0x2A, 0xC8, 0x96, 0x15, 0x4B, 0xA9, 0xF7, 0xB6, 0xE8, 0x0A, 0x54, 0xD7, 0x89, 0x6B, 0x35,
];

pub fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0, |crc, b| CRC8[(crc ^ b) as usize])
}

/// Two's complement of the byte sum: the frame's trailing checksum.
pub fn checksum(data: &[u8]) -> u8 {
    (!data.iter().fold(0u8, |a, b| a.wrapping_add(*b))).wrapping_add(1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    TooShort(usize),
    LengthMismatch { declared: usize, actual: usize },
    BadChecksum { expected: u8, found: u8 },
    WrongDeviceType(u8),
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort(n) => write!(f, "frame is {n} bytes, shorter than the {HEADER_LEN}-byte header"),
            Self::LengthMismatch { declared, actual } => {
                write!(f, "frame declares {declared} bytes but {actual} arrived")
            }
            Self::BadChecksum { expected, found } => {
                write!(f, "frame checksum is 0x{found:02X}, expected 0x{expected:02X}")
            }
            Self::WrongDeviceType(t) => write!(f, "frame is for device type 0x{t:02X}, not an air conditioner"),
        }
    }
}

/// A command payload: the bytes, then the message id, then the CRC over both,
/// framed for an air conditioner.
pub fn command(frame_type: u8, payload: &[u8], message_id: u8) -> Vec<u8> {
    let len = HEADER_LEN + payload.len() + 2;
    let mut f = Vec::with_capacity(len + 1);
    f.extend_from_slice(&[0xAA, len as u8, AIR_CONDITIONER, 0, 0, 0, 0, 0, 0, frame_type]);
    f.extend_from_slice(payload);
    f.push(message_id);
    let crc = crc8(&f[HEADER_LEN..]);
    f.push(crc);
    let sum = checksum(&f[1..]);
    f.push(sum);
    f
}

/// Check a received frame and return its payload (between header and checksum).
///
/// A frame longer than `len` claims is tolerated, as msmart does; shorter is
/// truncated and refused.
pub fn parse(frame: &[u8]) -> Result<&[u8], FrameError> {
    if frame.len() < HEADER_LEN {
        return Err(FrameError::TooShort(frame.len()));
    }
    let declared = frame[1] as usize + 1;
    if frame.len() < declared {
        return Err(FrameError::LengthMismatch { declared, actual: frame.len() });
    }
    let frame = &frame[..declared];
    let expected = checksum(&frame[1..frame.len() - 1]);
    let found = frame[frame.len() - 1];
    if expected != found {
        return Err(FrameError::BadChecksum { expected, found });
    }
    if frame[2] != AIR_CONDITIONER {
        return Err(FrameError::WrongDeviceType(frame[2]));
    }
    Ok(&frame[HEADER_LEN..frame.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc8_matches_msmart() {
        assert_eq!(crc8(&[]), 0);
        let payload = [
            0x41, 0x81, 0x00, 0xFF, 0x03, 0xFF, 0x00, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x03, 0x11,
        ];
        assert_eq!(crc8(&payload), 0xF4);
    }

    #[test]
    fn a_flipped_bit_and_a_short_frame_are_refused() {
        let mut f = command(QUERY, &[9, 9, 9], 1);
        assert!(parse(&f).is_ok());
        f[11] ^= 1;
        assert!(matches!(parse(&f), Err(FrameError::BadChecksum { .. })));
        assert_eq!(parse(&[0xAA, 2]), Err(FrameError::TooShort(2)));
        let f = command(QUERY, &[1, 2, 3], 1);
        assert!(matches!(parse(&f[..f.len() - 2]), Err(FrameError::LengthMismatch { .. })));
    }

    #[test]
    fn the_commercial_appliance_type_is_refused() {
        let mut f = command(QUERY, &[0], 1);
        f[2] = 0xCC;
        let last = f.len() - 1;
        f[last] = checksum(&f[1..last]);
        assert_eq!(parse(&f), Err(FrameError::WrongDeviceType(0xCC)));
    }
}
