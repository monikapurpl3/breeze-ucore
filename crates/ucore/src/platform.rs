//! What a platform gives the core: time, randomness, non-blocking sockets,
//! files and a log. Everything else is the core's.
//!
//! Non-blocking because the core is one thread that never waits: it asks, and
//! comes back on the next poll. That maps onto std sockets, smoltcp and lwIP
//! alike, which is what keeps a new chip an adapter rather than a port.

use alloc::string::String;
use alloc::vec::Vec;
use core::net::IpAddr;

/// A connection handle, the platform's to number.
pub type Conn = u32;

/// The kinds of failure the core acts on differently: some are worth another
/// attempt on a fresh connection, others never will be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetKind {
    /// Nothing to read or room to write yet; try again later.
    WouldBlock,
    TimedOut,
    /// Reset, aborted, broken pipe.
    Reset,
    /// The other end closed the connection.
    Closed,
    Refused,
    Other,
}

/// A failure, with the platform's own wording for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetError {
    pub kind: NetKind,
    pub message: String,
}

impl NetError {
    pub fn new(kind: NetKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

pub trait Platform {
    /// Monotonic milliseconds, from any start.
    fn now_ms(&mut self) -> u64;
    /// Wall-clock Unix milliseconds, once the platform knows the time.
    fn unix_ms(&mut self) -> Option<u64>;
    /// How far local time is ahead of UTC at that moment, in seconds, DST
    /// included: timers and schedules run on local time, as Breeze Core's do.
    /// `None` while the platform does not know its zone; they wait.
    fn local_offset(&mut self, unix_s: i64) -> Option<i32>;
    fn random(&mut self, buf: &mut [u8]);

    /// Start connecting to a unit; [`Platform::is_connected`] says when done.
    fn connect(&mut self, ip: IpAddr, port: u16) -> Result<Conn, NetError>;
    /// `Ok(false)` while the connection is still being made.
    fn is_connected(&mut self, conn: Conn) -> Result<bool, NetError>;
    /// A client on the API port, with its address. Only called while the core
    /// has room for another, so a backlog is the platform's to keep.
    fn accept(&mut self) -> Option<(Conn, IpAddr)>;
    fn send(&mut self, conn: Conn, data: &[u8]) -> Result<usize, NetError>;
    /// `Ok(0)` when the other end has closed; `WouldBlock` when nothing is in.
    fn recv(&mut self, conn: Conn, buf: &mut [u8]) -> Result<usize, NetError>;
    /// Close a connection or a UDP socket.
    fn close(&mut self, conn: Conn);

    /// A UDP socket on any local port, allowed to broadcast: discovery.
    fn udp_open(&mut self) -> Result<Conn, NetError>;
    fn udp_send(&mut self, sock: Conn, to: IpAddr, port: u16, data: &[u8]) -> Result<(), NetError>;
    /// One datagram and who sent it; `WouldBlock` when none is waiting.
    fn udp_recv(&mut self, sock: Conn, buf: &mut [u8]) -> Result<(usize, IpAddr), NetError>;

    /// A store file (`config.json`, `devices.json`, ...), if it exists.
    fn read_file(&mut self, name: &str) -> Option<Vec<u8>>;

    /// Start replacing a store file whole. Its new content follows in
    /// [`Platform::file_write`] pieces, and [`Platform::file_end`] makes it
    /// the file, so a crash leaves the old one or the new one and never half
    /// of either. In pieces so that a chip never holds a whole file: the
    /// programs of a busy household are tens of kilobytes of JSON.
    /// `private` files are the owner's alone (`config.json` is readable by
    /// the admin group, the rest are not). One file at a time.
    fn file_begin(&mut self, name: &str, private: bool) -> Result<(), String>;
    /// The next piece of the file begun.
    fn file_write(&mut self, data: &[u8]) -> Result<(), String>;
    /// `keep`: make what was written the file. Otherwise drop it, and the old
    /// file stays.
    fn file_end(&mut self, keep: bool) -> Result<(), String>;

    /// Replace a store file whole with `data`.
    fn write_file(&mut self, name: &str, data: &[u8], private: bool) -> Result<(), String> {
        self.file_begin(name, private)?;
        let wrote = self.file_write(data);
        let ended = self.file_end(wrote.is_ok());
        wrote.and(ended)
    }
    fn log(&mut self, line: &str);
}
