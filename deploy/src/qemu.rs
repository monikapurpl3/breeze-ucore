//! A QEMU board, running: Espressif's QEMU (which the user installs; it is
//! looked for on the PATH) on the deployment's flash image, its emulated
//! Ethernet on QEMU's user network, μCore's API forwarded to this computer
//! only, and its console written to `serial.log`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use crate::boards::Board;

#[derive(Default)]
pub struct Running(Mutex<BTreeMap<String, Child>>);

/// Where a program is on the PATH.
fn on_path(program: &str) -> Option<PathBuf> {
    let names = [format!("{program}{}", std::env::consts::EXE_SUFFIX), program.to_string()];
    std::env::split_paths(&std::env::var_os("PATH")?).flat_map(|dir| names.iter().map(move |n| dir.join(n))).find(|p| p.is_file())
}

pub fn find(board: &Board) -> Result<PathBuf, String> {
    let q = board.qemu.as_ref().ok_or("this board does not run in QEMU")?;
    on_path(&q.program).ok_or_else(|| {
        format!(
            "{} is not on the PATH. This board runs in Espressif's QEMU, which the deployer does not install: \
             get it from https://github.com/espressif/qemu/releases and put its bin folder on the PATH, then start the deployer again.",
            q.program
        )
    })
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

    /// Start the board; its console goes to `dir/serial.log`, which starts
    /// afresh.
    pub fn start(&self, id: &str, board: &Board, dir: &Path, port: u16) -> Result<(), String> {
        if self.is_running(id) {
            return Ok(());
        }
        let program = find(board)?;
        let q = board.qemu.as_ref().ok_or("no QEMU machine")?;
        if !dir.join("flash.bin").exists() {
            return Err("build it first: there is no flash image yet".into());
        }
        let _ = std::fs::write(dir.join("serial.log"), b"");
        let mut child = Command::new(&program)
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
