//! Programs, as Breeze Core keeps them in `programs.json` and runs them:
//! favourites (a named scene, applied when asked), schedules (day and time
//! triggers) and curves (a daily time-to-temperature curve, tracked).
//!
//! The arithmetic is Breeze Core's to the bit: the same interpolation, and
//! banker's rounding to the half degree, because Python's `round()` is what
//! the curves were first set by.

use crate::api::ControlRequest;
use crate::clock;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

pub const KINDS: [&str; 3] = ["favourite", "schedule", "curve"];
pub const NAME_MAX_CHARS: usize = 64;
const MODES: [&str; 5] = ["AUTO", "COOL", "DRY", "HEAT", "FAN_ONLY"];
const FAN_SPEEDS: [i64; 6] = [20, 40, 60, 80, 100, 102];
const MINUTES_PER_DAY: i64 = 1440;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleEntry {
    /// 0 = Monday .. 6 = Sunday; empty is every day.
    #[serde(default)]
    pub days: Vec<u8>,
    pub time: String,
    pub settings: ControlRequest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    pub time: String,
    pub temperature: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurveConfig {
    #[serde(default = "default_mode")]
    pub operational_mode: String,
    #[serde(default = "default_fan")]
    pub fan_speed: i64,
    #[serde(default)]
    pub points: Vec<CurvePoint>,
}

fn default_mode() -> String {
    "COOL".into()
}

fn default_fan() -> i64 {
    102
}

fn default_true() -> bool {
    true
}

fn default_kind() -> String {
    "favourite".into()
}

/// `id` last: Breeze Core's `Program` subclasses the spec, so pydantic
/// appended it, and the file says so.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub unit_ids: Vec<String>,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub favourite: Option<ControlRequest>,
    #[serde(default)]
    pub schedule: Vec<ScheduleEntry>,
    #[serde(default)]
    pub curve: Option<CurveConfig>,
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProgramsDoc {
    #[serde(default)]
    pub programs: Vec<Program>,
}

/// What a client sends: a program without its id.
#[derive(Debug, Clone, Deserialize)]
pub struct ProgramSpec {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub unit_ids: Vec<String>,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub favourite: Option<ControlRequest>,
    #[serde(default)]
    pub schedule: Vec<ScheduleEntry>,
    #[serde(default)]
    pub curve: Option<CurveConfig>,
}

/// Strict `HH:MM` for what a client sends, as Breeze Core's
/// `^([01]\d|2[0-3]):([0-5]\d)$`.
pub fn valid_hhmm(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' || !b.iter().enumerate().all(|(i, c)| i == 2 || c.is_ascii_digit()) {
        return false;
    }
    (b[0] - b'0') * 10 + (b[1] - b'0') <= 23 && (b[3] - b'0') * 10 + (b[4] - b'0') <= 59
}

/// Lenient minutes past midnight, for what is already on disk.
pub fn minutes_of_day(hhmm: &str) -> Option<i64> {
    let (h, m) = hhmm.split_once(':')?;
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some(h * 60 + m)
}

/// The settings' own check, in Breeze Core's words; the status is the
/// route's to choose.
fn settings_ok(s: &ControlRequest) -> Result<(), String> {
    s.validate().map_err(|(_, m)| m)
}

impl ProgramSpec {
    /// Validate, normalise as Breeze Core does before storing (days sorted
    /// and deduplicated, a curve's mode upper-cased), and attach the id.
    pub fn into_program(mut self, id: String) -> Result<Program, String> {
        let chars = self.name.chars().count();
        if chars == 0 {
            return Err("name must not be empty".into());
        }
        if chars > NAME_MAX_CHARS {
            return Err(alloc::format!("name must be at most {NAME_MAX_CHARS} characters, got {chars}"));
        }
        if !KINDS.contains(&self.kind.as_str()) {
            return Err(alloc::format!("kind must be one of {}, got '{}'", KINDS.join(", "), self.kind));
        }
        if let Some(fav) = &self.favourite {
            settings_ok(fav)?;
        }
        for entry in &mut self.schedule {
            if let Some(bad) = entry.days.iter().find(|d| **d > 6) {
                return Err(alloc::format!("days must be 0 (Mon) .. 6 (Sun), got {bad}"));
            }
            entry.days.sort_unstable();
            entry.days.dedup();
            if !valid_hhmm(&entry.time) {
                return Err(alloc::format!("time must be 'HH:MM' (24h), got '{}'", entry.time));
            }
            settings_ok(&entry.settings)?;
        }
        if let Some(curve) = &mut self.curve {
            curve.operational_mode = curve.operational_mode.to_uppercase();
            if !MODES.contains(&curve.operational_mode.as_str()) {
                return Err(alloc::format!("operational_mode must be one of {}, got '{}'", MODES.join(", "), curve.operational_mode));
            }
            if !FAN_SPEEDS.contains(&curve.fan_speed) {
                return Err(alloc::format!("fan_speed must be one of {FAN_SPEEDS:?}, got {}", curve.fan_speed));
            }
            for point in &curve.points {
                if !valid_hhmm(&point.time) {
                    return Err(alloc::format!("time must be 'HH:MM' (24h), got '{}'", point.time));
                }
                if !(16.0..=30.0).contains(&point.temperature) {
                    return Err(alloc::format!("curve temperatures must be between 16 and 30, got {}", point.temperature));
                }
            }
        }
        Ok(Program {
            name: self.name,
            enabled: self.enabled,
            unit_ids: self.unit_ids,
            kind: self.kind,
            favourite: self.favourite,
            schedule: self.schedule,
            curve: self.curve,
            id,
        })
    }
}

