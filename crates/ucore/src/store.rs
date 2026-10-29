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

    pub fn find_unit_mut(&mut self, id: &str) -> Option<&mut UnitConfig> {
        self.units.iter_mut().find(|u| alloc::string::ToString::to_string(&u.id) == id)
    }

    /// A unit already there is updated in place; its credentials are only
    /// ever replaced *with* credentials.
    pub fn add_or_update_unit(&mut self, unit: UnitConfig) {
        match self.units.iter_mut().find(|u| u.id == unit.id) {
            Some(existing) => {
                existing.ip = unit.ip;
                existing.port = unit.port;
                existing.name = unit.name;
                if unit.token.is_some() {
                    existing.token = unit.token;
                }
                if unit.key.is_some() {
                    existing.key = unit.key;
                }
            }
            None => self.units.push(unit),
        }
    }

    pub fn remove_unit(&mut self, id: &str) -> bool {
        let before = self.units.len();
        self.units.retain(|u| alloc::string::ToString::to_string(&u.id) != id);
        self.units.len() != before
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
