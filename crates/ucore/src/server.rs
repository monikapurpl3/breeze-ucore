//! The loop: accept clients, read their requests, hand unit work to the units,
//! answer when the units do. Called over and over by the platform; never waits.

use crate::api::{self, ControlRequest, Envelope, Guard, Resolved, Route, UnitOffline, UnitState, UnitView};
use crate::auth;
use crate::http::{self, Parsed, Response};
use crate::platform::{Conn, NetKind, Platform};
use crate::proto::discover::{self, Discovered};
use crate::store::{AppConfig, DevicesDoc, UnitConfig};
use crate::programs::{self, Program, ProgramSpec, ProgramsDoc};
use crate::timers::{self, Timer, TimerRequest, TimersDoc, When};
use crate::unit::{Outcome, ReqId, Timing, Unit};
use crate::clock;
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

/// Breeze Core's settings that μCore keeps, with Breeze Core's defaults.
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
    /// `AC_TIMER_TICK`: seconds between passes over the timers.
    pub timer_tick: u64,
    /// `AC_SCHED_TICK`: seconds between passes over the programs (5 at least).
    pub sched_tick: u64,
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
            timer_tick: 15,
            sched_tick: 30,
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
    /// A program applied now: one unit after another, as Breeze Core does,
    /// stopping at the first that fails.
    Apply(Box<Apply>),
}

struct Apply {
    current: (u64, ReqId),
    rest: Vec<u64>,
    request: ControlRequest,
    /// The units' answers so far, serialised.
    done: Vec<Vec<u8>>,
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
            Pending::Apply(a) => a.current.0 == id || a.rest.contains(&id),
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
    timers: TimersDoc,
    runner: Runner,
    programs: ProgramsDoc,
    scheduler: Scheduler,
    /// Units' work a timer or a program asked for: nobody waits on it.
    jobs: Vec<Job>,
}

struct Job {
    req: ReqId,
    unit: u64,
    kind: JobKind,
}

enum JobKind {
    Timer,
    Schedule { name: String },
    Curve { program: String, name: String, temp: f64 },
}

/// The scheduler's own bookkeeping, as Breeze Core keeps it.
#[derive(Default)]
struct Scheduler {
    runs: u64,
    last_run: Option<String>,
    next: u64,
    /// (program, entry) -> the minute it last fired: once per minute.
    fired: BTreeMap<(String, usize), String>,
    /// (program, unit) -> the setpoint last applied: a curve moves in half
    /// degrees, and a hand's nudge stands until the curve moves on.
    curve_applied: BTreeMap<(String, String), f64>,
}

