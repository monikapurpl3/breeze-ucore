//! The store files on raw NOR flash, for a chip with no filesystem: a log in
//! one half of a partition, compacted into the other half when it fills.
//!
//! Every write appends a record; the newest record of a name is the file.
//! A record's header is written *last*, so a power cut part-way through a
//! write leaves the old record the newest one: the old file or the new one,
//! never half of either, which is what [`Platform::file_end`] promises.
//! Compaction copies the live records into the other half, and only then
//! writes that half's header with the next generation, so a cut during it
//! leaves the old half in charge. Nothing is ever kept whole in RAM except
//! what [`FlashLog::read`] hands back.
//!
//! Layout, little-endian, everything 4-byte aligned:
//!
//! ```text
//! half:    "UCLG" | generation u32 | half size u32 | crc32 of those 12 bytes
//! record:  "UCRC" | data length u32 | crc32 of name and data | flags u32
//!          | name (padded to 4) | data (padded to 4)
//! flags:   name length (bits 0-7) | private (bit 8) | dead (bit 9)
//! ```
//!
//! A dead record is a write that was abandoned: skipped when reading, never
//! copied. Erased flash reads 0xFF, so a header whose first word is
//! 0xFFFFFFFF is the end of the log.
//!
//! [`Platform::file_end`]: crate::platform::Platform::file_end

use alloc::string::String;
use alloc::vec::Vec;

/// What a chip's flash driver offers, offsets within the partition.
pub trait Flash {
    /// Bytes the partition holds; a multiple of two erase sectors.
    fn size(&self) -> u32;
    /// `offset` and `buf.len()` multiples of 4.
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), String>;
    /// `offset` and `data.len()` multiples of 4, into erased flash only.
    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), String>;
    /// Erase `len` bytes at `offset`, both multiples of [`SECTOR`].
    fn erase(&mut self, offset: u32, len: u32) -> Result<(), String>;
}

/// The erase sector of every flash this has been written for (ESP32, RP2350).
pub const SECTOR: u32 = 4096;
const HALF_MAGIC: u32 = u32::from_le_bytes(*b"UCLG");
const REC_MAGIC: u32 = u32::from_le_bytes(*b"UCRC");
const ERASED: u32 = u32::MAX;
const HEADER: u32 = 16;
const PRIVATE: u32 = 1 << 8;
const DEAD: u32 = 1 << 9;
/// Room kept free before a write begins, above the file's last size, so that
/// a file that grows a little does not run out of room half-way through.
const SLACK: u32 = 4096;
/// The staging buffer for writes: flash wants whole, aligned words.
const CHUNK: usize = 256;

fn pad4(n: u32) -> u32 {
    (n + 3) & !3
}

/// CRC-32 (IEEE, as zlib), a nibble at a time: a 64-byte table rather than
/// a 1 KB one.
#[derive(Clone, Copy)]
struct Crc(u32);

impl Crc {
    const TABLE: [u32; 16] = [
        0x0000_0000, 0x1DB7_1064, 0x3B6E_20C8, 0x26D9_30AC, 0x76DC_4190, 0x6B6B_51F4, 0x4DB2_6158, 0x5005_713C,
        0xEDB8_8320, 0xF00F_9344, 0xD6D6_A3E8, 0xCB61_B38C, 0x9B64_C2B0, 0x86D3_D2D4, 0xA00A_E278, 0xBDBD_F21C,
    ];

    fn new() -> Self {
        Crc(!0)
    }

    fn feed(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = Self::TABLE[((c ^ u32::from(b)) & 0xF) as usize] ^ (c >> 4);
            c = Self::TABLE[((c ^ (u32::from(b) >> 4)) & 0xF) as usize] ^ (c >> 4);
        }
        self.0 = c;
    }

    fn done(self) -> u32 {
        !self.0
    }
}

fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

struct Entry {
    name: String,
    /// Where the record's header is.
    at: u32,
    len: u32,
    private: bool,
}

