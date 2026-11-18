//! From μCore's source to a board's flash: the toolchains, the build, the
//! image, and the bundle in it.
//!
//! The toolchains go into the helper's own folder (`tools/`), never anywhere
//! the user's other programs would see: rustup and cargo with their homes
//! there, Espressif's Xtensa toolchain installed by espup into that rustup,
//! espflash beside them. Nothing is added to the PATH or the environment.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ucore::bundle;
use ucore::store::AppConfig;

use crate::boards::{self, Board};
use crate::jobs::Say;
use crate::net;
use crate::store::Deployment;

/// What the downloads are pinned to.
const ESPUP_VERSION: &str = "v0.19.0";
const ESP_TOOLCHAIN: &str = "1.97.0.0";
const ESPFLASH_VERSION: &str = "v4.7.0";
/// The nightly an ESP32-C6 is built with.
const NIGHTLY: &str = "nightly-2026-11-07";

/// SHA-256 of the pinned downloads, per platform.
const ESPUP_SHA256: &[(&str, &str)] = &[
    ("x86_64-pc-windows-msvc", "c0ceeb4894064cfedb0e3c6e31173d260719d47578843823f81f84fe6fbacf96"),
    ("x86_64-unknown-linux-gnu", "f31411c64dffff4da0f0e23c432b9a5fda48942ff8695b8b21009ceb7007bcc0"),
    ("aarch64-unknown-linux-gnu", "a7e841449edbc78ce2443416e90027890b52896698767b6e45a5caafc880498b"),
    ("x86_64-apple-darwin", "f075dd9ca0e9c64ee5f342b27f9cf25369e2445fd67ac39f51aac7f39078916d"),
    ("aarch64-apple-darwin", "e18e722774b5fa2e8de73753ffe14370a621af7366ad5bdb73d7c42054fa2cdd"),
];
const ESPFLASH_SHA256: &[(&str, &str)] = &[
    ("x86_64-pc-windows-msvc", "2b10333e4f4f1a26024259b0cc1942ba17f27b3c530621c6111f4c10ac70213e"),
    ("x86_64-unknown-linux-gnu", "8ab3bafeb212ccb1d7a9c422936b9bc86f8b00fe2731053ed8ab875c8d2f0c4d"),
    ("aarch64-unknown-linux-gnu", "3158e1c78f3e80ad0389c1d925b69736e1955a60b3cadf932e6d0d74d731b448"),
    ("x86_64-apple-darwin", "3e3ca415f15f58000d351ca8dd22137c2d04384a8153f65b831553074601ee03"),
    ("aarch64-apple-darwin", "4313a4cf3e05036d4e1d8edeef938a11661eb5b9ba5759cabbf48ad4f6fbc180"),
];

/// This computer, as release files name it.
fn platform() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        (os, arch) => Err(format!("this deployer cannot install build tools on {os} {arch}")),
    }
}

/// The host the ordinary Rust toolchains are for. On Windows the GNU one,
/// which carries its own linker: no Visual Studio needed.
fn rust_host() -> Result<&'static str, String> {
    Ok(match platform()? {
        "x86_64-pc-windows-msvc" => "x86_64-pc-windows-gnu",
        other => other,
    })
}

/// Espressif publishes its Xtensa Rust for the MSVC host only on Windows, so
/// building for an ESP32-S3 there needs Microsoft's linker; found the way
/// Rust's own `cc` crate finds it.
fn msvc_linker_found() -> bool {
    !cfg!(windows) || cc::windows_registry::find_tool("x86_64-pc-windows-msvc", "link.exe").is_some()
}

