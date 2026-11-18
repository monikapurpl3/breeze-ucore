//! The wall clock, from SNTP (RFC 4330): one 48-byte question to a time
//! server, the answer's transmit time plus half the round trip. A chip has no
//! battery-backed clock, so until the first answer the core knows no time,
//! and its timers and schedules wait (Breeze Core's `/api/health` says so).
//!
//! The server is `env.json`'s `ntp`, a name or an address (see
//! `ucore::bundle`). Asked again every hour, and every few seconds until it
//! first answers.

use alloc::string::String;
use smoltcp::iface::SocketHandle;
use smoltcp::phy::Device;
use smoltcp::socket::{dns, udp};
use smoltcp::wire::{DnsQueryType, IpAddress, IpEndpoint};

use crate::net::{now_ms, Chip};

const NTP_TO_UNIX_S: u64 = 2_208_988_800;
const RETRY_MS: u64 = 5_000;
const RESYNC_MS: u64 = 3_600_000;
const ANSWER_MS: u64 = 3_000;

enum State {
    Idle { next: u64 },
    Resolving(dns::QueryHandle, u64),
    Asking { sock: SocketHandle, sent: u64 },
}

pub struct Clock {
    server: String,
    /// Unix milliseconds at a moment of the monotonic clock.
    synced: Option<(u64, u64)>,
    state: State,
}

impl Clock {
    pub fn new(server: String) -> Self {
        Self { server, synced: None, state: State::Idle { next: 0 } }
    }

    pub fn unix_ms(&self, mono: u64) -> Option<u64> {
        self.synced.map(|(unix, at)| unix + (mono - at))
    }
}

impl<D: Device> Chip<D> {
    pub(crate) fn sntp(&mut self) {
        if self.ip.is_none() {
            return;
        }
        let now = now_ms();
        let retry = if self.clock.synced.is_some() { RESYNC_MS } else { RETRY_MS };
        match self.clock.state {
            State::Idle { next } if now >= next => {
                if let Ok(ip) = self.clock.server.parse() {
                    self.ask(IpAddress::Ipv4(ip), now, retry);
                    return;
                }
                let query = self.sockets.get_mut::<dns::Socket>(self.dns).start_query(self.iface.context(), &self.clock.server, DnsQueryType::A);
                self.clock.state = match query {
                    Ok(q) => State::Resolving(q, now),
                    Err(_) => State::Idle { next: now + retry },
                };
            }
            State::Idle { .. } => {}
            State::Resolving(q, since) => match self.sockets.get_mut::<dns::Socket>(self.dns).get_query_result(q) {
                Ok(addrs) => match addrs.first() {
                    Some(ip) => self.ask(*ip, now, retry),
                    None => self.clock.state = State::Idle { next: now + retry },
                },
                Err(dns::GetQueryResultError::Pending) if now - since < 10_000 => {}
                Err(_) => self.clock.state = State::Idle { next: now + retry },
            },
            State::Asking { sock, sent } => {
                let mut buf = [0u8; 48];
                let got = self.sockets.get_mut::<udp::Socket>(sock).recv_slice(&mut buf).ok();
                if let Some((48, _)) = got {
                    let secs = u64::from(u32::from_be_bytes([buf[40], buf[41], buf[42], buf[43]]));
                    let frac = u64::from(u32::from_be_bytes([buf[44], buf[45], buf[46], buf[47]]));
                    if secs > NTP_TO_UNIX_S {
                        let unix = (secs - NTP_TO_UNIX_S) * 1000 + ((frac * 1000) >> 32) + (now - sent) / 2;
                        let first = self.clock.synced.is_none();
                        self.clock.synced = Some((unix, now));
                        if first {
                            esp_println::println!("clock: set from {}", self.clock.server);
                        }
                    }
                } else if now - sent < ANSWER_MS {
                    return;
                }
                self.sockets.remove(sock);
                let retry = if self.clock.synced.is_some() { RESYNC_MS } else { RETRY_MS };
                self.clock.state = State::Idle { next: now + retry };
            }
        }
    }

    fn ask(&mut self, server: IpAddress, now: u64, retry: u64) {
        let rx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 1], alloc::vec![0; 64]);
        let tx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 1], alloc::vec![0; 64]);
        let mut s = udp::Socket::new(rx, tx);
        let port = self.ephemeral();
        let mut question = [0u8; 48];
        // LI 0, version 4, mode 3 (client).
        question[0] = 0x23;
        if s.bind(port).is_err() || s.send_slice(&question, IpEndpoint::new(server, 123)).is_err() {
            self.clock.state = State::Idle { next: now + retry };
            return;
        }
        let sock = self.sockets.add(s);
        self.clock.state = State::Asking { sock, sent: now };
    }
}
