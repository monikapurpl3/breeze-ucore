//! The loop: accept clients, read their requests, hand unit work to the units,
//! answer when the units do. Called over and over by the platform; never waits.

use crate::api::{self, ControlRequest, Envelope, Guard, Resolved, Route, UnitOffline, UnitState, UnitView};
use crate::auth;
use crate::http::{self, Parsed, Response};
use crate::platform::{Conn, NetKind, Platform};
use crate::store::{AppConfig, DevicesDoc};
use crate::unit::{Outcome, ReqId, Timing, Unit};
use alloc::string::String;
use alloc::vec::Vec;
use core::net::IpAddr;

/// Clients served at once; the rest wait in the platform's backlog.
pub const MAX_CLIENTS: usize = 4;
/// A client that sends nothing for this long is dropped.
const CLIENT_IDLE_MS: u64 = 30_000;

#[derive(Debug, Clone)]
pub struct Settings {
    pub timing: Timing,
    pub min_auth_version: u8,
    /// The build's commit, for `/api/version`.
    pub commit: &'static str,
}

impl Default for Settings {
    fn default() -> Self {
        Self { timing: Timing::default(), min_auth_version: 1, commit: option_env!("UCORE_COMMIT").unwrap_or("unknown") }
    }
}

enum Pending {
    State { unit: usize, req: ReqId },
    Caps { unit: usize, req: ReqId },
    Control { unit: usize, req: ReqId, request: ControlRequest },
    Batch { parts: Vec<(usize, ReqId, Option<Outcome>)> },
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
}

impl Core {
    /// Load `config.json` and `devices.json`. A file that will not parse is
    /// logged and treated as empty: on a chip, a server that answers 401 is
    /// easier to put right than one that restarts forever.
    pub fn new(p: &mut impl Platform, settings: Settings) -> Self {
        fn load<T: serde::de::DeserializeOwned + Default>(p: &mut impl Platform, name: &str) -> T {
            match p.read_file(name).map(|b| serde_json::from_slice(&b)) {
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
        let units = config
            .units
            .iter()
            .map(|u| {
                Unit::new(u.id as u64, u.name.clone(), u.ip.clone(), u.port, u.token.as_deref(), u.key.as_deref(), settings.timing)
            })
            .collect();
        Self { settings, config, devices, units, clients: Vec::new(), next_req: 1 }
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
    fn dispatch(&mut self, r: &http::Request, _peer: IpAddr, p: &mut impl Platform) -> Result<Response, Pending> {
        let (guard, route) = match api::resolve(&r.method, &r.path) {
            Resolved::Route(g, route) => (g, route),
            Resolved::MethodNotAllowed(allow) => {
                return Ok(Response::detail(405, "Method Not Allowed").with_header("Allow", allow))
            }
            Resolved::NotFound => return Ok(Response::detail(404, "Not Found")),
        };
        if guard != Guard::Open {
            if let Err(rejection) = auth::check_api_key(r.header("x-api-key"), self.config.api_key.as_deref()) {
                return Ok(rejection);
            }
        }
        if guard == Guard::Full {
            let now = p.unix_ms().map_or(0.0, |ms| ms as f64 / 1000.0);
            let token = r.header("authorization").and_then(auth::bearer);
            if let Err(rejection) = auth::check_v1(&self.devices, token, now) {
                return Ok(rejection);
            }
        }
        let unit_of = |units: &[Unit]| {
            let id: u64 = api::unit_segment(&r.path).parse().ok()?;
            units.iter().position(|u| u.id == id)
        };
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
                    parts.push((u, req, None));
                }
                Err(Pending::Batch { parts })
            }
            Route::UnitState => {
                let Some(unit) = unit_of(&self.units) else { return Ok(api::unknown_unit(&r.path)) };
                let req = self.req();
                self.units[unit].read(req);
                Err(Pending::State { unit, req })
            }
            Route::Capabilities => {
                let Some(unit) = unit_of(&self.units) else { return Ok(api::unknown_unit(&r.path)) };
                let req = self.req();
                match self.units[unit].capabilities(req) {
                    Some(caps) => Ok(Response::json(200, &caps.view(alloc::string::ToString::to_string(&self.units[unit].id)))),
                    None => Err(Pending::Caps { unit, req }),
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
                Err(Pending::Control { unit, req, request })
            }
        }
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
        let response = match (pending, outcome) {
            (Pending::State { unit, .. }, Outcome::State(result)) => {
                let u = &units[*unit];
                match result {
                    Ok(s) => Response::json(200, &UnitState::of(u, &s)),
                    // An unreachable unit is a fact about the unit, not a failed
                    // request: an offline record, not an error status.
                    Err(e) => Response::json(200, &UnitOffline::of(u, &e)),
                }
            }
            (Pending::Caps { unit, .. }, Outcome::Caps(result)) => match result {
                Ok(caps) => Response::json(200, &caps.view(alloc::string::ToString::to_string(&units[*unit].id))),
                Err(e) => Response::detail(503, &alloc::string::ToString::to_string(&e)),
            },
            (Pending::Control { unit, request, .. }, Outcome::Control(result)) => match result {
                Ok((sent, echoed)) => Response::json(
                    200,
                    &api::ControlView { state: UnitState::of(&units[*unit], &echoed), not_applied: request.not_applied(&sent, &echoed) },
                ),
                Err(e) => Response::detail(503, &alloc::string::ToString::to_string(&e)),
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
                    let u = &units[*unit];
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

#[derive(serde::Serialize)]
struct Health {
    status: &'static str,
}

/// For the log line at start.
pub fn banner(core: &Core) -> String {
    alloc::format!("Breeze μcore {} (API {}), {} unit(s)", crate::product_version(), crate::NATIVE, core.units.len())
}
