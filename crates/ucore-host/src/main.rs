//! Breeze μCore on an ordinary computer: for developing it, for the
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
    /// The store file being written, between file_begin and file_end.
    pending: Option<Pending>,
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

    fn local_offset(&mut self, unix_s: i64) -> Option<i32> {
        // The system's zone, read as Breeze Core reads it (chrono's Local).
        use chrono::{Offset, TimeZone};
        chrono::Local.timestamp_opt(unix_s, 0).single().map(|t| t.offset().fix().local_minus_utc())
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

    fn file_begin(&mut self, name: &str, private: bool) -> Result<(), String> {
        if let Some(old) = self.pending.take() {
            old.abandon();
        }
        self.pending = Some(Pending::begin(&self.dir, name, private)?);
        Ok(())
    }

    fn file_write(&mut self, data: &[u8]) -> Result<(), String> {
        self.pending.as_mut().ok_or("no store file begun")?.write(data)
    }

    fn file_end(&mut self, keep: bool) -> Result<(), String> {
        let pending = self.pending.take().ok_or("no store file begun")?;
        if keep {
            pending.finish()
        } else {
            pending.abandon();
            Ok(())
        }
    }

    fn log(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

/// A store file being written, Breeze Core's way: a temporary file beside
/// it, then its mode, its owner, and a rename over the old one. config.json
/// is 640 (the admin group reads it), the rest 600.
struct Pending {
    path: PathBuf,
    tmp: PathBuf,
    private: bool,
    file: std::io::BufWriter<std::fs::File>,
}

fn at(p: &std::path::Path, e: std::io::Error) -> String {
    format!("{}: {e}", p.display())
}

impl Pending {
    fn begin(dir: &std::path::Path, name: &str, private: bool) -> Result<Self, String> {
        let path = dir.join(name);
        let tmp = path.with_extension("tmp");
        std::fs::create_dir_all(dir).map_err(|e| at(dir, e))?;
        let file = std::fs::File::create(&tmp).map_err(|e| at(&tmp, e))?;
        Ok(Self { path, tmp, private, file: std::io::BufWriter::new(file) })
    }

    fn write(&mut self, data: &[u8]) -> Result<(), String> {
        self.file.write_all(data).map_err(|e| at(&self.tmp, e))
    }

    fn finish(self) -> Result<(), String> {
        let Self { path, tmp, private, file } = self;
        let flushed = file.into_inner().map_err(|e| at(&tmp, e.into_error())).and_then(|f| f.sync_all().map_err(|e| at(&tmp, e)));
        if let Err(e) = flushed {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if private { 0o600 } else { 0o640 };
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| at(&tmp, e))?;
            if let Some(owner) = owner::of(&path) {
                if let Err(e) = owner::give(owner, &tmp, &path) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            }
        }
        #[cfg(not(unix))]
        let _ = private;
        std::fs::rename(&tmp, &path).map_err(|e| at(&path, e))
    }

    fn abandon(self) {
        let Self { tmp, file, .. } = self;
        drop(file);
        let _ = std::fs::remove_file(&tmp);
    }
}

/// A whole store file at once, through [`Pending`].
#[cfg(all(test, unix))]
fn write_store(dir: &std::path::Path, name: &str, data: &[u8], private: bool) -> Result<(), String> {
    let mut pending = Pending::begin(dir, name, private)?;
    match pending.write(data) {
        Ok(()) => pending.finish(),
        Err(e) => {
            pending.abandon();
            Err(e)
        }
    }
}

/// Who a store file belongs to, as Breeze Core 4.4.0's store keeps it: a
/// renamed-in file is the writer's, so a μCore run as root (or a tool run
/// with sudo beside it) would leave config.json root:root for a service that
/// runs as its own user. A replaced file keeps its owner and group, a new one
/// gets its directory's; root that cannot hand it over keeps the old file.
#[cfg(unix)]
mod owner {
    use std::os::unix::fs::{chown, MetadataExt};
    use std::path::Path;

    pub fn of(path: &Path) -> Option<(u32, u32)> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
                std::fs::metadata(dir).ok()?
            }
            Err(_) => return None,
        };
        Some((meta.uid(), meta.gid()))
    }

    pub fn give((uid, gid): (u32, u32), tmp: &Path, path: &Path) -> Result<(), String> {
        let written = std::fs::metadata(tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        if (written.uid(), written.gid()) == (uid, gid) {
            return Ok(());
        }
        match chown(tmp, Some(uid), Some(gid)) {
            Ok(()) => Ok(()),
            Err(e) if written.uid() == 0 => Err(format!(
                "{} was left as it was: the new copy could not be given to its owner, {uid}:{gid} ({e})",
                path.display()
            )),
            Err(_) => {
                let _ = chown(tmp, None, Some(gid));
                Ok(())
            }
        }
    }
}

/// Breeze Core's environment variables for what μCore keeps.
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
        timer_tick: var("AC_TIMER_TICK").unwrap_or(d.timer_tick),
        sched_tick: var("AC_SCHED_TICK").unwrap_or(d.sched_tick),
        stream_tick: var("AC_STREAM_TICK").unwrap_or(d.stream_tick),
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
    let mut platform = Host { start: Instant::now(), listener, next: 0, conns: HashMap::new(), dir, pending: None };
    let mut core = ucore::Core::new(&mut platform, settings());
    eprintln!("{} listening on {host}:{port}", ucore::banner(&core));
    loop {
        let wait = core.poll(&mut platform);
        std::thread::sleep(Duration::from_millis(wait.clamp(1, 50)));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::write_store;
    use std::os::unix::fs::{chown, MetadataExt};

    /// Breeze Core 4.4.0's store test, for μCore's: a replaced file keeps
    /// its owner, a new one gets its directory's, and the mode is the store's.
    /// Only root can give a file away, so as anyone else this proves nothing
    /// and returns early; it runs as root in a container.
    #[test]
    fn a_replaced_file_keeps_its_owner_and_a_new_one_gets_its_directorys() {
        let dir = std::env::temp_dir().join(format!("ucore-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if chown(&dir, Some(1842), Some(1842)).is_err() {
            eprintln!("not root: nothing to show");
            return;
        }
        let meta = |name: &str| {
            let m = std::fs::metadata(dir.join(name)).unwrap();
            (m.uid(), m.gid(), m.mode() & 0o777)
        };
        write_store(&dir, "config.json", b"{}", false).unwrap();
        assert_eq!(meta("config.json"), (1842, 1842, 0o640), "a new file: its directory's owner");
        chown(dir.join("config.json"), Some(1843), Some(1844)).unwrap();
        write_store(&dir, "config.json", b"{\"a\":1}", false).unwrap();
        assert_eq!(meta("config.json"), (1843, 1844, 0o640), "a replaced file keeps its owner");
        write_store(&dir, "devices.json", b"{}", true).unwrap();
        assert_eq!(meta("devices.json"), (1842, 1842, 0o600));
        assert!(!dir.join("config.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
