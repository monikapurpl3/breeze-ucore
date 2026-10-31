//! Breeze Core's API, the routes μCore keeps: their shapes, their wording,
//! their status codes. Every struct here serialises in the field order Breeze
//! Core's answers have, and the differential test holds it to that.

use crate::proto::ac::{Mode, Setpoint, State, Swing};
use crate::unit::{Change, DeviceError, Unit};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// What `/api/version` advertises: only what μCore has, in Breeze Core's order.
pub const FEATURES: &[&str] = &[
    "batch_state",
    "beep_control",
    "config_api",
    "control_feedback",
    "delete_unit",
    "device_pairing",
    "ed25519_auth",
    "sleep_timer",
    "timer_at",
    "unit_capabilities",
    "whoami",
];
pub const AUTH_VERSIONS: &[u8] = &[1, 2];

/// Which guard a route sits behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Nothing at all: `/api/health` only.
    Open,
    /// The API key.
    ApiKey,
    /// The API key and a paired device.
    Full,
    /// The API key and a client on the LAN: approving a pairing, managing
    /// devices. A leaked key must not be enough to gain control.
    AdminLan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Health,
    Version,
    Units,
    AllStates,
    UnitState,
    Capabilities,
    Control,
    EnrollStart,
    EnrollPoll,
    EnrollApprove,
    Devices,
    RevokeDevice,
    Whoami,
    Upgrade,
    Config,
    AddUnit,
    Rename,
    DeleteUnit,
    Timers,
    TimerCreate,
    TimerStatus,
    TimerCancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Route(Guard, Route),
    /// The path exists, not with this method: 405 with `Allow`.
    MethodNotAllowed(&'static str),
    NotFound,
}

/// Breeze Core's routing, in its order, less what μCore leaves out (which is
/// then simply not there: 404, as a release without the feature answers).
/// `None` marks a route of Breeze Core's that μCore has not built yet: known,
/// so that another method gets Breeze Core's 405.
pub fn resolve(method: &str, path: &str) -> Resolved {
    use Guard::*;
    let fixed: &[(&str, &str, Guard, Option<Route>)] = &[
        ("GET", "/api/health", Open, Some(Route::Health)),
        ("GET", "/api/version", ApiKey, Some(Route::Version)),
        ("GET", "/api/units", Full, Some(Route::Units)),
        ("GET", "/api/units/state", Full, Some(Route::AllStates)),
        ("GET", "/api/config", Full, Some(Route::Config)),
        ("POST", "/api/units", Full, Some(Route::AddUnit)),
        ("POST", "/api/auth/enroll/start", ApiKey, Some(Route::EnrollStart)),
        ("POST", "/api/auth/enroll/poll", ApiKey, Some(Route::EnrollPoll)),
        ("POST", "/api/auth/enroll/approve", AdminLan, Some(Route::EnrollApprove)),
        ("GET", "/api/auth/devices", AdminLan, Some(Route::Devices)),
        ("GET", "/api/auth/whoami", Full, Some(Route::Whoami)),
        ("POST", "/api/auth/upgrade", Full, Some(Route::Upgrade)),
        ("GET", "/api/timers/status", Full, Some(Route::TimerStatus)),
        ("GET", "/api/timers", Full, Some(Route::Timers)),
        ("POST", "/api/timers", Full, Some(Route::TimerCreate)),
        ("GET", "/api/programs/status", Full, None),
        ("GET", "/api/programs", Full, None),
        ("POST", "/api/programs", Full, None),
    ];
    if path == "/api/units/stream" || path == "/api/units/scan" {
        return Resolved::NotFound;
    }
    for (m, p, g, r) in fixed {
        if *p == path && *m == method {
            return r.map_or(Resolved::NotFound, |r| Resolved::Route(*g, r));
        }
    }
    for (m, p, _, _) in fixed {
        if *p == path {
            return Resolved::MethodNotAllowed(m);
        }
    }
    if let Some(timer_id) = path.strip_prefix("/api/timers/") {
        if timer_id.is_empty() || timer_id.contains('/') {
            return Resolved::NotFound;
        }
        return if method == "DELETE" { Resolved::Route(Full, Route::TimerCancel) } else { Resolved::MethodNotAllowed("DELETE") };
    }
    if let Some(token_id) = path.strip_prefix("/api/auth/devices/") {
        if token_id.is_empty() || token_id.contains('/') {
            return Resolved::NotFound;
        }
        return if method == "DELETE" { Resolved::Route(AdminLan, Route::RevokeDevice) } else { Resolved::MethodNotAllowed("DELETE") };
    }
    let Some(rest) = path.strip_prefix("/api/units/") else { return Resolved::NotFound };
    let Some((_, tail)) = rest.split_once('/') else {
        return match (rest.is_empty(), method) {
            (true, _) => Resolved::NotFound,
            (false, "PATCH") => Resolved::Route(Guard::Full, Route::Rename),
            (false, "DELETE") => Resolved::Route(Guard::Full, Route::DeleteUnit),
            (false, _) => Resolved::MethodNotAllowed("PATCH"),
        };
    };
    match (tail, method) {
        ("state", "GET") => Resolved::Route(Guard::Full, Route::UnitState),
        ("state", _) => Resolved::MethodNotAllowed("GET"),
        ("capabilities", "GET") => Resolved::Route(Guard::Full, Route::Capabilities),
        ("capabilities", _) => Resolved::MethodNotAllowed("GET"),
        ("control", "POST") => Resolved::Route(Guard::Full, Route::Control),
        ("control", _) => Resolved::MethodNotAllowed("POST"),
        _ => Resolved::NotFound,
    }
}

