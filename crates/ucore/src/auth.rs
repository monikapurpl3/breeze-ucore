//! Who may call: the API key, then a per-device credential. Breeze Core's
//! rules and wording, including the reason codes and `retryable`, which are
//! wire contract (a client that cannot tell a drifted clock from a revoked
//! credential deletes its key and strands its user).

use crate::http::Response;
use crate::store::{DeviceRecord, DevicesDoc};
use alloc::string::String;
use core::fmt::Write as _;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    NoCredential,
    UnknownKey,
    BadApiKey,
}

impl Reason {
    fn code(self) -> &'static str {
        match self {
            Self::NoCredential => "no_credential",
            Self::UnknownKey => "unknown_key",
            Self::BadApiKey => "bad_api_key",
        }
    }
}

#[derive(Serialize)]
struct Body<'a> {
    detail: Detail<'a>,
}

#[derive(Serialize)]
struct Detail<'a> {
    error: &'static str,
    detail: &'a str,
    retryable: bool,
}

/// A 401 in Breeze Core's shape: `{"detail": {"error", "detail", "retryable"}}`.
pub fn unauthorized(reason: Reason, detail: &str) -> Response {
    Response::json(401, &Body { detail: Detail { error: reason.code(), detail, retryable: false } })
}

/// Constant time over equal lengths.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The shared API key. With none configured, nothing gets in.
pub fn check_api_key(presented: Option<&str>, expected: Option<&str>) -> Result<(), Response> {
    let presented = presented.unwrap_or("");
    let expected = expected.unwrap_or("");
    if same(presented.as_bytes(), expected.as_bytes()) && !expected.is_empty() {
        return Ok(());
    }
    Err(unauthorized(
        if presented.is_empty() { Reason::NoCredential } else { Reason::BadApiKey },
        "missing or invalid X-API-Key header",
    ))
}

/// `Authorization: Bearer <token>`, any case of `bearer`.
pub fn bearer(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// SHA-256 hex of a secret, as `devices.json` stores it.
pub fn hash_secret(secret: &str) -> String {
    let digest = crate::proto::crypto::sha256(&[secret.as_bytes()]);
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// A v1 device token: the matching unexpired record. Every record is compared
/// (constant time each) and the last match wins, as in Breeze Core.
pub fn check_v1<'a>(devices: &'a DevicesDoc, token: Option<&str>, now: f64) -> Result<&'a DeviceRecord, Response> {
    let token = token.unwrap_or("");
    let found = if token.is_empty() {
        None
    } else {
        let candidate = hash_secret(token);
        let mut found = None;
        for r in &devices.devices {
            if let Some(stored) = r.token_hash.as_deref() {
                if same(stored.as_bytes(), candidate.as_bytes()) {
                    found = Some(r);
                }
            }
        }
        found.filter(|r| !r.expires_at.is_some_and(|e| e < now))
    };
    found.ok_or_else(|| {
        unauthorized(
            if token.is_empty() { Reason::NoCredential } else { Reason::UnknownKey },
            "missing, invalid, or expired device token — enroll this device to get one",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_takes_any_case_and_refuses_empty() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer   "), None);
    }

    #[test]
    fn hash_secret_is_sha256_hex() {
        assert_eq!(hash_secret("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn no_configured_key_lets_nobody_in() {
        assert!(check_api_key(Some(""), None).is_err());
        assert!(check_api_key(Some("k"), Some("k")).is_ok());
        assert!(check_api_key(Some("k2"), Some("k")).is_err());
    }
}
