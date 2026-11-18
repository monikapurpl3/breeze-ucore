//! Breeze Core's store files, in Breeze Core's formats, so a configuration
//! moves between the two either way. Field order is the files' order.

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

fn default_port() -> u16 {
    6444
}

fn default_auth_version() -> u8 {
    1
}

/// A unit in `config.json`. `token` and `key` are hex; without them a V3 unit
/// cannot be reached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnitConfig {
    pub name: String,
    pub ip: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub id: i64,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub units: Vec<UnitConfig>,
}

impl AppConfig {
    /// By the id as written: `0123` is not unit 123 here, as in Breeze Core.
    pub fn find_unit(&self, id: &str) -> Option<&UnitConfig> {
        self.units.iter().find(|u| alloc::string::ToString::to_string(&u.id) == id)
    }
}

/// A paired client in `devices.json`: a hashed v1 token or a v2 public key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub token_id: String,
    pub label: String,
    #[serde(default = "default_auth_version")]
    pub auth_version: u8,
    #[serde(default)]
    pub token_hash: Option<String>,
    #[serde(default)]
    pub public_key: Option<String>,
    pub created_at: f64,
    #[serde(default)]
    pub expires_at: Option<f64>,
    #[serde(default)]
    pub last_used: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DevicesDoc {
    #[serde(default)]
    pub devices: Vec<DeviceRecord>,
}

/// A store file's bytes, as Breeze Core writes them: two-space indent, no
/// trailing newline, floats in their shortest round-tripping form.
pub fn to_json<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec_pretty(value).unwrap_or_default()
}

/// `{"<key>": [items]}` in exactly the bytes [`to_json`] gives, handed to
/// `out` a piece at a time and never whole: each item is serialised on its
/// own, and indented by the four spaces its place in
/// the list adds. That is sound because pretty JSON never has a newline
/// inside a string (it is written `\n`), so every newline in an item is one
/// of the formatter's own.
pub fn write_list<T: Serialize>(key: &str, items: &[T], out: &mut dyn FnMut(&[u8]) -> Result<(), String>) -> Result<(), String> {
    let quoted = serde_json::to_vec(key).map_err(|e| alloc::format!("{e}"))?;
    out(b"{\n  ")?;
    out(&quoted)?;
    if items.is_empty() {
        return out(b": []\n}");
    }
    out(b": [\n")?;
    for (i, item) in items.iter().enumerate() {
        // One item's worth: a few hundred bytes to a couple of kilobytes.
        let buf = serde_json::to_vec_pretty(item).map_err(|e| alloc::format!("{e}"))?;
        out(b"    ")?;
        for (j, line) in buf.split(|b| *b == b'\n').enumerate() {
            if j > 0 {
                out(b"\n    ")?;
            }
            out(line)?;
        }
        out(if i + 1 < items.len() { b",\n" } else { b"\n" })?;
    }
    out(b"  ]\n}")
}

/// Hex to bytes; `None` for anything that is not an even run of hex digits.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}
