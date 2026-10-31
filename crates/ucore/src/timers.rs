//! One-shot timers, as Breeze Core keeps them in `timers.json` and answers
//! for them on `/api/timers`.
//!
//! A client asks for **minutes**, or for a number of days and an `HH:MM`, and
//! the server computes the moment from its own clock: a phone in another zone,
//! or with a drifting clock, never decides when a unit switches. Two kinds, at
//! most one of each per unit: a **sleep** timer (in N minutes, apply the
//! settings, by default off) and a **scheduled start** (on day D at HH:MM,
//! switch on).

use crate::api::ControlRequest;
use crate::clock::{self, LocalMs};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

/// A day: "I am going to sleep", not a scheduling system.
pub const MAX_MINUTES: u32 = 24 * 60;
/// A long holiday; anything further, or repeating, is a program.
pub const MAX_START_DAYS: u32 = 30;
pub const MAX_UNITS: usize = 64;
/// How late a scheduled start may still fire: a restart, not a day.
pub const START_GRACE_MS: i64 = 15 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Sleep,
    Start,
}

impl Kind {
    fn is_sleep(&self) -> bool {
        *self == Kind::Sleep
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Sleep => "sleep",
            Kind::Start => "start",
        }
    }
}

/// Field for field Breeze Core's stored timer: the settings keep their nulls,
/// and `kind` is written only for a start, so files from before starts
/// existed stay byte-for-byte what they were.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timer {
    pub id: String,
    #[serde(default)]
    pub unit_ids: Vec<String>,
    pub minutes: u32,
    pub created_at: String,
    pub fires_at: String,
    pub settings: ControlRequest,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Kind::is_sleep")]
    pub kind: Kind,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TimersDoc {
    #[serde(default)]
    pub timers: Vec<Timer>,
}

#[derive(Debug, Deserialize)]
pub struct TimerRequest {
    #[serde(default)]
    pub unit_ids: Vec<String>,
    #[serde(default)]
    pub minutes: Option<u32>,
    #[serde(default)]
    pub days: Option<u32>,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub settings: Option<ControlRequest>,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, PartialEq)]
pub enum When {
    In(u32),
    At { days: u32, at: String },
}

impl TimerRequest {
    /// Exactly one thing asked for, or Breeze Core's reason why not.
    pub fn when(&self) -> Result<When, &'static str> {
        match (self.minutes, &self.at) {
            (Some(_), Some(_)) => Err("give minutes or at, not both"),
            (None, None) if self.days.is_some() => Err("days needs at (an HH:MM)"),
            (None, None) => Err("say when: minutes, or at (with days)"),
            (Some(_), None) if self.days.is_some() => Err("days goes with at, not with minutes"),
            (Some(m), None) => Ok(When::In(m)),
            (None, Some(_)) if self.settings.is_some() => {
                Err("a scheduled start only switches the unit on; settings are for sleep timers")
            }
            (None, Some(at)) => Ok(When::At { days: self.days.unwrap_or(0), at: at.clone() }),
        }
    }
}

fn power(on: bool) -> ControlRequest {
    ControlRequest { power_state: Some(on), ..ControlRequest::default() }
}

/// A sleep timer, `minutes` after `now`.
pub fn build_sleep(id: String, unit_ids: Vec<String>, minutes: u32, settings: Option<ControlRequest>, label: &str, now: LocalMs) -> Result<Timer, String> {
    if minutes == 0 || minutes > MAX_MINUTES {
        return Err(alloc::format!("minutes must be between 1 and {MAX_MINUTES}, got {minutes}"));
    }
    if unit_ids.len() > MAX_UNITS {
        return Err(alloc::format!("at most {MAX_UNITS} units, got {}", unit_ids.len()));
    }
    let created = clock::seconds(now);
    Ok(Timer {
        id,
        unit_ids,
        minutes,
        created_at: clock::format(created),
        fires_at: clock::format(created + i64::from(minutes) * 60),
        settings: settings.unwrap_or_else(|| power(false)),
        label: label.to_string(),
        kind: Kind::Sleep,
    })
}

/// A scheduled start: `days` after the server's today, at `at` on its clock.
/// `minutes` is still filled in (rounded up to the moment), because every
/// older client reads it.
pub fn build_start(id: String, unit_ids: Vec<String>, days: u32, at: &str, label: &str, now: LocalMs) -> Result<Timer, String> {
    if days > MAX_START_DAYS {
        return Err(alloc::format!("days must be between 0 and {MAX_START_DAYS}, got {days}"));
    }
    if unit_ids.len() > MAX_UNITS {
        return Err(alloc::format!("at most {MAX_UNITS} units, got {}", unit_ids.len()));
    }
    let Some(time) = clock::parse_hhmm(at) else {
        return Err(alloc::format!("at must be a time as HH:MM, got {at:?}"));
    };
    let fires = (clock::seconds(now).div_euclid(clock::DAY_S) + i64::from(days)) * clock::DAY_S + time;
    if fires * 1000 <= now {
        return Err(alloc::format!("{} has already passed on the server's clock", clock::format_on(fires)));
    }
    let seconds = (fires * 1000 - now) / 1000;
    Ok(Timer {
        id,
        unit_ids,
        minutes: ((seconds + 59) / 60) as u32,
        created_at: clock::format(clock::seconds(now)),
        fires_at: clock::format(fires),
        settings: power(true),
        label: label.to_string(),
        kind: Kind::Start,
    })
}

