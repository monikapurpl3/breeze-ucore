//! The loop: accept clients, read their requests, hand unit work to the units,
//! answer when the units do. Called over and over by the platform; never waits.

use crate::api::{self, ControlRequest, Envelope, Guard, Resolved, Route, UnitOffline, UnitState, UnitView};
use crate::auth;
use crate::http::{self, Parsed, Response};
use crate::platform::{Conn, NetKind, Platform};
use crate::proto::discover::{self, Discovered};
use crate::store::{AppConfig, DevicesDoc, UnitConfig};
use crate::unit::{Outcome, ReqId, Timing, Unit};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr};

/// How long adding a unit listens for it to answer the hello, as Breeze Core.
const PROBE_LISTEN_MS: u64 = 3_000;

/// Clients served at once; the rest wait in the platform's backlog.
pub const MAX_CLIENTS: usize = 4;
/// A client that sends nothing for this long is dropped.
const CLIENT_IDLE_MS: u64 = 30_000;

/// Breeze Core's settings that μcore keeps, with Breeze Core's defaults.
#[derive(Debug, Clone)]
pub struct Settings {
    pub timing: Timing,
    /// `AC_MIN_AUTH_VERSION`: devices below it get 426.
    pub min_auth_version: u8,
    /// `AC_BEHIND_PROXY`: trust `X-Forwarded-For` for the LAN rule.
    pub behind_proxy: bool,
    /// `AC_CODE_TTL`, seconds a pairing code lives.
    pub code_ttl: u64,
    /// `AC_TOKEN_TTL_DAYS`; 0 or less never expires.
    pub token_ttl_days: i64,
    /// `AC_AUTH_SKEW_SECONDS`: how far a signed request's clock may be off.
    pub auth_skew: u64,
    /// The build's commit, for `/api/version`.
    pub commit: &'static str,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            timing: Timing::default(),
            min_auth_version: 1,
            behind_proxy: false,
            code_ttl: 60,
            token_ttl_days: 3650,
            auth_skew: 60,
            commit: option_env!("UCORE_COMMIT").unwrap_or("unknown"),
        }
    }
}

/// What a client is waiting for. Units by id, so removing one never
/// redirects an answer to another.
enum Pending {
    State { unit: u64, req: ReqId },
    Caps { unit: u64, req: ReqId },
    Control { unit: u64, req: ReqId, request: ControlRequest },
    Batch { parts: Vec<(u64, ReqId, Option<Outcome>)> },
    /// Adding a unit: the hello is out, replies are being collected.
    Probe(Box<Probe>),
}

struct Probe {
    sock: Conn,
    until: u64,
    ip: Ipv4Addr,
    request: api::AddUnitRequest,
    found: BTreeMap<u64, Discovered>,
}

impl Pending {
    fn unit(&self, id: u64) -> bool {
        match self {
            Pending::State { unit, .. } | Pending::Caps { unit, .. } | Pending::Control { unit, .. } => *unit == id,
            Pending::Batch { parts } => parts.iter().any(|(u, _, _)| *u == id),
            Pending::Probe(_) => false,
        }
    }
}

enum Phase {
    Reading,
    Waiting(Pending),
    Writing { out: Vec<u8>, at: usize },
}

struct Client {
    conn: Conn,
    peer: IpAddr,
    buf: Vec<u8>,
    since: u64,
    phase: Phase,
}

pub struct Core {
    settings: Settings,
    config: AppConfig,
    devices: DevicesDoc,
    units: Vec<Unit>,
    clients: Vec<Client>,
    next_req: ReqId,
    enrollment: auth::Enrollment,
    nonces: auth::Nonces,
}