struct Pending {
    name: String,
    private: bool,
    at: u32,
    data_at: u32,
    /// Bytes of data handed over so far, written or staged.
    len: u32,
    crc: Crc,
    buf: [u8; CHUNK],
    staged: usize,
    failed: bool,
}

pub struct FlashLog<F: Flash> {
    flash: F,
    half: u32,
    /// The half in charge: 0 or 1.
    active: u32,
    generation: u32,
    /// The next free byte of the active half, from its start. `half` when
    /// the log must be compacted before anything more is written to it.
    end: u32,
    entries: Vec<Entry>,
    pending: Option<Pending>,
}

impl<F: Flash> FlashLog<F> {
    /// Open the log, or start one on flash that holds none. Never fails for
    /// what is on the flash, only for the flash itself.
    pub fn open(mut flash: F) -> Result<Self, String> {
        let half = flash.size() / 2 / SECTOR * SECTOR;
        if half < 2 * SECTOR {
            return Err("the store partition is too small".into());
        }
        let mut best: Option<(u32, u32)> = None;
        for h in 0..2 {
            let mut b = [0u8; 16];
            flash.read(h * half, &mut b)?;
            let mut crc = Crc::new();
            crc.feed(&b[..12]);
            if word(&b, 0) == HALF_MAGIC && word(&b, 8) == half && word(&b, 12) == crc.done() {
                let generation = word(&b, 4);
                if best.is_none_or(|(_, g)| generation > g) {
                    best = Some((h, generation));
                }
            }
        }
        let mut log = Self { flash, half, active: 0, generation: 0, end: half, entries: Vec::new(), pending: None };
        match best {
            Some((h, generation)) => {
                log.active = h;
                log.generation = generation;
                log.scan()?;
            }
            None => log.start_half(0, 1)?,
        }
        Ok(log)
    }

    fn base(&self) -> u32 {
        self.active * self.half
    }

    /// Erase a half and make it the active, empty log of that generation.
    fn start_half(&mut self, h: u32, generation: u32) -> Result<(), String> {
        self.flash.erase(h * self.half, self.half)?;
        self.write_half_header(h, generation)?;
        self.active = h;
        self.generation = generation;
        self.end = HEADER;
        self.entries.clear();
        Ok(())
    }

