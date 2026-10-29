//! Air-conditioner commands and state reports.
//!
//! The wire values are the protocol's and some are counter-intuitive: swing
//! **HORIZONTAL is 0x3 and VERTICAL is 0xC**. The names are Breeze Core's REST
//! API's (`AUTO COOL DRY HEAT FAN_ONLY`, `OFF VERTICAL HORIZONTAL BOTH`).

use super::frame;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto = 1,
    Cool = 2,
    Dry = 3,
    Heat = 4,
    FanOnly = 5,
}

impl Mode {
    pub fn from_wire(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Auto,
            2 => Self::Cool,
            3 => Self::Dry,
            4 => Self::Heat,
            5 => Self::FanOnly,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Cool => "COOL",
            Self::Dry => "DRY",
            Self::Heat => "HEAT",
            Self::FanOnly => "FAN_ONLY",
        }
    }

    /// Upper-cased first, as Breeze Core does (Unicode upper case, not ASCII:
    /// a dotless ı becomes I there too).
    pub fn from_name(name: &str) -> Option<Self> {
        let upper = name.to_uppercase();
        [Self::Auto, Self::Cool, Self::Dry, Self::Heat, Self::FanOnly]
            .into_iter()
            .find(|m| m.name() == upper)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swing {
    Off = 0x0,
    Horizontal = 0x3,
    Vertical = 0xC,
    Both = 0xF,
}

impl Swing {
    pub fn from_wire(v: u8) -> Option<Self> {
        Some(match v {
            0x0 => Self::Off,
            0x3 => Self::Horizontal,
            0xC => Self::Vertical,
            0xF => Self::Both,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Horizontal => "HORIZONTAL",
            Self::Vertical => "VERTICAL",
            Self::Both => "BOTH",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        let upper = name.to_uppercase();
        [Self::Off, Self::Vertical, Self::Horizontal, Self::Both]
            .into_iter()
            .find(|s| s.name() == upper)
    }
}

/// Fan speed: 20/40/60/80/100 are steps, 102 is "the unit decides".
pub const FAN_AUTO: u8 = 102;

/// Query state, indoor sensor. Fixed bytes the units expect, reproduced exactly.
pub fn get_state(message_id: u8) -> Vec<u8> {
    let payload = [
        0x41, 0x81, 0x00, 0xFF, 0x03, 0xFF, 0x00, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x03,
    ];
    frame::command(frame::QUERY, &payload, message_id)
}

/// Query capabilities; `more` asks for the second half some units keep back.
pub fn get_capabilities(message_id: u8, more: bool) -> Vec<u8> {
    if more {
        frame::command(frame::QUERY, &[0xB5, 0x01, 0x01, 0x01], message_id)
    } else {
        frame::command(frame::QUERY, &[0xB5, 0x01, 0x00], message_id)
    }
}

/// Everything a control command carries. A unit has no "change only the fan":
/// every command restates the whole configuration, so a partial change is laid
/// over the last report ([`Setpoint::from_state`]), never over defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Setpoint {
    pub power_on: bool,
    pub mode: Mode,
    pub target_temperature: f32,
    pub fan_speed: u8,
    pub swing: Swing,
    pub eco: bool,
    pub turbo: bool,
    pub sleep: bool,
    pub fahrenheit: bool,
    pub purifier: bool,
    pub aux_heat: bool,
    pub independent_aux_heat: bool,
    pub follow_me: bool,
    pub freeze_protection: bool,
    pub target_humidity: u8,
    /// Off unless asked: a schedule at 2 a.m. should be silent.
    pub beep: bool,
}

impl Setpoint {
    pub fn from_state(s: &State) -> Self {
        Self {
            power_on: s.power_on,
            mode: s.mode.unwrap_or(Mode::Auto),
            target_temperature: s.target_temperature,
            fan_speed: s.fan_speed,
            swing: s.swing.unwrap_or(Swing::Off),
            eco: s.eco,
            turbo: s.turbo,
            sleep: s.sleep,
            fahrenheit: s.fahrenheit,
            purifier: s.purifier,
            aux_heat: s.aux_heat,
            independent_aux_heat: s.independent_aux_heat,
            follow_me: s.follow_me,
            freeze_protection: s.freeze_protection.unwrap_or(false),
            target_humidity: s.target_humidity.unwrap_or(40),
            beep: false,
        }
    }
}

const CONTROL_SOURCE: u8 = 0x02;

fn flag(on: bool, bit: u8) -> u8 {
    if on {
        bit
    } else {
        0
    }
}

/// A control command for a whole setpoint.
///
/// 17-30 °C goes in byte 2's low nibble (biased by 16); anything else, 16 °C
/// included, in byte 18 (biased by 12). The half-degree bit applies either way.
pub fn set_state(message_id: u8, s: &Setpoint) -> Vec<u8> {
    let integral = s.target_temperature as i32;
    let half = s.target_temperature - integral as f32 > 0.0;
    let (mut temp, temp_alt) = if (17..=30).contains(&integral) {
        (((integral - 16) as u8) & 0xF, 0u8)
    } else {
        (0u8, ((integral - 12) as u8) & 0x1F)
    };
    temp |= flag(half, 0x10);
    let payload = [
        0x40,
        CONTROL_SOURCE | flag(s.beep, 0x40) | flag(s.power_on, 0x01),
        temp | ((s.mode as u8 & 0x7) << 5),
        s.fan_speed,
        0x7F,
        0x7F,
        0x00,
        0x30 | (s.swing as u8 & 0x3F),
        flag(s.follow_me, 0x80) | flag(s.turbo, 0x20),
        flag(s.eco, 0x80) | flag(s.purifier, 0x20) | flag(s.aux_heat, 0x08),
        flag(s.sleep, 0x01) | flag(s.turbo, 0x02) | flag(s.fahrenheit, 0x04),
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        temp_alt,
        s.target_humidity & 0x7F,
        0x00,
        flag(s.freeze_protection, 0x80),
        flag(s.independent_aux_heat, 0x08),
        0x00,
    ];
    frame::command(frame::CONTROL, &payload, message_id)
}

/// A `0xC0` state report. `None` where the protocol has an absent value (no
/// outdoor probe reports 0xFF) or one no firmware documents.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub power_on: bool,
    pub target_temperature: f32,
    pub mode: Option<Mode>,
    pub fan_speed: u8,
    pub swing: Option<Swing>,
    pub turbo: bool,
    pub eco: bool,
    pub sleep: bool,
    pub fahrenheit: bool,
    pub indoor_temperature: Option<f32>,
    pub outdoor_temperature: Option<f32>,
    pub aux_heat: bool,
    pub independent_aux_heat: bool,
    pub follow_me: bool,
    pub purifier: bool,
    pub error_code: u8,
    pub target_humidity: Option<u8>,
    pub freeze_protection: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseError {
    TooShort(usize),
    NotAStateReport(u8),
}

impl core::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort(n) => write!(f, "state report is only {n} bytes"),
            Self::NotAStateReport(t) => write!(f, "report type is 0x{t:02X}, not 0xC0"),
        }
    }
}