/// A query parameter, as Breeze Core reads one: the first `key=value` for
/// the key (bare flags skipped), `+` as a space, then percent-decoded.
pub fn query_value(target: &str, key: &str) -> Option<String> {
    let query = target.split_once('?').map_or("", |(_, q)| q);
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        if k == key {
            return Some(percent_decode(&v.replace('+', " ")));
        }
    }
    None
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // As Breeze Core: a `%XY` that ends the string is left alone.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = core::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `{id}` in `/api/units/{id}/...`, as asked for.
pub fn unit_segment(path: &str) -> &str {
    path.strip_prefix("/api/units/").and_then(|r| r.split('/').next()).unwrap_or("")
}

pub fn unknown_unit(path: &str) -> crate::http::Response {
    crate::http::Response::detail(404, &alloc::format!("Unknown unit '{}'", unit_segment(path)))
}

#[derive(Serialize)]
pub struct Version<'a> {
    pub name: &'a str,
    pub version: String,
    pub commit: &'a str,
    pub features: &'a [&'a str],
    pub auth_versions: &'a [u8],
    pub min_auth_version: u8,
    pub units: usize,
}

#[derive(Serialize)]
pub struct UnitSummary<'a> {
    pub id: String,
    pub name: &'a str,
    pub ip: &'a str,
}

/// One unit's state. `id` is a string (48-bit ids go in URLs), enums are names.
#[derive(Serialize)]
pub struct UnitState<'a> {
    pub id: String,
    pub name: &'a str,
    pub ip: &'a str,
    pub online: bool,
    pub power_state: bool,
    pub operational_mode: &'static str,
    pub target_temperature: f64,
    pub indoor_temperature: Option<f64>,
    pub outdoor_temperature: Option<f64>,
    pub fan_speed: u8,
    pub swing_mode: &'static str,
    pub eco: bool,
    pub turbo: bool,
}

impl<'a> UnitState<'a> {
    /// Temperatures widened from f32, as Breeze Core widens them.
    pub fn of(unit: &'a Unit, s: &State) -> Self {
        Self {
            id: unit.id.to_string(),
            name: &unit.name,
            ip: &unit.ip,
            online: true,
            power_state: s.power_on,
            operational_mode: s.mode.map_or("UNKNOWN", Mode::name),
            target_temperature: s.target_temperature as f64,
            indoor_temperature: s.indoor_temperature.map(|t| t as f64),
            outdoor_temperature: s.outdoor_temperature.map(|t| t as f64),
            fan_speed: s.fan_speed,
            swing_mode: s.swing.map_or("UNKNOWN", Swing::name),
            eco: s.eco,
            turbo: s.turbo,
        }
    }
}

/// A control's answer: the state, then which requested fields the unit did
/// not take.
#[derive(Serialize)]
pub struct ControlView<'a> {
    #[serde(flatten)]
    pub state: UnitState<'a>,
    pub not_applied: Vec<&'static str>,
}

#[derive(Serialize)]
pub struct UnitOffline<'a> {
    pub id: String,
    pub name: &'a str,
    pub ip: &'a str,
    pub online: bool,
    pub error: String,
}