impl Core {
    /// Load `config.json` and `devices.json`. A file that will not parse is
    /// logged and treated as empty: on a chip, a server that answers 401 is
    /// easier to put right than one that restarts forever.
    pub fn new(p: &mut impl Platform, settings: Settings) -> Self {
        fn load<T: serde::de::DeserializeOwned + Default>(p: &mut impl Platform, name: &str) -> T {
            let bytes = p.read_file(name).filter(|b| !b.iter().all(u8::is_ascii_whitespace));
            match bytes.map(|b| serde_json::from_slice(&b)) {
                Some(Ok(v)) => v,
                Some(Err(e)) => {
                    p.log(&alloc::format!("{name}: {e}; starting without it"));
                    T::default()
                }
                None => T::default(),
            }
        }
        let config: AppConfig = load(p, "config.json");
        let devices: DevicesDoc = load(p, "devices.json");
        let units = config.units.iter().map(|u| unit_from(u, settings.timing)).collect();
        let enrollment = auth::Enrollment::new(settings.code_ttl, settings.token_ttl_days);
        let nonces = auth::Nonces::new(settings.auth_skew);
        Self { settings, config, devices, units, clients: Vec::new(), next_req: 1, enrollment, nonces }
    }

    fn req(&mut self) -> ReqId {
        let r = self.next_req;
        self.next_req = self.next_req.wrapping_add(1);
        r
    }

    /// One turn. Returns how long the platform may sleep before the next.
    pub fn poll(&mut self, p: &mut impl Platform) -> u64 {
        let now = p.now_ms();
        while self.clients.len() < MAX_CLIENTS {
            let Some((conn, peer)) = p.accept() else { break };
            self.clients.push(Client { conn, peer, buf: Vec::new(), since: now, phase: Phase::Reading });
        }
        for i in 0..self.clients.len() {
            if matches!(self.clients[i].phase, Phase::Reading) {
                self.read_client(i, p, now);
            }
        }
        for u in 0..self.units.len() {
            for (req, outcome) in self.units[u].poll(p, now) {
                self.deliver(req, outcome);
            }
        }
        for i in 0..self.clients.len() {
            if matches!(self.clients[i].phase, Phase::Waiting(Pending::Probe(_))) {
                self.probe(i, p, now);
            }
        }
        self.write_clients(p, now);
        let busy = self.units.iter().any(Unit::busy) || !self.clients.is_empty();
        let next = self.units.iter().filter_map(Unit::deadline).min();
        match next {
            Some(t) => t.saturating_sub(now).min(if busy { 5 } else { 50 }),
            None if busy => 5,
            None => 50,
        }
    }

    fn read_client(&mut self, i: usize, p: &mut impl Platform, now: u64) {
        let mut chunk = [0u8; 512];
        loop {
            match p.recv(self.clients[i].conn, &mut chunk) {
                Ok(0) => {
                    self.clients[i].phase = Phase::Writing { out: Vec::new(), at: 0 };
                    return;
                }
                Ok(n) => {
                    let c = &mut self.clients[i];
                    c.buf.extend_from_slice(&chunk[..n]);
                    c.since = now;
                    if c.buf.len() > http::MAX_HEAD + http::MAX_BODY + 4 {
                        break;
                    }
                }
                Err(e) if e.kind == NetKind::WouldBlock => break,
                Err(_) => {
                    self.clients[i].phase = Phase::Writing { out: Vec::new(), at: 0 };
                    return;
                }
            }
        }
        match http::parse(&self.clients[i].buf) {
            Parsed::Incomplete => {
                if now.saturating_sub(self.clients[i].since) > CLIENT_IDLE_MS {
                    self.clients[i].phase = Phase::Writing { out: Vec::new(), at: 0 };
                }
            }
            Parsed::Bad(status, message) => self.answer(i, Response::detail(status, message)),
            Parsed::Done(request) => {
                let peer = self.clients[i].peer;
                match self.dispatch(&request, peer, p) {
                    Ok(response) => self.answer(i, response),
                    Err(pending) => self.clients[i].phase = Phase::Waiting(pending),
                }
            }
        }
    }

    fn answer(&mut self, i: usize, response: Response) {
        self.clients[i].phase = Phase::Writing { out: response.to_bytes(), at: 0 };
    }