impl ProgramsDoc {
    pub fn get(&self, id: &str) -> Option<&Program> {
        self.programs.iter().find(|p| p.id == id)
    }
}

// No libm in a no_std core: these are exact for every value a curve makes,
// and agree with f64's own methods (|x| < 2^52, which the casts need).
fn trunc(x: f64) -> f64 {
    x as i64 as f64
}

fn floor(x: f64) -> f64 {
    let t = trunc(x);
    if x < t {
        t - 1.0
    } else {
        t
    }
}

/// Half away from zero, as `f64::round`.
fn round(x: f64) -> f64 {
    let t = trunc(x);
    let frac = x - t;
    if frac >= 0.5 {
        t + 1.0
    } else if frac <= -0.5 {
        t - 1.0
    } else {
        t
    }
}

/// Snap to the nearest half degree with banker's rounding (Python's
/// `round()`), clamped to 16..30.
pub fn round_half(t: f64) -> f64 {
    let doubled = t * 2.0;
    let off = doubled - trunc(doubled);
    let rounded = if off == 0.5 || off == -0.5 {
        let lower = floor(doubled);
        if (lower as i64) % 2 == 0 {
            lower
        } else {
            lower + 1.0
        }
    } else {
        round(doubled)
    };
    (rounded / 2.0).clamp(16.0, 30.0)
}

/// The setpoint for this minute of the day, the points read as a cycle:
/// a curve with points at 08:00 and 22:00 still says something at 03:00.
pub fn curve_setpoint(points: &[CurvePoint], minute: i64) -> Option<f64> {
    let mut pts: Vec<(i64, f64)> = points.iter().filter_map(|p| minutes_of_day(&p.time).map(|m| (m, p.temperature))).collect();
    if pts.is_empty() {
        return None;
    }
    pts.sort_by_key(|(m, _)| *m);
    if pts.len() == 1 {
        return Some(round_half(pts[0].1));
    }
    let n = pts.len();
    for i in 0..n {
        let (am, at) = pts[i];
        let (bm, bt) = pts[(i + 1) % n];
        let span = match (bm - am).rem_euclid(MINUTES_PER_DAY) {
            0 => MINUTES_PER_DAY,
            s => s,
        };
        let off = (minute - am).rem_euclid(MINUTES_PER_DAY);
        if off < span {
            return Some(round_half(at + (bt - at) * (off as f64 / span as f64)));
        }
    }
    Some(round_half(pts[0].1))
}

/// The minute of the day of a local time, seconds ignored.
pub fn minute_of(local_secs: i64) -> i64 {
    local_secs.rem_euclid(clock::DAY_S) / 60
}

/// What a curve wants now: on, in its mode and fan, at the curve's setpoint
/// (none when it has no points; the scheduler then leaves it alone).
pub fn curve_request(curve: &CurveConfig, local_secs: i64) -> ControlRequest {
    ControlRequest {
        power_state: Some(true),
        operational_mode: Some(curve.operational_mode.clone()),
        fan_speed: Some(curve.fan_speed),
        target_temperature: curve_setpoint(&curve.points, minute_of(local_secs)),
        ..ControlRequest::default()
    }
}

/// The schedule entries due this minute, by index: the right weekday (or
/// every day) and the time as written.
pub fn due_entries(entries: &[ScheduleEntry], local_secs: i64) -> Vec<usize> {
    let weekday = clock::weekday(local_secs) as u8;
    let minute = minute_of(local_secs);
    let hhmm = alloc::format!("{:02}:{:02}", minute / 60, minute % 60);
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| (e.days.is_empty() || e.days.contains(&weekday)) && e.time == hhmm)
        .map(|(i, _)| i)
        .collect()
}

/// `YYYY-MM-DD HH:MM`: an entry fires once under each.
pub fn minute_stamp(local_secs: i64) -> String {
    let mut s = clock::format(local_secs);
    s.truncate(16);
    s.replace('T', " ")
}

impl Program {
    pub fn is_schedule(&self) -> bool {
        self.kind == "schedule"
    }

    pub fn is_curve(&self) -> bool {
        self.kind == "curve"
    }

    /// Its units, or every unit (in configuration order).
    pub fn targets(&self, all: impl Iterator<Item = u64>) -> Vec<u64> {
        if self.unit_ids.is_empty() {
            all.collect()
        } else {
            self.unit_ids.iter().filter_map(|s| s.parse().ok()).collect()
        }
    }
}

