//! Simulated air conditioners, for trying μCore without real ones:
//! breeze-mock (<https://github.com/monikapurpl3/breeze-mock>) plays them on
//! the real V3 LAN protocol, on this computer, beside a board in QEMU.
//!
//! The deployer makes the units itself, as a household's real ones would be:
//! an id, a token and a key each, random, kept in config.json like any other
//! unit's. deployment.json remembers which units are the simulated ones, and
//! their blueprint (`mock.xml`) is written from config.json each time they
//! start, so a rename or a removal on the page reaches them as well.
//!
//! They listen on 127.0.0.1 only, one port each: no firewall asks, and nothing
//! on the network sees them. QEMU's user network shows this computer to the
//! board as 10.0.2.2, which is the address config.json gives them.
//!
//! breeze-mock is the pinned build on aspic, checked against its SHA-256 and
//! kept in the helper's tools folder. It is downloaded the first time
//! simulated units are made, and only then.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use ucore::store::{AppConfig, UnitConfig};

use crate::net;

pub const VERSION: &str = "1.0.1";
const BASE_URL: &str = "https://aspic.salataputarica.hr.eu.org/breeze-mock";
/// SHA-256 of each pinned build, by the label in its file name.
const SHA256: &[(&str, &str)] = &[
    ("linux-amd64", "9877d8a6010ef9157cd6f4724361cf315d8f6bebc22d514323f91f14b33dfc2f"),
    ("linux-arm64", "ed4ddc6a89ca4367e45c443940063a602e5e5142f8286679fda2666e41174a5c"),
    ("macos-arm64", "d9b5bb550aa5f4aa8b876875e307b57b5c383a06b296814aa26748a51bf9c175"),
    ("macos-x86_64", "324fd9bade391514dae15f370a239701363bba16b180bb356394b6b999ecf9c8"),
    ("windows-x86_64.exe", "585329313e1c40fb165e5f04815975b41310159b602b1ee09fab76ff72330e2f"),
];

/// As many as μCore's budgets were measured with.
pub const MAX: usize = 8;
/// This computer, as a board in QEMU sees it.
pub const QEMU_HOST: &str = "10.0.2.2";
/// Each deployment's simulated units get a block of [`MAX`] ports from here.
const FIRST_PORT: u16 = 6450;

/// Which of a deployment's units are simulated, and their ports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Simulated {
    /// The first port of the deployment's block.
    pub port: u16,
    /// The simulated units' ids, in the order they were made.
    pub ids: Vec<i64>,
}

fn label() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("windows-x86_64.exe"),
        ("linux", "x86_64") => Ok("linux-amd64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("macos", "x86_64") => Ok("macos-x86_64"),
        ("macos", "aarch64") => Ok("macos-arm64"),
        (os, arch) => Err(format!("there is no breeze-mock for {os} {arch}")),
    }
}

/// Where the pinned breeze-mock is kept.
pub fn program(tools: &Path) -> PathBuf {
    tools.join("bin").join(format!("breeze-mock-{VERSION}{}", std::env::consts::EXE_SUFFIX))
}