impl Timer {
    /// Whole seconds until it fires, never below zero; an unreadable time is
    /// due now, because "off, late" is the safe way to be wrong.
    pub fn seconds_remaining(&self, now: LocalMs) -> i64 {
        clock::parse(&self.fires_at).map_or(0, |due| ((due * 1000 - now) / 1000).max(0))
    }

    pub fn is_due(&self, now: LocalMs) -> bool {
        self.seconds_remaining(now) == 0
    }

    /// A start whose moment went by more than the grace ago, while the server
    /// was not running: dropped, not fired. "On at 07:30" happening at 14:00
    /// is not what anybody asked for; a late "off" is harmless.
    pub fn missed(&self, now: LocalMs) -> bool {
        self.kind == Kind::Start && clock::parse(&self.fires_at).is_none_or(|due| now - due * 1000 > START_GRACE_MS)
    }

    /// An empty list means every unit.
    pub fn covers(&self, unit_id: &str) -> bool {
        self.unit_ids.is_empty() || self.unit_ids.iter().any(|u| u == unit_id)
    }

    /// The wire form: settings without their nulls, `seconds_remaining` from
    /// the server's clock, and `kind` always said.
    pub fn view(&self, now: LocalMs) -> TimerView<'_> {
        TimerView { timer: self, remaining: self.seconds_remaining(now) }
    }
}

impl TimersDoc {
    /// A new timer replaces any of its kind already promising something about
    /// these units: "off in 30" twice means a change of mind.
    pub fn replace_for_units(&mut self, unit_ids: &[String], kind: Kind) {
        self.timers.retain(|t| t.kind != kind || !unit_ids.iter().any(|u| t.covers(u)));
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.timers.len();
        self.timers.retain(|t| t.id != id);
        self.timers.len() != before
    }

    pub fn next_fires_at(&self) -> Option<&str> {
        self.timers.iter().map(|t| t.fires_at.as_str()).min()
    }
}

/// Which kinds a listing shows, from `?kind=`: sleep timers only unless asked,
/// so a client from before starts never reads one as "off in 53 h".
pub fn listed_kinds(kind: Option<&str>) -> Result<&'static [Kind], String> {
    match kind {
        None | Some("sleep") => Ok(&[Kind::Sleep]),
        Some("start") => Ok(&[Kind::Start]),
        Some("all") => Ok(&[Kind::Sleep, Kind::Start]),
        Some(other) => Err(alloc::format!("kind must be sleep, start or all, not {other:?}")),
    }
}

pub struct TimerView<'a> {
    timer: &'a Timer,
    remaining: i64,
}

/// Breeze Core builds this as a JSON object from the stored form and then
/// sets two keys: `seconds_remaining` lands at the end, and `kind` where it
/// already was (a start) or after it (a sleep timer).
impl Serialize for TimerView<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let t = self.timer;
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("id", &t.id)?;
        m.serialize_entry("unit_ids", &t.unit_ids)?;
        m.serialize_entry("minutes", &t.minutes)?;
        m.serialize_entry("created_at", &t.created_at)?;
        m.serialize_entry("fires_at", &t.fires_at)?;
        m.serialize_entry("settings", &Present(&t.settings))?;
        m.serialize_entry("label", &t.label)?;
        if t.kind == Kind::Start {
            m.serialize_entry("kind", t.kind.as_str())?;
            m.serialize_entry("seconds_remaining", &self.remaining)?;
        } else {
            m.serialize_entry("seconds_remaining", &self.remaining)?;
            m.serialize_entry("kind", t.kind.as_str())?;
        }
        m.end()
    }
}

/// A control request without its nulls.
struct Present<'a>(&'a ControlRequest);

