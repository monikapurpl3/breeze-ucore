//! The Breeze μCore deployer's helper: builds μCore for one household's
//! board, units and network, from a page in the browser.
//!
//! The helper does the work; the page asks for it. The page is served here
//! (`http://127.0.0.1:47820`) or, with `--aspic`, loaded from aspic and
//! pointed at this helper. Either way it can talk to nothing but the helper,
//! and the helper only to the hosts `net.rs` names. DESIGN.md's Deployment
//! section has the reasoning.

mod boards;
mod firmware;
mod jobs;
mod mock;
mod net;
mod qemu;
mod store;
mod units;
mod web;
mod zones;

use std::path::{Path, PathBuf};
use std::sync::Arc;

const USAGE: &str = "\
ucore-deploy -- builds Breeze μCore for your board, from a page in your browser

Usage: ucore-deploy [--port PORT] [--aspic] [--no-open] [--source DIR] [--data DIR]

  --port PORT    where the helper listens on this computer (default 47820)
  --aspic        open the deployer's page on aspic instead of this helper's own
  --no-open      do not open a browser; print the link
  --source DIR   μCore's source to build from (default: the checkout this
                 helper was built in, if it is there; else downloaded);
                 `--source github` downloads it even then
  --data DIR     where deployments and build tools are kept
  -h, --help     this
";

/// The μCore commit this helper was built from: what it downloads to build,
/// so the helper and the firmware always match.
const SOURCE_REV: &str = env!("UCORE_DEPLOY_REV");
const ASPIC_PAGE: &str = "https://aspic.salataputarica.hr.eu.org/ucore-deploy/";

pub struct App {
    pub data: PathBuf,
    pub token: String,
    pub port: u16,
    pub store: store::Store,
    pub tools: firmware::Tools,
    pub log: net::Log,
    pub jobs: jobs::Jobs,
    pub qemu: qemu::Running,
    pub mock: mock::Running,
    pub local_source: Option<PathBuf>,
}

impl App {
    /// μCore's source: the local checkout, or this helper's commit,
    /// downloaded once.
    pub fn source(&self, say: &jobs::Say) -> Result<PathBuf, String> {
        if let Some(p) = &self.local_source {
            return Ok(p.clone());
        }
        let dir = self.data.join("src").join(SOURCE_REV);
        if dir.join("ports").join("esp32").join("Cargo.toml").exists() {
            return Ok(dir);
        }
        say.line(format!("Downloading μCore's source ({SOURCE_REV})..."));
        let url = format!("https://codeload.github.com/monikapurpl3/breeze-ucore/tar.gz/{SOURCE_REV}");
        let bytes = net::download(&self.log, &url, "μCore's source code", None)?;
        let tmp = self.data.join("src").join(format!("{SOURCE_REV}.partial"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(std::io::Cursor::new(bytes)));
        for entry in archive.entries().map_err(|e| e.to_string())? {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path().map_err(|e| e.to_string())?.into_owned();
            // Without the archive's top folder; nothing may climb out.
            let inner: PathBuf = path.components().skip(1).collect();
            if inner.as_os_str().is_empty() || inner.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
                continue;
            }
            let to = tmp.join(&inner);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            entry.unpack(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        }
        std::fs::rename(&tmp, &dir).map_err(|e| e.to_string())?;
        Ok(dir)
    }
}

fn data_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("ucore-deploy")
}

/// The checkout this helper was built in, if it is still there.
fn local_checkout() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    root.join("ports").join("esp32").join("Cargo.toml").exists().then(|| root.to_path_buf())
}

fn open_browser(url: &str) -> bool {
    let status = if cfg!(windows) {
        std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).status()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    status.is_ok_and(|s| s.success())
}

fn main() {
    let mut port = 47820u16;
    let (mut aspic, mut no_open) = (false, false);
    let (mut source, mut data) = (None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|p| p.parse().ok()).unwrap_or_else(|| usage_error("--port needs a number")),
            "--aspic" => aspic = true,
            "--no-open" => no_open = true,
            "--source" => source = Some(PathBuf::from(args.next().unwrap_or_else(|| usage_error("--source needs a folder")))),
            "--data" => data = Some(PathBuf::from(args.next().unwrap_or_else(|| usage_error("--data needs a folder")))),
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            "-V" | "--version" => {
                println!("ucore-deploy {} (μCore {SOURCE_REV})", env!("CARGO_PKG_VERSION"));
                return;
            }
            other => usage_error(&format!("unexpected argument {other:?}")),
        }
    }
    let data = data.unwrap_or_else(data_dir);
    let local_source = match source {
        Some(s) if s.as_os_str() == "github" => None,
        Some(s) => Some(s),
        None => local_checkout(),
    };
    let store = store::Store::open(&data).unwrap_or_else(|e| fail(&e));
    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw).unwrap_or_else(|e| fail(&e.to_string()));
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();

    // This computer only: nothing on the network can reach the helper.
    let server = tiny_http::Server::http(("127.0.0.1", port)).unwrap_or_else(|e| fail(&format!("cannot listen on 127.0.0.1:{port}: {e} (another deployer running? try --port)")));
    let app = Arc::new(App { tools: firmware::Tools::new(&data), data, token, port, store, log: net::Log::default(), jobs: jobs::Jobs::default(), qemu: qemu::Running::default(), mock: mock::Running::default(), local_source });

    let stopper = Arc::clone(&app);
    let _ = ctrlc::set_handler(move || {
        stopper.qemu.stop_all();
        stopper.mock.stop_all();
        std::process::exit(0);
    });

    let url = if aspic { format!("{ASPIC_PAGE}#port={port}&t={}", app.token) } else { format!("http://127.0.0.1:{port}/#t={}", app.token) };
    println!("Breeze μCore deployer {}", env!("CARGO_PKG_VERSION"));
    println!("  Everything stays on this computer: deployments and tools in {}", app.data.display());
    match &app.local_source {
        Some(p) => println!("  Building μCore from {}", p.display()),
        None => println!("  Building μCore {SOURCE_REV}, downloaded from GitHub when first needed"),
    }
    println!("  The page: {url}");
    println!("  (That link holds this session's key: keep it to yourself. Ctrl-C stops the deployer.)");
    if !no_open && !open_browser(&url) {
        println!("  Could not open a browser; open the link above yourself.");
    }

    for request in server.incoming_requests() {
        let app = Arc::clone(&app);
        std::thread::spawn(move || web::serve(app, request));
    }
}

fn usage_error(message: &str) -> ! {
    eprintln!("ucore-deploy: {message}\n\n{USAGE}");
    std::process::exit(2);
}

fn fail(message: &str) -> ! {
    eprintln!("ucore-deploy: {message}");
    std::process::exit(1);
}