    fn write_half_header(&mut self, h: u32, generation: u32) -> Result<(), String> {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&HALF_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&generation.to_le_bytes());
        b[8..12].copy_from_slice(&self.half.to_le_bytes());
        let mut crc = Crc::new();
        crc.feed(&b[..12]);
        b[12..16].copy_from_slice(&crc.done().to_le_bytes());
        self.flash.write(h * self.half, &b)
    }

    /// Read the active half's records into the index. Anything that is not
    /// a whole record, or flash after the last record that is not erased,
    /// means a write was cut short: the records before it stand, and the log
    /// is compacted before the next write.
    fn scan(&mut self) -> Result<(), String> {
        self.entries.clear();
        let mut at = HEADER;
        let mut clean = true;
        while at + HEADER <= self.half {
            let mut h = [0u8; 16];
            self.flash.read(self.base() + at, &mut h)?;
            if word(&h, 0) == ERASED {
                break;
            }
            let (len, crc, flags) = (word(&h, 4), word(&h, 8), word(&h, 12));
            let name_len = flags & 0xFF;
            let size = HEADER + pad4(name_len) + pad4(len);
            if word(&h, 0) != REC_MAGIC || len > self.half || at + size > self.half {
                clean = false;
                break;
            }
            if flags & DEAD == 0 {
                let mut name = alloc::vec![0u8; pad4(name_len) as usize];
                self.flash.read(self.base() + at + HEADER, &mut name)?;
                name.truncate(name_len as usize);
                let mut sum = Crc::new();
                sum.feed(&name);
                self.crc_data(at + HEADER + pad4(name_len), len, &mut sum)?;
                let Ok(name) = String::from_utf8(name) else {
                    clean = false;
                    break;
                };
                // A whole header over data that does not match it: the flash
                // let a bit go. That copy is skipped, so the file is its
                // previous copy, and the records after it still count.
                if sum.done() == crc {
                    self.entries.retain(|e| e.name != name);
                    self.entries.push(Entry { name, at, len, private: flags & PRIVATE != 0 });
                }
            }
            at += size;
        }
        if clean {
            clean = self.erased_from(at)?;
        }
        self.end = if clean { at } else { self.half };
        Ok(())
    }

    fn crc_data(&mut self, from: u32, len: u32, sum: &mut Crc) -> Result<(), String> {
        let mut buf = [0u8; 64];
        let mut done = 0;
        while done < len {
            let n = (len - done).min(64);
            self.flash.read(self.base() + from + done, &mut buf[..pad4(n) as usize])?;
            sum.feed(&buf[..n as usize]);
            done += n;
        }
        Ok(())
    }

    fn erased_from(&mut self, at: u32) -> Result<bool, String> {
        let mut buf = [0u8; 64];
        let mut off = at;
        while off < self.half {
            let n = (self.half - off).min(64);
            self.flash.read(self.base() + off, &mut buf[..n as usize])?;
            if buf[..n as usize].iter().any(|b| *b != 0xFF) {
                return Ok(false);
            }
            off += n;
        }
        Ok(true)
    }

    /// The newest copy of a file, if there is one.
    pub fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(e) = self.entries.iter().find(|e| e.name == name) else { return Ok(None) };
        let (from, len) = (e.at + HEADER + pad4(e.name.len() as u32), e.len);
        let mut out = alloc::vec![0u8; pad4(len) as usize];
        self.flash.read(self.base() + from, &mut out)?;
        out.truncate(len as usize);
        Ok(Some(out))
    }

    /// The names of the files there are.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// The flash underneath, for a log built in RAM to be written out.
    pub fn into_flash(self) -> F {
        self.flash
    }

    /// Bytes free in the active half.
    pub fn free(&self) -> u32 {
        self.half - self.end
    }

    /// Copy the live records into the other half, which then takes over.
    fn compact(&mut self) -> Result<(), String> {
        let (old, new) = (self.active, 1 - self.active);
        self.flash.erase(new * self.half, self.half)?;
        let mut at = HEADER;
        let mut moved = Vec::with_capacity(self.entries.len());
        let mut buf = [0u8; CHUNK];
        for e in &self.entries {
            let size = HEADER + pad4(e.name.len() as u32) + pad4(e.len);
            if at + size > self.half {
                return Err("the store is full".into());
            }
            // Header last, as in any other write.
            let mut done = HEADER;
            while done < size {
                let n = (size - done).min(CHUNK as u32);
                self.flash.read(old * self.half + e.at + done, &mut buf[..n as usize])?;
                self.flash.write(new * self.half + at + done, &buf[..n as usize])?;
                done += n;
            }
            self.flash.read(old * self.half + e.at, &mut buf[..16])?;
            self.flash.write(new * self.half + at, &buf[..16])?;
            moved.push(Entry { name: e.name.clone(), at, len: e.len, private: e.private });
            at += size;
        }
        self.write_half_header(new, self.generation + 1)?;
        self.active = new;
        self.generation += 1;
        self.end = at;
        self.entries = moved;
        Ok(())
    }

    /// Start replacing a file; see [`Platform::file_begin`](crate::platform::Platform::file_begin).
    pub fn begin(&mut self, name: &str, private: bool) -> Result<(), String> {
        if name.is_empty() || name.len() > 255 {
            return Err("a store file needs a name of 1 to 255 bytes".into());
        }
        if self.pending.is_some() {
            self.end(false)?;
        }
        let last = self.entries.iter().find(|e| e.name == name).map_or(0, |e| e.len);
        let need = HEADER + pad4(name.len() as u32) + pad4(last) + SLACK;
        if self.free() < need {
            self.compact()?;
            if self.free() < need {
                return Err("the store is full".into());
            }
        }
        let at = self.end;
        let data_at = at + HEADER + pad4(name.len() as u32);
        let mut crc = Crc::new();
        crc.feed(name.as_bytes());
        let mut padded = [0xFFu8; 260];
        padded[..name.len()].copy_from_slice(name.as_bytes());
        // The name goes down now; if the write is abandoned, the dead header
        // still says how far to skip.
        self.flash.write(self.base() + at + HEADER, &padded[..pad4(name.len() as u32) as usize])?;
        self.pending = Some(Pending { name: name.into(), private, at, data_at, len: 0, crc, buf: [0xFF; CHUNK], staged: 0, failed: false });
        Ok(())
    }

    /// The next piece of the file begun.
    pub fn write(&mut self, mut data: &[u8]) -> Result<(), String> {
        let base = self.base();
        let half = self.half;
        let p = self.pending.as_mut().ok_or("no store file begun")?;
        if p.failed {
            return Err("this store file has already failed".into());
        }
        while !data.is_empty() {
            let n = data.len().min(CHUNK - p.staged);
            p.buf[p.staged..p.staged + n].copy_from_slice(&data[..n]);
            p.crc.feed(&data[..n]);
            p.staged += n;
            p.len += n as u32;
            data = &data[n..];
            if p.staged == CHUNK {
                let to = p.data_at + p.len - CHUNK as u32;
                // A header's worth stays free behind it, for the next record.
                if to + CHUNK as u32 + HEADER > half {
                    p.failed = true;
                    return Err("the store is full".into());
                }
                if let Err(e) = self.flash.write(base + to, &p.buf) {
                    p.failed = true;
                    return Err(e);
                }
                p.buf = [0xFF; CHUNK];
                p.staged = 0;
            }
        }
        Ok(())
    }

    /// `keep`: the file just written becomes the file. Otherwise it is
    /// dropped and the old one stays.
    pub fn end(&mut self, keep: bool) -> Result<(), String> {
        let base = self.base();
        let half = self.half;
        let Some(mut p) = self.pending.take() else { return Err("no store file begun".into()) };
        let mut result = Ok(());
        if p.staged > 0 && !p.failed {
            let to = p.data_at + p.len - p.staged as u32;
            let n = pad4(p.staged as u32);
            if to + n + HEADER > half {
                p.failed = true;
                result = Err(String::from("the store is full"));
            } else if let Err(e) = self.flash.write(base + to, &p.buf[..n as usize]) {
                p.failed = true;
                result = Err(e);
            }
        }
        let name_len = p.name.len() as u32;
        let keep = keep && !p.failed;
        // How far the record reaches: a dead header must cover every byte
        // that may have been programmed, and data is only ever programmed
        // within what was handed over.
        let reach = pad4(p.len).min(half - p.at - HEADER - pad4(name_len));
        let mut h = [0u8; 16];
        h[0..4].copy_from_slice(&REC_MAGIC.to_le_bytes());
        h[4..8].copy_from_slice(&(if keep { p.len } else { reach }).to_le_bytes());
        h[8..12].copy_from_slice(&(if keep { p.crc.done() } else { 0 }).to_le_bytes());
        let flags = name_len | if p.private { PRIVATE } else { 0 } | if keep { 0 } else { DEAD };
        h[12..16].copy_from_slice(&flags.to_le_bytes());
        self.flash.write(base + p.at, &h)?;
        self.end = p.data_at + if keep { pad4(p.len) } else { reach };
        if keep {
            self.entries.retain(|e| e.name != p.name);
            self.entries.push(Entry { name: p.name, at: p.at, len: p.len, private: p.private });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{Flash, FlashLog, SECTOR};
    use alloc::string::String;
    use alloc::vec::Vec;

    /// NOR flash as it behaves: a write can only clear bits, an erase sets a
    /// whole sector back to 0xFF. And it can lose power after a given number
    /// of bytes have been programmed.
    struct Nor {
        mem: Vec<u8>,
        budget: Option<usize>,
    }

    impl Nor {
        fn new(size: usize) -> Self {
            Self { mem: alloc::vec![0xFF; size], budget: None }
        }

        fn spend(&mut self, n: usize) -> Result<usize, String> {
            match &mut self.budget {
                None => Ok(n),
                Some(0) => Err("power cut".into()),
                Some(b) => {
                    let k = n.min(*b);
                    *b -= k;
                    Ok(k)
                }
            }
        }
    }

    impl Flash for &mut Nor {
        fn size(&self) -> u32 {
            self.mem.len() as u32
        }
        fn read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), String> {
            assert_eq!(offset % 4, 0, "aligned read");
            assert_eq!(buf.len() % 4, 0, "aligned read");
            buf.copy_from_slice(&self.mem[offset as usize..offset as usize + buf.len()]);
            Ok(())
        }
        fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), String> {
            assert_eq!(offset % 4, 0, "aligned write");
            assert_eq!(data.len() % 4, 0, "aligned write");
            let k = self.spend(data.len())?;
            for (i, b) in data[..k].iter().enumerate() {
                let m = &mut self.mem[offset as usize + i];
                assert_eq!(*m & *b, *b, "writing to flash that is not erased, at {}", offset as usize + i);
                *m &= *b;
            }
            if k < data.len() {
                return Err("power cut".into());
            }
            Ok(())
        }
        fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
            assert_eq!(offset % SECTOR, 0);
            assert_eq!(len % SECTOR, 0);
            // A cut erase leaves sectors erased, or not yet: whole sectors.
            for s in (offset..offset + len).step_by(SECTOR as usize) {
                self.spend(1)?;
                self.mem[s as usize..(s + SECTOR) as usize].fill(0xFF);
            }
            Ok(())
        }
    }

    fn put(log: &mut FlashLog<&mut Nor>, name: &str, data: &[u8]) -> Result<(), String> {
        log.begin(name, true)?;
        let wrote = data.chunks(97).try_for_each(|c| log.write(c));
        let ended = log.end(wrote.is_ok());
        wrote.and(ended)
    }

    fn content(i: usize, len: usize) -> Vec<u8> {
        (0..len).map(|k| (k * 31 + i * 7) as u8).collect()
    }

    #[test]
    fn files_survive_reopening_and_compaction() {
        let mut nor = Nor::new(64 * 1024);
        {
            let mut log = FlashLog::open(&mut nor).unwrap();
            for round in 0..40 {
                for (n, name) in ["config.json", "devices.json", "programs.json"].iter().enumerate() {
                    put(&mut log, name, &content(round * 3 + n, 500 + n * 2000 + round)).unwrap();
                }
            }
            assert!(log.generation > 1, "it compacted");
            assert_eq!(log.read("programs.json").unwrap().unwrap(), content(39 * 3 + 2, 500 + 4000 + 39));
        }
        let mut log = FlashLog::open(&mut nor).unwrap();
        for (n, name) in ["config.json", "devices.json", "programs.json"].iter().enumerate() {
            assert_eq!(log.read(name).unwrap().unwrap(), content(39 * 3 + n, 500 + n * 2000 + 39), "{name}");
        }
        assert_eq!(log.read("timers.json").unwrap(), None);
    }

    #[test]
    fn an_abandoned_write_leaves_the_old_file() {
        let mut nor = Nor::new(32 * 1024);
        let mut log = FlashLog::open(&mut nor).unwrap();
        put(&mut log, "a", b"old").unwrap();
        log.begin("a", true).unwrap();
        log.write(&[7; 1000]).unwrap();
        log.end(false).unwrap();
        put(&mut log, "b", b"next").unwrap();
        assert_eq!(log.read("a").unwrap().unwrap(), b"old");
        drop(log);
        let mut log = FlashLog::open(&mut nor).unwrap();
        assert_eq!(log.read("a").unwrap().unwrap(), b"old");
        assert_eq!(log.read("b").unwrap().unwrap(), b"next");
    }

    #[test]
    fn a_file_too_big_fails_and_leaves_the_old_one() {
        let mut nor = Nor::new(16 * 1024);
        let mut log = FlashLog::open(&mut nor).unwrap();
        put(&mut log, "a", b"old").unwrap();
        assert!(put(&mut log, "a", &[1; 9000]).is_err());
        assert_eq!(log.read("a").unwrap().unwrap(), b"old");
        put(&mut log, "a", b"new").unwrap();
        drop(log);
        assert_eq!(FlashLog::open(&mut nor).unwrap().read("a").unwrap().unwrap(), b"new");
    }

    /// A bit gone bad in a file's newest copy: its previous copy is the
    /// file, and the files written after it are untouched.
    #[test]
    fn a_damaged_copy_falls_back_to_the_one_before() {
        let mut nor = Nor::new(32 * 1024);
        let at = {
            let mut log = FlashLog::open(&mut nor).unwrap();
            put(&mut log, "a", b"first").unwrap();
            put(&mut log, "a", b"second").unwrap();
            let at = log.entries.iter().find(|e| e.name == "a").unwrap().at;
            put(&mut log, "b", b"after").unwrap();
            at
        };
        // The data of "a"'s newest copy: header, then the name padded to 4.
        nor.mem[(at + 16 + 4 + 2) as usize] ^= 0x10;
        let mut log = FlashLog::open(&mut nor).unwrap();
        assert_eq!(log.read("a").unwrap().unwrap(), b"first");
        assert_eq!(log.read("b").unwrap().unwrap(), b"after");
        put(&mut log, "a", b"third").unwrap();
        assert_eq!(FlashLog::open(&mut nor).unwrap().read("a").unwrap().unwrap(), b"third");
    }

    /// The promise, tested at every byte: power lost after any number of
    /// programmed bytes (or erased sectors), during a write or a compaction,
    /// and the reopened store holds every file either as it was or as it was
    /// being written, never anything else; and it keeps working after.
    #[test]
    fn a_power_cut_anywhere_leaves_old_or_new() {
        // A log that is nearly full, so that the cut writes compact.
        let mut base = Nor::new(16 * 1024);
        {
            let mut log = FlashLog::open(&mut base).unwrap();
            for round in 0..6 {
                put(&mut log, "x", &content(round, 900)).unwrap();
                put(&mut log, "y", &content(100 + round, 300)).unwrap();
            }
        }
        let (old_x, old_y) = (content(5, 900), content(105, 300));
        let new_x = content(999, 1300);
        let mut cuts = 0;
        for budget in 0.. {
            let mut nor = Nor { mem: base.mem.clone(), budget: Some(budget) };
            let finished = {
                let mut log = FlashLog::open(&mut nor).unwrap();
                put(&mut log, "x", &new_x).is_ok()
            };
            nor.budget = None;
            let mut log = FlashLog::open(&mut nor).unwrap();
            let x = log.read("x").unwrap().unwrap();
            assert!(x == old_x || x == new_x, "x is neither, cut after {budget}");
            if finished {
                assert_eq!(x, new_x);
            }
            assert_eq!(log.read("y").unwrap().unwrap(), old_y, "y, cut after {budget}");
            put(&mut log, "x", b"after").unwrap();
            put(&mut log, "y", b"after too").unwrap();
            let mut log = FlashLog::open(&mut nor).unwrap();
            assert_eq!(log.read("x").unwrap().unwrap(), b"after");
            assert_eq!(log.read("y").unwrap().unwrap(), b"after too");
            if finished {
                break;
            }
            cuts += 1;
        }
        assert!(cuts > 1000, "the cuts covered the write and the compaction ({cuts})");
    }
}
