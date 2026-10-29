//! AES-ECB and AES-CBC, MD5 and SHA-256: all the protocol needs.
//!
//! Two key sizes, and mixing them up costs an afternoon: the packet layer's
//! fixed key (MD5 of [`SIGN_KEY`]) is 16 bytes, so AES-**128**-ECB; a V3
//! session uses the unit's 32-byte key, so AES-**256**-CBC with a zero IV.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use alloc::vec::Vec;
use md5::{Digest as _, Md5};
use sha2::Sha256;

/// The vendor's fixed signing key. Public; it makes the format self-checking.
pub const SIGN_KEY: &[u8] = b"xhdiwjnchekd4d512chdjx5d8e4c394D2D7S";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    Misaligned(usize),
    BadPadding(usize),
}

impl core::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Misaligned(n) => write!(f, "{n} bytes is not a multiple of the 16-byte AES block"),
            Self::BadPadding(p) => write!(f, "invalid PKCS#7 padding byte {p}"),
        }
    }
}

/// MD5 of `data` followed by the signing key: a V2 packet's last 16 bytes.
pub fn sign(data: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(data);
    h.update(SIGN_KEY);
    h.finalize().into()
}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn ecb() -> aes::Aes128 {
    let key: [u8; 16] = Md5::digest(SIGN_KEY).into();
    aes::Aes128::new(&GenericArray::from(key))
}

/// AES-128-ECB with PKCS#7 padding, under the fixed key.
pub fn encrypt_ecb(data: &[u8]) -> Vec<u8> {
    let cipher = ecb();
    let pad = 16 - data.len() % 16;
    let mut buf = Vec::with_capacity(data.len() + pad);
    buf.extend_from_slice(data);
    buf.resize(data.len() + pad, pad as u8);
    for block in buf.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    buf
}

pub fn decrypt_ecb(data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if data.is_empty() || data.len() % 16 != 0 {
        return Err(CryptoError::Misaligned(data.len()));
    }
    let cipher = ecb();
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    let pad = out[out.len() - 1] as usize;
    if pad == 0 || pad > 16 || pad > out.len() {
        return Err(CryptoError::BadPadding(pad));
    }
    out.truncate(out.len() - pad);
    Ok(out)
}

/// AES-256-CBC, zero IV, no padding: every payload here is block-aligned.
pub fn encrypt_cbc(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if data.len() % 16 != 0 {
        return Err(CryptoError::Misaligned(data.len()));
    }
    let cipher = aes::Aes256::new(&GenericArray::from(*key));
    let mut prev = [0u8; 16];
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        for i in 0..16 {
            block[i] = chunk[i] ^ prev[i];
        }
        cipher.encrypt_block(GenericArray::from_mut_slice(&mut block));
        prev = block;
        out.extend_from_slice(&block);
    }
    Ok(out)
}

pub fn decrypt_cbc(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if data.len() % 16 != 0 {
        return Err(CryptoError::Misaligned(data.len()));
    }
    let cipher = aes::Aes256::new(&GenericArray::from(*key));
    let mut prev = [0u8; 16];
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        cipher.decrypt_block(GenericArray::from_mut_slice(&mut block));
        for i in 0..16 {
            out.push(block[i] ^ prev[i]);
        }
        prev.copy_from_slice(chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecb_round_trips_and_pads_a_whole_block_when_aligned() {
        for n in [0usize, 1, 15, 16, 17, 40] {
            let data: Vec<u8> = (0..n as u8).collect();
            let enc = encrypt_ecb(&data);
            assert_eq!(enc.len(), (n / 16 + 1) * 16);
            assert_eq!(decrypt_ecb(&enc).unwrap(), data);
        }
    }

    #[test]
    fn cbc_round_trips_and_refuses_misaligned_input() {
        let key = [7u8; 32];
        let data = [3u8; 48];
        assert_eq!(decrypt_cbc(&key, &encrypt_cbc(&key, &data).unwrap()).unwrap(), data);
        assert_eq!(encrypt_cbc(&key, &[0; 5]), Err(CryptoError::Misaligned(5)));
    }
}
