//! One air conditioner: its session, and the work queued for it.
//!
//! What Breeze Core's device layer does with a thread and a lock per unit, as
//! a state machine polled from the one loop: connect, handshake, wait for the
//! unit to settle, drain stale bytes, send, resend up to three times, discard
//! late replies, and retry once on a fresh connection when the failure is the
//! kind a reconnect fixes. Reads arriving while one is in flight share it;
//! controls arriving while the unit is busy merge into one command.

use crate::platform::{Conn, NetError, NetKind, Platform};
use crate::proto::ac::{self, Mode, Setpoint, State, Swing};
use crate::proto::caps::Capabilities;
use crate::proto::frame::{self, FrameError};
use crate::proto::lan::{self, LanError};
use crate::proto::packet::{self, PacketError};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::IpAddr;

/// A request waiting on a unit, numbered by the core.
pub type ReqId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub connect_ms: u64,
    pub reply_ms: u64,
    pub sends: u32,
    pub settle_ms: u64,
    pub linger_ms: u64,
}

impl Default for Timing {
    fn default() -> Self {
        // Connect waits a little longer than a platform's own 5 s timeout, so
        // that its wording reaches the client when it has one.
        Self { connect_ms: 6_000, reply_ms: 2_000, sends: 3, settle_ms: lan::SETTLE_MS, linger_ms: 1_000 }
    }
}

const ATTEMPTS: u32 = 2;
const MAX_PACKET: usize = 8192;
/// A report this recent is what a control builds on; older, it asks first.
const CONTROL_BASE_MAX_AGE_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceError {
    Net(NetError),
    Protocol(LanError),
    Packet(PacketError),
    Frame(FrameError),
    Response(ac::ResponseError),
    PacketTooLarge(usize),
    MissingCredentials,
}

impl DeviceError {
    /// Worth another attempt on a fresh connection: timeouts, resets, a closed
    /// connection, a stale session's garbage. A rejected key never is.
    fn retryable(&self) -> bool {
        match self {
            Self::Net(e) => matches!(e.kind, NetKind::TimedOut | NetKind::WouldBlock | NetKind::Reset | NetKind::Closed),
            Self::Protocol(LanError::BadSignature | LanError::Incomplete { .. }) => true,
            _ => false,
        }
    }

    fn timed_out() -> Self {
        Self::Net(NetError::new(NetKind::TimedOut, "the unit did not reply"))
    }
}

impl core::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Net(e) => write!(f, "network error: {}", e.message),
            Self::Protocol(e) => write!(f, "session error: {e}"),
            Self::Packet(e) => write!(f, "packet error: {e}"),
            Self::Frame(e) => write!(f, "frame error: {e}"),
            Self::Response(e) => write!(f, "state report error: {e}"),
            Self::PacketTooLarge(n) => write!(f, "unit claimed a {n}-byte packet; refusing"),
            Self::MissingCredentials => write!(f, "unit has no V3 token and key configured"),
        }
    }
}

/// The fields of a control request a unit acts on; `None` leaves a field as
/// the unit last reported it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Change {
    pub power_on: Option<bool>,
    pub mode: Option<Mode>,
    pub target_temperature: Option<f32>,
    pub fan_speed: Option<u8>,
    pub swing: Option<Swing>,
    pub eco: Option<bool>,
    pub turbo: Option<bool>,
    pub beep: Option<bool>,
}

impl Change {
    /// A later request's fields win.
    pub fn merge(&mut self, later: &Change) {
        macro_rules! take {
            ($($f:ident),*) => { $( if later.$f.is_some() { self.$f = later.$f; } )* };
        }
        take!(power_on, mode, target_temperature, fan_speed, swing, eco, turbo, beep);
    }

    pub fn apply_to(&self, s: &mut Setpoint) {
        macro_rules! set {
            ($($f:ident => $t:ident),*) => { $( if let Some(v) = self.$f { s.$t = v; } )* };
        }
        set!(power_on => power_on, mode => mode, target_temperature => target_temperature,
             fan_speed => fan_speed, swing => swing, eco => eco, turbo => turbo, beep => beep);
    }
}

