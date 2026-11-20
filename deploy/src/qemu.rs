//! A QEMU board, running: Espressif's QEMU (which the user installs) on the
//! deployment's flash image, its emulated Ethernet on QEMU's user network,
//! μCore's API forwarded to this computer only, and its console written to
//! `serial.log`.
//!
//! Finding QEMU: first where the helper was told it is (remembered in its
//! data folder, `qemu.json`), then the PATH. When neither has it, a build
//! asks in the helper's own window, and only there: which program the
//! helper runs is for the person at its console to say, never the page
//! (aspic's copy included). The answer may be the folder the release was
//! unpacked to; it is searched, and a program is kept only once it has shown
//! that it runs its board's machine. One answer can cover both QEMUs.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use crate::boards::{self, Board};
use crate::jobs::Say;

const EXE: &str = std::env::consts::EXE_SUFFIX;
const RELEASES: &str = "https://github.com/espressif/qemu/releases";

#[derive(Default)]
pub struct Running(Mutex<BTreeMap<String, Child>>);

/// Where a program is on the PATH.
fn on_path(program: &str) -> Option<PathBuf> {
    let names = [format!("{program}{EXE}"), program.to_string()];
    std::env::split_paths(&std::env::var_os("PATH")?).flat_map(|dir| names.iter().map(move |n| dir.join(n))).find(|p| p.is_file())
}

/// Where `program` is under `dir`: in it, or in the `bin` or `qemu/bin` of
/// it or of one of its folders, which is how Espressif's releases unpack
/// (`qemu/bin/qemu-system-xtensa`) and how people keep several
/// (`espressif/qemu-9.2.2/xtensa/qemu/bin`).
pub fn search(dir: &Path, program: &str) -> Option<PathBuf> {
    let mut roots = vec![dir.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut subs: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_dir()).collect();
        subs.sort();
        roots.extend(subs);
    }
    let file = format!("{program}{EXE}");
    roots
        .iter()
        .flat_map(|r| [r.clone(), r.join("bin"), r.join("qemu").join("bin")])
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
}

/// What someone typed for a folder: quotes taken off (a path copied from
/// Explorer has them), `~` for the home folder, and a path to the program
/// itself taken as its folder.
pub fn clean(answer: &str) -> PathBuf {
    let mut s = answer.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            s = &s[1..s.len() - 1];
        }
    }
    let mut path = match (s.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(s),
    };
    if path.is_file() {
        path.pop();
    }
    path
}

/// Whether `-machine help`'s answer lists `machine`.
pub fn lists_machine(help: &str, machine: &str) -> bool {
    help.lines().any(|l| l.split_whitespace().next() == Some(machine))
}

/// That `program` is a QEMU that runs `machine`: its version line if so.
fn runs(program: &Path, machine: &str) -> Result<String, String> {
    let out = |args: &[&str]| {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .map_err(|e| format!("cannot start {}: {e}", program.display()))
    };
    if !lists_machine(&out(&["-machine", "help"])?, machine) {
        return Err(format!("{} does not run the {machine} machine: it is not Espressif's QEMU, or too old", program.display()));
    }
    Ok(out(&["--version"])?.lines().next().unwrap_or("QEMU").trim().to_string())
}

/// The helper's own window: its terminal (`/dev/tty`, since under
/// `curl ... | sh` standard input is the script), or the console on Windows.
struct Console {
    input: Box<dyn BufRead>,
    output: Box<dyn Write>,
}

impl Console {
    fn open() -> Option<Self> {
        #[cfg(unix)]
        {
            let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
            let out = tty.try_clone().ok()?;
            Some(Self { input: Box::new(std::io::BufReader::new(tty)), output: Box::new(out) })
        }
        #[cfg(not(unix))]
        {
            Some(Self { input: Box::new(std::io::stdin().lock()), output: Box::new(std::io::stdout()) })
        }
    }

    fn say(&mut self, text: &str) {
        let _ = self.output.write_all(text.as_bytes());
        let _ = self.output.flush();
    }

    /// A line, or `None` when nobody can answer (no input at all).
    fn ask(&mut self, prompt: &str) -> Option<String> {
        self.say(prompt);
        let mut line = String::new();
        match self.input.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim().to_string()),
        }
    }
}

/// Where the QEMUs are: remembered, on the PATH, or asked for.
pub struct Finder {
    file: PathBuf,
    known: Mutex<BTreeMap<String, PathBuf>>,
    /// One question in the window at a time.
    asking: Mutex<()>,
}