fn pinned(table: &[(&str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

const EXE: &str = std::env::consts::EXE_SUFFIX;

pub struct Tools {
    root: PathBuf,
}

impl Tools {
    pub fn new(data: &Path) -> Self {
        Self { root: data.join("tools") }
    }

    fn rustup_home(&self) -> PathBuf {
        self.root.join("rustup")
    }

    fn cargo_home(&self) -> PathBuf {
        self.root.join("cargo")
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// A command that sees only the helper's toolchains.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(program);
        let mut path = vec![self.cargo_home().join("bin"), self.bin()];
        path.push(self.rustup_home().join("toolchains").join("esp").join("xtensa-esp-elf").join("bin"));
        path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        c.env("RUSTUP_HOME", self.rustup_home())
            .env("CARGO_HOME", self.cargo_home())
            .env("PATH", std::env::join_paths(path).unwrap_or_default())
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("CARGO_TARGET_DIR");
        c
    }

    fn rustup(&self) -> PathBuf {
        self.cargo_home().join("bin").join(format!("rustup{EXE}"))
    }

    fn cargo(&self) -> PathBuf {
        self.cargo_home().join("bin").join(format!("cargo{EXE}"))
    }

    pub fn espflash(&self) -> PathBuf {
        self.bin().join(format!("espflash{EXE}"))
    }

    /// Everything a chip's build needs, installed if it is not there yet.
    pub fn ensure(&self, chip: &str, log: &net::Log, say: &Say) -> Result<(), String> {
        std::fs::create_dir_all(self.bin()).map_err(|e| format!("{}: {e}", self.bin().display()))?;
        self.ensure_rustup(log, say)?;
        match boards::chip(chip).ok_or_else(|| format!("no such chip {chip}"))?.toolchain {
            "esp" => self.ensure_esp(log, say)?,
            _ => self.ensure_nightly(say)?,
        }
        self.ensure_espflash(log, say)
    }

    fn ensure_rustup(&self, log: &net::Log, say: &Say) -> Result<(), String> {
        if self.rustup().exists() {
            return Ok(());
        }
        say.line("Installing rustup and cargo into the deployer's own folder (not on your PATH)...");
        let plat = platform()?;
        let url = format!("https://static.rust-lang.org/rustup/dist/{plat}/rustup-init{EXE}");
        let sum = net::download(log, &format!("{url}.sha256"), "rustup's checksum", None)?;
        let sum = String::from_utf8_lossy(&sum).split_whitespace().next().unwrap_or("").to_string();
        let bytes = net::download(log, &url, "rustup, to install Rust", Some(&sum))?;
        let init = self.root.join(format!("rustup-init{EXE}"));
        std::fs::write(&init, bytes).map_err(|e| format!("{}: {e}", init.display()))?;
        executable(&init)?;
        log.note("download", "static.rust-lang.org", "rustup installing a minimal Rust (no toolchain yet)");
        run(
            self.command(&init).args(["-y", "--no-modify-path", "--profile", "minimal", "--default-toolchain", "none", "--default-host", rust_host()?]),
            say,
        )?;
        let _ = std::fs::remove_file(&init);
        Ok(())
    }

    fn ensure_nightly(&self, say: &Say) -> Result<(), String> {
        let dir = self.rustup_home().join("toolchains").join(format!("{NIGHTLY}-{}", rust_host()?));
        if dir.exists() {
            return Ok(());
        }
        say.line(format!("Installing Rust {NIGHTLY} for RISC-V chips..."));
        run(self.command(self.rustup()).args(["toolchain", "install", NIGHTLY, "--profile", "minimal", "--component", "rust-src"]), say)
    }

    /// Espressif's Rust and GCC for Xtensa chips, by espup, into the
    /// helper's rustup. On Windows espup also writes its paths into the
    /// user's environment; they are put back as they were straight after.
    fn ensure_esp(&self, log: &net::Log, say: &Say) -> Result<(), String> {
        if self.rustup_home().join("toolchains").join("esp").join("xtensa-esp-elf").exists() {
            return Ok(());
        }
        let plat = platform()?;
        if !msvc_linker_found() {
            return Err("Building for an ESP32-S3 on Windows needs Microsoft's C++ build tools: Espressif makes its \
                        Xtensa Rust for that host only. Install \"Build Tools for Visual Studio\" with \"Desktop \
                        development with C++\" (free, from Microsoft), then build again. An ESP32-C6 board needs \
                        none of it."
                .into());
        }
        let espup = self.bin().join(format!("espup{EXE}"));
        if !espup.exists() {
            let url = format!("https://github.com/esp-rs/espup/releases/download/{ESPUP_VERSION}/espup-{plat}{EXE}");
            let bytes = net::download(log, &url, "espup, Espressif's installer for its Rust toolchain", pinned(ESPUP_SHA256, plat))?;
            std::fs::write(&espup, bytes).map_err(|e| format!("{}: {e}", espup.display()))?;
            executable(&espup)?;
        }
        say.line(format!("Installing Espressif's Rust {ESP_TOOLCHAIN} and GCC for the ESP32-S3 (about a gigabyte; a while)..."));
        log.note("download", "github.com", "espup downloading Espressif's Rust, GCC and LLVM for Xtensa");
        let saved = user_environment();
        let export = self.root.join("export-esp.sh");
        let outcome = run(
            self.command(&espup).args(["install", "-t", "esp32s3", "-v", ESP_TOOLCHAIN, "-d", plat, "-f"]).arg(&export),
            say,
        );
        restore_user_environment(saved, say);
        outcome
    }

    fn ensure_espflash(&self, log: &net::Log, say: &Say) -> Result<(), String> {
        if self.espflash().exists() {
            return Ok(());
        }
        let plat = platform()?;
        let url = format!("https://github.com/esp-rs/espflash/releases/download/{ESPFLASH_VERSION}/espflash-{plat}.zip");
        let bytes = net::download(log, &url, "espflash, to make flash images", pinned(ESPFLASH_SHA256, plat))?;
        say.line("Unpacking espflash...");
        let program = unzip_one(bytes, &format!("espflash{EXE}")).map_err(|e| format!("espflash: {e}"))?;
        std::fs::write(self.espflash(), program).map_err(|e| e.to_string())?;
        executable(&self.espflash())
    }

    /// Build μCore for a board; the ELF's path.
    pub fn build(&self, src: &Path, build_dir: &Path, board: &Board, seed: Option<&Path>, log: &net::Log, say: &Say) -> Result<PathBuf, String> {
        let chip = boards::chip(&board.chip).ok_or("no such chip")?;
        let toolchain = if chip.toolchain == "esp" { "esp".to_string() } else { NIGHTLY.to_string() };
        let port = src.join("ports").join("esp32");
        let features = format!("{},{}", board.chip, board.network);
        say.line(format!("Building μCore for {} ({features})...", board.name));
        // Cargo fetches the Rust libraries μCore is made of (esp-hal, smoltcp,
        // serde and the rest) from crates.io, the first time.
        log.note("download", "index.crates.io, static.crates.io", "cargo fetching the Rust libraries μCore is built from (crates.io)");
        let mut c = self.command(self.cargo());
        c.current_dir(&port)
            .env("RUSTUP_TOOLCHAIN", &toolchain)
            .env("CARGO_TARGET_DIR", build_dir)
            .args(["build", "--release", "--target", chip.target, "--no-default-features", "--features", &features]);
        match seed {
            Some(s) => c.env("UCORE_SEED_DIR", s),
            None => c.env_remove("UCORE_SEED_DIR"),
        };
        run(&mut c, say)?;
        Ok(build_dir.join(chip.target).join("release").join("ucore"))
    }

    /// The flash image: bootloader, partition table and app from espflash.
    pub fn image(&self, src: &Path, board: &Board, elf: &Path, out: &Path, say: &Say) -> Result<(), String> {
        let table = src.join("ports").join("esp32").join("partitions.csv");
        say.line("Making the flash image...");
        run(
            self.command(self.espflash())
                .args(["save-image", "--chip", &board.chip, "--merge", "--flash-size", &board.flash_size, "--partition-table"])
                .arg(&table)
                .arg(elf)
                .arg(out),
            say,
        )
    }
}

/// One file out of a zip archive, by its name, read into memory: nothing is
/// written where the archive says.
fn unzip_one(bytes: Vec<u8>, name: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut file = zip.by_name(name).map_err(|e| format!("{name}: {e}"))?;
    let mut out = Vec::new();
    file.by_ref().take(256 << 20).read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let _ = path;
    Ok(())
}

/// Run a command, its output going to the job's lines as it comes.
fn run(command: &mut Command, say: &Say) -> Result<(), String> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("cannot start {:?}: {e}", command.get_program()))?;
    let out = child.stdout.take().expect("piped");
    let err = child.stderr.take().expect("piped");
    let say2 = say.clone();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            say2.line(format!("  {line}"));
        }
    });
    for line in BufReader::new(err).lines().map_while(Result::ok) {
        say.line(format!("  {line}"));
    }
    let _ = reader.join();
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed ({status})", command.get_program()))
    }
}