/// What a waiting request gets.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    State(Result<State, DeviceError>),
    Caps(Result<Capabilities, DeviceError>),
    /// What was sent, and what the unit then reported.
    Control(Result<(Setpoint, State), DeviceError>),
}

#[derive(Debug, Clone, PartialEq)]
enum Ask {
    Refresh,
    Caps { more: bool },
    Apply(Setpoint),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Connecting { deadline: u64 },
    Handshaking { sends: u32, deadline: u64 },
    Settling { until: u64 },
    Draining { deadline: u64 },
    Awaiting { sends: u32, deadline: u64 },
}

struct Exchange {
    ask: Ask,
    frame: Vec<u8>,
    attempt: u32,
    step: Step,
    resent: bool,
}

struct Session {
    key: [u8; 32],
    since: u64,
}

pub struct Unit {
    pub id: u64,
    pub name: String,
    pub ip: String,
    addr: Option<IpAddr>,
    port: u16,
    creds: Option<(Vec<u8>, [u8; 32])>,
    timing: Timing,
    conn: Option<Conn>,
    rx: Vec<u8>,
    tx: Vec<u8>,
    session: Option<Session>,
    packet_id: u16,
    message_id: u8,
    exchange: Option<Exchange>,
    linger_until: Option<u64>,
    readers: Vec<ReqId>,
    caps_waiters: Vec<ReqId>,
    /// The first may be in progress (`control_running`); a later one gathers
    /// what arrives meanwhile.
    controls: VecDeque<(Change, Vec<ReqId>)>,
    control_running: bool,
    pub last_state: Option<(State, u64)>,
    pub caps: Option<Capabilities>,
    /// The first page while the second is being asked for.
    caps_first: Option<Capabilities>,
    pub online: bool,
}

impl Unit {
    pub fn new(id: u64, name: String, ip: String, port: u16, token: Option<&str>, key: Option<&str>, timing: Timing) -> Self {
        let token = token.and_then(crate::store::unhex).filter(|t| !t.is_empty());
        let key = key.and_then(crate::store::unhex).and_then(|k| <[u8; 32]>::try_from(k).ok());
        Self {
            id,
            addr: ip.parse().ok(),
            name,
            ip,
            port,
            creds: token.zip(key),
            timing,
            conn: None,
            rx: Vec::new(),
            tx: Vec::new(),
            session: None,
            packet_id: 1,
            message_id: 1,
            exchange: None,
            linger_until: None,
            readers: Vec::new(),
            caps_waiters: Vec::new(),
            controls: VecDeque::new(),
            control_running: false,
            last_state: None,
            caps: None,
            caps_first: None,
            online: false,
        }
    }

    pub fn read(&mut self, req: ReqId) {
        self.readers.push(req);
    }

    /// Capabilities never change while a unit is powered: asked once, kept.
    pub fn capabilities(&mut self, req: ReqId) -> Option<Capabilities> {
        if let Some(c) = &self.caps {
            return Some(c.clone());
        }
        self.caps_waiters.push(req);
        None
    }

    pub fn control(&mut self, req: ReqId, change: Change) {
        let gathering = self.controls.len() > usize::from(self.control_running);
        match self.controls.back_mut() {
            Some((c, waiters)) if gathering => {
                c.merge(&change);
                waiters.push(req);
            }
            _ => self.controls.push_back((change, alloc::vec![req])),
        }
    }

    /// Whether anything is queued or under way.
    pub fn busy(&self) -> bool {
        self.exchange.is_some() || !self.readers.is_empty() || !self.caps_waiters.is_empty() || !self.controls.is_empty()
    }

    fn next_message_id(&mut self) -> u8 {
        let id = self.message_id;
        self.message_id = self.message_id.wrapping_add(1);
        id
    }

