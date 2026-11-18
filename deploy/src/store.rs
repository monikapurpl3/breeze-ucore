//! The deployments, each a folder in the helper's own data folder:
//!
//! ```text
//! deployments/<id>/deployment.json   the name, the board, the environment
//! deployments/<id>/config.json       the API key and the units (Breeze Core's format)
//! deployments/<id>/flash.bin         a QEMU board's flash, once built
//! deployments/<id>/serial.log        a QEMU board's console
//! deployments/<id>/mock.xml          simulated units' blueprint, written at each start
//! deployments/<id>/mock.log          what breeze-mock said
//! ```
//!
//! config.json and deployment.json hold secrets (unit tokens, the Wi-Fi
//! password, the API key), so on Unix they are the user's alone (0600, in
//! 0700 folders); on Windows the data folder is under the user's own
//! profile.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ucore::bundle::Env;
use ucore::store::AppConfig;

use crate::boards::Board;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    pub name: String,
    pub board: Board,
    pub env: Env,
    /// The zone's name as picked on the page (env.tz holds its rule).
    #[serde(default)]
    pub zone: String,
    /// Where a QEMU board's API is forwarded to on this computer.
    #[serde(default = "default_port")]
    pub qemu_port: u16,
    /// Which units are simulated by breeze-mock, if any.
    #[serde(default)]
    pub simulated: Option<crate::mock::Simulated>,
}

fn default_port() -> u16 {
    18420
}

pub struct Store {
    root: PathBuf,
}

fn private_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Replace a file whole: a temporary file beside it, then a rename.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The API key's shape, as Breeze Core mints it (`secrets.token_urlsafe(24)`).
pub fn new_api_key() -> String {
    const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = [0u8; 24];
    getrandom::getrandom(&mut bytes).expect("the system's random source");
    // 24 bytes are 32 characters of base64, no padding.
    let mut out = String::with_capacity(32);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        for shift in [18, 12, 6, 0] {
            out.push(URL_SAFE[((n >> shift) & 63) as usize] as char);
        }
    }
    out
}

impl Store {
    pub fn open(data: &Path) -> Result<Self, String> {
        let root = data.join("deployments");
        private_dir(data)?;
        private_dir(&root)?;
        Ok(Self { root })
    }

    pub fn dir(&self, id: &str) -> Result<PathBuf, String> {
        // An id is ours: lower-case letters, digits and dashes, nothing that
        // could climb out of the folder.
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(format!("no deployment {id:?}"));
        }
        Ok(self.root.join(id))
    }

    pub fn list(&self) -> Vec<Deployment> {
        let mut out: Vec<Deployment> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| self.load(&e.file_name().to_string_lossy()).ok())
            .collect();
        out.sort_by_key(|d| d.name.to_lowercase());
        out
    }

    pub fn load(&self, id: &str) -> Result<Deployment, String> {
        let path = self.dir(id)?.join("deployment.json");
        let bytes = std::fs::read(&path).map_err(|_| format!("no deployment {id:?}"))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn save(&self, d: &Deployment) -> Result<(), String> {
        let dir = self.dir(&d.id)?;
        private_dir(&dir)?;
        write_private(&dir.join("deployment.json"), &serde_json::to_vec_pretty(d).map_err(|e| e.to_string())?)
    }

    /// A new deployment, with an API key of its own and no units yet.
    pub fn create(&self, name: &str, board: Board) -> Result<Deployment, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("a deployment needs a name".into());
        }
        board.check()?;
        let slug: String = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let mut suffix = [0u8; 2];
        getrandom::getrandom(&mut suffix).map_err(|e| e.to_string())?;
        let id = format!("{}-{:02x}{:02x}", if slug.is_empty() { "board" } else { &slug[..slug.len().min(24)] }, suffix[0], suffix[1]);
        let used: Vec<u16> = self.list().iter().map(|d| d.qemu_port).collect();
        let qemu_port = (18420..18500).find(|p| !used.contains(p)).unwrap_or(18420);
        let d = Deployment { id, name: name.into(), board, env: Env::default(), zone: String::new(), qemu_port, simulated: None };
        self.save(&d)?;
        let config = AppConfig { api_key: Some(new_api_key()), units: Vec::new() };
        self.save_config(&d.id, &config)?;
        Ok(d)
    }

    pub fn config(&self, id: &str) -> Result<AppConfig, String> {
        let path = self.dir(id)?.join("config.json");
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Breeze Core's own bytes for config.json, so it moves to any Breeze Core.
    pub fn save_config(&self, id: &str, config: &AppConfig) -> Result<(), String> {
        write_private(&self.dir(id)?.join("config.json"), &ucore::store::to_json(config))
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        let dir = self.dir(id)?;
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn api_keys_have_breeze_cores_shape() {
        let k = super::new_api_key();
        assert_eq!(k.len(), 32);
        assert!(k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(k, super::new_api_key());
    }

    #[test]
    fn ids_cannot_climb_out() {
        let s = super::Store { root: std::path::PathBuf::from("x") };
        for bad in ["", "..", "a/b", "A", "a\\b", "a.b"] {
            assert!(s.dir(bad).is_err(), "{bad:?}");
        }
        assert!(s.dir("home-1a2b").is_ok());
    }
}
