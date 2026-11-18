//! What the deployer gives a μCore, apart from its firmware: `config.json`
//! (the API key and the units, Breeze Core's own format) and `env.json`
//! (μCore's environment: Wi-Fi, zone, time server, name), in a small flash
//! partition of their own (`ucorecfg`).
//!
//! μCore only reads it. The deployer writes it whole over USB, so a new unit
//! or Wi-Fi password needs no new firmware, and the store (paired devices,
//! programs, timers) is never touched by it. The partition is a
//! [`FlashLog`](crate::flashlog::FlashLog) like the store, built on the
//! deployer's machine with [`image`].

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::flashlog::{Flash, FlashLog};

pub const CONFIG: &str = "config.json";
pub const ENV: &str = "env.json";

/// μCore's environment, which Breeze Core takes from the machine it runs on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Env {
    /// The Wi-Fi network; empty on a wired or emulated board.
    #[serde(default)]
    pub wifi_ssid: String,
    #[serde(default)]
    pub wifi_password: String,
    /// The zone as a POSIX TZ rule (see [`crate::tz`]).
    #[serde(default = "default_tz")]
    pub tz: String,
    /// The time server: a name or an address.
    #[serde(default = "default_ntp")]
    pub ntp: String,
    /// The name the board gives the network.
    #[serde(default = "default_hostname")]
    pub hostname: String,
}

fn default_tz() -> String {
    "UTC0".into()
}

fn default_ntp() -> String {
    "pool.ntp.org".into()
}

fn default_hostname() -> String {
    "breeze-ucore".into()
}

impl Default for Env {
    fn default() -> Self {
        Self { wifi_ssid: String::new(), wifi_password: String::new(), tz: default_tz(), ntp: default_ntp(), hostname: default_hostname() }
    }
}

/// Flash that is only RAM: for building an image, and for a board (or an
/// emulator) whose flash cannot be used.
pub struct RamFlash(pub Vec<u8>);

impl RamFlash {
    /// Erased flash of that size.
    pub fn new(size: usize) -> Self {
        Self(alloc::vec![0xFF; size])
    }
}

impl Flash for RamFlash {
    fn size(&self) -> u32 {
        self.0.len() as u32
    }

    fn read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), String> {
        let at = offset as usize;
        let src = self.0.get(at..at + buf.len()).ok_or("read past the end")?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), String> {
        let at = offset as usize;
        let dst = self.0.get_mut(at..at + data.len()).ok_or("write past the end")?;
        // As NOR flash: a write only clears bits.
        for (m, b) in dst.iter_mut().zip(data) {
            *m &= *b;
        }
        Ok(())
    }

    fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
        let at = offset as usize;
        self.0.get_mut(at..at + len as usize).ok_or("erase past the end")?.fill(0xFF);
        Ok(())
    }
}

/// The partition's bytes, holding exactly these files: what the deployer
/// writes to `ucorecfg`.
pub fn image(size: usize, files: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let mut log = FlashLog::open(RamFlash::new(size))?;
    for (name, bytes) in files {
        log.begin(name, true)?;
        let wrote = log.write(bytes);
        let ended = log.end(wrote.is_ok());
        wrote.and(ended)?;
    }
    Ok(log.into_flash().0)
}

/// Read the environment, or the defaults if there is none or it does not
/// parse (said through `log`).
pub fn env<F: Flash>(log: &mut FlashLog<F>, warn: &mut dyn FnMut(&str)) -> Env {
    match log.read(ENV) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn(&alloc::format!("{ENV}: {e}; using the defaults"));
            Env::default()
        }),
        Ok(None) => Env::default(),
        Err(e) => {
            warn(&alloc::format!("{ENV}: {e}; using the defaults"));
            Env::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_opens_with_its_files() {
        let config = br#"{"api_key": "k", "units": []}"#;
        let env = br#"{"wifi_ssid": "home", "wifi_password": "secret", "tz": "CET-1CEST,M3.5.0,M10.5.0/3"}"#;
        let bytes = image(32 * 1024, &[(CONFIG, config), (ENV, env)]).unwrap();
        assert_eq!(bytes.len(), 32 * 1024);
        let mut log = FlashLog::open(RamFlash(bytes)).unwrap();
        assert_eq!(log.read(CONFIG).unwrap().unwrap(), config);
        let e = super::env(&mut log, &mut |_| panic!("no warning"));
        assert_eq!((e.wifi_ssid.as_str(), e.tz.as_str(), e.ntp.as_str()), ("home", "CET-1CEST,M3.5.0,M10.5.0/3", "pool.ntp.org"));
    }

    #[test]
    fn no_environment_is_the_defaults() {
        let mut log = FlashLog::open(RamFlash::new(16 * 1024)).unwrap();
        assert_eq!(env(&mut log, &mut |_| panic!("no warning")), Env::default());
        let bytes = image(16 * 1024, &[(ENV, b"not json")]).unwrap();
        let mut warned = false;
        assert_eq!(env(&mut FlashLog::open(RamFlash(bytes)).unwrap(), &mut |_| warned = true), Env::default());
        assert!(warned);
    }
}