    fn start(&mut self, ask: Ask, p: &mut impl Platform, now: u64, out: &mut Vec<(ReqId, Outcome)>) {
        let id = self.next_message_id();
        let frame = match &ask {
            Ask::Refresh => ac::get_state(id),
            Ask::Caps { more } => ac::get_capabilities(id, *more),
            Ask::Apply(sp) => ac::set_state(id, sp),
        };
        self.exchange = Some(Exchange { ask, frame, attempt: 1, step: Step::Draining { deadline: 0 }, resent: false });
        self.begin_attempt(p, now, out);
    }

    /// Pick the next piece of work when idle: a control first (it answers
    /// waiting readers too), then a read, then capabilities.
    fn schedule(&mut self, p: &mut impl Platform, now: u64, out: &mut Vec<(ReqId, Outcome)>) {
        if self.exchange.is_some() || self.linger_until.is_some_and(|t| now < t) {
            return;
        }
        self.linger_until = None;
        if let Some((change, _)) = self.controls.front() {
            self.control_running = true;
            match self.last_state.as_ref().filter(|(_, at)| now.saturating_sub(*at) <= CONTROL_BASE_MAX_AGE_MS) {
                Some((base, _)) => {
                    let mut sp = Setpoint::from_state(base);
                    change.apply_to(&mut sp);
                    self.start(Ask::Apply(sp), p, now, out);
                }
                None => self.start(Ask::Refresh, p, now, out),
            }
        } else if !self.readers.is_empty() {
            self.start(Ask::Refresh, p, now, out);
        } else if !self.caps_waiters.is_empty() {
            self.start(Ask::Caps { more: false }, p, now, out);
        }
    }

    pub fn poll(&mut self, p: &mut impl Platform, now: u64) -> Vec<(ReqId, Outcome)> {
        let mut out = Vec::new();
        if let Some(until) = self.linger_until {
            // Late replies to a resent request are read and thrown away.
            let _ = self.fill(p);
            while let Ok(Some(_)) = self.take_packet() {}
            if now >= until {
                self.linger_until = None;
            }
        }
        if self.exchange.is_some() {
            let result = self.flush(p).and_then(|_| self.advance(p, now, &mut out));
            if let Err(e) = result {
                self.failed(e, p, now, &mut out);
            }
        }
        self.schedule(p, now, &mut out);
        out
    }

