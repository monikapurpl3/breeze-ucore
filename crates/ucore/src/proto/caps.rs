//! What a unit says it can do: the reply to a `0xB5` query, a count followed by
//! `[id_lo, id_hi, size, value...]` records. Most values pick a *set* of
//! features rather than a boolean, so each id has its own accepted values;
//! the tables are msmart's, the only source there is.
//!
//! The answer's shape is built here too (`view`), byte for byte Breeze Core's.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use serde::Serialize;

/// Everything a unit reported. An absent record is an absent feature.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Capabilities {
    pub cool: bool,
    pub heat: bool,
    pub dry: bool,
    pub auto: bool,
    pub swing_horizontal: bool,
    pub swing_vertical: bool,
    fan_silent: bool,
    fan_low: bool,
    fan_medium: bool,
    fan_high: bool,
    fan_auto: bool,
    pub fan_custom: bool,
    saw_fan: bool,
    pub eco: bool,
    pub turbo_cool: bool,
    pub turbo_heat: bool,
    pub freeze_protection: bool,
    pub display_control: bool,
    pub humidity_manual_set: bool,
    pub decimals: bool,
    /// Six limits: cool, auto, heat, each (min, max).
    pub temperatures: [Option<f64>; 6],
    /// More to report: a second query is worth sending.
    pub additional: bool,
    /// Ids seen and not interpreted, so an unknown feature stays visible.
    pub unknown_ids: BTreeSet<u16>,
}

fn one_of(value: &[u8], accepted: &[u8]) -> bool {
    value.first().is_some_and(|v| accepted.contains(v))
}

/// Ids recognised but not interpreted here: Breeze Core reads them into
/// fields no route reports, so they must not count as unknown.
const KNOWN_UNUSED: &[u16] = &[
    0x0009, 0x000A, 0x0018, 0x0030, 0x0032, 0x0033, 0x0039, 0x0040, 0x0042, 0x0043, 0x004B, 0x0059,
    0x0067, 0x0093, 0x0094, 0x00CD, 0x0216, 0x0217, 0x0219, 0x021E, 0x022C,
];

impl Capabilities {
    /// Decode `[0xB5, count, records...]`. Never fails: a truncated record ends
    /// the walk, since half a list beats none from firmware nobody controls.
    pub fn parse(payload: &[u8]) -> Self {
        let mut c = Self::default();
        if payload.len() < 2 {
            return c;
        }
        let mut rest = &payload[2..];
        for _ in 0..payload[1] {
            if rest.len() < 3 {
                break;
            }
            let id = u16::from_le_bytes([rest[0], rest[1]]);
            let size = rest[2] as usize;
            if size == 0 {
                rest = &rest[3..];
                continue;
            }
            if rest.len() < 3 + size {
                break;
            }
            c.apply(id, &rest[3..3 + size], rest);
            rest = &rest[3 + size..];
        }
        if rest.len() > 1 {
            c.additional = rest[rest.len() - 2] != 0;
        }
        c
    }

    /// One record; `record` includes its header, which the temperature
    /// record is read from by absolute offset, as the reference does.
    fn apply(&mut self, id: u16, v: &[u8], record: &[u8]) {
        match id {
            0x0214 => {
                self.heat = one_of(v, &[1, 2, 4, 6, 7, 9, 10, 11, 12, 13]);
                self.cool = one_of(v, &[0, 1, 3, 4, 5, 6, 7, 8, 9, 11, 13, 14, 15]);
                self.dry = one_of(v, &[0, 1, 5, 6, 9, 11, 13, 14, 15]);
                self.auto = one_of(v, &[0, 1, 2, 7, 8, 9, 13, 14]);
            }
            0x0215 => {
                self.swing_horizontal = one_of(v, &[1, 3]);
                self.swing_vertical = one_of(v, &[0, 1]);
            }
            0x0210 => {
                self.saw_fan = true;
                self.fan_silent = one_of(v, &[6, 9]);
                self.fan_low = one_of(v, &[3, 4, 5, 6, 7, 9]);
                self.fan_medium = one_of(v, &[5, 6, 7]);
                self.fan_high = one_of(v, &[3, 4, 5, 6, 7, 9]);
                self.fan_auto = one_of(v, &[4, 5, 6, 9]);
                self.fan_custom = one_of(v, &[1]);
            }
            0x0212 => self.eco = one_of(v, &[1, 2]),
            0x021A => {
                self.turbo_heat = one_of(v, &[1, 3]);
                self.turbo_cool = one_of(v, &[0, 1]);
            }
            0x0213 => self.freeze_protection = one_of(v, &[1]),
            0x0224 => self.display_control = one_of(v, &[1, 2, 100]),
            0x021F => self.humidity_manual_set = one_of(v, &[2, 3]),
            0x0222 => {} // Fahrenheit: read by Breeze Core, reported by no route.
            0x0225 => {
                if record.len() < 9 || v.len() < 6 {
                    return;
                }
                for i in 0..6 {
                    self.temperatures[i] = Some(record[3 + i] as f64 * 0.5);
                }
                self.decimals = if v.len() > 6 && record.len() > 9 { record[9] != 0 } else { record[2] != 0 };
            }
            id if KNOWN_UNUSED.contains(&id) => {}
            other => {
                self.unknown_ids.insert(other);
            }
        }
    }