impl Finder {
    pub fn new(data: &Path) -> Self {
        let file = data.join("qemu.json");
        let known = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { file, known: Mutex::new(known), asking: Mutex::new(()) }
    }

    /// "Not found", and what to do about it: `then`.
    fn not_found(board: &Board, program: &str, then: &str) -> String {
        format!("{program} (Espressif's QEMU, for \"{}\") is not on the PATH, and the helper has not been told where it is. {then}", board.name)
    }

    /// The program a board runs in, without asking.
    pub fn find(&self, board: &Board) -> Result<PathBuf, String> {
        let q = board.qemu.as_ref().ok_or("this board does not run in QEMU")?;
        let remembered = self.known.lock().ok().and_then(|k| k.get(&q.program).cloned());
        if let Some(p) = remembered.filter(|p| p.is_file()) {
            return Ok(p);
        }
        on_path(&q.program).ok_or_else(|| {
            Self::not_found(board, &q.program, &format!("Build it: the build asks for its folder in the helper's window. QEMU itself: {RELEASES}."))
        })
    }

    /// The same, asking in the helper's window when it is not found, and
    /// remembering what the answer finds.
    pub fn find_or_ask(&self, board: &Board, say: &Say) -> Result<PathBuf, String> {
        if let Ok(p) = self.find(board) {
            return Ok(p);
        }
        let q = board.qemu.as_ref().ok_or("this board does not run in QEMU")?;
        let _turn = self.asking.lock().map_err(|_| "the question about QEMU")?;
        // Another build may have asked while this one waited.
        if let Ok(p) = self.find(board) {
            return Ok(p);
        }
        let no_console = || {
            Self::not_found(board, &q.program, &format!("Nor can it ask: it has no window to ask in. Put QEMU's bin folder on the PATH and start the helper again. QEMU itself: {RELEASES}."))
        };
        let mut console = Console::open().ok_or_else(no_console)?;
        say.line(format!("{} is not on the PATH: the helper is asking for Espressif's QEMU in its own window (where you started it). Answer there.", q.program));
        console.say(&format!(
            "\n  A build for \"{}\" needs Espressif's QEMU ({}), which is not on the PATH.\n  \
             Where is it? The folder you unpacked it to is enough. Enter skips.\n",
            board.name, q.program
        ));
        for _ in 0..3 {
            let Some(answer) = console.ask("  QEMU folder: ") else {
                return Err(no_console());
            };
            if answer.is_empty() {
                console.say("  Skipped.\n");
                return Err(Self::not_found(board, &q.program, "Skipped in the helper's window: build again to be asked again."));
            }
            let dir = clean(&answer);
            let found = self.take_from(&dir);
            for (program, version) in &found {
                console.say(&format!("  Found {} ({version}).\n", program.display()));
                say.line(format!("Found {} ({version}); remembered.", program.display()));
            }
            if let Ok(p) = self.find(board) {
                console.say(&format!("  Remembered in {}. Back to the page.\n", self.file.display()));
                return Ok(p);
            }
            console.say(&format!("  No {} that runs {} under {}. Try again, or Enter to skip.\n", q.program, q.machine, dir.display()));
        }
        Err(Self::not_found(board, &q.program, "Not found where the helper was told: build again to be asked again."))
    }

