//! The boards μCore can be built for: the repository's `boards/*.toml`, and
//! the user's own, which the page edits and the deployment keeps a copy of.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    /// The file's name without `.toml`, or `custom`.
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub chip: String,
    pub network: String,
    pub flash_size: String,
    #[serde(default)]
    pub qemu: Option<Qemu>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Qemu {
    pub program: String,
    pub machine: String,
}

/// The repository's boards, as this helper was built with them.
const BUILT_IN: &[(&str, &str)] = &[
    ("qemu-esp32s3", include_str!("../../boards/qemu-esp32s3.toml")),
    ("qemu-esp32c6", include_str!("../../boards/qemu-esp32c6.toml")),
];

pub fn built_in() -> Vec<Board> {
    BUILT_IN
        .iter()
        .map(|(id, text)| {
            let mut b: Board = toml::from_str(text).unwrap_or_else(|e| panic!("boards/{id}.toml: {e}"));
            b.id = (*id).into();
            b
        })
        .collect()
}

/// What a chip needs to be built: the Rust target, the toolchain, and
/// whether its emulator can keep a store.
pub struct Chip {
    pub target: &'static str,
    /// The rustup toolchain: `esp` (Espressif's, for Xtensa) or `nightly`.
    pub toolchain: &'static str,
}

pub fn chip(name: &str) -> Option<Chip> {
    match name {
        "esp32s3" => Some(Chip { target: "xtensa-esp32s3-none-elf", toolchain: "esp" }),
        "esp32c6" => Some(Chip { target: "riscv32imac-unknown-none-elf", toolchain: "nightly" }),
        _ => None,
    }
}

impl Board {
    /// Whatever is wrong with it, said for a person.
    pub fn check(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("the board needs a name".into());
        }
        if chip(&self.chip).is_none() {
            return Err(format!("chip {:?}: esp32s3 or esp32c6", self.chip));
        }
        if !matches!(self.flash_size.as_str(), "4mb" | "8mb" | "16mb") {
            return Err(format!("flash size {:?}: 4mb, 8mb or 16mb", self.flash_size));
        }
        match (self.network.as_str(), &self.qemu) {
            ("qemu", Some(q)) if !q.program.trim().is_empty() && !q.machine.trim().is_empty() => Ok(()),
            ("qemu", _) => Err("a QEMU board needs its QEMU program and machine".into()),
            ("wifi", _) => Ok(()),
            (other, _) => Err(format!("network {other:?}: qemu or wifi")),
        }
    }

    pub fn is_qemu(&self) -> bool {
        self.network == "qemu"
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_built_in_boards_are_whole() {
        let boards = super::built_in();
        assert_eq!(boards.len(), 2);
        for b in boards {
            b.check().unwrap_or_else(|e| panic!("{}: {e}", b.id));
            assert!(b.is_qemu());
        }
    }
}