    /// Fold in a second response: OR, temperatures from whichever had them, and
    /// the second's word on whether there is more.
    pub fn merge(&mut self, o: &Capabilities) {
        macro_rules! or {
            ($($f:ident),*) => { $( self.$f |= o.$f; )* };
        }
        or!(
            cool, heat, dry, auto, swing_horizontal, swing_vertical, fan_silent, fan_low, fan_medium,
            fan_high, fan_auto, fan_custom, saw_fan, eco, turbo_cool, turbo_heat, freeze_protection,
            display_control, humidity_manual_set, decimals
        );
        for i in 0..6 {
            if self.temperatures[i].is_none() {
                self.temperatures[i] = o.temperatures[i];
            }
        }
        self.unknown_ids.extend(o.unknown_ids.iter().copied());
        self.additional = o.additional;
    }

    /// A named fan step. No fan record at all means the reference's default
    /// set (all but SILENT); custom speeds imply every named one.
    fn fan(&self, named: bool, default: bool) -> bool {
        if self.saw_fan {
            named || self.fan_custom
        } else {
            default
        }
    }

    fn swing_both(&self) -> bool {
        self.swing_horizontal && self.swing_vertical
    }

    /// The answer to `GET /api/units/{id}/capabilities`.
    pub fn view(&self, id: String) -> CapabilitiesView {
        let mut modes = Vec::from(["FAN_ONLY"]);
        for (on, name) in [(self.dry, "DRY"), (self.cool, "COOL"), (self.heat, "HEAT"), (self.auto, "AUTO")] {
            if on {
                modes.push(name);
            }
        }
        let mut swing = Vec::from(["OFF"]);
        for (on, name) in
            [(self.swing_horizontal, "HORIZONTAL"), (self.swing_vertical, "VERTICAL"), (self.swing_both(), "BOTH")]
        {
            if on {
                swing.push(name);
            }
        }
        let mut fans = Vec::new();
        for (on, name) in [
            (self.fan(self.fan_silent, false), "SILENT"),
            (self.fan(self.fan_low, true), "LOW"),
            (self.fan(self.fan_medium, true), "MEDIUM"),
            (self.fan(self.fan_high, true), "HIGH"),
            (self.fan(self.fan_auto, true), "AUTO"),
            (self.fan_custom, "MAX"),
        ] {
            if on {
                fans.push(name);
            }
        }
        // "unknown, show everything" when no swing record came at all.
        let known = self.swing_horizontal || self.swing_vertical;
        let t = &self.temperatures;
        let min = [t[0], t[2], t[4]].iter().map(|v| v.unwrap_or(16.0)).fold(f64::INFINITY, f64::min);
        let max = [t[1], t[3], t[5]].iter().map(|v| v.unwrap_or(30.0)).fold(f64::NEG_INFINITY, f64::max);
        CapabilitiesView {
            id,
            operational_modes: modes,
            swing_modes: swing,
            supports_vertical_swing: known.then(|| self.swing_vertical || self.swing_both()),
            supports_horizontal_swing: known.then(|| self.swing_horizontal || self.swing_both()),
            fan_speeds: fans,
            supports_custom_fan_speed: self.fan_custom,
            min_target_temperature: Degrees(min),
            max_target_temperature: Degrees(max),
            supports_eco: self.eco,
            supports_turbo: self.turbo_cool || self.turbo_heat,
            supports_display_control: self.display_control,
            supports_freeze_protection: self.freeze_protection,
            supports_humidity: self.humidity_manual_set,
            unrecognised_capability_ids: self.unknown_ids.iter().map(|id| alloc::format!("0x{id:04X}")).collect(),
        }
    }
}

/// A temperature written as an integer when whole (`16`, not `16.0`), as
/// Breeze Core writes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Degrees(pub f64);

impl Serialize for Degrees {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.0.is_finite() && self.0 == (self.0 as i64) as f64 {
            s.serialize_i64(self.0 as i64)
        } else {
            s.serialize_f64(self.0)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CapabilitiesView {
    pub id: String,
    pub operational_modes: Vec<&'static str>,
    pub swing_modes: Vec<&'static str>,
    pub supports_vertical_swing: Option<bool>,
    pub supports_horizontal_swing: Option<bool>,
    pub fan_speeds: Vec<&'static str>,
    pub supports_custom_fan_speed: bool,
    pub min_target_temperature: Degrees,
    pub max_target_temperature: Degrees,
    pub supports_eco: bool,
    pub supports_turbo: bool,
    pub supports_display_control: bool,
    pub supports_freeze_protection: bool,
    pub supports_humidity: bool,
    pub unrecognised_capability_ids: Vec<String>,
}
