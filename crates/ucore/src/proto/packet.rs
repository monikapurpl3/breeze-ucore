//! The V2 packet: a frame wrapped in a 40-byte header, ECB-encrypted, with an
//! MD5 tail. V3 units carry the same packet inside a session (`lan.rs`).
//!
//! ```text
//! 5A 5A | 01 11 | len(LE16) | 20 00 | msg id (4) | timestamp (8) |
//! device id (LE64) | 12 reserved | AES-ECB(frame) | MD5(all above + SIGN_KEY)
//! ```

use super::crypto::{self, CryptoError};
use alloc::vec::Vec;

pub const HEADER_LEN: usize = 40;
pub const TAIL_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketError {
    TooShort(usize),
    NotAPacket([u8; 2]),
    Truncated { declared: usize, actual: usize },
    BadDigest,
    Crypto(CryptoError),
}

impl core::fmt::Display for PacketError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort(n) => write!(f, "packet is only {n} bytes"),
            Self::NotAPacket(b) => write!(f, "packet starts 0x{:02X}{:02X}, not 0x5A5A", b[0], b[1]),
            Self::Truncated { declared, actual } => write!(f, "packet declares {declared} bytes but {actual} arrived"),
            Self::BadDigest => write!(f, "packet MD5 digest does not match"),
            Self::Crypto(e) => write!(f, "{e}"),
        }
    }
}

/// The 8-byte timestamp field, `[centiseconds, s, m, H, D, M, YY, YYYY/100]`,
/// from Unix time. Nothing seems to check it; zeros when the clock is unset.
pub fn timestamp(unix_ms: Option<u64>) -> [u8; 8] {
    let Some(ms) = unix_ms else { return [0; 8] };
    let secs = ms / 1000;
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u8;
    let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }) as u32;
    [
        ((ms % 1000) / 10) as u8,
        (rem % 60) as u8,
        ((rem % 3600) / 60) as u8,
        (rem / 3600) as u8,
        day,
        month,
        (year % 100) as u8,
        (year / 100) as u8,
    ]
}

/// Wrap a framed message in a V2 packet addressed to `device_id`.
pub fn encode(device_id: u64, frame: &[u8], ts: [u8; 8]) -> Vec<u8> {
    let body = crypto::encrypt_ecb(frame);
    let length = (HEADER_LEN + body.len() + TAIL_LEN) as u16;
    let mut p = Vec::with_capacity(length as usize);
    p.extend_from_slice(&[0x5A, 0x5A, 0x01, 0x11]);
    p.extend_from_slice(&length.to_le_bytes());
    p.extend_from_slice(&[0x20, 0x00, 0, 0, 0, 0]);
    p.extend_from_slice(&ts);
    p.extend_from_slice(&device_id.to_le_bytes());
    p.extend_from_slice(&[0u8; 12]);
    p.extend_from_slice(&body);
    let digest = crypto::sign(&p);
    p.extend_from_slice(&digest);
    p
}

/// Verify a V2 packet and return the frame inside it.
pub fn decode(packet: &[u8]) -> Result<Vec<u8>, PacketError> {
    if packet.len() < HEADER_LEN + TAIL_LEN {
        return Err(PacketError::TooShort(packet.len()));
    }
    if packet[..2] != [0x5A, 0x5A] {
        return Err(PacketError::NotAPacket([packet[0], packet[1]]));
    }
    let declared = u16::from_le_bytes([packet[4], packet[5]]) as usize;
    if packet.len() < declared || declared < HEADER_LEN + TAIL_LEN {
        return Err(PacketError::Truncated { declared, actual: packet.len() });
    }
    let split = declared - TAIL_LEN;
    if crypto::sign(&packet[..split]) != packet[split..declared] {
        return Err(PacketError::BadDigest);
    }
    crypto::decrypt_ecb(&packet[HEADER_LEN..split]).map_err(PacketError::Crypto)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packet_round_trips_and_a_tampered_one_is_refused() {
        let frame = [0xAAu8, 1, 2, 3, 4, 5];
        let mut p = encode(0x1234_5678_9ABC, &frame, timestamp(Some(1_790_000_000_123)));
        assert_eq!(decode(&p).unwrap(), frame);
        p[45] ^= 1;
        assert_eq!(decode(&p), Err(PacketError::BadDigest));
    }

    #[test]
    fn the_timestamp_is_a_civil_date() {
        // 1_790_000_000 s is 2026-09-21 14:13:20 UTC.
        assert_eq!(timestamp(Some(1_790_000_000_123)), [12, 20, 13, 14, 21, 9, 26, 20]);
        assert_eq!(timestamp(None), [0; 8]);
    }
}