impl<'a> UnitOffline<'a> {
    pub fn of(unit: &'a Unit, e: &DeviceError) -> Self {
        Self { id: unit.id.to_string(), name: &unit.name, ip: &unit.ip, online: false, error: e.to_string() }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum UnitView<'a> {
    Online(UnitState<'a>),
    Offline(UnitOffline<'a>),
}

/// `GET /api/units/state`: an envelope, so one unplugged unit lands in
/// `errors` while the rest still come back.
#[derive(Serialize)]
pub struct Envelope<'a> {
    pub states: Vec<UnitView<'a>>,
    pub errors: Vec<UnitView<'a>>,
}

#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

/// Breeze Core's, Midea account fields included: μCore has no cloud, and says
/// so the way a Breeze Core built without one does.
#[derive(Debug, Deserialize)]
pub struct AddUnitRequest {
    pub ip: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub midea_account: Option<String>,
    #[serde(default)]
    pub midea_password: Option<String>,
    #[serde(default)]
    pub midea_region: Option<String>,
}

/// A configured unit as a client sees it: never its token or key, only
/// whether it has them.
#[derive(Serialize)]
pub struct ConfigUnit<'a> {
    pub id: String,
    pub name: &'a str,
    pub ip: &'a str,
    pub port: u16,
    pub has_v3_credentials: bool,
}

impl<'a> From<&'a crate::store::UnitConfig> for ConfigUnit<'a> {
    fn from(u: &'a crate::store::UnitConfig) -> Self {
        Self {
            id: u.id.to_string(),
            name: &u.name,
            ip: &u.ip,
            port: u.port,
            has_v3_credentials: u.token.is_some() && u.key.is_some(),
        }
    }
}

#[derive(Serialize)]
pub struct ConfigView<'a> {
    pub units: Vec<ConfigUnit<'a>>,
}

pub const NAME_MAX_CHARS: usize = 64;

fn one() -> u8 {
    1
}

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    #[serde(default)]
    pub label: String,
    #[serde(default = "one")]
    pub auth_version: u8,
    #[serde(default)]
    pub public_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PollRequest {
    #[serde(default)]
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ApproveRequest {
    #[serde(default)]
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct UpgradeRequest {
    pub public_key: String,
}

#[derive(Serialize)]
pub struct Started<'a> {
    pub session_id: &'a str,
    pub user_code: &'a str,
    pub expires_in: u64,
}

#[derive(Serialize)]
pub struct Approved<'a> {
    pub token_id: &'a str,
    pub label: &'a str,
}

/// A collected pairing; `device_token` only for v1, which hands one over.
#[derive(Serialize)]
pub struct Collected<'a> {
    pub status: &'static str,
    pub token_id: &'a str,
    pub label: &'a str,
    pub auth_version: u8,
    pub expires_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_token: Option<&'a str>,
}

#[derive(Serialize)]
pub struct Status {
    pub status: &'static str,
}

/// A paired device as the API shows it: never its secret or key.
#[derive(Serialize)]
pub struct DeviceView<'a> {
    pub token_id: &'a str,
    pub label: &'a str,
    pub auth_version: u8,
    pub created_at: f64,
    pub expires_at: Option<f64>,
    pub last_used: Option<f64>,
}

impl<'a> From<&'a crate::store::DeviceRecord> for DeviceView<'a> {
    fn from(d: &'a crate::store::DeviceRecord) -> Self {
        Self {
            token_id: &d.token_id,
            label: &d.label,
            auth_version: d.auth_version,
            created_at: d.created_at,
            expires_at: d.expires_at,
            last_used: d.last_used,
        }
    }
}

#[derive(Serialize)]
pub struct Upgraded<'a> {
    pub token_id: &'a str,
    pub auth_version: u8,
}

/// A control request, field for field Breeze Core's, so serde's own messages
/// for a malformed one come out the same, and a stored one (a timer's, a
/// program's) is written with every field, nulls included.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ControlRequest {
    #[serde(default)]
    pub power_state: Option<bool>,
    #[serde(default)]
    pub operational_mode: Option<String>,
    #[serde(default)]
    pub target_temperature: Option<f64>,
    #[serde(default)]
    pub fan_speed: Option<i64>,
    #[serde(default)]
    pub swing_mode: Option<String>,
    #[serde(default)]
    pub eco: Option<bool>,
    #[serde(default)]
    pub turbo: Option<bool>,
    #[serde(default)]
    pub beep: Option<bool>,
}

const ALLOWED_FAN_SPEEDS: [i64; 6] = [20, 40, 60, 80, 100, 102];