/// Windows: the user's PATH and LIBCLANG_PATH as they are now, to put back
/// after espup. Elsewhere espup leaves the environment alone.
fn user_environment() -> Option<(String, String)> {
    if !cfg!(windows) {
        return None;
    }
    let get = |name: &str| {
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &format!("[Environment]::GetEnvironmentVariable('{name}','User')")])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim_end_matches(['\r', '\n']).to_string())
    };
    Some((get("PATH")?, get("LIBCLANG_PATH").unwrap_or_default()))
}

fn restore_user_environment(saved: Option<(String, String)>, say: &Say) {
    let Some((path, libclang)) = saved else { return };
    let quote = |s: &str| s.replace('\'', "''");
    let libclang = if libclang.is_empty() { "$null".to_string() } else { format!("'{}'", quote(&libclang)) };
    let script = format!(
        "[Environment]::SetEnvironmentVariable('PATH','{}','User'); [Environment]::SetEnvironmentVariable('LIBCLANG_PATH',{libclang},'User')",
        quote(&path)
    );
    match Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).status() {
        Ok(s) if s.success() => say.line("Your PATH and LIBCLANG_PATH are as they were before espup."),
        _ => say.line("!! espup changed your user PATH and LIBCLANG_PATH, and they could not be put back: remove its entries by hand."),
    }
}

