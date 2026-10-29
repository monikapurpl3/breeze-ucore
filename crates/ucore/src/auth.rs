//! Who may call, and how a device comes to be allowed.
//!
//! The API key first; then a per-device credential, either a v1 bearer token
//! (stored hashed) or a v2 Ed25519 signature over a canonical string (only the
//! public key stored). Pairing is the three-step handshake: a client asks with
//! the API key, an admin **on the LAN** approves the code, the client collects
//! its credential once. Breeze Core's rules, order and wording throughout,
//! including the reason codes and `retryable`, which are wire contract.
//!
//! The order of the v2 checks is load-bearing: the clock before the signature
//! (a drifted phone gets `clock_skew` with the server's time and can re-sign),
//! the signature before the nonce (or anyone could burn a victim's next nonce).

use crate::http::Response;
use crate::platform::Platform;
use crate::store::{DeviceRecord, DevicesDoc};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use base64::Engine as _;
use core::fmt::Write as _;
use core::net::IpAddr;
use serde::Serialize;
use sha3::Digest as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    NoCredential,
    UnknownKey,
    BadSignature,
    BadApiKey,
    ClockSkew,
    Replay,
    IncompleteSignature,
}

impl Reason {
    fn code(self) -> &'static str {
        match self {
            Self::NoCredential => "no_credential",
            Self::UnknownKey => "unknown_key",
            Self::BadSignature => "bad_signature",
            Self::BadApiKey => "bad_api_key",
            Self::ClockSkew => "clock_skew",
            Self::Replay => "replay",
            Self::IncompleteSignature => "incomplete_signature",
        }
    }

    fn retryable(self) -> bool {
        matches!(self, Self::ClockSkew | Self::Replay | Self::IncompleteSignature)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    server_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_skew_seconds: Option<u64>,
}

/// A 401: `{"detail": {"error", "detail", "retryable"}}`.
pub fn unauthorized(reason: Reason, detail: &str) -> Response {
    Response::json(
        401,
        &Body {
            detail: Detail {
                error: reason.code(),
                detail,
                retryable: reason.retryable(),
                server_time: None,
                client_time: None,
                max_skew_seconds: None,
            },
        },
    )
}

#[derive(Serialize)]
struct UpgradeBody {
    detail: UpgradeDetail,
}

#[derive(Serialize)]
struct UpgradeDetail {
    error: &'static str,
    min_auth_version: u8,
    detail: &'static str,
}

/// A 426: this device's auth version is below what the server requires.
pub fn upgrade_required(min_auth_version: u8) -> Response {
    Response::json(
        426,
        &UpgradeBody {
            detail: UpgradeDetail {
                error: "auth_upgrade_required",
                min_auth_version,
                detail: "This version of the app uses an outdated, less secure connection. Please update Breeze to continue controlling your units.",
            },
        },
    )
}

/// Constant time over equal lengths.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
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
    hex(&crate::proto::crypto::sha256(&[secret.as_bytes()]))
}

/// The client as the LAN rule judges it: `X-Forwarded-For`'s last entry only
/// behind a proxy, otherwise the socket's peer.
pub fn client_ip(peer: Option<IpAddr>, forwarded_for: Option<&str>, behind_proxy: bool) -> Option<IpAddr> {
    if behind_proxy {
        if let Some(ip) = forwarded_for.and_then(|x| x.rsplit(',').next()).and_then(|l| l.trim().parse().ok()) {
            return Some(ip);
        }
    }
    peer
}

pub fn is_private_ip(ip: Option<IpAddr>) -> bool {
    match ip {
        Some(IpAddr::V4(v4)) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        Some(IpAddr::V6(v6)) => {
            let s = v6.segments();
            v6.is_loopback() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
        None => false,
    }
}

// ------------------------------------------------------------------- signing

/// Base64 in either alphabet, padded or not, as clients send it.
pub fn b64_decode(value: &str) -> Option<Vec<u8>> {
    let normalised: String =
        value.trim_end_matches('=').chars().map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        }).collect();
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(normalised).ok()
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `breeze-auth-v2\n{METHOD}\n{path?query}\n{timestamp}\n{nonce}\n{sha3_512(body)}`,
/// which three clients already reproduce byte for byte.
pub fn canonical(method: &str, path: &str, timestamp: &str, nonce: &str, body: &[u8]) -> Vec<u8> {
    alloc::format!(
        "breeze-auth-v2\n{}\n{path}\n{timestamp}\n{nonce}\n{}",
        method.to_uppercase(),
        hex(&sha3::Sha3_512::digest(body))
    )
    .into_bytes()
}

