//! Breeze Core's API, the routes μcore keeps: their shapes, their wording,
//! their status codes. Every struct here serialises in the field order Breeze
//! Core's answers have, and the differential test holds it to that.

use crate::proto::ac::{Mode, Setpoint, State, Swing};
use crate::unit::{Change, DeviceError, Unit};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// What `/api/version` advertises: only what μcore has, in Breeze Core's order.
pub const FEATURES: &[&str] = &["batch_state", "beep_control", "control_feedback", "unit_capabilities"];
pub const AUTH_VERSIONS: &[u8] = &[1];

/// Which guard a route sits behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Nothing at all: `/api/health` only.
    Open,
    /// The API key.
    ApiKey,
    /// The API key and a paired device.
    Full,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Route(Guard, Route),
    /// The path exists, not with this method: 405 with `Allow`.
    MethodNotAllowed(&'static str),
    NotFound,
}

/// Breeze Core's routing, less what μcore leaves out (which is then simply not
/// there: 404, as a release without the feature answers).
pub fn resolve(method: &str, path: &str) -> Resolved {
    let fixed: &[(&str, &str, Guard, Option<Route>)] = &[
        ("GET", "/api/health", Guard::Open, Some(Route::Health)),
        ("GET", "/api/version", Guard::ApiKey, Some(Route::Version)),
        ("GET", "/api/units", Guard::Full, Some(Route::Units)),
        ("GET", "/api/units/state", Guard::Full, Some(Route::AllStates)),
        // Known, so that another method is a 405; not built yet (milestone 2).
        ("POST", "/api/units", Guard::Full, None),
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
    let Some(rest) = path.strip_prefix("/api/units/") else { return Resolved::NotFound };
    let Some((_, tail)) = rest.split_once('/') else {
        return match (rest.is_empty(), method) {
            (true, _) => Resolved::NotFound,
            (false, "PATCH" | "DELETE") => Resolved::NotFound, // milestone 2
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

/// A control request, field for field Breeze Core's, so serde's own messages
/// for a malformed one come out the same.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
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
