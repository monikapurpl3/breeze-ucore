//! The two flash partitions μCore keeps its files in (see `partitions.csv`),
//! each a [`ucore::flashlog::FlashLog`]:
//!
//! - `ucore`, the store: what μCore itself writes (paired devices, programs,
//!   timers). A power cut leaves every file as it was or as it was being
//!   written.
//! - `ucorecfg`, the bundle: `config.json` and `env.json`, written whole by
//!   the deployer over USB and only read here (see [`ucore::bundle`]). A new
//!   configuration needs no new firmware and leaves the store alone.

// An emulated C6 keeps both in RAM (see `open`), so the partition code goes
// unused there.
#![cfg_attr(all(feature = "qemu", feature = "esp32c6"), allow(dead_code))]

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use esp_storage::FlashStorage;
use ucore::bundle::RamFlash;
use ucore::flashlog::{Flash, FlashLog};

// Only an emulated C6 reads it (see open).
#[cfg_attr(not(all(feature = "qemu", feature = "esp32c6")), allow(dead_code))]
mod seed {
    include!(concat!(env!("OUT_DIR"), "/seed.rs"));
}

const STORE: &str = "ucore";
const BUNDLE: &str = "ucorecfg";
const TABLE_AT: u32 = 0x8000;

/// A partition, through the one esp-storage driver both logs share. Writes go
/// from a word-aligned buffer of its own: esp-storage would otherwise copy an
/// unaligned one through a 4 KB buffer on the stack.
pub struct Partition {
    flash: &'static RefCell<FlashStorage<'static>>,
    offset: u32,
    size: u32,
}

#[repr(C, align(4))]
struct Aligned([u8; 256]);

impl Flash for Partition {
    fn size(&self) -> u32 {
        self.size
    }

    fn read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), String> {
        read_flash(&mut self.flash.borrow_mut(), self.offset + offset, buf)
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), String> {
        let mut staged = Aligned([0; 256]);
        let mut flash = self.flash.borrow_mut();
        for (i, chunk) in data.chunks(256).enumerate() {
            staged.0[..chunk.len()].copy_from_slice(chunk);
            flash
                .write_nor(self.offset + offset + (i * 256) as u32, &staged.0[..chunk.len()])
                .map_err(|e| alloc::format!("flash write: {e:?}"))?;
        }
        Ok(())
    }

    fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
        let from = self.offset + offset;
        self.flash.borrow_mut().erase(from, from + len).map_err(|e| alloc::format!("flash erase: {e:?}"))
    }
}

/// Where a log lives: a partition, or RAM (an emulated C6, below).
pub enum Backing {
    Part(Partition),
    #[cfg_attr(not(all(feature = "qemu", feature = "esp32c6")), allow(dead_code))]
    Ram(RamFlash),
}

impl Flash for Backing {
    fn size(&self) -> u32 {
        match self {
            Self::Part(p) => p.size(),
            Self::Ram(r) => r.size(),
        }
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), String> {
        match self {
            Self::Part(p) => p.read(offset, buf),
            Self::Ram(r) => r.read(offset, buf),
        }
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), String> {
        match self {
            Self::Part(p) => p.write(offset, data),
            Self::Ram(r) => r.write(offset, data),
        }
    }
    fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
        match self {
            Self::Part(p) => p.erase(offset, len),
            Self::Ram(r) => r.erase(offset, len),
        }
    }
}

pub type Log = FlashLog<Backing>;

fn read_flash(flash: &mut FlashStorage<'static>, at: u32, buf: &mut [u8]) -> Result<(), String> {
    flash.read_nor(at, buf).map_err(|e| alloc::format!("flash read: {e:?}"))
}

/// The offset and size of the partition with that label, from the table at
/// 0x8000: 32-byte entries (magic `aa 50`, type, subtype, offset, size,
/// label), ended by the MD5 entry (`eb eb`) or erased flash.
fn find_partition(flash: &mut FlashStorage<'static>, label: &str) -> Option<(u32, u32)> {
    let mut entry = [0u8; 32];
    for i in 0..96 {
        read_flash(flash, TABLE_AT + i * 32, &mut entry).ok()?;
        if entry[0] != 0xAA || entry[1] != 0x50 {
            return None;
        }
        let name = &entry[12..28];
        let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(16)];
        if name == label.as_bytes() {
            let word = |i: usize| u32::from_le_bytes([entry[i], entry[i + 1], entry[i + 2], entry[i + 3]]);
            return Some((word(4), word(8)));
        }
    }
    None
}

/// Open the store and the bundle.
///
/// Espressif's QEMU loses a byte at every 16-byte boundary of a flash read on
/// its ESP32-C6 machine (the S3 machine, and real chips, read correctly; the
/// bootloader reads through the cache and does not notice). So an emulated C6
/// keeps both logs in RAM: the store empty, the bundle built at boot from the
/// build's seed (`UCORE_SEED_DIR`). Everything but esp-storage itself still
/// runs there, and nothing outlives a reset.
pub fn open(flash: esp_hal::peripherals::FLASH<'static>) -> (Log, Log) {
    #[cfg(not(all(feature = "qemu", feature = "esp32c6")))]
    let (store, bundle) = {
        let shared: &'static RefCell<FlashStorage<'static>> = alloc::boxed::Box::leak(alloc::boxed::Box::new(RefCell::new(FlashStorage::new(flash))));
        let part = |label: &str| {
            let (offset, size) = find_partition(&mut shared.borrow_mut(), label).unwrap_or_else(|| panic!("a partition called {label}"));
            Backing::Part(Partition { flash: shared, offset, size })
        };
        let (store, bundle) = (part(STORE), part(BUNDLE));
        (FlashLog::open(store).expect("the store partition"), FlashLog::open(bundle).expect("the bundle partition"))
    };
    #[cfg(all(feature = "qemu", feature = "esp32c6"))]
    let (store, bundle) = {
        let _ = flash;
        let image = ucore::bundle::image(32 * 1024, seed::SEED).expect("the seed fits");
        (
            FlashLog::open(Backing::Ram(RamFlash::new(64 * 1024))).expect("the store"),
            FlashLog::open(Backing::Ram(RamFlash(image))).expect("the bundle"),
        )
    };
    let names = |log: &Log| log.names().collect::<Vec<_>>().join(", ");
    esp_println::println!("store: {} KB free; {}", store.free() / 1024, names(&store));
    if bundle.names().next().is_none() {
        esp_println::println!("bundle: empty. Deploy a configuration with the μCore deployer.");
    } else {
        esp_println::println!("bundle: {}", names(&bundle));
    }
    (store, bundle)
}