fn public_key(b64: &str) -> Option<ed25519_dalek::VerifyingKey> {
    let bytes: [u8; 32] = b64_decode(b64)?.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&bytes).ok()
}

pub fn public_key_is_valid(b64: &str) -> bool {
    public_key(b64).is_some()
}

fn signature_ok(public_key_b64: &str, message: &[u8], signature_b64: &str) -> bool {
    let (Some(key), Some(sig)) = (public_key(public_key_b64), b64_decode(signature_b64)) else { return false };
    let Ok(sig) = <[u8; 64]>::try_from(sig.as_slice()) else { return false };
    key.verify_strict(message, &ed25519_dalek::Signature::from_bytes(&sig)).is_ok()
}

/// What a device presented.
pub struct Presented<'a> {
    pub bearer: Option<&'a str>,
    pub auth_version: Option<&'a str>,
    pub key_id: Option<&'a str>,
    pub timestamp: Option<&'a str>,
    pub nonce: Option<&'a str>,
    pub signature: Option<&'a str>,
    pub method: &'a str,
    /// Query included: the signature covers it.
    pub path: &'a str,
    pub body: &'a [u8],
}

/// Nonces seen inside the clock window. Bounded: when full, the one closest to
/// expiring is dropped (only a request older than it could be replayed).
pub struct Nonces {
    skew: u64,
    seen: Vec<(String, f64)>,
}

pub const MAX_NONCES: usize = 256;

impl Nonces {
    pub fn new(skew_seconds: u64) -> Self {
        Self { skew: skew_seconds, seen: Vec::new() }
    }

    fn in_window(&self, timestamp: &str, now: f64) -> bool {
        match timestamp.parse::<f64>() {
            Ok(ts) if ts.is_finite() => (now - ts).abs() <= self.skew as f64,
            _ => false,
        }
    }

    fn check_and_store(&mut self, nonce: &str, now: f64) -> bool {
        self.seen.retain(|(_, expiry)| *expiry >= now);
        if self.seen.iter().any(|(n, _)| n == nonce) {
            return false;
        }
        if self.seen.len() >= MAX_NONCES {
            self.seen.remove(0);
        }
        self.seen.push((nonce.to_string(), now + self.skew as f64));
        true
    }
}

/// Rounded half away from zero, as `f64::round`, which `core` lacks.
fn round(x: f64) -> f64 {
    let t = x as i64 as f64;
    let f = x - t;
    if f >= 0.5 {
        t + 1.0
    } else if f <= -0.5 {
        t - 1.0
    } else {
        t
    }
}

pub enum Decision {
    Allow(String),
    Reject(Response),
}

/// The device credential, v1 or v2 by the `X-Breeze-Auth-Version` header.
pub fn verify_device(devices: &DevicesDoc, p: &Presented<'_>, nonces: &mut Nonces, now: f64, min_auth_version: u8) -> Decision {
    let found: Result<&DeviceRecord, Response> = if p.auth_version == Some("2") {
        verify_v2(devices, p, nonces, now, min_auth_version)
    } else {
        verify_v1(devices, p.bearer, now, min_auth_version)
    };
    match found {
        Ok(r) => Decision::Allow(r.token_id.clone()),
        Err(response) => Decision::Reject(response),
    }
}

fn verify_v1<'a>(devices: &'a DevicesDoc, token: Option<&str>, now: f64, min: u8) -> Result<&'a DeviceRecord, Response> {
    let token = token.unwrap_or("");
    let found = if token.is_empty() {
        None
    } else {
        let candidate = hash_secret(token);
        let mut found = None;
        for r in &devices.devices {
            if r.token_hash.as_deref().is_some_and(|stored| same(stored.as_bytes(), candidate.as_bytes())) {
                found = Some(r);
            }
        }
        found.filter(|r| !r.expires_at.is_some_and(|e| e < now))
    };
    let Some(record) = found else {
        return Err(unauthorized(
            if token.is_empty() { Reason::NoCredential } else { Reason::UnknownKey },
            "missing, invalid, or expired device token — enroll this device to get one",
        ));
    };
    if record.auth_version < min {
        return Err(upgrade_required(min));
    }
    Ok(record)
}

