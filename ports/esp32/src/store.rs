//! The store files in flash: the `ucore` partition (see `partitions.csv`),
//! as a [`ucore::flashlog::FlashLog`], so a power cut leaves every file
//! either as it was or as it was being written.
//!
//! A build can carry a seed (`UCORE_SEED_DIR`): those files are written in
//! at boot when the store does not have them yet, and never over anything
//! it has.

// An emulated C6 keeps the log in RAM (see RamFlash), so the partition code
// goes unused there.
#![cfg_attr(all(feature = "qemu", feature = "esp32c6"), allow(dead_code))]

use alloc::string::String;
use alloc::vec::Vec;
use esp_storage::FlashStorage;
use ucore::flashlog::{Flash, FlashLog};

include!(concat!(env!("OUT_DIR"), "/seed.rs"));

const PARTITION: &str = "ucore";
const TABLE_AT: u32 = 0x8000;

/// The partition, through esp-storage. Writes go from a word-aligned buffer
/// of its own: esp-storage would otherwise copy an unaligned one through a
/// 4 KB buffer on the stack.
pub struct Partition {
    flash: FlashStorage<'static>,
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
        read_flash(&mut self.flash, self.offset + offset, buf)
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), String> {
        let mut staged = Aligned([0; 256]);
        for (i, chunk) in data.chunks(256).enumerate() {
            staged.0[..chunk.len()].copy_from_slice(chunk);
            self.flash
                .write_nor(self.offset + offset + (i * 256) as u32, &staged.0[..chunk.len()])
                .map_err(|e| alloc::format!("flash write: {e:?}"))?;
        }
        Ok(())
    }

    fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
        self.flash.erase(self.offset + offset, self.offset + offset + len).map_err(|e| alloc::format!("flash erase: {e:?}"))
    }
}

#[cfg(not(all(feature = "qemu", feature = "esp32c6")))]
pub type Store = FlashLog<Partition>;

/// Espressif's QEMU loses a byte at every 16-byte boundary of a flash read on
/// its ESP32-C6 machine (the S3 machine, and real chips, read correctly; the
/// bootloader reads through the cache and does not notice). So an emulated
/// C6 keeps the log on 64 KB of RAM instead: everything but esp-storage
/// itself still runs, and nothing outlives a reset.
#[cfg(all(feature = "qemu", feature = "esp32c6"))]
pub type Store = FlashLog<RamFlash>;

#[cfg(all(feature = "qemu", feature = "esp32c6"))]
pub struct RamFlash(alloc::boxed::Box<[u8]>);

#[cfg(all(feature = "qemu", feature = "esp32c6"))]
impl Flash for RamFlash {
    fn size(&self) -> u32 {
        self.0.len() as u32
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), String> {
        buf.copy_from_slice(&self.0[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), String> {
        for (m, b) in self.0[offset as usize..].iter_mut().zip(data) {
            *m &= *b;
        }
        Ok(())
    }
    fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
        self.0[offset as usize..(offset + len) as usize].fill(0xFF);
        Ok(())
    }
}

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

/// Find the partition, open the log on it, and write in the seed.
pub fn open(flash: esp_hal::peripherals::FLASH<'static>) -> Store {
    #[cfg(not(all(feature = "qemu", feature = "esp32c6")))]
    let mut store = {
        let mut flash = FlashStorage::new(flash);
        let (offset, size) = find_partition(&mut flash, PARTITION).expect("a partition called ucore");
        FlashLog::open(Partition { flash, offset, size }).expect("the store partition")
    };
    #[cfg(all(feature = "qemu", feature = "esp32c6"))]
    let mut store = {
        let _ = flash;
        FlashLog::open(RamFlash(alloc::vec![0xFF; 64 * 1024].into_boxed_slice())).expect("the store")
    };
    for (name, bytes) in SEED {
        if store.read(name).ok().flatten().is_none() {
            let wrote = store.begin(name, *name != "config.json").and_then(|()| store.write(bytes));
            let _ = store.end(wrote.is_ok());
        }
    }
    let names: Vec<&str> = store.names().collect();
    esp_println::println!("store: {} KB free; {}", store.free() / 1024, names.join(", "));
    store
}