/// Program ids as unit ids are compared: as text.
pub fn key(program: &Program, unit: u64) -> (String, String) {
    (program.id.clone(), unit.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pts(list: &[(&str, f64)]) -> Vec<CurvePoint> {
        list.iter().map(|(t, v)| CurvePoint { time: t.to_string(), temperature: *v }).collect()
    }

    #[test]
    fn rounding_is_bankers_as_pythons() {
        // Python: round(40.5) == 40, round(41.5) == 42.
        assert_eq!(round_half(20.25), 20.0);
        assert_eq!(round_half(20.75), 21.0);
        assert_eq!(round_half(20.3), 20.5);
        assert_eq!(round_half(20.1), 20.0);
        assert_eq!(round_half(35.0), 30.0);
        assert_eq!(round_half(3.0), 16.0);
        for x in [16.0, 18.37, 22.4999, 22.5, 29.76] {
            let doubled: f64 = x * 2.0;
            let off = doubled - doubled.trunc();
            let expect = if off.abs() == 0.5 { if (doubled.floor() as i64) % 2 == 0 { doubled.floor() } else { doubled.floor() + 1.0 } } else { doubled.round() };
            assert_eq!(round_half(x), (expect / 2.0).clamp(16.0, 30.0), "{x}");
        }
    }

    #[test]
    fn a_curve_is_a_cycle_over_the_day() {
        let p = pts(&[("08:00", 20.0), ("22:00", 24.0)]);
        assert_eq!(curve_setpoint(&p, 8 * 60), Some(20.0));
        assert_eq!(curve_setpoint(&p, 15 * 60), Some(22.0));
        // 22:00 to 08:00 through midnight: 03:00 is half way.
        assert_eq!(curve_setpoint(&p, 3 * 60), Some(22.0));
        assert_eq!(curve_setpoint(&pts(&[("12:00", 23.3)]), 0), Some(23.5));
        assert_eq!(curve_setpoint(&[], 0), None);
        assert_eq!(curve_setpoint(&pts(&[("noon", 23.0)]), 0), None);
    }

    #[test]
    fn a_spec_is_checked_and_normalised_as_breeze_core_does() {
        let spec = |j: &str| serde_json::from_str::<ProgramSpec>(j).unwrap().into_program("p1".into());
        let p = spec(r#"{"name": "Week", "kind": "schedule", "schedule": [{"days": [4, 0, 4], "time": "07:30", "settings": {"power_state": true}}]}"#).unwrap();
        assert_eq!(p.schedule[0].days, [0, 4]);
        let c = spec(r#"{"name": "Warm", "kind": "curve", "curve": {"operational_mode": "heat", "points": [{"time": "06:00", "temperature": 21}]}}"#).unwrap();
        assert_eq!(c.curve.as_ref().unwrap().operational_mode, "HEAT");
        for (body, err) in [
            (r#"{"name": ""}"#, "name must not be empty"),
            (r#"{"name": "x", "kind": "macro"}"#, "kind must be one of favourite, schedule, curve, got 'macro'"),
            (r#"{"name": "x", "kind": "schedule", "schedule": [{"days": [7], "time": "07:30", "settings": {}}]}"#, "days must be 0 (Mon) .. 6 (Sun), got 7"),
            (r#"{"name": "x", "kind": "schedule", "schedule": [{"time": "7:30", "settings": {}}]}"#, "time must be 'HH:MM' (24h), got '7:30'"),
            (r#"{"name": "x", "kind": "curve", "curve": {"operational_mode": "warp"}}"#, "operational_mode must be one of AUTO, COOL, DRY, HEAT, FAN_ONLY, got 'WARP'"),
            (r#"{"name": "x", "kind": "curve", "curve": {"fan_speed": 7}}"#, "fan_speed must be one of [20, 40, 60, 80, 100, 102], got 7"),
            (r#"{"name": "x", "kind": "curve", "curve": {"points": [{"time": "06:00", "temperature": 31.5}]}}"#, "curve temperatures must be between 16 and 30, got 31.5"),
            (r#"{"name": "x", "favourite": {"fan_speed": 7}}"#, "fan_speed 7 is not one of [20, 40, 60, 80, 100, 102]"),
        ] {
            assert_eq!(spec(body), Err(err.into()), "{body}");
        }
    }

    #[test]
    fn an_entry_is_due_on_its_weekday_and_minute() {
        let e = |days: &[u8], time: &str| ScheduleEntry { days: days.to_vec(), time: time.into(), settings: ControlRequest::default() };
        // A Monday.
        let monday = clock::parse("2026-10-26T07:30:45").unwrap();
        let entries = [e(&[0], "07:30"), e(&[1], "07:30"), e(&[], "07:30"), e(&[0], "07:31")];
        assert_eq!(due_entries(&entries, monday), [0, 2]);
        assert_eq!(minute_stamp(monday), "2026-10-26 07:30");
    }
}