/// The pinned breeze-mock, downloaded if it is not there yet.
pub fn ensure(tools: &Path, log: &net::Log, say: &dyn Fn(String)) -> Result<PathBuf, String> {
    let program = program(tools);
    if program.exists() {
        return Ok(program);
    }
    let label = label()?;
    let sha = SHA256.iter().find(|(l, _)| *l == label).map(|(_, s)| *s);
    say(format!("Downloading breeze-mock {VERSION} from aspic, to play the air conditioners..."));
    let url = format!("{BASE_URL}/breeze-mock-{VERSION}-{label}");
    let bytes = net::download(log, &url, &format!("breeze-mock {VERSION}, to simulate air conditioners"), sha)?;
    let dir = program.parent().ok_or("no tools folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let part = program.with_extension("part");
    std::fs::write(&part, bytes).map_err(|e| format!("{}: {e}", part.display()))?;
    crate::firmware::executable(&part)?;
    std::fs::rename(&part, &program).map_err(|e| format!("{}: {e}", program.display()))?;
    say(format!("breeze-mock {VERSION} checked against its SHA-256 and kept in {}.", dir.display()));
    Ok(program)
}

/// A block of ports that no other deployment's simulated units have.
pub fn free_block(taken: &[u16]) -> u16 {
    (0..64u16).map(|i| FIRST_PORT + i * MAX as u16).find(|p| !taken.contains(p)).unwrap_or(FIRST_PORT)
}

fn random_hex(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    getrandom::getrandom(&mut raw).expect("the system's random source");
    let mut out = String::with_capacity(bytes * 2);
    for b in raw {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// A device id of a real unit's size: 48 bits, 13 to 15 digits.
fn random_id() -> i64 {
    let mut raw = [0u8; 8];
    getrandom::getrandom(&mut raw).expect("the system's random source");
    let n = (u64::from_le_bytes(raw) & ((1 << 48) - 1)).max(1_000_000_000_000);
    n as i64
}

/// Put `count` new simulated units in `config`, in place of the ones `old`
/// names; real units are left as they are. Returns the new set.
pub fn replace(config: &mut AppConfig, old: Option<&Simulated>, count: usize, port: u16) -> Result<Simulated, String> {
    if !(1..=MAX).contains(&count) {
        return Err(format!("1 to {MAX} simulated units"));
    }
    if let Some(o) = old {
        config.units.retain(|u| !o.ids.contains(&u.id));
    }
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let id = loop {
            let id = random_id();
            if !config.units.iter().any(|u| u.id == id) {
                break id;
            }
        };
        config.units.push(UnitConfig {
            name: format!("Simulated {}", i + 1),
            ip: QEMU_HOST.into(),
            port: port + i as u16,
            id,
            token: Some(random_hex(64)),
            key: Some(random_hex(32)),
        });
        ids.push(id);
    }
    Ok(Simulated { port, ids })
}

/// Take the simulated units out of `config`.
pub fn remove(config: &mut AppConfig, sim: &Simulated) {
    config.units.retain(|u| !sim.ids.contains(&u.id));
}

/// The simulated units still in `config`, in the order they were made.
pub fn units<'a>(config: &'a AppConfig, sim: &Simulated) -> Vec<(usize, &'a UnitConfig)> {
    sim.ids.iter().enumerate().filter_map(|(i, id)| config.units.iter().find(|u| u.id == *id).map(|u| (i, u))).collect()
}

fn attr(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&apos;".into(),
            c if c.is_control() => " ".into(),
            c => c.to_string(),
        })
        .collect()
}