impl Serialize for Present<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let c = self.0;
        let mut m = s.serialize_map(None)?;
        macro_rules! some {
            ($($f:ident),*) => {$(if let Some(v) = &c.$f { m.serialize_entry(stringify!($f), v)?; })*};
        }
        some!(power_state, operational_mode, target_temperature, fan_speed, swing_mode, eco, turbo, beep);
        m.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> LocalMs {
        clock::parse(text).unwrap() * 1000
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_sleep_timer_fires_the_minutes_later_and_switches_off() {
        let t = build_sleep("t1".into(), ids(&["1"]), 45, None, "bedtime", at("2026-08-23T21:00:00") + 700).unwrap();
        assert_eq!((t.created_at.as_str(), t.fires_at.as_str()), ("2026-08-23T21:00:00", "2026-08-23T21:45:00"));
        assert_eq!(t.settings, power(false));
        assert_eq!(build_sleep("t".into(), vec![], 0, None, "", 0), Err("minutes must be between 1 and 1440, got 0".into()));
        assert_eq!(build_sleep("t".into(), vec![], 1441, None, "", 0), Err("minutes must be between 1 and 1440, got 1441".into()));
    }

    #[test]
    fn a_start_is_on_the_servers_date_and_never_in_the_past() {
        let now = at("2026-09-28T07:00:00") + 400;
        let s = build_start("s1".into(), ids(&["1"]), 1, "07:30", "", now).unwrap();
        assert_eq!(s.fires_at, "2026-09-29T07:30:00");
        // 24 h 29 min 59.6 s, rounded up.
        assert_eq!(s.minutes, 1470);
        assert_eq!(build_start("s".into(), vec![], 0, "06:59", "", now), Err("06:59 on 2026-09-28 has already passed on the server's clock".into()));
        assert_eq!(build_start("s".into(), vec![], 31, "07:30", "", now), Err("days must be between 0 and 30, got 31".into()));
        assert_eq!(build_start("s".into(), vec![], 0, "7.30", "", now), Err("at must be a time as HH:MM, got \"7.30\"".into()));
    }

    #[test]
    fn a_start_missed_by_more_than_the_grace_is_dropped() {
        let s = build_start("s1".into(), ids(&["1"]), 1, "07:30", "", at("2026-09-28T07:00:00")).unwrap();
        assert!(!s.missed(at("2026-09-29T07:45:00")));
        assert!(s.missed(at("2026-09-29T07:45:01")));
        let sleep = build_sleep("t1".into(), ids(&["1"]), 30, None, "", at("2026-09-28T07:00:00")).unwrap();
        assert!(!sleep.missed(at("2026-10-01T09:00:00")));
        let broken = Timer { fires_at: "whenever".into(), ..s };
        assert!(broken.missed(0) && broken.is_due(0));
    }

    #[test]
    fn due_counts_whole_seconds_toward_zero_as_chrono_does() {
        let t = build_sleep("t".into(), vec![], 1, None, "", at("2026-01-01T00:00:00")).unwrap();
        assert_eq!(t.seconds_remaining(at("2026-01-01T00:00:00")), 60);
        assert_eq!(t.seconds_remaining(at("2026-01-01T00:00:59") + 1), 0);
        assert!(!t.is_due(at("2026-01-01T00:00:58") + 999));
    }

    #[test]
    fn one_of_each_kind_per_unit() {
        let now = at("2026-01-01T12:00:00");
        let mut doc = TimersDoc::default();
        doc.timers.push(build_sleep("a".into(), ids(&["1"]), 30, None, "", now).unwrap());
        doc.timers.push(build_start("b".into(), ids(&["1"]), 1, "07:00", "", now).unwrap());
        doc.timers.push(build_sleep("c".into(), vec![], 30, None, "", now).unwrap());
        doc.replace_for_units(&ids(&["1"]), Kind::Sleep);
        assert_eq!(doc.timers.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn the_wire_form_has_breeze_cores_key_order() {
        let now = at("2026-01-01T12:00:00");
        let sleep = build_sleep("a".into(), ids(&["1"]), 30, None, "x", now).unwrap();
        assert_eq!(
            serde_json::to_string(&sleep.view(now)).unwrap(),
            r#"{"id":"a","unit_ids":["1"],"minutes":30,"created_at":"2026-01-01T12:00:00","fires_at":"2026-01-01T12:30:00","settings":{"power_state":false},"label":"x","seconds_remaining":1800,"kind":"sleep"}"#
        );
        let start = build_start("b".into(), vec![], 0, "13:00", "", now).unwrap();
        assert!(serde_json::to_string(&start.view(now)).unwrap().ends_with(r#""label":"","kind":"start","seconds_remaining":3600}"#));
        assert_eq!(
            serde_json::to_string(&sleep).unwrap(),
            r#"{"id":"a","unit_ids":["1"],"minutes":30,"created_at":"2026-01-01T12:00:00","fires_at":"2026-01-01T12:30:00","settings":{"power_state":false,"operational_mode":null,"target_temperature":null,"fan_speed":null,"swing_mode":null,"eco":null,"turbo":null,"beep":null},"label":"x"}"#
        );
    }
}
