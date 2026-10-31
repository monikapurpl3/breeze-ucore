//! Server-local wall-clock time, kept as Breeze Core keeps it: naive (no zone),
//! written in whole seconds as `YYYY-MM-DDTHH:MM:SS`. Timers and schedules run
//! on it, DST-naive: 45 minutes from now is 45 minutes of wall clock, exactly
//! `datetime.now() + timedelta(minutes=45)`, which is what the stored strings
//! and the scheduler have to agree on.
//!
//! The zone itself is the platform's ([`Platform::local_offset`]); the
//! calendar is here, small, and the same on every chip.
//!
//! [`Platform::local_offset`]: crate::platform::Platform::local_offset

use crate::platform::Platform;
use alloc::string::String;

/// Milliseconds of naive local time since 1970-01-01T00:00:00.
pub type LocalMs = i64;

pub const DAY_S: i64 = 86_400;

/// Local time now, if the platform knows both the time and the zone.
pub fn now(p: &mut impl Platform) -> Option<LocalMs> {
    let unix = i64::try_from(p.unix_ms()?).ok()?;
    let offset = p.local_offset(unix.div_euclid(1000))?;
    Some(unix + i64::from(offset) * 1000)
}

/// Whole seconds, floored: what `%S` makes of a time with a fraction.
pub fn seconds(t: LocalMs) -> i64 {
    t.div_euclid(1000)
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day number.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// `YYYY-MM-DDTHH:MM:SS` of a time in whole local seconds.
pub fn format(secs: i64) -> String {
    let (y, mo, d) = civil_from_days(secs.div_euclid(DAY_S));
    let s = secs.rem_euclid(DAY_S);
    alloc::format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// `HH:MM on YYYY-MM-DD`, the way a refusal reads a moment back.
pub fn format_on(secs: i64) -> String {
    let (y, mo, d) = civil_from_days(secs.div_euclid(DAY_S));
    let s = secs.rem_euclid(DAY_S);
    alloc::format!("{:02}:{:02} on {y:04}-{mo:02}-{d:02}", s / 3600, s / 60 % 60)
}

/// Read what [`format`] writes. As chrono reads it for Breeze Core: one or two
/// digits for each field after the year, nothing after the seconds.
pub fn parse(text: &str) -> Option<i64> {
    let (date, time) = text.split_once('T')?;
    let mut d = date.splitn(3, '-');
    let year = d.next().filter(|y| y.len() == 4)?;
    let fields = |s: Option<&str>| s.filter(|v| (1..=2).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_digit()))?.parse::<u32>().ok();
    let (y, mo, da) = (year.bytes().all(|b| b.is_ascii_digit()).then(|| year.parse::<i64>().ok())??, fields(d.next())?, fields(d.next())?);
    let mut t = time.splitn(3, ':');
    let (h, mi, s) = (fields(t.next())?, fields(t.next())?, fields(t.next())?);
    if !(1..=12).contains(&mo) || da == 0 || da > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(days_from_civil(y, mo, da) * DAY_S + i64::from(h * 3600 + mi * 60 + s))
}

/// `HH:MM`, 24-hour, as clients send it: one-digit hours are fine, minutes
/// are two digits. Seconds into the day.
pub fn parse_hhmm(text: &str) -> Option<i64> {
    let (h, m) = text.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (hour, minute): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (hour < 24 && minute < 60).then_some(i64::from(hour * 3600 + minute * 60))
}

/// Monday is 0, as Python's `weekday()`.
pub fn weekday(secs: i64) -> u32 {
    // 1970-01-01 was a Thursday.
    (secs.div_euclid(DAY_S) + 3).rem_euclid(7) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calendar_round_trips_over_centuries() {
        for days in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(20_513), (2026, 3, 1));
    }

    #[test]
    fn times_are_written_and_read_as_breeze_core_does() {
        let t = parse("2026-10-31T07:05:09").unwrap();
        assert_eq!(format(t), "2026-10-31T07:05:09");
        assert_eq!(format_on(t), "07:05 on 2026-10-31");
        assert_eq!(parse("2024-02-29T23:59:59").map(format).as_deref(), Some("2024-02-29T23:59:59"));
        assert_eq!(parse("2026-1-5T7:00:00").map(format).as_deref(), Some("2026-01-05T07:00:00"));
        for bad in ["2025-02-29T00:00:00", "2026-13-01T00:00:00", "2026-10-31T24:00:00", "2026-10-31 07:05:09", "2026-10-31T07:05:09Z", "whenever", "", "26-10-31T07:05:09"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_time_of_day_is_hh_mm() {
        assert_eq!(parse_hhmm("07:30"), Some(7 * 3600 + 30 * 60));
        assert_eq!(parse_hhmm(" 7:30 "), Some(7 * 3600 + 30 * 60));
        for bad in ["24:00", "07:60", "7:5", "073:00", ":30", "07-30", "seven"] {
            assert_eq!(parse_hhmm(bad), None, "{bad}");
        }
    }

    #[test]
    fn weekdays_start_on_monday() {
        assert_eq!(weekday(parse("2026-10-26T12:00:00").unwrap()), 0);
        assert_eq!(weekday(parse("2026-11-01T00:00:00").unwrap()), 6);
        assert_eq!(weekday(parse("1969-12-31T23:59:59").unwrap()), 2);
    }
}
