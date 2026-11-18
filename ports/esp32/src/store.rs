//! The store files in flash: the `ucore` partition (see `partitions.csv`),
//! as a [`ucore::flashlog::FlashLog`], so a power cut leaves every file
//! either as it was or as it was being written.
//!
//! A build can carry a seed (`UCORE_SEED_DIR`): those files are written in
//! at boot when the store does not have them yet, and never over anything
//! it has.

use alloc::string::String;
use alloc::vec::Vec;
use esp_storage::FlashStorage;
use ucore::flashlog::{Flash, FlashLog};

include!(concat!(env!("OUT_DIR"), "/seed.rs"));

const PARTITION: &str = "ucore";

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
        self.flash.read_nor(self.offset + offset, buf).map_err(|e| alloc::format!("flash read: {e:?}"))
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

pub type Store = FlashLog<Partition>;

/// Find the partition, open the log on it, and write in the seed.
pub fn open(flash: esp_hal::peripherals::FLASH<'static>) -> Store {
    let mut flash = FlashStorage::new(flash);
    let mut table = [0u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN];
    let (offset, size) = {
        let parts = esp_bootloader_esp_idf::partitions::read_partition_table(&mut flash, &mut table).expect("a partition table");
        let part = parts.iter().find(|p| p.label_as_str() == PARTITION).expect("a partition called ucore");
        (part.offset(), part.len())
    };
    let mut store = FlashLog::open(Partition { flash, offset, size }).expect("the store partition");
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