    /// Every QEMU the boards use that is under `dir` and runs its machine:
    /// remembered, and returned with its version.
    fn take_from(&self, dir: &Path) -> Vec<(PathBuf, String)> {
        let mut wanted: BTreeMap<String, String> = BTreeMap::new();
        for b in boards::built_in() {
            if let Some(q) = b.qemu {
                wanted.entry(q.program).or_insert(q.machine);
            }
        }
        let mut found = Vec::new();
        for (program, machine) in wanted {
            if let Some(path) = search(dir, &program) {
                if let Ok(version) = runs(&path, &machine) {
                    if let Ok(mut k) = self.known.lock() {
                        k.insert(program, path.clone());
                    }
                    found.push((path, version));
                }
            }
        }
        if !found.is_empty() {
            if let Ok(k) = self.known.lock() {
                if let Ok(bytes) = serde_json::to_vec_pretty(&*k) {
                    let _ = crate::store::write_private(&self.file, &bytes);
                }
            }
        }
        found
    }
}

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

    /// Start the board in `program`; its console goes to `dir/serial.log`,
    /// which starts afresh.
    pub fn start(&self, id: &str, program: &Path, board: &Board, dir: &Path, port: u16) -> Result<(), String> {
        if self.is_running(id) {
            return Ok(());
        }
        let q = board.qemu.as_ref().ok_or("no QEMU machine")?;
        if !dir.join("flash.bin").exists() {
            return Err("build it first: there is no flash image yet".into());
        }
        let _ = std::fs::write(dir.join("serial.log"), b"");
        let mut child = Command::new(program)
            .current_dir(dir)
            .args(["-display", "none", "-machine", &q.machine, "-drive", "file=flash.bin,if=mtd,format=raw", "-nic"])
            // Forwarded to this computer only: nothing on the network can
            // reach it, and no firewall asks.
            .arg(format!("user,model=open_eth,hostfwd=tcp:127.0.0.1:{port}-:8420"))
            .args(["-serial", "file:serial.log", "-monitor", "none"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", program.display()))?;
        // QEMU says at once when it cannot start (a port taken, a bad image).
        std::thread::sleep(std::time::Duration::from_millis(700));
        if let Ok(Some(status)) = child.try_wait() {
            let mut why = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut why);
            }
            return Err(format!("QEMU stopped at once ({status}): {}", why.trim()));
        }
        self.0.lock().map_err(|_| "the board list")?.insert(id.into(), child);
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

    /// Stop every board, as the helper exits.
    pub fn stop_all(&self) {
        if let Ok(mut g) = self.0.lock() {
            for (_, mut c) in std::mem::take(&mut *g) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

/// The console from byte `since` on, and the length so far.
pub fn serial(dir: &Path, since: u64) -> (String, u64) {
    let bytes = std::fs::read(dir.join("serial.log")).unwrap_or_default();
    let len = bytes.len() as u64;
    let from = since.min(len) as usize;
    // Without the terminal's colour codes.
    let text = String::from_utf8_lossy(&bytes[from..]).to_string();
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        clean.push(c);
    }
    (clean, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ucore-deploy-qemu-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn a_folder_is_searched_the_way_releases_unpack() {
        let root = scratch("search");
        let x = root.join("xtensa").join("qemu").join("bin").join(format!("qemu-system-xtensa{EXE}"));
        let r = root.join("riscv32").join("qemu").join("bin").join(format!("qemu-system-riscv32{EXE}"));
        touch(&x);
        touch(&r);
        // The folder above both, either release's own folder, and its bin.
        assert_eq!(search(&root, "qemu-system-xtensa"), Some(x.clone()));
        assert_eq!(search(&root, "qemu-system-riscv32"), Some(r.clone()));
        assert_eq!(search(&root.join("xtensa"), "qemu-system-xtensa"), Some(x.clone()));
        assert_eq!(search(x.parent().unwrap(), "qemu-system-xtensa"), Some(x.clone()));
        assert_eq!(search(&root.join("xtensa"), "qemu-system-riscv32"), None);
        assert_eq!(search(&root.join("nowhere"), "qemu-system-xtensa"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn answers_are_cleaned() {
        assert_eq!(clean("  \"C:\\Program Files\\qemu\"  "), PathBuf::from("C:\\Program Files\\qemu"));
        assert_eq!(clean("'/opt/qemu'"), PathBuf::from("/opt/qemu"));
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(clean("~/qemu"), PathBuf::from(home).join("qemu"));
        }
        // The program itself: its folder.
        let root = scratch("clean");
        let p = root.join(format!("qemu-system-xtensa{EXE}"));
        touch(&p);
        assert_eq!(clean(&p.display().to_string()), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn machines_are_read_from_qemus_list() {
        let help = "Supported machines are:\nesp32                Espressif ESP32 machine\nesp32s3              Espressif ESP32S3 machine\nnone                 empty machine\n";
        assert!(lists_machine(help, "esp32s3"));
        assert!(!lists_machine(help, "esp32c6"));
        assert!(!lists_machine("esp32s3x  something\n", "esp32s3"));
    }

    /// The real thing: set UCORE_TEST_QEMU_DIR to a folder holding
    /// Espressif's QEMUs.
    #[test]
    fn espressifs_qemu_is_recognised() {
        let Some(dir) = std::env::var_os("UCORE_TEST_QEMU_DIR") else { return };
        let data = scratch("finder");
        let f = Finder::new(&data);
        let found = f.take_from(Path::new(&dir));
        assert!(!found.is_empty(), "nothing found under {dir:?}");
        for b in boards::built_in() {
            if found.iter().any(|(p, _)| p.file_stem().and_then(|s| s.to_str()) == b.qemu.as_ref().map(|q| q.program.as_str())) {
                assert!(f.find(&b).is_ok(), "{}", b.name);
            }
        }
        // Remembered for the next run.
        assert!(Finder::new(&data).known.lock().unwrap().len() == found.len());
        let _ = std::fs::remove_dir_all(&data);
    }
}
