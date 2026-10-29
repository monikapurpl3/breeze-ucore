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
    fn close(&mut self, conn: Conn);

    /// A store file (`config.json`, `devices.json`, ...), if it exists.
    fn read_file(&mut self, name: &str) -> Option<Vec<u8>>;
    fn log(&mut self, line: &str);
}
