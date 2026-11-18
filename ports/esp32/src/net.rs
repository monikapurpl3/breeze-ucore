//! μCore's platform on a chip: smoltcp for the network, the chip's clock and
//! random number generator, the store, and the serial console.
//!
//! smoltcp is non-blocking and polled, as the core is, so each `Platform`
//! call is a direct socket operation and nothing waits: the main loop polls
//! the interface, then the core, then the interface again, and idles until a
//! frame arrives or someone's deadline comes up.

use alloc::vec;
use alloc::vec::Vec;
use core::net::IpAddr;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::Device;
use smoltcp::socket::{dhcpv4, dns, tcp, udp};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Cidr};
use ucore::platform::{Conn, NetError, NetKind, Platform};

/// Breeze Core's port.
pub const API_PORT: u16 = 8420;
/// Sockets waiting for the next API client; the core takes them as it has room.
const LISTENERS: usize = 2;
/// Socket buffers. The core keeps its own request and response buffers, so
/// these only need to cover what is in flight.
const CLIENT_RX: usize = 1024;
const CLIENT_TX: usize = 2048;
const UNIT_RX: usize = 512;
const UNIT_TX: usize = 512;
const CONNECT_TIMEOUT_MS: u64 = 5_000;
/// The longest the loop sleeps without looking at the network.
const MAX_IDLE_MS: u64 = 20;

pub fn now_ms() -> u64 {
    esp_hal::time::Instant::now().duration_since_epoch().as_millis()
}

fn now() -> Instant {
    Instant::from_millis(now_ms() as i64)
}

fn to_smol(ip: IpAddr) -> Result<IpAddress, NetError> {
    match ip {
        IpAddr::V4(a) => Ok(IpAddress::Ipv4(a)),
        IpAddr::V6(_) => Err(NetError::new(NetKind::Other, "no IPv6 here")),
    }
}

fn to_std(ip: IpAddress) -> IpAddr {
    match ip {
        IpAddress::Ipv4(a) => IpAddr::V4(a),
    }
}

enum Kind {
    Tcp { since: u64 },
    Udp,
}

struct Open {
    conn: Conn,
    handle: SocketHandle,
    kind: Kind,
}

pub struct Chip<D: Device> {
    pub(crate) dev: D,
    pub(crate) iface: Interface,
    pub(crate) sockets: SocketSet<'static>,
    open: Vec<Open>,
    listening: Vec<SocketHandle>,
    /// Closed by the core, still sending what they hold, then the FIN.
    closing: Vec<SocketHandle>,
    next: Conn,
    port: u16,
    dhcp: SocketHandle,
    pub(crate) dns: SocketHandle,
    pub(crate) ip: Option<Ipv4Cidr>,
    pub(crate) clock: crate::sntp::Clock,
    tz: ucore::tz::Rule,
    rng: esp_hal::rng::Rng,
    store: crate::store::Store,
}

impl<D: Device> Chip<D> {
    pub fn new(mut dev: D, mac: [u8; 6], rng: esp_hal::rng::Rng, tz: ucore::tz::Rule, store: crate::store::Store) -> Self {
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        config.random_seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
        let iface = Interface::new(config, &mut dev, now());
        let mut sockets = SocketSet::new(vec![]);
        let dhcp = sockets.add(dhcpv4::Socket::new());
        let dns = sockets.add(dns::Socket::new(&[], vec![None]));
        let port = 49_152 + (rng.random() % 16_384) as u16;
        Self {
            dev,
            iface,
            sockets,
            open: Vec::new(),
            listening: Vec::new(),
            closing: Vec::new(),
            next: 0,
            port,
            dhcp,
            dns,
            ip: None,
            clock: crate::sntp::Clock::new(),
            tz,
            rng,
            store,
        }
    }

    pub(crate) fn ephemeral(&mut self) -> u16 {
        self.port = if self.port == u16::MAX { 49_152 } else { self.port + 1 };
        self.port
    }

