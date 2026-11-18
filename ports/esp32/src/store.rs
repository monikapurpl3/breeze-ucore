//! The store files, in RAM for now: seeded at build time from
//! `UCORE_SEED_DIR` (config.json and the rest, as Breeze Core writes them),
//! replaced whole on every write, gone at a reset. Flash comes with
//! milestone 4; the core only ever sees read_file and write_file, so that is
//! a change here and nowhere else.

use alloc::string::String;
use alloc::vec::Vec;

include!(concat!(env!("OUT_DIR"), "/seed.rs"));

pub struct Store {
    files: Vec<(String, Vec<u8>)>,
}

impl Store {
    pub fn seeded() -> Self {
        Self { files: SEED.iter().map(|(n, b)| (String::from(*n), b.to_vec())).collect() }
    }

    pub fn read(&self, name: &str) -> Option<Vec<u8>> {
        self.files.iter().find(|(n, _)| n == name).map(|(_, b)| b.clone())
    }

    pub fn write(&mut self, name: &str, data: &[u8]) {
        match self.files.iter_mut().find(|(n, _)| n == name) {
            Some((_, b)) => {
                b.clear();
                b.extend_from_slice(data);
                b.shrink_to_fit();
            }
            None => self.files.push((String::from(name), data.to_vec())),
        }
    }
}