/// A partition's offset and size, from μCore's partitions.csv.
pub fn partition(src: &Path, name: &str) -> Result<(u64, u64), String> {
    let path = src.join("ports").join("esp32").join("partitions.csv");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let number = |s: &str| {
        let s = s.trim();
        s.strip_prefix("0x").map_or_else(|| s.parse().ok(), |h| u64::from_str_radix(h, 16).ok())
    };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        if cols.len() >= 5 && cols[0] == name {
            if let (Some(o), Some(s)) = (number(cols[3]), number(cols[4])) {
                return Ok((o, s));
            }
        }
    }
    Err(format!("{} has no partition {name}", path.display()))
}

/// The bundle's bytes for a deployment: config.json and env.json.
pub fn bundle_image(src: &Path, config: &AppConfig, d: &Deployment) -> Result<Vec<u8>, String> {
    let (_, size) = partition(src, "ucorecfg")?;
    let config = ucore::store::to_json(config);
    let env = serde_json::to_vec_pretty(&d.env).map_err(|e| e.to_string())?;
    bundle::image(size as usize, &[(bundle::CONFIG, &config), (bundle::ENV, &env)])
}

/// Write bytes into a flash image file at an offset.
pub fn patch(image: &Path, offset: u64, bytes: &[u8]) -> Result<(), String> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(image).map_err(|e| format!("{}: {e}", image.display()))?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())
}

/// Read a region of a flash image file.
pub fn region(image: &Path, offset: u64, size: u64) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(image).map_err(|e| format!("{}: {e}", image.display()))?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut out = vec![0u8; size as usize];
    f.read_exact(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_file_comes_out_of_a_zip() {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("README", opts).unwrap();
        zip.write_all(b"not this").unwrap();
        zip.start_file("espflash", opts).unwrap();
        zip.write_all(b"the program").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        assert_eq!(super::unzip_one(bytes.clone(), "espflash").unwrap(), b"the program");
        assert!(super::unzip_one(bytes, "espflash.exe").is_err());
        assert!(super::unzip_one(b"not a zip".to_vec(), "espflash").is_err());
    }

    #[test]
    fn partitions_are_read_from_the_table() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        assert_eq!(super::partition(src, "ucore").unwrap(), (0x310000, 0x40000));
        assert_eq!(super::partition(src, "ucorecfg").unwrap(), (0x350000, 0x8000));
        assert!(super::partition(src, "nope").is_err());
    }
}
