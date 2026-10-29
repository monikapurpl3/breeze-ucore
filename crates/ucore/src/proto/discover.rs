//! Discovery: the 72-byte hello, and what a unit says back about itself.
//!
//! The reply's first bytes say the generation: `5A5A` is V2, `8370` is V3
//! (the same V2 reply in an 8-byte header and a 16-byte hash), `<` is V1's XML,
//! which is not supported. The id is in the clear; the rest is ECB-encrypted.

use super::crypto;
use alloc::string::String;
use core::net::Ipv4Addr;

/// The vendor's hello, reproduced exactly.
pub const HELLO: [u8; 72] = [
    0x5a, 0x5a, 0x01, 0x11, 0x48, 0x00, 0x92, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x7f, 0x75, 0xbd, 0x6b, 0x3e, 0x4f, 0x8b, 0x76, 0x2e, 0x84, 0x9c, 0x6e, 0x57, 0x8d,
    0x65, 0x90, 0x03, 0x6e, 0x9d, 0x43, 0x42, 0xa5, 0x0f, 0x1f, 0x56, 0x9e, 0xb8, 0xec, 0x91, 0x8e, 0x92, 0xe5,
];

/// Units listen on both; some firmware answers only one.
pub const PORTS: [u16; 2] = [6445, 20086];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V2,
    V3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// Where the reply came from: authoritative over what the unit claims.
    pub ip: Ipv4Addr,
    pub port: u16,
    pub id: u64,
    pub ssid: String,
    pub device_type: u8,
    pub version: Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverError {
    UnknownVersion([u8; 2]),
    V1Unsupported,
    Truncated(usize),
    Undecryptable,
    NoDeviceType(String),
}

impl core::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownVersion(b) => write!(f, "reply starts 0x{:02X}{:02X}, no known version", b[0], b[1]),
            Self::V1Unsupported => write!(f, "V1 device: XML discovery is not supported"),
            Self::Truncated(n) => write!(f, "reply of {n} bytes is too short"),
            Self::Undecryptable => write!(f, "reply payload would not decrypt"),
            Self::NoDeviceType(s) => write!(f, "cannot read a device type from SSID {s:?}"),
        }
    }
}

pub fn parse(src: Ipv4Addr, reply: &[u8]) -> Result<Discovered, DiscoverError> {
    let truncated = || DiscoverError::Truncated(reply.len());
    let version = match reply {
        [0x5a, 0x5a, ..] => Version::V2,
        [0x83, 0x70, ..] => Version::V3,
        [b'<', ..] => return Err(DiscoverError::V1Unsupported),
        [a, b, ..] => return Err(DiscoverError::UnknownVersion([*a, *b])),
        _ => return Err(truncated()),
    };
    let body = match version {
        Version::V3 => reply.get(8..reply.len().checked_sub(16).ok_or_else(truncated)?).ok_or_else(truncated)?,
        Version::V2 => reply,
    };
    let end = body.len().checked_sub(16).ok_or_else(truncated)?;
    let encrypted = body.get(40..end).ok_or_else(truncated)?;
    let id = body.get(20..26).ok_or_else(truncated)?.iter().rev().fold(0u64, |n, b| (n << 8) | *b as u64);
    let plain = crypto::decrypt_ecb(encrypted).map_err(|_| DiscoverError::Undecryptable)?;
    if plain.len() < 41 {
        return Err(DiscoverError::Truncated(plain.len()));
    }
    let port = u16::from_le_bytes([plain[4], plain[5]]);
    let name_len = plain[40] as usize;
    let ssid = String::from_utf8_lossy(plain.get(41..41 + name_len).ok_or(DiscoverError::Truncated(plain.len()))?).into_owned();
    // `net_ac_19CA`: the middle field is the appliance type, in hex.
    let device_type = ssid
        .split('_')
        .nth(1)
        .and_then(|t| u8::from_str_radix(t, 16).ok())
        .ok_or_else(|| DiscoverError::NoDeviceType(ssid.clone()))?;
    Ok(Discovered { ip: src, port, id, ssid, device_type, version })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// A reply as a unit builds one: placeholder identity, real layout.
    fn reply(version: Version, id: u64, ip: [u8; 4], port: u16, ssid: &str) -> Vec<u8> {
        let mut plain = alloc::vec![ip[3], ip[2], ip[1], ip[0]];
        plain.extend_from_slice(&port.to_le_bytes());
        plain.extend_from_slice(&[0, 0]);
        plain.extend_from_slice(b"000000P0000000Q1AC00000000000000");
        plain.push(ssid.len() as u8);
        plain.extend_from_slice(ssid.as_bytes());
        let mut v2 = alloc::vec![0x5a, 0x5a, 0x01, 0x11, 0, 0, 0x7a, 0x80];
        v2.resize(20, 0);
        v2.extend_from_slice(&id.to_le_bytes()[..6]);
        v2.resize(40, 0);
        v2.extend_from_slice(&crypto::encrypt_ecb(&plain));
        let sign = crypto::sign(&v2);
        v2.extend_from_slice(&sign);
        match version {
            Version::V2 => v2,
            Version::V3 => {
                let mut v3 = alloc::vec![0x83, 0x70, 0, 0, 0x20, 0x0f, 0, 0];
                v3.extend_from_slice(&v2);
                v3.extend_from_slice(&[0u8; 16]);
                v3
            }
        }
    }

    #[test]
    fn both_generations_parse_and_the_sender_is_authoritative() {
        let src = Ipv4Addr::new(10, 77, 1, 21);
        for version in [Version::V2, Version::V3] {
            let d = parse(src, &reply(version, 151_068_422_923_926, [10, 77, 1, 99], 6444, "net_ac_23E4")).unwrap();
            assert_eq!((d.version, d.id, d.ip, d.port, d.device_type), (version, 151_068_422_923_926, src, 6444, 0xAC));
        }
    }

    #[test]
    fn a_v1_unit_and_truncation_are_errors_not_panics() {
        assert_eq!(parse(Ipv4Addr::LOCALHOST, b"<xml/>"), Err(DiscoverError::V1Unsupported));
        let full = reply(Version::V3, 1, [10, 0, 0, 1], 6444, "net_ac_0001");
        for n in 0..full.len() {
            let _ = parse(Ipv4Addr::LOCALHOST, &full[..n]);
        }
    }
}
