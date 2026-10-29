//! The V3 session: `0x8370` framing, the token handshake, encrypted requests.
//!
//! ```text
//! 83 70 | len(BE16) | 20 | pad<<4 | type | packet id (BE16) | payload | sign
//! ```
//!
//! `len` covers the payload, its padding and the 32-byte signature but not the
//! packet id, so a packet is `len + 8` bytes.

use super::crypto::{self, CryptoError};
use alloc::vec::Vec;

pub const HANDSHAKE_REQUEST: u8 = 0x0;
pub const HANDSHAKE_RESPONSE: u8 = 0x1;
pub const ENCRYPTED_RESPONSE: u8 = 0x3;
pub const ENCRYPTED_REQUEST: u8 = 0x6;
pub const ERROR: u8 = 0xF;

/// How long a session key is trusted before re-authenticating.
pub const SESSION_LIFETIME_MS: u64 = 12 * 60 * 60 * 1000;

/// A unit ignores its first request for about a second after a handshake;
/// without this wait every first command times out looking like a network
/// fault.
pub const SETTLE_MS: u64 = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanError {
    TooShort(usize),
    NotAPacket([u8; 2]),
    BadMagic(u8),
    Incomplete { need: usize, have: usize },
    DeviceError,
    UnexpectedType(u8),
    BadSignature,
    BadHandshakeLength(usize),
    Rejected,
    Crypto(CryptoError),
}

impl core::fmt::Display for LanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort(n) => write!(f, "packet is only {n} bytes"),
            Self::NotAPacket(b) => write!(f, "packet starts 0x{:02X}{:02X}, not 0x8370", b[0], b[1]),
            Self::BadMagic(m) => write!(f, "magic byte is 0x{m:02X}, not 0x20"),
            Self::Incomplete { need, have } => write!(f, "need {need} bytes, have {have}"),
            Self::DeviceError => write!(f, "device returned an error packet"),
            Self::UnexpectedType(t) => write!(f, "unexpected packet type {t}"),
            Self::BadSignature => write!(f, "packet signature does not match"),
            Self::BadHandshakeLength(n) => write!(f, "handshake payload is {n} bytes, expected 64"),
            Self::Rejected => write!(f, "device rejected the token and key"),
            Self::Crypto(e) => write!(f, "{e}"),
        }
    }
}

impl From<CryptoError> for LanError {
    fn from(e: CryptoError) -> Self {
        Self::Crypto(e)
    }
}

/// The size of the packet starting at `buf`, once its 6-byte header is in.
pub fn packet_len(buf: &[u8]) -> Option<usize> {
    (buf.len() >= 6).then(|| u16::from_be_bytes([buf[2], buf[3]]) as usize + 8)
}

fn header(length: usize, flags: u8) -> [u8; 6] {
    let l = (length as u16).to_be_bytes();
    [0x83, 0x70, l[0], l[1], 0x20, flags]
}

/// The handshake that opens a session: the unit's token, in the clear.
pub fn handshake(packet_id: u16, token: &[u8]) -> Vec<u8> {
    let mut p = header(token.len(), HANDSHAKE_REQUEST).to_vec();
    p.extend_from_slice(&packet_id.to_be_bytes());
    p.extend_from_slice(token);
    p
}

/// The session key from a handshake reply: 32 encrypted bytes and their
/// SHA-256. A digest mismatch is the only sign that the key is wrong.
pub fn session_key(key: &[u8; 32], reply: &[u8]) -> Result<[u8; 32], LanError> {
    if reply.len() != 64 {
        return Err(LanError::BadHandshakeLength(reply.len()));
    }
    let plain = crypto::decrypt_cbc(key, &reply[..32])?;
    if crypto::sha256(&[&plain])[..] != reply[32..] {
        return Err(LanError::Rejected);
    }
    let mut s = [0u8; 32];
    for i in 0..32 {
        s[i] = plain[i] ^ key[i];
    }
    Ok(s)
}

/// Encrypted framing, either direction (a test device answers through here).
pub fn encrypted(session: &[u8; 32], packet_id: u16, data: &[u8], kind: u8) -> Vec<u8> {
    let rem = (data.len() + 2) % 16;
    let pad = if rem != 0 { 16 - rem } else { 0 };
    let hdr = header(data.len() + pad + 32, ((pad as u8) << 4) | kind);
    let mut payload = Vec::with_capacity(2 + data.len() + pad);
    payload.extend_from_slice(&packet_id.to_be_bytes());
    payload.extend_from_slice(data);
    payload.resize(2 + data.len() + pad, 0);
    let digest = crypto::sha256(&[&hdr, &payload]);
    let mut p = hdr.to_vec();
    p.extend_from_slice(&crypto::encrypt_cbc(session, &payload).expect("aligned by construction"));
    p.extend_from_slice(&digest);
    p
}

