//! Everything the helper sends beyond this computer goes through here, and is
//! written down as it goes: the page shows the list, so "nothing leaves this
//! computer except..." is something to check, not to take on trust.
//!
//! There are three kinds of destination, and no others:
//! - downloads: μCore's source, and the build tools (a fixed list of hosts);
//! - Midea's cloud, with the user's own account, to fetch the units' tokens;
//! - this network: looking for air conditioners, and trying a token on one.

use std::io::Read;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct Connection {
    /// Unix seconds.
    pub at: u64,
    /// `download`, `midea` or `lan`.
    pub kind: &'static str,
    pub host: String,
    pub why: String,
}

#[derive(Default)]
pub struct Log(Mutex<Vec<Connection>>);

impl Log {
    pub fn note(&self, kind: &'static str, host: &str, why: &str) {
        let at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if let Ok(mut v) = self.0.lock() {
            v.push(Connection { at, kind, host: host.into(), why: why.into() });
        }
    }

    pub fn all(&self) -> Vec<Connection> {
        self.0.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

/// The only hosts a download may come from.
const DOWNLOAD_HOSTS: &[&str] = &["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com", "codeload.github.com", "static.rust-lang.org"];

fn host_of(url: &str) -> &str {
    url.split("://").nth(1).unwrap_or(url).split(['/', ':']).next().unwrap_or("")
}

/// Download a file whole, from one of [`DOWNLOAD_HOSTS`], and check its
/// SHA-256 when one is known.
pub fn download(log: &Log, url: &str, why: &str, sha256: Option<&str>) -> Result<Vec<u8>, String> {
    let host = host_of(url);
    if !DOWNLOAD_HOSTS.contains(&host) {
        return Err(format!("{host} is not a host this deployer downloads from"));
    }
    log.note("download", host, why);
    let agent = ureq::AgentBuilder::new()
        .redirects(5)
        .user_agent(concat!("ucore-deploy/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
    // A redirect may land on GitHub's file servers; say where it went.
    let landed = host_of(response.get_url()).to_string();
    if landed != host {
        if !DOWNLOAD_HOSTS.contains(&landed.as_str()) {
            return Err(format!("{url} redirected to {landed}, which this deployer does not download from"));
        }
        log.note("download", &landed, why);
    }
    let mut bytes = Vec::new();
    response.into_reader().take(2 << 30).read_to_end(&mut bytes).map_err(|e| format!("{url}: {e}"))?;
    if let Some(want) = sha256 {
        let got = sha256_hex(&bytes);
        if !got.eq_ignore_ascii_case(want.trim()) {
            return Err(format!("{url} is not the file expected (SHA-256 {got}, wanted {want})"));
        }
    }
    Ok(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = ucore::proto::crypto::sha256(&[bytes]);
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn hosts_are_read_from_urls() {
        assert_eq!(super::host_of("https://github.com/esp-rs/espup/releases"), "github.com");
        assert_eq!(super::host_of("https://static.rust-lang.org:443/x"), "static.rust-lang.org");
    }

    #[test]
    fn sha256_is_hex() {
        assert_eq!(super::sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