fn verify_v2<'a>(devices: &'a DevicesDoc, p: &Presented<'_>, nonces: &mut Nonces, now: f64, min: u8) -> Result<&'a DeviceRecord, Response> {
    let incomplete = || unauthorized(Reason::IncompleteSignature, "incomplete signature headers");
    let (Some(key_id), Some(timestamp), Some(nonce), Some(signature)) = (p.key_id, p.timestamp, p.nonce, p.signature) else {
        return Err(incomplete());
    };
    if key_id.is_empty() || timestamp.is_empty() || nonce.is_empty() || signature.is_empty() {
        return Err(incomplete());
    }
    let record = devices
        .devices
        .iter()
        .find(|r| r.token_id == key_id && r.auth_version == 2)
        .filter(|r| !r.expires_at.is_some_and(|e| e < now))
        .ok_or_else(|| unauthorized(Reason::UnknownKey, "unknown, expired, or non-v2 device key"))?;
    if record.auth_version < min {
        return Err(upgrade_required(min));
    }
    if !nonces.in_window(timestamp, now) {
        return Err(Response::json(
            401,
            &Body {
                detail: Detail {
                    error: Reason::ClockSkew.code(),
                    detail: "request timestamp outside the allowed window — check the device clock",
                    retryable: true,
                    server_time: Some(round(now * 1000.0) / 1000.0),
                    client_time: Some(timestamp),
                    max_skew_seconds: Some(nonces.skew),
                },
            },
        ));
    }
    let message = canonical(p.method, p.path, timestamp, nonce, p.body);
    if !signature_ok(record.public_key.as_deref().unwrap_or(""), &message, signature) {
        return Err(unauthorized(Reason::BadSignature, "bad request signature"));
    }
    if !nonces.check_and_store(nonce, now) {
        return Err(unauthorized(Reason::Replay, "replayed request (nonce already used) — retry with a fresh nonce"));
    }
    Ok(record)
}

// ------------------------------------------------------------------ pairing

struct Session {
    id: String,
    code_hash: String,
    label: String,
    expires_at: f64,
    auth_version: u8,
    public_key: Option<String>,
    /// The minted record and, for v1, the token handed over once.
    approved: Option<(DeviceRecord, Option<String>)>,
}

/// Pairings under way. Seconds old and in memory only: a restart loses them
/// and whoever was pairing starts again. Bounded; a full table refuses more.
pub struct Enrollment {
    sessions: Vec<Session>,
    code_ttl: u64,
    token_ttl_days: i64,
}

pub const MAX_PENDING: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum EnrollError {
    BadPublicKey,
    Full,
}

impl core::fmt::Display for EnrollError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadPublicKey => write!(f, "public_key is not a valid Ed25519 key"),
            Self::Full => write!(f, "too many pairings under way; try again in a minute"),
        }
    }
}

pub struct Started {
    pub session_id: String,
    pub user_code: String,
    pub expires_in: u64,
}

/// What a poll finds.
pub enum Poll {
    Pending,
    Unknown,
    Approved(DeviceRecord, Option<String>),
}

/// Upper case, no spaces or dashes: what a person may type for `ABCD-EFGH`.
fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| !c.is_whitespace() && *c != '-').flat_map(char::to_uppercase).collect()
}

fn random(p: &mut impl Platform, n: usize) -> Vec<u8> {
    let mut b = alloc::vec![0u8; n];
    p.random(&mut b);
    b
}

/// Eight characters of RFC 4648 base32 from 40 random bits, as `ABCD-EFGH`.
fn pairing_code(p: &mut impl Platform) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let n = random(p, 5).iter().fold(0u64, |n, b| (n << 8) | *b as u64);
    let chars: String = (0..8).map(|i| ALPHABET[((n >> (35 - i * 5)) & 0x1F) as usize] as char).collect();
    alloc::format!("{}-{}", &chars[..4], &chars[4..])
}

