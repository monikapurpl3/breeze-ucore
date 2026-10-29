//! Breeze μcore on an ordinary computer: for developing it, for the
//! differential test against Breeze Core, and as a very small build for a
//! router. The same environment as Breeze Core: `AC_CONFIG_DIR` holds
//! `config.json` and `devices.json`; `BREEZE_HOST` and `BREEZE_PORT` say
//! where to listen.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ucore::platform::{Conn, NetError, NetKind, Platform};

enum Sock {
    /// A blocking connect runs on its own thread, so this one never waits.
    Connecting(Receiver<std::io::Result<TcpStream>>),
    Open(TcpStream),
    Udp(UdpSocket),
}

struct Host {
    start: Instant,
    listener: TcpListener,
    next: Conn,
    conns: HashMap<Conn, Sock>,
    dir: PathBuf,
}

fn net_error(e: std::io::Error) -> NetError {
    let kind = match e.kind() {
        ErrorKind::WouldBlock => NetKind::WouldBlock,
        ErrorKind::TimedOut => NetKind::TimedOut,
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => NetKind::Reset,
        ErrorKind::UnexpectedEof => NetKind::Closed,
        ErrorKind::ConnectionRefused => NetKind::Refused,
        _ => NetKind::Other,
    };
    NetError::new(kind, e.to_string())
}

fn closed() -> NetError {
    NetError::new(NetKind::Closed, "connection closed")
}

impl Host {
    fn id(&mut self, sock: Sock) -> Conn {
        self.next = self.next.wrapping_add(1);
        self.conns.insert(self.next, sock);
        self.next
    }

    fn stream(&mut self, conn: Conn) -> Result<&mut TcpStream, NetError> {
        match self.conns.get_mut(&conn) {
            Some(Sock::Open(s)) => Ok(s),
            Some(Sock::Connecting(_)) => Err(NetError::new(NetKind::WouldBlock, "still connecting")),
            Some(Sock::Udp(_)) | None => Err(closed()),
        }
    }
}

impl Platform for Host {
    fn now_ms(&mut self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn unix_ms(&mut self) -> Option<u64> {
        SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as u64)
    }

    fn random(&mut self, buf: &mut [u8]) {
        getrandom::getrandom(buf).expect("the system's random source");
    }

    fn connect(&mut self, ip: IpAddr, port: u16) -> Result<Conn, NetError> {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(5)));
        });
        Ok(self.id(Sock::Connecting(rx)))
    }

    fn is_connected(&mut self, conn: Conn) -> Result<bool, NetError> {
        let result = match self.conns.get(&conn) {
            Some(Sock::Open(_) | Sock::Udp(_)) => return Ok(true),
            Some(Sock::Connecting(rx)) => match rx.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => return Ok(false),
                Err(TryRecvError::Disconnected) => return Err(closed()),
            },
            None => return Err(closed()),
        };
        let stream = result.map_err(net_error)?;
        stream.set_nonblocking(true).map_err(net_error)?;
        // Small packets to a unit that answers one at a time: nothing to batch.
        stream.set_nodelay(true).map_err(net_error)?;
        self.conns.insert(conn, Sock::Open(stream));
        Ok(true)
    }

    fn accept(&mut self) -> Option<(Conn, IpAddr)> {
        let (stream, peer) = self.listener.accept().ok()?;
        stream.set_nonblocking(true).ok()?;
        Some((self.id(Sock::Open(stream)), peer.ip()))
    }

    fn send(&mut self, conn: Conn, data: &[u8]) -> Result<usize, NetError> {
        self.stream(conn)?.write(data).map_err(net_error)
    }

    fn recv(&mut self, conn: Conn, buf: &mut [u8]) -> Result<usize, NetError> {
        self.stream(conn)?.read(buf).map_err(net_error)
    }

    fn close(&mut self, conn: Conn) {
        self.conns.remove(&conn);
    }

    fn udp_open(&mut self) -> Result<Conn, NetError> {
        let s = UdpSocket::bind("0.0.0.0:0").map_err(net_error)?;
        s.set_broadcast(true).map_err(net_error)?;
        s.set_nonblocking(true).map_err(net_error)?;
        Ok(self.id(Sock::Udp(s)))
    }

    fn udp_send(&mut self, sock: Conn, to: IpAddr, port: u16, data: &[u8]) -> Result<(), NetError> {
        match self.conns.get(&sock) {
            Some(Sock::Udp(s)) => s.send_to(data, SocketAddr::new(to, port)).map(|_| ()).map_err(net_error),
            _ => Err(closed()),
        }
    }

    fn udp_recv(&mut self, sock: Conn, buf: &mut [u8]) -> Result<(usize, IpAddr), NetError> {
        match self.conns.get(&sock) {
            Some(Sock::Udp(s)) => s.recv_from(buf).map(|(n, from)| (n, from.ip())).map_err(net_error),
            _ => Err(closed()),
        }
    }

    fn read_file(&mut self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.dir.join(name)).ok()
    }

    /// Breeze Core's way: a temporary file, its mode, then a rename over the
    /// old one. config.json is 640 (the admin group reads it), the rest 600.
    fn write_file(&mut self, name: &str, data: &[u8], private: bool) -> Result<(), String> {
        let path = self.dir.join(name);
        let tmp = path.with_extension("tmp");
        let at = |p: &std::path::Path, e: std::io::Error| format!("{}: {e}", p.display());
        std::fs::create_dir_all(&self.dir).map_err(|e| at(&self.dir, e))?;
        std::fs::write(&tmp, data).map_err(|e| at(&tmp, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if private { 0o600 } else { 0o640 };
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| at(&tmp, e))?;
        }
        #[cfg(not(unix))]
        let _ = private;
        std::fs::rename(&tmp, &path).map_err(|e| at(&path, e))
    }

    fn log(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

/// Breeze Core's environment variables for what μcore keeps.
fn settings() -> ucore::Settings {
    fn var<T: std::str::FromStr>(name: &str) -> Option<T> {
        std::env::var(name).ok().and_then(|v| v.parse().ok())
    }
    let d = ucore::Settings::default();
    ucore::Settings {
        min_auth_version: var("AC_MIN_AUTH_VERSION").unwrap_or(d.min_auth_version),
        behind_proxy: matches!(std::env::var("AC_BEHIND_PROXY").as_deref(), Ok("1" | "true" | "yes")),
        code_ttl: var("AC_CODE_TTL").unwrap_or(d.code_ttl),
        token_ttl_days: var("AC_TOKEN_TTL_DAYS").unwrap_or(d.token_ttl_days),
        auth_skew: var("AC_AUTH_SKEW_SECONDS").unwrap_or(d.auth_skew),
        ..d
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("breeze-ucore {}", ucore::product_version());
        return;
    }
    // Breeze Core's default: this machine only, until told otherwise.
    let host = std::env::var("BREEZE_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("BREEZE_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8420u16);
    let dir = std::env::var_os("AC_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let listener = TcpListener::bind((host.as_str(), port)).unwrap_or_else(|e| {
        eprintln!("cannot listen on {host}:{port}: {e}");
        std::process::exit(1);
    });
    listener.set_nonblocking(true).expect("a non-blocking listener");
    let mut platform = Host { start: Instant::now(), listener, next: 0, conns: HashMap::new(), dir };
    let mut core = ucore::Core::new(&mut platform, settings());
    eprintln!("{} listening on {host}:{port}", ucore::banner(&core));
    loop {
        let wait = core.poll(&mut platform);
        std::thread::sleep(Duration::from_millis(wait.clamp(1, 50)));
    }
}