    /// When the next timer falls, if anything is waiting on one.
    pub fn deadline(&self) -> Option<u64> {
        let step = self.exchange.as_ref().map(|x| match x.step {
            Step::Connecting { deadline }
            | Step::Handshaking { deadline, .. }
            | Step::Draining { deadline }
            | Step::Awaiting { deadline, .. } => deadline,
            Step::Settling { until } => until,
        });
        match (step, self.linger_until) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn begin_attempt(&mut self, p: &mut impl Platform, now: u64, out: &mut Vec<(ReqId, Outcome)>) {
        if self.creds.is_none() {
            return self.failed(DeviceError::MissingCredentials, p, now, out);
        }
        let live = self.conn.is_some()
            && self.session.as_ref().is_some_and(|s| now.saturating_sub(s.since) < lan::SESSION_LIFETIME_MS);
        let step = if live {
            Ok(Step::Draining { deadline: 0 })
        } else {
            self.drop_session(p);
            match self.addr {
                None => Err(DeviceError::Net(NetError::new(NetKind::Other, "invalid unit address"))),
                Some(addr) => p.connect(addr, self.port).map(|c| {
                    self.conn = Some(c);
                    Step::Connecting { deadline: now + self.timing.connect_ms }
                }).map_err(DeviceError::Net),
            }
        };
        match step {
            Ok(Step::Draining { .. }) => {
                if let Err(e) = self.drain_then_send(p, now) {
                    self.failed(e, p, now, out);
                }
            }
            Ok(step) => self.exchange.as_mut().expect("in an exchange").step = step,
            Err(e) => self.failed(e, p, now, out),
        }
    }

    fn advance(&mut self, p: &mut impl Platform, now: u64, out: &mut Vec<(ReqId, Outcome)>) -> Result<(), DeviceError> {
        let step = self.exchange.as_ref().expect("in an exchange").step;
        match step {
            Step::Connecting { deadline } => {
                let conn = self.conn.expect("connecting");
                if p.is_connected(conn).map_err(DeviceError::Net)? {
                    self.send_handshake(p)?;
                    self.set_step(Step::Handshaking { sends: 1, deadline: now + self.timing.reply_ms });
                } else if now >= deadline {
                    return Err(DeviceError::Net(NetError::new(NetKind::TimedOut, "connection timed out")));
                }
            }
            Step::Handshaking { sends, deadline } => {
                self.fill(p)?;
                if let Some(pkt) = self.take_packet()? {
                    let key = self.creds.as_ref().expect("checked").1;
                    match lan::decode(&pkt, None).map_err(DeviceError::Protocol)? {
                        lan::Reply::Handshake(payload) => {
                            let session = lan::session_key(&key, &payload).map_err(DeviceError::Protocol)?;
                            self.session = Some(Session { key: session, since: now });
                        }
                        lan::Reply::Response(_) => {
                            return Err(DeviceError::Protocol(LanError::UnexpectedType(lan::ENCRYPTED_RESPONSE)))
                        }
                    }
                    self.set_step(Step::Settling { until: now + self.timing.settle_ms });
                } else if now >= deadline {
                    if sends >= self.timing.sends {
                        return Err(DeviceError::timed_out());
                    }
                    self.send_handshake(p)?;
                    self.set_step(Step::Handshaking { sends: sends + 1, deadline: now + self.timing.reply_ms });
                }
            }
            Step::Settling { until } => {
                if now >= until {
                    self.drain_then_send(p, now)?;
                }
            }
            Step::Draining { deadline } => {
                // A partial packet was left in the buffer: let it finish, then
                // discard it, then ask.
                self.fill(p)?;
                if self.take_packet()?.is_some() {
                    self.send_request(p, now)?;
                } else if now >= deadline {
                    return Err(DeviceError::timed_out());
                }
            }
            Step::Awaiting { sends, deadline } => {
                self.fill(p)?;
                if let Some(pkt) = self.take_packet()? {
                    let key = self.session.as_ref().expect("authenticated").key;
                    let v2 = match lan::decode(&pkt, Some(&key)).map_err(DeviceError::Protocol)? {
                        lan::Reply::Response(v2) => v2,
                        lan::Reply::Handshake(_) => {
                            return Err(DeviceError::Protocol(LanError::UnexpectedType(lan::HANDSHAKE_RESPONSE)))
                        }
                    };
                    let inner = packet::decode(&v2).map_err(DeviceError::Packet)?;
                    let payload = frame::parse(&inner).map_err(DeviceError::Frame)?.to_vec();
                    self.answered(payload, p, now, out)?;
                } else if now >= deadline {
                    if sends >= self.timing.sends {
                        return Err(DeviceError::timed_out());
                    }
                    self.exchange.as_mut().expect("in an exchange").resent = true;
                    self.send_encrypted(p)?;
                    self.set_step(Step::Awaiting { sends: sends + 1, deadline: now + self.timing.reply_ms });
                }
            }
        }
        Ok(())
    }

    fn set_step(&mut self, step: Step) {
        self.exchange.as_mut().expect("in an exchange").step = step;
    }

    /// Throw away whatever is already in, then ask. A packet still arriving is
    /// waited for first, so its tail is not read as the answer.
    fn drain_then_send(&mut self, p: &mut impl Platform, now: u64) -> Result<(), DeviceError> {
        self.fill(p)?;
        while self.take_packet()?.is_some() {}
        if !self.rx.is_empty() {
            self.set_step(Step::Draining { deadline: now + self.timing.reply_ms });
            return Ok(());
        }
        self.send_request(p, now)
    }

    fn send_request(&mut self, p: &mut impl Platform, now: u64) -> Result<(), DeviceError> {
        self.send_encrypted(p)?;
        self.set_step(Step::Awaiting { sends: 1, deadline: now + self.timing.reply_ms });
        Ok(())
    }

    fn send_handshake(&mut self, p: &mut impl Platform) -> Result<(), DeviceError> {
        let token = &self.creds.as_ref().expect("checked").0;
        let bytes = lan::handshake(self.packet_id, token);
        self.bump_packet_id();
        self.tx.extend_from_slice(&bytes);
        self.flush(p)
    }

    fn send_encrypted(&mut self, p: &mut impl Platform) -> Result<(), DeviceError> {
        let key = self.session.as_ref().expect("authenticated").key;
        let v2 = packet::encode(self.id, &self.exchange.as_ref().expect("in an exchange").frame, packet::timestamp(p.unix_ms()));
        let bytes = lan::encrypted(&key, self.packet_id, &v2, lan::ENCRYPTED_REQUEST);
        self.bump_packet_id();
        self.tx.extend_from_slice(&bytes);
        self.flush(p)
    }

    fn bump_packet_id(&mut self) {
        self.packet_id = self.packet_id.wrapping_add(1) & 0xFFF;
    }

    fn flush(&mut self, p: &mut impl Platform) -> Result<(), DeviceError> {
        let Some(conn) = self.conn else { return Ok(()) };
        while !self.tx.is_empty() {
            match p.send(conn, &self.tx) {
                Ok(0) => break,
                Ok(n) => {
                    self.tx.drain(..n);
                }
                Err(e) if e.kind == NetKind::WouldBlock => break,
                Err(e) => return Err(DeviceError::Net(e)),
            }
        }
        Ok(())
    }

    fn fill(&mut self, p: &mut impl Platform) -> Result<(), DeviceError> {
        let Some(conn) = self.conn else { return Ok(()) };
        let mut buf = [0u8; 512];
        loop {
            match p.recv(conn, &mut buf) {
                Ok(0) => return Err(DeviceError::Net(NetError::new(NetKind::Closed, "the unit closed the connection"))),
                Ok(n) => {
                    self.rx.extend_from_slice(&buf[..n]);
                    if self.rx.len() > MAX_PACKET * 2 {
                        return Ok(());
                    }
                }
                Err(e) if e.kind == NetKind::WouldBlock => return Ok(()),
                Err(e) => return Err(DeviceError::Net(e)),
            }
        }
    }

    fn take_packet(&mut self) -> Result<Option<Vec<u8>>, DeviceError> {
        if self.rx.len() >= 2 && self.rx[..2] != [0x83, 0x70] {
            let head = [self.rx[0], self.rx[1]];
            self.rx.clear();
            return Err(DeviceError::Protocol(LanError::NotAPacket(head)));
        }
        let Some(total) = lan::packet_len(&self.rx) else { return Ok(None) };
        if total > MAX_PACKET {
            self.rx.clear();
            return Err(DeviceError::PacketTooLarge(total));
        }
        if self.rx.len() < total {
            return Ok(None);
        }
        Ok(Some(self.rx.drain(..total).collect()))
    }

    /// The unit is going: close its connection.
    pub fn shutdown(mut self, p: &mut impl Platform) {
        self.drop_session(p);
    }

    fn drop_session(&mut self, p: &mut impl Platform) {
        if let Some(c) = self.conn.take() {
            p.close(c);
        }
        self.session = None;
        self.rx.clear();
        self.tx.clear();
        self.linger_until = None;
    }

    /// The unit answered the exchange under way.
    fn answered(&mut self, payload: Vec<u8>, p: &mut impl Platform, now: u64, out: &mut Vec<(ReqId, Outcome)>) -> Result<(), DeviceError> {
        let x = self.exchange.as_ref().expect("in an exchange");
        let state = match &x.ask {
            Ask::Refresh | Ask::Apply(_) => Some(State::parse(&payload).map_err(DeviceError::Response)?),
            Ask::Caps { .. } => None,
        };
        let x = self.exchange.take().expect("in an exchange");
        if x.resent {
            self.linger_until = Some(now + self.timing.linger_ms);
        }
        self.online = true;
        match (x.ask, state) {
            (Ask::Refresh, Some(state)) => self.got_state(state, None, now, p, out),
            (Ask::Apply(sent), Some(state)) => self.got_state(state, Some(sent), now, p, out),
            (Ask::Caps { more }, _) => {
                let page = Capabilities::parse(&payload);
                let caps = match self.caps_first.take() {
                    Some(mut first) if more => {
                        first.merge(&page);
                        first
                    }
                    _ => page,
                };
                if caps.additional && !more {
                    // The rest of the list. Only the first query failing is an
                    // error: half a capability list beats none.
                    self.caps_first = Some(caps);
                    self.start(Ask::Caps { more: true }, p, now, out);
                } else {
                    self.got_caps(caps, out);
                }
            }
            _ => unreachable!("a state report for a state query"),
        }
        Ok(())
    }

    fn got_caps(&mut self, caps: Capabilities, out: &mut Vec<(ReqId, Outcome)>) {
        self.caps = Some(caps.clone());
        for r in self.caps_waiters.drain(..) {
            out.push((r, Outcome::Caps(Ok(caps.clone()))));
        }
    }

    fn got_state(&mut self, state: State, sent: Option<Setpoint>, now: u64, p: &mut impl Platform, out: &mut Vec<(ReqId, Outcome)>) {
        self.last_state = Some((state.clone(), now));
        for r in self.readers.drain(..) {
            out.push((r, Outcome::State(Ok(state.clone()))));
        }
        if !self.control_running {
            return;
        }
        match sent {
            // The fresh report a control needed; now send the control.
            None => {
                let (change, _) = self.controls.front().expect("a control is running");
                let mut sp = Setpoint::from_state(&state);
                change.apply_to(&mut sp);
                self.start(Ask::Apply(sp), p, now, out);
            }
            Some(sent) => {
                let (_, waiters) = self.controls.pop_front().expect("a control is running");
                self.control_running = false;
                for r in waiters {
                    out.push((r, Outcome::Control(Ok((sent.clone(), state.clone())))));
                }
            }
        }
    }

    fn failed(&mut self, e: DeviceError, p: &mut impl Platform, now: u64, out: &mut Vec<(ReqId, Outcome)>) {
        self.drop_session(p);
        let Some(x) = self.exchange.as_mut() else { return };
        if e.retryable() && x.attempt < ATTEMPTS {
            x.attempt += 1;
            x.resent = false;
            return self.begin_attempt(p, now, out);
        }
        let x = self.exchange.take().expect("in an exchange");
        self.online = false;
        // Whoever was waiting on this unit learns of the failure, as Breeze
        // Core's readers do from the unit's last failure.
        for r in self.readers.drain(..) {
            out.push((r, Outcome::State(Err(e.clone()))));
        }
        match x.ask {
            Ask::Caps { more: true } => {
                // The second page failing still leaves the first.
                let first = self.caps_first.take().unwrap_or_default();
                self.got_caps(first, out);
            }
            Ask::Caps { more: false } => {
                for r in self.caps_waiters.drain(..) {
                    out.push((r, Outcome::Caps(Err(e.clone()))));
                }
            }
            Ask::Refresh | Ask::Apply(_) if self.control_running => {
                let (_, waiters) = self.controls.pop_front().expect("a control is running");
                self.control_running = false;
                for r in waiters {
                    out.push((r, Outcome::Control(Err(e.clone()))));
                }
            }
            _ => {}
        }
    }
}