    /// A response now, or the unit work it is waiting for.
    fn dispatch(&mut self, r: &http::Request, peer: IpAddr, p: &mut impl Platform) -> Result<Response, Pending> {
        let (guard, route) = match api::resolve(&r.method, &r.path) {
            Resolved::Route(g, route) => (g, route),
            Resolved::MethodNotAllowed(allow) => {
                return Ok(Response::detail(405, "Method Not Allowed").with_header("Allow", allow))
            }
            Resolved::NotFound => return Ok(Response::detail(404, "Not Found")),
        };
        // The API key first, always: the cheap check, and the one that keeps
        // strangers away from the device store.
        if guard != Guard::Open {
            if let Err(rejection) = auth::check_api_key(r.header("x-api-key"), self.config.api_key.as_deref()) {
                return Ok(rejection);
            }
        }
        let now = p.unix_ms().map_or(0.0, |ms| ms as f64 / 1000.0);
        if guard == Guard::AdminLan {
            let ip = auth::client_ip(Some(peer), r.header("x-forwarded-for"), self.settings.behind_proxy);
            if !auth::is_private_ip(ip) {
                // 403, not 401: the credential was fine, the place was not.
                return Ok(Response::detail(403, "this admin action must come from the local network"));
            }
        }
        let mut device: Option<String> = None;
        if guard == Guard::Full {
            let presented = auth::Presented {
                bearer: r.header("authorization").and_then(auth::bearer),
                auth_version: r.header("x-breeze-auth-version"),
                key_id: r.header("x-breeze-key-id"),
                timestamp: r.header("x-breeze-timestamp"),
                nonce: r.header("x-breeze-nonce"),
                signature: r.header("x-breeze-signature"),
                method: &r.method,
                path: &r.target,
                body: &r.body,
            };
            match auth::verify_device(&self.devices, &presented, &mut self.nonces, now, self.settings.min_auth_version) {
                auth::Decision::Allow(token_id) => {
                    // In memory only, as Breeze Core: a store write on every
                    // request would turn each read of a thermostat into one.
                    if let Some(d) = self.devices.devices.iter_mut().find(|d| d.token_id == token_id) {
                        d.last_used = Some(now);
                    }
                    device = Some(token_id);
                }
                auth::Decision::Reject(rejection) => return Ok(rejection),
            }
        }
        if let Some(response) = self.auth_route(route, r, device.as_deref(), now, p) {
            return Ok(response);
        }
        // As Breeze Core reads it for unit routes: the segment as a number.
        let unit_of = |units: &[Unit]| {
            let id: u64 = api::unit_segment(&r.path).parse().ok()?;
            units.iter().position(|u| u.id == id)
        };
        let now_mono = p.now_ms();
        if let Some(answer) = self.config_route(route, r, p, now_mono) {
            return answer;
        }
        match route {
            Route::Health => Ok(Response::json(200, &Health { status: "ok" })),
            Route::Version => Ok(Response::json(
                200,
                &api::Version {
                    name: "Breeze μcore",
                    version: crate::wire_version(),
                    commit: self.settings.commit,
                    features: api::FEATURES,
                    auth_versions: api::AUTH_VERSIONS,
                    min_auth_version: self.settings.min_auth_version,
                    units: self.units.len(),
                },
            )),
            Route::Units => {
                let list: Vec<_> = self
                    .units
                    .iter()
                    .map(|u| api::UnitSummary { id: alloc::string::ToString::to_string(&u.id), name: &u.name, ip: &u.ip })
                    .collect();
                Ok(Response::json(200, &list))
            }
            Route::AllStates => {
                let mut parts = Vec::with_capacity(self.units.len());
                for u in 0..self.units.len() {
                    let req = self.req();
                    self.units[u].read(req);
                    parts.push((self.units[u].id, req, None));
                }
                Err(Pending::Batch { parts })
            }
            Route::UnitState => {
                let Some(u) = unit_of(&self.units) else { return Ok(api::unknown_unit(&r.path)) };
                let req = self.req();
                self.units[u].read(req);
                Err(Pending::State { unit: self.units[u].id, req })
            }
            Route::Capabilities => {
                let Some(u) = unit_of(&self.units) else { return Ok(api::unknown_unit(&r.path)) };
                let req = self.req();
                match self.units[u].capabilities(req) {
                    Some(caps) => Ok(Response::json(200, &caps.view(self.units[u].id.to_string()))),
                    None => Err(Pending::Caps { unit: self.units[u].id, req }),
                }
            }
            Route::Control => {
                let Some(unit) = unit_of(&self.units) else { return Ok(api::unknown_unit(&r.path)) };
                let request: ControlRequest = match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(e) => return Ok(Response::detail(422, &alloc::format!("invalid control request: {e}"))),
                };
                if let Err((status, message)) = request.validate() {
                    return Ok(Response::detail(status, &message));
                }
                let req = self.req();
                self.units[unit].control(req, request.change());
                Err(Pending::Control { unit: self.units[unit].id, req, request })
            }
            // Answered by auth_route above.
            _ => Ok(Response::detail(404, "Not Found")),
        }
    }

    fn save_config(&self, p: &mut impl Platform) -> Result<(), Response> {
        p.write_file("config.json", &crate::store::to_json(&self.config), false)
            .map_err(|e| Response::detail(500, &alloc::format!("cannot write the config: {e}")))
    }

    /// The configuration and the unit list: `None` for any other route.
    fn config_route(&mut self, route: Route, r: &http::Request, p: &mut impl Platform, now: u64) -> Option<Result<Response, Pending>> {
        let segment = api::unit_segment(&r.path);
        Some(Ok(match route {
            Route::Config => {
                let view = api::ConfigView { units: self.config.units.iter().map(api::ConfigUnit::from).collect() };
                Response::json(200, &view)
            }
            Route::Rename => {
                let request: api::RenameRequest = match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(e) => return Some(Ok(Response::detail(422, &alloc::format!("invalid rename request: {e}")))),
                };
                let name = request.name.trim().to_string();
                let chars = name.chars().count();
                if chars == 0 {
                    return Some(Ok(Response::detail(422, "name must not be empty")));
                }
                if chars > api::NAME_MAX_CHARS {
                    let m = alloc::format!("name must be at most {} characters, got {chars}", api::NAME_MAX_CHARS);
                    return Some(Ok(Response::detail(422, &m)));
                }
                let Some(unit) = self.config.find_unit_mut(segment) else {
                    return Some(Ok(Response::detail(404, &alloc::format!("Unknown unit '{segment}'"))));
                };
                let previous = core::mem::replace(&mut unit.name, name.clone());
                if let Err(e) = self.save_config(p) {
                    if let Some(unit) = self.config.find_unit_mut(segment) {
                        unit.name = previous;
                    }
                    return Some(Ok(e));
                }
                if let Some(u) = self.units.iter_mut().find(|u| u.id.to_string() == segment) {
                    u.name = name;
                }
                Response::json(200, &api::ConfigUnit::from(self.config.find_unit(segment).expect("just renamed")))
            }
            Route::DeleteUnit => {
                if self.config.find_unit(segment).is_none() {
                    return Some(Ok(Response::detail(404, &alloc::format!("Unknown unit '{segment}'"))));
                }
                let previous = self.config.units.clone();
                self.config.remove_unit(segment);
                if let Err(e) = self.save_config(p) {
                    self.config.units = previous;
                    return Some(Ok(e));
                }
                if let Some(i) = self.units.iter().position(|u| u.id.to_string() == segment) {
                    let id = self.units[i].id;
                    self.units.remove(i).shutdown(p);
                    self.abandon(id);
                }
                Response { status: 204, body: Vec::new(), extra: Vec::new() }
            }
            Route::AddUnit => {
                let request: api::AddUnitRequest = match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(e) => return Some(Ok(Response::detail(422, &alloc::format!("invalid add request: {e}")))),
                };
                let Ok(ip) = request.ip.trim().parse::<Ipv4Addr>() else {
                    return Some(Ok(Response::detail(422, &alloc::format!("'{}' is not an IPv4 address", request.ip))));
                };
                if request.name.as_ref().is_some_and(|n| n.chars().count() > api::NAME_MAX_CHARS) {
                    let m = alloc::format!("name must be at most {} characters", api::NAME_MAX_CHARS);
                    return Some(Ok(Response::detail(422, &m)));
                }
                let hex = |v: &str, n: usize| v.len() == n && v.bytes().all(|b| b.is_ascii_hexdigit());
                if request.token.as_deref().is_some_and(|t| !hex(t, 128)) {
                    return Some(Ok(Response::detail(422, "token must be 128 hex characters")));
                }
                if request.key.as_deref().is_some_and(|k| !hex(k, 64)) {
                    return Some(Ok(Response::detail(422, "key must be 64 hex characters")));
                }
                if request.token.is_some() != request.key.is_some() {
                    return Some(Ok(Response::detail(422, "token and key must be given together")));
                }
                // Breeze Core probes only private addresses.
                if !ip.is_private() {
                    return Some(Ok(Response::detail(503, "discovery failed: refusing to scan a non-private subnet")));
                }
                let sock = match p.udp_open() {
                    Ok(s) => s,
                    Err(e) => return Some(Ok(Response::detail(503, &alloc::format!("discovery failed: {}", e.message)))),
                };
                for port in discover::PORTS {
                    let _ = p.udp_send(sock, IpAddr::V4(ip), port, &discover::HELLO);
                }
                let probe = Probe { sock, until: now + PROBE_LISTEN_MS, ip, request, found: BTreeMap::new() };
                return Some(Err(Pending::Probe(Box::new(probe))));
            }
            _ => return None,
        }))
    }

    /// Collect hello replies; when the time is up, add what answered.
    fn probe(&mut self, i: usize, p: &mut impl Platform, now: u64) {
        let Phase::Waiting(Pending::Probe(probe)) = &mut self.clients[i].phase else { return };
        let mut buf = [0u8; 512];
        while let Ok((n, from)) = p.udp_recv(probe.sock, &mut buf) {
            if let IpAddr::V4(src) = from {
                if let Ok(d) = discover::parse(src, &buf[..n]) {
                    probe.found.entry(d.id).or_insert(d);
                }
            }
        }
        if now < probe.until {
            return;
        }
        let Phase::Waiting(Pending::Probe(probe)) = core::mem::replace(&mut self.clients[i].phase, Phase::Reading) else {
            return;
        };
        let Probe { sock, ip, mut request, found, .. } = *probe;
        p.close(sock);
        let response = self.add_found(ip, &mut request, found.into_values().next(), p);
        self.answer(i, response);
    }

    fn add_found(&mut self, ip: Ipv4Addr, request: &mut api::AddUnitRequest, found: Option<Discovered>, p: &mut impl Platform) -> Response {
        let Some(found) = found else {
            return Response::detail(404, &alloc::format!("no Midea unit answered at {ip}"));
        };
        let Ok(id) = i64::try_from(found.id) else {
            return Response::detail(503, "that unit reported an unusable id");
        };
        // No cloud here: credentials come with the request or not at all.
        if request.token.is_none() && request.midea_account.is_some() && request.midea_password.is_some() {
            return Response::detail(501, "this build has no cloud support; add the unit with a token and key instead");
        }
        let name = request
            .name
            .take()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .or_else(|| self.config.find_unit(&id.to_string()).map(|u| u.name.clone()))
            .unwrap_or_else(|| alloc::format!("AC {}", found.ip));
        let previous = self.config.units.clone();
        self.config.add_or_update_unit(UnitConfig {
            name,
            ip: found.ip.to_string(),
            port: found.port,
            id,
            token: request.token.take(),
            key: request.key.take(),
        });
        if let Err(e) = self.save_config(p) {
            self.config.units = previous;
            return e;
        }
        let stored = self.config.find_unit(&id.to_string()).expect("just added").clone();
        let unit = unit_from(&stored, self.settings.timing);
        match self.units.iter().position(|u| u.id == unit.id) {
            Some(i) => {
                let old = core::mem::replace(&mut self.units[i], unit);
                old.shutdown(p);
                self.abandon(stored.id as u64);
            }
            None => self.units.push(unit),
        }
        Response::json(201, &api::ConfigUnit::from(&stored))
    }

    /// Answer everyone still waiting on a unit that has just been removed or
    /// replaced: its work will not finish.
    fn abandon(&mut self, id: u64) {
        for c in self.clients.iter_mut() {
            if matches!(&c.phase, Phase::Waiting(pending) if pending.unit(id)) {
                let r = Response::detail(503, "the unit was removed or replaced while this request waited");
                c.phase = Phase::Writing { out: r.to_bytes(), at: 0 };
            }
        }
    }

    fn save_devices(&self, p: &mut impl Platform) -> Result<(), Response> {
        p.write_file("devices.json", &crate::store::to_json(&self.devices), true)
            .map_err(|e| Response::detail(500, &alloc::format!("cannot write the device store: {e}")))
    }

    /// Pairing and the paired: `None` for any other route.
    fn auth_route(&mut self, route: Route, r: &http::Request, device: Option<&str>, now: f64, p: &mut impl Platform) -> Option<Response> {
        Some(match route {
            Route::EnrollStart => {
                let request: api::StartRequest = match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(_) if r.body.is_empty() => api::StartRequest { label: String::new(), auth_version: 1, public_key: None },
                    Err(e) => return Some(Response::detail(422, &alloc::format!("invalid enrolment request: {e}"))),
                };
                match self.enrollment.start(p, &request.label, request.auth_version, request.public_key.as_deref(), now) {
                    Ok(s) => Response::json(
                        200,
                        &api::Started { session_id: &s.session_id, user_code: &s.user_code, expires_in: s.expires_in },
                    ),
                    Err(e @ auth::EnrollError::Full) => Response::detail(429, &alloc::string::ToString::to_string(&e)),
                    Err(e) => Response::detail(422, &alloc::string::ToString::to_string(&e)),
                }
            }
            Route::EnrollApprove => {
                let request: api::ApproveRequest = match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(e) => return Some(Response::detail(422, &alloc::format!("invalid approval request: {e}"))),
                };
                let Some(record) = self.enrollment.approve(p, &request.code, now) else {
                    return Some(Response::detail(404, "no pending enrolment matches that code"));
                };
                let body = Response::json(200, &api::Approved { token_id: &record.token_id, label: &record.label });
                self.devices.devices.push(record);
                match self.save_devices(p) {
                    Ok(()) => body,
                    Err(e) => e,
                }
            }
            Route::EnrollPoll => {
                let request: api::PollRequest = match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(e) => return Some(Response::detail(422, &alloc::format!("invalid poll request: {e}"))),
                };
                match self.enrollment.poll(&request.session_id, now) {
                    auth::Poll::Approved(rec, token) => Response::json(
                        200,
                        &api::Collected {
                            status: "approved",
                            token_id: &rec.token_id,
                            label: &rec.label,
                            auth_version: rec.auth_version,
                            expires_at: rec.expires_at,
                            device_token: token.as_deref(),
                        },
                    ),
                    auth::Poll::Pending => Response::json(200, &api::Status { status: "pending" }),
                    auth::Poll::Unknown => Response::json(200, &api::Status { status: "unknown" }),
                }
            }
            Route::Devices => {
                let list: Vec<api::DeviceView> = self.devices.devices.iter().map(api::DeviceView::from).collect();
                Response::json(200, &list)
            }
            Route::RevokeDevice => {
                let token_id = r.path.strip_prefix("/api/auth/devices/").unwrap_or_default();
                let before = self.devices.devices.len();
                self.devices.devices.retain(|d| d.token_id != token_id);
                if self.devices.devices.len() == before {
                    return Some(Response::detail(404, "no such device"));
                }
                match self.save_devices(p) {
                    Ok(()) => Response { status: 204, body: Vec::new(), extra: Vec::new() },
                    Err(e) => e,
                }
            }
            Route::Whoami => match self.devices.devices.iter().find(|d| Some(d.token_id.as_str()) == device) {
                Some(d) => Response::json(200, &api::DeviceView::from(d)),
                None => Response::detail(404, "device not found"),
            },
            Route::Upgrade => {
                let Some(token_id) = device else { return Some(Response::detail(401, "no authenticated device to upgrade")) };
                let request: api::UpgradeRequest = match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(e) => return Some(Response::detail(422, &alloc::format!("invalid upgrade request: {e}"))),
                };
                if request.public_key.len() > 128 || !auth::public_key_is_valid(&request.public_key) {
                    return Some(Response::detail(400, "invalid Ed25519 public_key"));
                }
                let Some(i) = self.devices.devices.iter().position(|d| d.token_id == token_id) else {
                    return Some(Response::detail(404, "device not found"));
                };
                let previous = self.devices.devices[i].clone();
                let d = &mut self.devices.devices[i];
                d.auth_version = 2;
                d.public_key = Some(request.public_key);
                d.token_hash = None;
                match self.save_devices(p) {
                    Ok(()) => Response::json(200, &api::Upgraded { token_id, auth_version: 2 }),
                    Err(e) => {
                        // Nothing changes unless the store does.
                        self.devices.devices[i] = previous;
                        e
                    }
                }
            }
            _ => return None,
        })
    }

    /// A unit finished something a client is waiting for.
    fn deliver(&mut self, req: ReqId, outcome: Outcome) {
        let Some(i) = self.clients.iter().position(|c| match &c.phase {
            Phase::Waiting(Pending::State { req: r, .. } | Pending::Caps { req: r, .. } | Pending::Control { req: r, .. }) => {
                *r == req
            }
            Phase::Waiting(Pending::Batch { parts }) => parts.iter().any(|(_, r, _)| *r == req),
            _ => false,
        }) else {
            return;
        };
        let Phase::Waiting(pending) = &mut self.clients[i].phase else { return };
        let units = &self.units;
        let find = |id: u64| units.iter().find(|u| u.id == id);
        let response = match (pending, outcome) {
            (Pending::State { unit, .. }, Outcome::State(result)) => {
                let Some(u) = find(*unit) else { return };
                match result {
                    Ok(s) => Response::json(200, &UnitState::of(u, &s)),
                    // An unreachable unit is a fact about the unit, not a failed
                    // request: an offline record, not an error status.
                    Err(e) => Response::json(200, &UnitOffline::of(u, &e)),
                }
            }
            (Pending::Caps { unit, .. }, Outcome::Caps(result)) => match result {
                Ok(caps) => Response::json(200, &caps.view(unit.to_string())),
                Err(e) => Response::detail(503, &e.to_string()),
            },
            (Pending::Control { unit, request, .. }, Outcome::Control(result)) => match (result, find(*unit)) {
                (Ok((sent, echoed)), Some(u)) => Response::json(
                    200,
                    &api::ControlView { state: UnitState::of(u, &echoed), not_applied: request.not_applied(&sent, &echoed) },
                ),
                (Ok(_), None) => return,
                (Err(e), _) => Response::detail(503, &e.to_string()),
            },
            (Pending::Batch { parts }, outcome) => {
                if let Some(part) = parts.iter_mut().find(|(_, r, _)| *r == req) {
                    part.2 = Some(outcome);
                }
                if parts.iter().any(|(_, _, o)| o.is_none()) {
                    return;
                }
                let mut env = Envelope { states: Vec::new(), errors: Vec::new() };
                for (unit, _, outcome) in parts.iter() {
                    let Some(u) = find(*unit) else { continue };
                    match outcome {
                        Some(Outcome::State(Ok(s))) => env.states.push(UnitView::Online(UnitState::of(u, s))),
                        Some(Outcome::State(Err(e))) => env.errors.push(UnitView::Offline(UnitOffline::of(u, e))),
                        _ => {}
                    }
                }
                Response::json(200, &env)
            }
            _ => return,
        };
        self.clients[i].phase = Phase::Writing { out: response.to_bytes(), at: 0 };
    }

    fn write_clients(&mut self, p: &mut impl Platform, now: u64) {
        let mut i = 0;
        while i < self.clients.len() {
            let c = &mut self.clients[i];
            let done = match &mut c.phase {
                Phase::Writing { out, at } => {
                    loop {
                        if *at >= out.len() {
                            break true;
                        }
                        match p.send(c.conn, &out[*at..]) {
                            Ok(0) => break false,
                            Ok(n) => {
                                *at += n;
                                c.since = now;
                            }
                            Err(e) if e.kind == NetKind::WouldBlock => break false,
                            Err(_) => break true,
                        }
                    }
                }
                _ => false,
            } || now.saturating_sub(c.since) > CLIENT_IDLE_MS && !matches!(c.phase, Phase::Waiting(_));
            if done {
                p.close(c.conn);
                self.clients.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}

fn unit_from(u: &UnitConfig, timing: Timing) -> Unit {
    Unit::new(u.id as u64, u.name.clone(), u.ip.clone(), u.port, u.token.as_deref(), u.key.as_deref(), timing)
}

#[derive(serde::Serialize)]
struct Health {
    status: &'static str,
}

/// For the log line at start.
pub fn banner(core: &Core) -> String {
    alloc::format!("Breeze μcore {} (API {}), {} unit(s)", crate::product_version(), crate::NATIVE, core.units.len())
}