/// Verify and decrypt an encrypted packet of either direction.
pub fn decrypt(packet: &[u8], session: &[u8; 32]) -> Result<(u8, Vec<u8>), LanError> {
    if packet.len() < 6 + 32 {
        return Err(LanError::TooShort(packet.len()));
    }
    let flags = packet[5];
    let kind = flags & 0xF;
    if kind != ENCRYPTED_REQUEST && kind != ENCRYPTED_RESPONSE {
        return Err(LanError::UnexpectedType(kind));
    }
    let (hdr, rest) = packet.split_at(6);
    let (body, digest) = rest.split_at(rest.len() - 32);
    let plain = crypto::decrypt_cbc(session, body)?;
    if crypto::sha256(&[hdr, &plain])[..] != *digest {
        return Err(LanError::BadSignature);
    }
    let pad = (flags >> 4) as usize;
    if plain.len() < 2 + pad {
        return Err(LanError::TooShort(plain.len()));
    }
    Ok((kind, plain[2..plain.len() - pad].to_vec()))
}

/// What a unit sent back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// 64 bytes for [`session_key`].
    Handshake(Vec<u8>),
    /// A V2 packet.
    Response(Vec<u8>),
}

/// Verify and unwrap a packet from a unit. `session` is needed only for
/// encrypted responses.
pub fn decode(packet: &[u8], session: Option<&[u8; 32]>) -> Result<Reply, LanError> {
    if packet.len() < 8 {
        return Err(LanError::TooShort(packet.len()));
    }
    if packet[..2] != [0x83, 0x70] {
        return Err(LanError::NotAPacket([packet[0], packet[1]]));
    }
    if packet[4] != 0x20 {
        return Err(LanError::BadMagic(packet[4]));
    }
    let need = packet_len(packet).expect("length checked");
    if packet.len() < need {
        return Err(LanError::Incomplete { need, have: packet.len() });
    }
    let packet = &packet[..need];
    match packet[5] & 0xF {
        ERROR => Err(LanError::DeviceError),
        HANDSHAKE_RESPONSE => Ok(Reply::Handshake(packet[8..].to_vec())),
        ENCRYPTED_RESPONSE => {
            let key = session.ok_or(LanError::UnexpectedType(ENCRYPTED_RESPONSE))?;
            Ok(Reply::Response(decrypt(packet, key)?.1))
        }
        other => Err(LanError::UnexpectedType(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device side of a handshake, as a test unit performs it.
    fn device_handshake(key: &[u8; 32], random: [u8; 32]) -> (Vec<u8>, [u8; 32]) {
        let enc = crypto::encrypt_cbc(key, &random).unwrap();
        let mut reply = enc.clone();
        reply.extend_from_slice(&crypto::sha256(&[&random]));
        let mut session = [0u8; 32];
        for i in 0..32 {
            session[i] = random[i] ^ key[i];
        }
        (reply, session)
    }

    #[test]
    fn a_handshake_derives_the_devices_session_key_and_a_wrong_key_is_rejected() {
        let key = [0x42u8; 32];
        let (reply, device_session) = device_handshake(&key, [9u8; 32]);
        assert_eq!(session_key(&key, &reply).unwrap(), device_session);
        assert_eq!(session_key(&[0x43; 32], &reply), Err(LanError::Rejected));
    }

    #[test]
    fn requests_and_responses_round_trip_with_their_padding() {
        let s = [5u8; 32];
        for n in [0usize, 14, 15, 30, 100] {
            let data: Vec<u8> = (0..n as u8).collect();
            let req = encrypted(&s, 7, &data, ENCRYPTED_REQUEST);
            assert_eq!(packet_len(&req), Some(req.len()));
            assert_eq!(decrypt(&req, &s).unwrap(), (ENCRYPTED_REQUEST, data.clone()));
            let resp = encrypted(&s, 7, &data, ENCRYPTED_RESPONSE);
            assert_eq!(decode(&resp, Some(&s)).unwrap(), Reply::Response(data));
        }
    }

    #[test]
    fn a_tampered_packet_fails_its_signature() {
        let s = [5u8; 32];
        let mut p = encrypted(&s, 1, b"hello", ENCRYPTED_RESPONSE);
        p[10] ^= 1;
        assert_eq!(decode(&p, Some(&s)), Err(LanError::BadSignature));
    }
}