/// Their blueprint: a loopback segment, and each unit with its own id, port,
/// token and key, so that config.json and breeze-mock agree by construction.
/// `name` names breeze-mock's run (a letter first, then letters, digits and
/// inner hyphens, at most 32).
pub fn blueprint(name: &str, config: &AppConfig, sim: &Simulated) -> String {
    // A little variety, so the units do not all look alike on a phone.
    const STATES: [(&str, &str); 4] = [("off", "cool"), ("on", "heat"), ("on", "auto"), ("off", "cool")];
    const LIMITED: &str = r#"<capabilities modes="auto cool heat fan-only" swing="vertical" fan="low high auto" eco="false" turbo="cool"/>"#;
    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    x.push_str("<!-- Written by the μCore deployer each time these units start, from the\n     deployment's config.json: changes made here are not kept. -->\n");
    let _ = writeln!(x, r#"<blueprint xmlns="urn:breeze-mock:blueprint:1" version="1" name="{name}">"#);
    x.push_str("  <network><segment name=\"lo\" cidr=\"127.0.0.0/24\" mode=\"loopback\" discovery=\"false\"/></network>\n");
    for (i, u) in units(config, sim) {
        let display = u.name.trim();
        let display = if display.is_empty() { format!("Simulated {}", i + 1) } else { attr(display).trim().to_string() };
        let (power, mode) = STATES[i % STATES.len()];
        let _ = writeln!(x, r#"  <unit key="u{}" id="{}" segment="lo" ip="127.0.0.1">"#, i + 1, u.id);
        let _ = writeln!(x, r#"    <epithets name="{display}"/>"#);
        let _ = writeln!(
            x,
            r#"    <v3 port="{}" token="{}" key="{}"/>"#,
            u.port,
            u.token.as_deref().unwrap_or_default().to_ascii_lowercase(),
            u.key.as_deref().unwrap_or_default().to_ascii_lowercase()
        );
        if i % 2 == 1 {
            let _ = writeln!(x, "    {LIMITED}");
        }
        let _ = writeln!(x, r#"    <state power="{power}" mode="{mode}" target="{}"/>"#, 21 + i % 5);
        x.push_str("  </unit>\n");
    }
    x.push_str("</blueprint>\n");
    x
}

/// breeze-mock's run name for a deployment.
pub fn run_name(deployment_id: &str) -> String {
    let mut name = format!("ucore-{deployment_id}");
    name.truncate(32);
    name.trim_end_matches('-').to_string()
}

/// Have breeze-mock itself read the blueprint, as a check that the two agree.
pub fn check(program: &Path, dir: &Path) -> Result<String, String> {
    let out = Command::new(program)
        .current_dir(dir)
        .args(["check", "mock.xml"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot start {}: {e}", program.display()))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).trim().to_string();
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("breeze-mock does not take the blueprint: {text}"))
    }
}

/// The simulated units running, per deployment.
#[derive(Default)]
pub struct Running(Mutex<BTreeMap<String, Child>>);

impl Running {
    pub fn is_running(&self, id: &str) -> bool {
        let Ok(mut g) = self.0.lock() else { return false };
        match g.get_mut(id).map(|c| c.try_wait()) {
            Some(Ok(None)) => true,
            Some(_) => {
                g.remove(id);
                false
            }
            None => false,
        }
    }

    /// Start them afresh from `blueprint` (written to `dir/mock.xml`), with
    /// what breeze-mock says in `dir/mock.log`.
    pub fn start(&self, id: &str, program: &Path, dir: &Path, blueprint: &str) -> Result<(), String> {
        self.stop(id);
        crate::store::write_private(&dir.join("mock.xml"), blueprint.as_bytes())?;
        let log_path = dir.join("mock.log");
        let log = std::fs::File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
        let log2 = log.try_clone().map_err(|e| e.to_string())?;
        let mut child = Command::new(program)
            .current_dir(dir)
            .args(["run", "mock.xml", "-q"])
            // Its own data folder inside the deployment's, not the user's.
            .env("BREEZE_MOCK_DATA", dir.join("mock-data"))
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log2)
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", program.display()))?;
        // It says at once when it cannot start (a port taken, say).
        std::thread::sleep(std::time::Duration::from_millis(700));
        if let Ok(Some(status)) = child.try_wait() {
            let why = std::fs::read_to_string(&log_path).unwrap_or_default();
            return Err(format!("the simulated units stopped at once ({status}): {}", why.trim()));
        }
        self.0.lock().map_err(|_| "the list of simulated units")?.insert(id.into(), child);
        Ok(())
    }

    pub fn stop(&self, id: &str) {
        if let Ok(mut g) = self.0.lock() {
            if let Some(mut c) = g.remove(id) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }

    /// Stop them all, as the helper exits.
    pub fn stop_all(&self) {
        if let Ok(mut g) = self.0.lock() {
            for (_, mut c) in std::mem::take(&mut *g) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real() -> UnitConfig {
        UnitConfig { name: "Kitchen".into(), ip: "192.168.1.50".into(), port: 6444, id: 150_000_000_000_001, token: None, key: None }
    }

    #[test]
    fn simulated_units_look_like_real_ones() {
        let mut c = AppConfig { api_key: None, units: vec![real()] };
        let s = replace(&mut c, None, 3, 6458).unwrap();
        assert_eq!(c.units.len(), 4);
        assert_eq!(s.ids.len(), 3);
        for (i, u) in units(&c, &s) {
            assert_eq!(u.ip, QEMU_HOST);
            assert_eq!(u.port, 6458 + i as u16);
            assert!((1_000_000_000_000..1 << 48).contains(&u.id), "{}", u.id);
            let t = u.token.as_deref().unwrap();
            let k = u.key.as_deref().unwrap();
            assert!(t.len() == 128 && k.len() == 64);
            assert!(t.chars().chain(k.chars()).all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
        let mut ids = s.ids.clone();
        ids.dedup();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn making_them_again_replaces_only_the_simulated_ones() {
        let mut c = AppConfig { api_key: None, units: vec![real()] };
        let first = replace(&mut c, None, 5, FIRST_PORT).unwrap();
        let second = replace(&mut c, Some(&first), 2, FIRST_PORT).unwrap();
        assert_eq!(c.units.len(), 3);
        assert!(c.units.iter().any(|u| u.name == "Kitchen"));
        assert!(first.ids.iter().all(|id| !c.units.iter().any(|u| u.id == *id)));
        remove(&mut c, &second);
        assert_eq!(c.units, vec![real()]);
        assert!(replace(&mut c, None, 0, FIRST_PORT).is_err());
        assert!(replace(&mut c, None, MAX + 1, FIRST_PORT).is_err());
    }

    #[test]
    fn blocks_of_ports_do_not_overlap() {
        assert_eq!(free_block(&[]), FIRST_PORT);
        assert_eq!(free_block(&[FIRST_PORT]), FIRST_PORT + MAX as u16);
        assert_eq!(free_block(&[FIRST_PORT + MAX as u16]), FIRST_PORT);
    }

    #[test]
    fn the_blueprint_follows_config_json() {
        let mut c = AppConfig { api_key: None, units: vec![real()] };
        let s = replace(&mut c, None, 3, FIRST_PORT).unwrap();
        // Renamed on the page, with characters XML must escape; one removed.
        c.units.iter_mut().find(|u| u.id == s.ids[0]).unwrap().name = "Tom & Ana's <room>".into();
        c.units.retain(|u| u.id != s.ids[1]);
        let x = blueprint("ucore-home-1a2b", &c, &s);
        assert!(x.contains(r#"<epithets name="Tom &amp; Ana&apos;s &lt;room&gt;"/>"#), "{x}");
        assert!(!x.contains(&s.ids[1].to_string()));
        assert!(x.contains(&format!(r#"<unit key="u3" id="{}" segment="lo" ip="127.0.0.1">"#, s.ids[2])));
        assert!(x.contains(&format!(r#"port="{}""#, FIRST_PORT + 2)));
        assert!(!x.contains("Kitchen"), "a real unit is not simulated");
        assert!(!x.replace("<!--", "").replace("-->", "").contains("--"), "XML comments may not hold --");
    }

    #[test]
    fn run_names_are_breeze_mock_names() {
        assert_eq!(run_name("home-1a2b"), "ucore-home-1a2b");
        let long = run_name("a-very-long-deployment-name-here-1a2b");
        assert!(long.len() <= 32 && !long.ends_with('-'), "{long}");
        assert!(run_name("2nd-home-0f0f").starts_with("ucore-"));
    }

    /// breeze-mock itself reads the blueprint: set BREEZE_MOCK_BIN to run it.
    #[test]
    fn breeze_mock_takes_the_blueprint() {
        let Some(bin) = std::env::var_os("BREEZE_MOCK_BIN") else { return };
        let mut c = AppConfig { api_key: None, units: Vec::new() };
        let s = replace(&mut c, None, MAX, FIRST_PORT).unwrap();
        c.units[0].name = "Tom & Ana's <room>".into();
        let dir = std::env::temp_dir().join(format!("ucore-deploy-mock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mock.xml"), blueprint(&run_name("test-0000"), &c, &s)).unwrap();
        let said = check(Path::new(&bin), &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let said = said.unwrap();
        assert!(said.contains(&format!("{MAX} unit(s)")), "{said}");
    }
}