    /// Move frames in and out, and keep the address, the listeners and the
    /// clock going.
    pub fn poll(&mut self) {
        self.iface.poll(now(), &mut self.dev, &mut self.sockets);
        let event = self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).poll();
        match event {
            Some(dhcpv4::Event::Configured(c)) => {
                let (address, router) = (c.address, c.router);
                let servers: Vec<IpAddress> = c.dns_servers.iter().map(|s| IpAddress::Ipv4(*s)).collect();
                self.iface.update_ip_addrs(|a| {
                    a.clear();
                    let _ = a.push(IpCidr::Ipv4(address));
                });
                match router {
                    Some(r) => {
                        let _ = self.iface.routes_mut().add_default_ipv4_route(r);
                    }
                    None => {
                        self.iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                self.sockets.get_mut::<dns::Socket>(self.dns).update_servers(&servers);
                self.ip = Some(address);
                esp_println::println!("network: {address}, gateway {router:?}");
            }
            Some(dhcpv4::Event::Deconfigured) => {
                self.iface.update_ip_addrs(|a| a.clear());
                self.iface.routes_mut().remove_default_ipv4_route();
                self.ip = None;
                esp_println::println!("network: lost the address");
            }
            None => {}
        }
        while self.listening.len() < LISTENERS {
            let mut s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; CLIENT_RX]), tcp::SocketBuffer::new(vec![0; CLIENT_TX]));
            if s.listen(API_PORT).is_err() {
                break;
            }
            let h = self.sockets.add(s);
            self.listening.push(h);
        }
        let mut i = 0;
        while i < self.closing.len() {
            let h = self.closing[i];
            if self.sockets.get::<tcp::Socket>(h).is_active() {
                i += 1;
            } else {
                self.sockets.remove(h);
                self.closing.swap_remove(i);
            }
        }
        self.sntp();
    }

    /// Sleep until a frame arrives or `core_wait` is up, whichever is first.
    pub fn idle(&mut self, core_wait: u64) {
        let net = self.iface.poll_delay(now(), &self.sockets).map_or(MAX_IDLE_MS, |d| d.total_millis());
        let until = now_ms() + core_wait.min(net).min(MAX_IDLE_MS);
        let delay = esp_hal::delay::Delay::new();
        while now_ms() < until {
            if self.dev.receive(now()).is_some() {
                return;
            }
            delay.delay_micros(200);
        }
    }

    pub fn sockets_in_use(&self) -> usize {
        self.sockets.iter().count()
    }

    fn add(&mut self, handle: SocketHandle, kind: Kind) -> Conn {
        self.next = self.next.wrapping_add(1);
        self.open.push(Open { conn: self.next, handle, kind });
        self.next
    }

    fn find(&self, conn: Conn) -> Result<&Open, NetError> {
        self.open.iter().find(|o| o.conn == conn).ok_or_else(|| NetError::new(NetKind::Closed, "connection closed"))
    }

    fn tcp(&mut self, conn: Conn) -> Result<&mut tcp::Socket<'static>, NetError> {
        let h = self.find(conn)?.handle;
        Ok(self.sockets.get_mut::<tcp::Socket>(h))
    }

    fn udp(&mut self, conn: Conn) -> Result<&mut udp::Socket<'static>, NetError> {
        let h = self.find(conn)?.handle;
        Ok(self.sockets.get_mut::<udp::Socket>(h))
    }
}

impl<D: Device> Platform for Chip<D> {
    fn now_ms(&mut self) -> u64 {
        now_ms()
    }

    fn unix_ms(&mut self) -> Option<u64> {
        self.clock.unix_ms(now_ms())
    }

    fn local_offset(&mut self, unix_s: i64) -> Option<i32> {
        Some(self.tz.offset_at(unix_s))
    }

    fn random(&mut self, buf: &mut [u8]) {
        self.rng.read(buf);
    }