/// How the timers are doing, for `/api/timers/status`.
#[derive(Default)]
struct Runner {
    runs: u64,
    fired: u64,
    errors: u64,
    last_run: Option<String>,
    /// When the next pass is due, on the monotonic clock.
    next: u64,
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
        let timers: TimersDoc = load(p, "timers.json");
        let programs: ProgramsDoc = load(p, "programs.json");
        let scheduler = Scheduler { next: p.now_ms() + settings.sched_tick.max(5) * 1000, ..Scheduler::default() };
        // As Breeze Core's runner: it sleeps a tick, then looks.
        let runner = Runner { next: p.now_ms() + settings.timer_tick.max(1) * 1000, ..Runner::default() };
        let units = config.units.iter().map(|u| unit_from(u, settings.timing)).collect();
        let enrollment = auth::Enrollment::new(settings.code_ttl, settings.token_ttl_days);
        let nonces = auth::Nonces::new(settings.auth_skew);
        Self { settings, config, devices, units, clients: Vec::new(), next_req: 1, enrollment, nonces, timers, runner, programs, scheduler, jobs: Vec::new() }
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
                if let Some(j) = self.jobs.iter().position(|j| j.req == req) {
                    let job = self.jobs.swap_remove(j);
                    self.count(job, outcome, p);
                } else {
                    self.deliver(req, outcome);
                }
            }
        }
        if now >= self.runner.next {
            self.run_timers(p);
            self.runner.next = now + self.settings.timer_tick.max(1) * 1000;
        }
        if now >= self.scheduler.next {
            self.run_scheduler(p);
            self.scheduler.next = now + self.settings.sched_tick.max(5) * 1000;
        }
        for i in 0..self.clients.len() {
            if matches!(self.clients[i].phase, Phase::Waiting(Pending::Probe(_))) {
                self.probe(i, p, now);
            }
        }
        self.write_clients(p, now);
        let busy = self.units.iter().any(Unit::busy) || !self.clients.is_empty();
        let next = self.units.iter().filter_map(Unit::deadline).chain([self.runner.next, self.scheduler.next]).min();
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
        if let Some(response) = self.timer_route(route, r, p) {
            return Ok(response);
        }
        if let Some(answer) = self.program_route(route, r, p) {
            return answer;
        }
        match route {
            Route::Health => Ok(Response::json(200, &Health { status: "ok" })),
            Route::Version => Ok(Response::json(
                200,
                &api::Version {
                    name: "Breeze μCore",
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

    fn save_timers(&self, p: &mut impl Platform) -> Result<(), String> {
        p.write_file("timers.json", &crate::store::to_json(&self.timers), true)
    }

    /// `/api/timers`: `None` for any other route.
    fn timer_route(&mut self, route: Route, r: &http::Request, p: &mut impl Platform) -> Option<Response> {
        let no_clock = || Response::detail(503, "the server does not know the time yet");
        Some(match route {
            Route::Timers => {
                let kinds = match timers::listed_kinds(api::query_value(&r.target, "kind").as_deref()) {
                    Ok(k) => k,
                    Err(e) => return Some(Response::detail(422, &e)),
                };
                let Some(now) = clock::now(p) else { return Some(no_clock()) };
                let list: Vec<_> = self.timers.timers.iter().filter(|t| kinds.contains(&t.kind)).map(|t| t.view(now)).collect();
                Response::json(200, &list)
            }
            Route::TimerCreate => {
                let request: TimerRequest = match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(e) => return Some(Response::detail(422, &alloc::format!("invalid timer request: {e}"))),
                };
                let when = match request.when() {
                    Ok(w) => w,
                    Err(e) => return Some(Response::detail(422, e)),
                };
                // 422 whatever the check: Breeze Core's control route says 400
                // for a bad name, its timers do not.
                if let Some(Err((_, e))) = request.settings.as_ref().map(ControlRequest::validate) {
                    return Some(Response::detail(422, &e));
                }
                // A timer that quietly covers nothing is worse than an error.
                let unknown: Vec<&str> = request
                    .unit_ids
                    .iter()
                    .filter(|id| !id.parse::<u64>().is_ok_and(|n| self.units.iter().any(|u| u.id == n)))
                    .map(String::as_str)
                    .collect();
                if !unknown.is_empty() {
                    return Some(Response::detail(404, &alloc::format!("unknown unit(s): {}", unknown.join(", "))));
                }
                let mut bytes = [0u8; 6];
                p.random(&mut bytes);
                let id: String = bytes.iter().map(|b| alloc::format!("{b:02x}")).collect();
                let Some(now) = clock::now(p) else { return Some(no_clock()) };
                let TimerRequest { unit_ids, settings, label, .. } = request;
                let built = match when {
                    When::In(minutes) => timers::build_sleep(id, unit_ids.clone(), minutes, settings, &label, now),
                    When::At { days, at } => timers::build_start(id, unit_ids.clone(), days, &at, &label, now),
                };
                let timer = match built {
                    Ok(t) => t,
                    Err(e) => return Some(Response::detail(422, &e)),
                };
                self.timers.replace_for_units(&unit_ids, timer.kind);
                self.timers.timers.push(timer.clone());
                if let Err(e) = self.save_timers(p) {
                    // Or the API would report a timer that vanishes on restart.
                    self.timers.remove(&timer.id);
                    return Some(Response::detail(500, &alloc::format!("cannot write the timer store: {e}")));
                }
                Response::json(201, &timer.view(now))
            }
            Route::TimerStatus => Response::json(
                200,
                &TimerStatus {
                    running: true,
                    tick_seconds: self.settings.timer_tick,
                    runs: self.runner.runs,
                    fired: self.runner.fired,
                    errors: self.runner.errors,
                    last_run: self.runner.last_run.as_deref(),
                    pending: self.timers.timers.len(),
                    next_fires_at: self.timers.next_fires_at(),
                },
            ),
            Route::TimerCancel => {
                let id = r.path.strip_prefix("/api/timers/").unwrap_or_default();
                if !self.timers.remove(id) {
                    return Some(Response::detail(404, "no such timer"));
                }
                if let Err(e) = self.save_timers(p) {
                    return Some(Response::detail(500, &alloc::format!("cannot write the timer store: {e}")));
                }
                Response { status: 204, body: Vec::new(), extra: Vec::new() }
            }
            _ => return None,
        })
    }

    /// One pass, as Breeze Core's runner makes it: fire what is due, then
    /// forget it in one write, so nothing fires twice.
    fn run_timers(&mut self, p: &mut impl Platform) {
        // No clock, no timers: they wait rather than fire at a guess.
        let Some(now) = clock::now(p) else { return };
        let due: Vec<Timer> = self.timers.timers.iter().filter(|t| t.is_due(now)).cloned().collect();
        for timer in &due {
            if timer.missed(now) {
                p.log(&alloc::format!(
                    "timer {}: a scheduled start for {} was missed while the server was not running -- dropped, not switched on late",
                    timer.id, timer.fires_at
                ));
                continue;
            }
            let targets: Vec<u64> = if timer.unit_ids.is_empty() {
                self.units.iter().map(|u| u.id).collect()
            } else {
                timer.unit_ids.iter().filter_map(|s| s.parse().ok()).collect()
            };
            for unit in targets {
                match self.units.iter().position(|u| u.id == unit) {
                    Some(u) => {
                        let req = self.req();
                        self.units[u].control(req, timer.settings.change());
                        self.jobs.push(Job { req, unit, kind: JobKind::Timer });
                    }
                    None => {
                        self.runner.errors += 1;
                        p.log(&alloc::format!("timer {} failed on unit {unit}: {NOT_A_UNIT}", timer.id));
                    }
                }
            }
        }
        if !due.is_empty() {
            for timer in &due {
                self.timers.remove(&timer.id);
            }
            if let Err(e) = self.save_timers(p) {
                p.log(&alloc::format!("cannot write the timer store after firing: {e}"));
                self.runner.errors += 1;
            }
        }
        self.runner.runs += 1;
        self.runner.last_run = Some(clock::format(clock::seconds(now)));
    }

    /// A timer's or a program's work on a unit came back.
    fn count(&mut self, job: Job, outcome: Outcome, p: &mut impl Platform) {
        let Outcome::Control(result) = outcome else { return };
        let unit = job.unit;
        match (job.kind, result) {
            (JobKind::Timer, Ok(_)) => self.runner.fired += 1,
            (JobKind::Timer, Err(e)) => {
                self.runner.errors += 1;
                p.log(&alloc::format!("a timer failed on unit {unit}: {e}"));
            }
            (JobKind::Schedule { .. }, Ok(_)) => {}
            // Logged, not counted, as Breeze Core: an unplugged unit is normal.
            (JobKind::Schedule { name }, Err(e)) => p.log(&alloc::format!("schedule '{name}' -> unit {unit} failed: {e}")),
            (JobKind::Curve { program, name, temp }, Ok(_)) => {
                // Only on success, so an unreachable unit is tried again.
                self.scheduler.curve_applied.insert((program, unit.to_string()), temp);
                p.log(&alloc::format!("curve '{name}' -> unit {unit} set {temp:.1}"));
            }
            (JobKind::Curve { name, .. }, Err(e)) => p.log(&alloc::format!("curve '{name}' -> unit {unit} failed: {e}")),
        }
    }

    fn save_programs(&self, p: &mut impl Platform) -> Result<(), Response> {
        p.write_file("programs.json", &crate::store::to_json(&self.programs), true)
            .map_err(|e| Response::detail(500, &alloc::format!("cannot write the program store: {e}")))
    }

    /// `/api/programs`: `None` for any other route.
    fn program_route(&mut self, route: Route, r: &http::Request, p: &mut impl Platform) -> Option<Result<Response, Pending>> {
        let id = r.path.strip_prefix("/api/programs/").unwrap_or_default();
        let missing = |id: &str| Response::detail(404, &alloc::format!("no program '{id}'"));
        Some(Ok(match route {
            Route::Programs => Response::json(200, &self.programs.programs),
            Route::ProgramStatus => Response::json(
                200,
                &ProgramStatus {
                    running: true,
                    tick_seconds: self.settings.sched_tick.max(5),
                    runs: self.scheduler.runs,
                    errors: 0,
                    last_run: self.scheduler.last_run.as_deref(),
                },
            ),
            Route::ProgramGet => match self.programs.get(id) {
                Some(program) => Response::json(200, program),
                None => missing(id),
            },
            Route::ProgramCreate => {
                let spec: ProgramSpec = match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(e) => return Some(Ok(Response::detail(422, &alloc::format!("invalid program: {e}")))),
                };
                let mut bytes = [0u8; 6];
                p.random(&mut bytes);
                let program = match spec.into_program(bytes.iter().map(|b| alloc::format!("{b:02x}")).collect()) {
                    Ok(v) => v,
                    Err(e) => return Some(Ok(Response::detail(422, &e))),
                };
                self.programs.programs.push(program.clone());
                if let Err(e) = self.save_programs(p) {
                    self.programs.programs.pop();
                    return Some(Ok(e));
                }
                Response::json(201, &program)
            }
            Route::ProgramUpdate => {
                let spec: ProgramSpec = match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(e) => return Some(Ok(Response::detail(422, &alloc::format!("invalid program: {e}")))),
                };
                // Checked before it is looked for, as Breeze Core: a bad body
                // is 422 whether or not the program exists.
                let program = match spec.into_program(id.to_string()) {
                    Ok(v) => v,
                    Err(e) => return Some(Ok(Response::detail(422, &e))),
                };
                let Some(slot) = self.programs.programs.iter().position(|q| q.id == id) else { return Some(Ok(missing(id))) };
                let previous = core::mem::replace(&mut self.programs.programs[slot], program.clone());
                if let Err(e) = self.save_programs(p) {
                    self.programs.programs[slot] = previous;
                    return Some(Ok(e));
                }
                Response::json(200, &program)
            }
            Route::ProgramDelete => {
                let Some(slot) = self.programs.programs.iter().position(|q| q.id == id) else { return Some(Ok(missing(id))) };
                let previous = self.programs.programs.remove(slot);
                if let Err(e) = self.save_programs(p) {
                    // Back at the end, as Breeze Core puts it back.
                    self.programs.programs.push(previous);
                    return Some(Ok(e));
                }
                Response { status: 204, body: Vec::new(), extra: Vec::new() }
            }
            Route::ProgramApply => {
                let id = id.strip_suffix("/apply").unwrap_or_default();
                let Some(program) = self.programs.get(id).cloned() else { return Some(Ok(missing(id))) };
                let request = if program.is_curve() {
                    match (&program.curve, clock::now(p)) {
                        (None, _) => return Some(Ok(Response::detail(400, "curve has no points"))),
                        (Some(_), None) => return Some(Ok(Response::detail(503, "the server does not know the time yet"))),
                        (Some(curve), Some(now)) => programs::curve_request(curve, clock::seconds(now)),
                    }
                } else if program.is_schedule() {
                    return Some(Ok(Response::detail(400, "schedule programs fire automatically; nothing to apply")));
                } else {
                    match program.favourite.clone() {
                        Some(scene) => scene,
                        None => return Some(Ok(Response::detail(400, "favourite has no settings"))),
                    }
                };
                let mut rest = program.targets(self.units.iter().map(|u| u.id));
                if rest.is_empty() {
                    return Some(Ok(Response { status: 200, body: b"[]".to_vec(), extra: Vec::new() }));
                }
                let first = rest.remove(0);
                return Some(self.apply_start(first, rest, request, Vec::new()).map_or_else(Err, Ok));
            }
            _ => return None,
        }))
    }

    /// Send a program's request to the next unit, or answer if it is not
    /// there. `Ok` is the answer, `Err` the wait.
    fn apply_start(&mut self, unit: u64, rest: Vec<u64>, request: ControlRequest, done: Vec<Vec<u8>>) -> Result<Response, Pending> {
        let Some(u) = self.units.iter().position(|x| x.id == unit) else {
            return Ok(Response::detail(503, NOT_A_UNIT));
        };
        let req = self.req();
        self.units[u].control(req, request.change());
        Err(Pending::Apply(Box::new(Apply { current: (unit, req), rest, request, done })))
    }

    /// A unit of an apply answered: on to the next, or the answer.
    fn apply_next(&mut self, unit: u64, outcome: Outcome, mut rest: Vec<u64>, request: ControlRequest, mut done: Vec<Vec<u8>>) -> Phase {
        let writing = |r: Response| Phase::Writing { out: r.to_bytes(), at: 0 };
        match outcome {
            Outcome::Control(Ok((sent, echoed))) => {
                let Some(u) = self.units.iter().find(|x| x.id == unit) else { return writing(Response::detail(503, NOT_A_UNIT)) };
                let view = api::ControlView { state: UnitState::of(u, &echoed), not_applied: request.not_applied(&sent, &echoed) };
                done.push(serde_json::to_vec(&view).unwrap_or_default());
            }
            Outcome::Control(Err(e)) => return writing(Response::detail(503, &e.to_string())),
            _ => return writing(Response::detail(503, NOT_A_UNIT)),
        }
        if rest.is_empty() {
            let mut body = b"[".to_vec();
            body.extend_from_slice(&done.join(&b','));
            body.push(b']');
            return writing(Response { status: 200, body, extra: Vec::new() });
        }
        let next = rest.remove(0);
        match self.apply_start(next, rest, request, done) {
            Ok(response) => writing(response),
            Err(pending) => Phase::Waiting(pending),
        }
    }

    /// One pass over the enabled programs, sharing one reading of the clock.
    fn run_scheduler(&mut self, p: &mut impl Platform) {
        let Some(now) = clock::now(p) else { return };
        let secs = clock::seconds(now);
        let enabled: Vec<Program> = self.programs.programs.iter().filter(|q| q.enabled).cloned().collect();
        for program in &enabled {
            if program.is_schedule() {
                self.run_schedule(program, secs, p);
            } else if program.is_curve() {
                self.run_curve(program, secs, p);
            }
        }
        self.scheduler.runs += 1;
        self.scheduler.last_run = Some(clock::format(secs));
    }

    fn run_schedule(&mut self, program: &Program, secs: i64, p: &mut impl Platform) {
        let due = programs::due_entries(&program.schedule, secs);
        if due.is_empty() {
            return;
        }
        let stamp = programs::minute_stamp(secs);
        for index in due {
            // Claimed before it is applied, as Breeze Core: a trigger that
            // fails does not retry for the rest of the minute.
            let key = (program.id.clone(), index);
            if self.scheduler.fired.get(&key) == Some(&stamp) {
                continue;
            }
            self.scheduler.fired.insert(key, stamp.clone());
            let entry = &program.schedule[index];
            p.log(&alloc::format!("schedule '{}' firing ({})", program.name, entry.time));
            for unit in program.targets(self.units.iter().map(|u| u.id)) {
                self.issue(unit, &entry.settings, JobKind::Schedule { name: program.name.clone() }, p);
            }
        }
    }

    fn run_curve(&mut self, program: &Program, secs: i64, p: &mut impl Platform) {
        let Some(curve) = &program.curve else { return };
        let request = programs::curve_request(curve, secs);
        // No points, nothing to track (applying it by hand still switches on).
        let Some(temp) = request.target_temperature else { return };
        for unit in program.targets(self.units.iter().map(|u| u.id)) {
            let key = programs::key(program, unit);
            if self.scheduler.curve_applied.get(&key) == Some(&temp) {
                continue;
            }
            // Still on its way from the last pass.
            if self.jobs.iter().any(|j| j.unit == unit && matches!(&j.kind, JobKind::Curve { program: id, .. } if *id == program.id)) {
                continue;
            }
            self.issue(unit, &request, JobKind::Curve { program: program.id.clone(), name: program.name.clone(), temp }, p);
        }
    }

    /// Hand a program's request to a unit, for nobody to wait on.
    fn issue(&mut self, unit: u64, request: &ControlRequest, kind: JobKind, p: &mut impl Platform) {
        match self.units.iter().position(|u| u.id == unit) {
            Some(u) => {
                let req = self.req();
                self.units[u].control(req, request.change());
                self.jobs.push(Job { req, unit, kind });
            }
            None => match kind {
                JobKind::Schedule { name } => p.log(&alloc::format!("schedule '{name}' -> unit {unit} failed: {NOT_A_UNIT}")),
                JobKind::Curve { name, .. } => p.log(&alloc::format!("curve '{name}' -> unit {unit} failed: {NOT_A_UNIT}")),
                JobKind::Timer => {}
            },
        }
    }

    /// Answer everyone still waiting on a unit that has just been removed or
    /// replaced: its work will not finish.
    fn abandon(&mut self, id: u64) {
        // A timer's work on it will not be answered either: a failure.
        let lost = self.jobs.iter().filter(|j| j.unit == id && matches!(j.kind, JobKind::Timer)).count();
        self.jobs.retain(|j| j.unit != id);
        self.runner.errors += lost as u64;
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
            Phase::Waiting(Pending::Apply(a)) => a.current.1 == req,
            _ => false,
        }) else {
            return;
        };
        if matches!(self.clients[i].phase, Phase::Waiting(Pending::Apply(_))) {
            let Phase::Waiting(Pending::Apply(a)) = core::mem::replace(&mut self.clients[i].phase, Phase::Reading) else {
                return;
            };
            let Apply { current, rest, request, done } = *a;
            self.clients[i].phase = self.apply_next(current.0, outcome, rest, request, done);
            return;
        }
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

/// What Breeze Core says of a unit id it does not have.
const NOT_A_UNIT: &str = "unit has no V3 token and key configured";

#[derive(serde::Serialize)]
struct ProgramStatus<'a> {
    running: bool,
    tick_seconds: u64,
    runs: u64,
    errors: u64,
    last_run: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct TimerStatus<'a> {
    running: bool,
    tick_seconds: u64,
    runs: u64,
    fired: u64,
    errors: u64,
    last_run: Option<&'a str>,
    pending: usize,
    next_fires_at: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct Health {
    status: &'static str,
}

/// For the log line at start.
pub fn banner(core: &Core) -> String {
    alloc::format!("Breeze μCore {} (API {}), {} unit(s)", crate::product_version(), crate::NATIVE, core.units.len())
}