impl ControlRequest {
    /// Breeze Core's checks, statuses and words: a bad enum member is 400, an
    /// out-of-range number 422.
    pub fn validate(&self) -> Result<(), (u16, String)> {
        if let Some(t) = self.target_temperature {
            if !(16.0..=30.0).contains(&t) {
                return Err((422, "target_temperature must be between 16 and 30".into()));
            }
            let doubled = t * 2.0;
            if doubled != (doubled as i64) as f64 {
                return Err((422, "target_temperature must be in 0.5 degree steps".into()));
            }
        }
        if let Some(f) = self.fan_speed {
            if !ALLOWED_FAN_SPEEDS.contains(&f) {
                return Err((422, alloc::format!("fan_speed {f} is not one of {ALLOWED_FAN_SPEEDS:?}")));
            }
        }
        if let Some(m) = &self.operational_mode {
            if Mode::from_name(m).is_none() {
                return Err((400, alloc::format!("Unknown mode: {m}")));
            }
        }
        if let Some(s) = &self.swing_mode {
            if Swing::from_name(s).is_none() {
                return Err((400, alloc::format!("Unknown swing mode: {s}")));
            }
        }
        Ok(())
    }

    pub fn change(&self) -> Change {
        Change {
            power_on: self.power_state,
            mode: self.operational_mode.as_deref().and_then(Mode::from_name),
            target_temperature: self.target_temperature.map(|v| v as f32),
            fan_speed: self.fan_speed.map(|v| v as u8),
            swing: self.swing_mode.as_deref().and_then(Swing::from_name),
            eco: self.eco,
            turbo: self.turbo,
            beep: self.beep,
        }
    }

    /// The fields this request asked for that the unit's report does not
    /// show, judged against what was actually sent (which may carry other
    /// requests merged with this one).
    pub fn not_applied(&self, sent: &Setpoint, echoed: &State) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut check = |asked: bool, differs: bool, name| {
            if asked && differs {
                out.push(name);
            }
        };
        check(self.power_state.is_some(), sent.power_on != echoed.power_on, "power_state");
        check(self.operational_mode.is_some(), Some(sent.mode) != echoed.mode, "operational_mode");
        check(
            self.target_temperature.is_some(),
            (sent.target_temperature - echoed.target_temperature).abs() > 0.01,
            "target_temperature",
        );
        check(self.fan_speed.is_some(), sent.fan_speed != echoed.fan_speed, "fan_speed");
        check(self.swing_mode.is_some(), Some(sent.swing) != echoed.swing, "swing_mode");
        check(self.eco.is_some(), sent.eco != echoed.eco, "eco");
        check(self.turbo.is_some(), sent.turbo != echoed.turbo, "turbo");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_matches_breeze_cores_for_what_is_kept() {
        assert_eq!(resolve("GET", "/api/health"), Resolved::Route(Guard::Open, Route::Health));
        assert_eq!(resolve("PUT", "/api/units"), Resolved::MethodNotAllowed("GET"));
        assert_eq!(resolve("GET", "/api/units/123"), Resolved::MethodNotAllowed("PATCH"));
        assert_eq!(resolve("GET", "/api/units/123/control"), Resolved::MethodNotAllowed("POST"));
        assert_eq!(resolve("GET", "/api/units/123/history"), Resolved::NotFound);
        assert_eq!(resolve("GET", "/api/system"), Resolved::NotFound);
        assert_eq!(resolve("GET", "/api/units/scan"), Resolved::NotFound);
        assert_eq!(resolve("POST", "/api/timers"), Resolved::Route(Guard::Full, Route::TimerCreate));
        assert_eq!(resolve("PUT", "/api/timers"), Resolved::MethodNotAllowed("GET"));
        assert_eq!(resolve("GET", "/api/timers/status"), Resolved::Route(Guard::Full, Route::TimerStatus));
        assert_eq!(resolve("GET", "/api/timers/abc"), Resolved::MethodNotAllowed("DELETE"));
        assert_eq!(resolve("DELETE", "/api/timers/a/b"), Resolved::NotFound);
    }

    #[test]
    fn validation_says_what_breeze_core_says() {
        let r = |j: &str| serde_json::from_str::<ControlRequest>(j).unwrap().validate();
        assert_eq!(r(r#"{"target_temperature": 99}"#), Err((422, "target_temperature must be between 16 and 30".into())));
        assert_eq!(r(r#"{"target_temperature": 22.3}"#), Err((422, "target_temperature must be in 0.5 degree steps".into())));
        assert_eq!(r(r#"{"fan_speed": 7}"#), Err((422, "fan_speed 7 is not one of [20, 40, 60, 80, 100, 102]".into())));
        assert_eq!(r(r#"{"operational_mode": "WARP"}"#), Err((400, "Unknown mode: WARP".into())));
        assert_eq!(r(r#"{"swing_mode": "up"}"#), Err((400, "Unknown swing mode: up".into())));
        assert_eq!(r(r#"{"operational_mode": "cool", "unknown": 1}"#), Ok(()));
    }
}