/// A temperature byte with its tenths nibble. Three paths, all load-bearing:
/// 0xFF is no sensor; in Celsius the nibble replaces the half-degree; in
/// Fahrenheit it only rounds to a half. In f32, as Breeze Core computes it.
fn temperature(data: u8, decimals: f32, fahrenheit: bool) -> Option<f32> {
    if data == 0xFF {
        return None;
    }
    let t = (data as f32 - 50.0) / 2.0;
    let whole = t as i32 as f32;
    let signed = |d: f32| if t >= 0.0 { whole + d } else { whole - d };
    Some(if !fahrenheit && decimals != 0.0 {
        signed(decimals)
    } else if decimals >= 0.5 {
        signed(0.5)
    } else {
        t
    })
}

impl State {
    pub fn parse(p: &[u8]) -> Result<Self, ResponseError> {
        if p.len() < 17 {
            return Err(ResponseError::TooShort(p.len()));
        }
        if p[0] != 0xC0 {
            return Err(ResponseError::NotAStateReport(p[0]));
        }
        let half = if p[2] & 0x10 != 0 { 0.5 } else { 0.0 };
        // Byte 13 repeats the setpoint with another bias on some firmware, and
        // wins when present: it survives the unit being set from its remote.
        let alt = p[13] & 0x1F;
        let target_temperature = if alt != 0 {
            alt as f32 + 12.0 + half
        } else {
            (p[2] & 0xF) as f32 + 16.0 + half
        };
        let fahrenheit = p[10] & 0x04 != 0;
        Ok(Self {
            power_on: p[1] & 0x1 != 0,
            target_temperature,
            mode: Mode::from_wire((p[2] >> 5) & 0x7),
            fan_speed: p[3] & 0x7F,
            swing: Swing::from_wire(p[7] & 0xF),
            // Turbo is in one of two places, by firmware.
            turbo: p[8] & 0x20 != 0 || p[10] & 0x02 != 0,
            eco: p[9] & 0x10 != 0,
            sleep: p[10] & 0x1 != 0,
            fahrenheit,
            indoor_temperature: temperature(p[11], (p[15] & 0xF) as f32 / 10.0, fahrenheit),
            outdoor_temperature: temperature(p[12], (p[15] >> 4) as f32 / 10.0, fahrenheit),
            aux_heat: p[9] & 0x08 != 0,
            independent_aux_heat: p[8] & 0x40 != 0,
            follow_me: p[8] & 0x80 != 0,
            purifier: p[9] & 0x20 != 0,
            error_code: p[16],
            target_humidity: p.get(19).map(|b| b & 0x7F),
            freeze_protection: p.get(21).map(|b| b & 0x80 != 0),
        })
    }
}