impl Enrollment {
    pub fn new(code_ttl: u64, token_ttl_days: i64) -> Self {
        Self { sessions: Vec::new(), code_ttl, token_ttl_days }
    }

    fn sweep(&mut self, now: f64) {
        self.sessions.retain(|s| s.expires_at >= now);
    }

    pub fn start(&mut self, p: &mut impl Platform, label: &str, auth_version: u8, public_key: Option<&str>, now: f64) -> Result<Started, EnrollError> {
        if auth_version == 2 && !public_key.is_some_and(public_key_is_valid) {
            return Err(EnrollError::BadPublicKey);
        }
        self.sweep(now);
        if self.sessions.len() >= MAX_PENDING {
            return Err(EnrollError::Full);
        }
        let session_id = b64url(&random(p, 18));
        let user_code = pairing_code(p);
        let label = label.trim();
        self.sessions.push(Session {
            id: session_id.clone(),
            code_hash: hash_secret(&normalize_code(&user_code)),
            label: if label.is_empty() { String::from("unnamed device") } else { label.to_string() },
            expires_at: now + self.code_ttl as f64,
            auth_version,
            public_key: public_key.map(str::to_string),
            approved: None,
        });
        Ok(Started { session_id, user_code, expires_in: self.code_ttl })
    }

    /// Mint the device for a code. The record is the caller's to store.
    pub fn approve(&mut self, p: &mut impl Platform, code: &str, now: f64) -> Option<DeviceRecord> {
        self.sweep(now);
        let target = hash_secret(&normalize_code(code));
        let i = self.sessions.iter().rposition(|s| s.approved.is_none() && same(s.code_hash.as_bytes(), target.as_bytes()))?;
        let expires_at = (self.token_ttl_days > 0).then_some(now + self.token_ttl_days as f64 * 86400.0);
        let token_id = hex(&random(p, 8));
        let s = &self.sessions[i];
        let (record, token) = if s.auth_version == 2 {
            let r = DeviceRecord {
                token_id,
                label: s.label.clone(),
                auth_version: 2,
                token_hash: None,
                public_key: s.public_key.clone(),
                created_at: now,
                expires_at,
                last_used: None,
            };
            (r, None)
        } else {
            let token = b64url(&random(p, 32));
            let r = DeviceRecord {
                token_id,
                label: s.label.clone(),
                auth_version: 1,
                token_hash: Some(hash_secret(&token)),
                public_key: None,
                created_at: now,
                expires_at,
                last_used: None,
            };
            (r, Some(token))
        };
        self.sessions[i].approved = Some((record.clone(), token));
        Some(record)
    }

    /// Collected once: an approved session is gone after its first poll.
    pub fn poll(&mut self, session_id: &str, now: f64) -> Poll {
        self.sweep(now);
        let Some(i) = self.sessions.iter().position(|s| s.id == session_id) else { return Poll::Unknown };
        if self.sessions[i].approved.is_none() {
            return Poll::Pending;
        }
        let s = self.sessions.remove(i);
        let (record, token) = s.approved.expect("checked");
        Poll::Approved(record, token)
    }
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

    #[test]
    fn codes_normalise_as_people_type_them() {
        assert_eq!(normalize_code(" abcd-efgh "), "ABCDEFGH");
    }

    #[test]
    fn round_is_half_away_from_zero() {
        assert_eq!((round(2.5), round(-2.5), round(2.4999), round(1792000000123.5)), (3.0, -3.0, 2.0, 1792000000124.0));
    }

    #[test]
    fn the_lan_rule() {
        let ip = |s: &str| Some(s.parse().unwrap());
        assert!(is_private_ip(ip("192.168.1.5")) && is_private_ip(ip("127.0.0.1")) && is_private_ip(ip("fe80::1")));
        assert!(!is_private_ip(ip("8.8.8.8")) && !is_private_ip(None));
        assert_eq!(client_ip(ip("127.0.0.1"), Some("1.2.3.4, 10.0.0.9"), true), ip("10.0.0.9"));
        assert_eq!(client_ip(ip("127.0.0.1"), Some("10.0.0.9"), false), ip("127.0.0.1"));
    }
}