    fn connect(&mut self, ip: IpAddr, port: u16) -> Result<Conn, NetError> {
        if self.ip.is_none() {
            return Err(NetError::new(NetKind::Other, "no network yet"));
        }
        let mut s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; UNIT_RX]), tcp::SocketBuffer::new(vec![0; UNIT_TX]));
        s.set_timeout(Some(Duration::from_millis(CONNECT_TIMEOUT_MS)));
        s.set_nagle_enabled(false);
        let local = self.ephemeral();
        s.connect(self.iface.context(), IpEndpoint::new(to_smol(ip)?, port), local)
            .map_err(|_| NetError::new(NetKind::Other, "cannot connect"))?;
        let h = self.sockets.add(s);
        Ok(self.add(h, Kind::Tcp { since: now_ms() }))
    }

    fn is_connected(&mut self, conn: Conn) -> Result<bool, NetError> {
        let since = match self.find(conn)?.kind {
            Kind::Tcp { since } => since,
            Kind::Udp => return Ok(true),
        };
        let s = self.tcp(conn)?;
        match s.state() {
            tcp::State::Established | tcp::State::CloseWait => {
                s.set_timeout(None);
                Ok(true)
            }
            tcp::State::SynSent | tcp::State::SynReceived => Ok(false),
            _ if now_ms().saturating_sub(since) + 100 >= CONNECT_TIMEOUT_MS => Err(NetError::new(NetKind::TimedOut, "connect timed out")),
            _ => Err(NetError::new(NetKind::Refused, "connection refused")),
        }
    }

    fn accept(&mut self) -> Option<(Conn, IpAddr)> {
        let i = self.listening.iter().position(|h| {
            matches!(self.sockets.get::<tcp::Socket>(*h).state(), tcp::State::Established | tcp::State::CloseWait)
        })?;
        let h = self.listening.swap_remove(i);
        let peer = self.sockets.get::<tcp::Socket>(h).remote_endpoint()?.addr;
        let conn = self.add(h, Kind::Tcp { since: now_ms() });
        Some((conn, to_std(peer)))
    }

    fn send(&mut self, conn: Conn, data: &[u8]) -> Result<usize, NetError> {
        let s = self.tcp(conn)?;
        if !s.may_send() {
            return Err(NetError::new(NetKind::Reset, "connection reset"));
        }
        match s.send_slice(data) {
            Ok(0) if !data.is_empty() => Err(NetError::new(NetKind::WouldBlock, "send buffer full")),
            Ok(n) => Ok(n),
            Err(_) => Err(NetError::new(NetKind::Reset, "connection reset")),
        }
    }

    fn recv(&mut self, conn: Conn, buf: &mut [u8]) -> Result<usize, NetError> {
        let s = self.tcp(conn)?;
        if s.can_recv() {
            s.recv_slice(buf).map_err(|_| NetError::new(NetKind::Reset, "connection reset"))
        } else if !s.may_recv() {
            Ok(0)
        } else {
            Err(NetError::new(NetKind::WouldBlock, "nothing yet"))
        }
    }

    fn close(&mut self, conn: Conn) {
        let Some(i) = self.open.iter().position(|o| o.conn == conn) else { return };
        let o = self.open.swap_remove(i);
        match o.kind {
            Kind::Tcp { .. } => {
                self.sockets.get_mut::<tcp::Socket>(o.handle).close();
                self.closing.push(o.handle);
            }
            Kind::Udp => {
                self.sockets.remove(o.handle);
            }
        }
    }

    fn udp_open(&mut self) -> Result<Conn, NetError> {
        let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 1024]);
        let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 2], vec![0; 512]);
        let mut s = udp::Socket::new(rx, tx);
        let port = self.ephemeral();
        s.bind(port).map_err(|_| NetError::new(NetKind::Other, "cannot bind"))?;
        let h = self.sockets.add(s);
        Ok(self.add(h, Kind::Udp))
    }

    fn udp_send(&mut self, sock: Conn, to: IpAddr, port: u16, data: &[u8]) -> Result<(), NetError> {
        let to = IpEndpoint::new(to_smol(to)?, port);
        self.udp(sock)?.send_slice(data, to).map_err(|_| NetError::new(NetKind::WouldBlock, "cannot send"))
    }

    fn udp_recv(&mut self, sock: Conn, buf: &mut [u8]) -> Result<(usize, IpAddr), NetError> {
        match self.udp(sock)?.recv_slice(buf) {
            Ok((n, meta)) => Ok((n, to_std(meta.endpoint.addr))),
            Err(_) => Err(NetError::new(NetKind::WouldBlock, "nothing yet")),
        }
    }

    fn read_file(&mut self, name: &str) -> Option<Vec<u8>> {
        self.store.read(name)
    }

    fn write_file(&mut self, name: &str, data: &[u8], _private: bool) -> Result<(), alloc::string::String> {
        self.store.write(name, data);
        Ok(())
    }

    fn log(&mut self, line: &str) {
        esp_println::println!("{line}");
    }
}
