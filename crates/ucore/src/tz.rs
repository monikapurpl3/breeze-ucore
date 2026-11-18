//! A POSIX TZ rule (`CET-1CEST,M3.5.0,M10.5.0/3`): how a chip with no
//! timezone database turns UTC into local time, DST included.
//!
//! The rule is the one the tz database itself ends every zone with (the
//! "footer" of a TZif file), so any zone's current rule can be copied from
//! there: Europe/Zagreb is `CET-1CEST,M3.5.0,M10.5.0/3`. The extensions the
//! database uses are kept: a transition time outside 0..24 h
//! (`M3.5.0/-1`, `M9.1.6/24`) and angle-bracketed names (`<+0545>-5:45`).
//!
//! An adapter keeps one parsed [`Rule`] and answers
//! [`Platform::local_offset`](crate::platform::Platform::local_offset) with
//! [`Rule::offset_at`]. Checked against the tz database itself, zone by zone
//! and year by year, in this module's tests.

use crate::clock::{days_from_civil, DAY_S};

/// When a DST period starts or ends, within a year, at a local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Day {
    /// `Jn`: day 1..=365, February 29th never counted.
    Julian1(u16),
    /// `n`: day 0..=365, February 29th counted in a leap year.
    Julian0(u16),
    /// `Mm.w.d`: month 1..=12, week 1..=5 (5 is the last), weekday 0..=6 (Sunday 0).
    Month { m: u8, w: u8, d: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Change {
    day: Day,
    /// Seconds after local midnight; may be negative or past 24 h.
    at: i32,
}

/// A parsed rule: standard time, and optionally DST with its two changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// Seconds east of UTC in standard time.
    std: i32,
    dst: Option<(i32, Change, Change)>,
}

impl Rule {
    /// A zone without DST, `offset` seconds east of UTC.
    pub const fn fixed(offset: i32) -> Self {
        Self { std: offset, dst: None }
    }

    /// Parse a POSIX TZ string. `None` for anything malformed.
    pub fn parse(text: &str) -> Option<Self> {
        let mut s = Scan { b: text.as_bytes(), i: 0 };
        s.name()?;
        let std = -s.offset()?;
        if s.done() {
            return Some(Self::fixed(std));
        }
        s.name()?;
        let dst = if matches!(s.peek(), Some(b',') | None) { std + 3600 } else { -s.offset()? };
        let (start, end) = if s.done() {
            // POSIX leaves the default rules to the implementation; glibc's
            // (and the tz database's) are the US ones.
            (Change { day: Day::Month { m: 3, w: 2, d: 0 }, at: 7200 }, Change { day: Day::Month { m: 11, w: 1, d: 0 }, at: 7200 })
        } else {
            s.eat(b',')?;
            let start = s.change()?;
            s.eat(b',')?;
            let end = s.change()?;
            (start, end)
        };
        s.done().then_some(Self { std, dst: Some((dst, start, end)) })
    }

    /// Seconds east of UTC at that moment.
    pub fn offset_at(&self, unix_s: i64) -> i32 {
        let Some((dst, start, end)) = self.dst else { return self.std };
        // The year as standard local time sees it; a transition near New
        // Year can only fall in the year of the moment it is compared with.
        let (year, _, _) = crate::clock::civil_from_days((unix_s + i64::from(self.std)).div_euclid(DAY_S));
        let in_dst = |year: i64| {
            // The start is written in standard time, the end in DST.
            let on = start.utc(year) - i64::from(self.std);
            let off = end.utc(year) - i64::from(dst);
            if on < off {
                on <= unix_s && unix_s < off
            } else {
                // Southern hemisphere: DST spans New Year.
                !(off <= unix_s && unix_s < on)
            }
        };
        if in_dst(year) {
            dst
        } else {
            self.std
        }
    }
}

impl Change {
    /// Seconds from the epoch to this change in `year`, as a local time
    /// (the caller subtracts the offset in force before it).
    fn utc(self, year: i64) -> i64 {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let jan1 = days_from_civil(year, 1, 1);
        let day = match self.day {
            Day::Julian1(n) => {
                let n = i64::from(n) - 1;
                // Day 60 and on is after February, which a leap year makes longer.
                jan1 + n + i64::from(leap && n >= 59)
            }
            Day::Julian0(n) => jan1 + i64::from(n),
            Day::Month { m, w, d } => {
                let first = days_from_civil(year, u32::from(m), 1);
                // 1970-01-01 was a Thursday (4).
                let wd_first = (first + 4).rem_euclid(7);
                let mut day = first + (i64::from(d) - wd_first).rem_euclid(7) + 7 * (i64::from(w) - 1);
                let next = if m == 12 { days_from_civil(year + 1, 1, 1) } else { days_from_civil(year, u32::from(m) + 1, 1) };
                while day >= next {
                    day -= 7;
                }
                day
            }
        };
        day * DAY_S + i64::from(self.at)
    }
}

struct Scan<'a> {
    b: &'a [u8],
    i: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn done(&self) -> bool {
        self.i == self.b.len()
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        (self.peek()? == c).then(|| self.i += 1)
    }

    /// A zone abbreviation: three or more letters, or `<...>` of letters,
    /// digits and signs.
    fn name(&mut self) -> Option<()> {
        let from = self.i;
        if self.peek() == Some(b'<') {
            self.i += 1;
            while self.peek()?.is_ascii_alphanumeric() || matches!(self.peek()?, b'+' | b'-') {
                self.i += 1;
            }
            self.eat(b'>')?;
            return (self.i - from >= 5).then_some(());
        }
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.i += 1;
        }
        (self.i - from >= 3).then_some(())
    }

    fn number(&mut self, max: u32) -> Option<u32> {
        let from = self.i;
        let mut n: u32 = 0;
        while let Some(c) = self.peek().filter(u8::is_ascii_digit) {
            n = n.checked_mul(10)?.checked_add(u32::from(c - b'0'))?;
            self.i += 1;
        }
        (self.i > from && n <= max).then_some(n)
    }

    /// `[+-]hh[:mm[:ss]]` in seconds, POSIX's sign (west positive).
    fn hms(&mut self, max_h: u32) -> Option<i32> {
        let sign = match self.peek() {
            Some(b'-') => {
                self.i += 1;
                -1
            }
            Some(b'+') => {
                self.i += 1;
                1
            }
            _ => 1,
        };
        let mut secs = self.number(max_h)? * 3600;
        if self.peek() == Some(b':') {
            self.i += 1;
            secs += self.number(59)? * 60;
            if self.peek() == Some(b':') {
                self.i += 1;
                secs += self.number(59)?;
            }
        }
        Some(sign * i32::try_from(secs).ok()?)
    }

    fn offset(&mut self) -> Option<i32> {
        self.hms(24)
    }

    fn change(&mut self) -> Option<Change> {
        let day = match self.peek()? {
            b'J' => {
                self.i += 1;
                let n = self.number(365)?;
                (n >= 1).then_some(Day::Julian1(n as u16))?
            }
            b'M' => {
                self.i += 1;
                let m = self.number(12)?;
                self.eat(b'.')?;
                let w = self.number(5)?;
                self.eat(b'.')?;
                let d = self.number(6)?;
                (m >= 1 && w >= 1).then_some(Day::Month { m: m as u8, w: w as u8, d: d as u8 })?
            }
            _ => Day::Julian0(self.number(365)? as u16),
        };
        let at = if self.peek() == Some(b'/') {
            self.i += 1;
            // RFC 8536's extension: -167..167 hours.
            self.hms(167)?
        } else {
            7200
        };
        Some(Change { day, at })
    }
}

#[cfg(test)]
mod tests {
    use super::Rule;
    use chrono::{Offset, TimeZone};

    #[test]
    fn rejects_what_is_not_a_rule() {
        for bad in ["", "CE-1", "CET", "CET-1CEST,M3.5.0", "CET-1CEST,M13.5.0,M10.5.0", "CET-1CEST,M3.6.0,M10.5.0", "<+05>", "CET-1 ", "CET-25"] {
            assert_eq!(Rule::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn fixed_zones() {
        assert_eq!(Rule::parse("UTC0").unwrap().offset_at(0), 0);
        assert_eq!(Rule::parse("JST-9").unwrap().offset_at(1_800_000_000), 9 * 3600);
        assert_eq!(Rule::parse("<+0545>-5:45").unwrap().offset_at(1_800_000_000), 5 * 3600 + 45 * 60);
        assert_eq!(Rule::parse("<-03>3").unwrap().offset_at(1_800_000_000), -3 * 3600);
    }

    /// The external check: every hour from 2000 to 2040 against the tz
    /// database's own answer, for zones whose current rule has held all that
    /// time, chosen to cover each corner of the format.
    #[test]
    fn agrees_with_the_tz_database() {
        let zones: &[(&str, chrono_tz::Tz, i32)] = &[
            // Northern, last Sunday, an end at 03:00.
            ("CET-1CEST,M3.5.0,M10.5.0/3", chrono_tz::Europe::Zagreb, 2000),
            ("GMT0BST,M3.5.0/1,M10.5.0", chrono_tz::Europe::London, 2000),
            // The US default rules, written out and left out.
            ("EST5EDT,M3.2.0,M11.1.0", chrono_tz::America::New_York, 2008),
            ("EST5EDT", chrono_tz::America::New_York, 2008),
            // Southern hemisphere: DST across New Year.
            ("AEST-10AEDT,M10.1.0,M4.1.0/3", chrono_tz::Australia::Sydney, 2008),
            ("NZST-12NZDT,M9.5.0,M4.1.0/3", chrono_tz::Pacific::Auckland, 2008),
            // Quarter-hour offsets, minutes in the transition times.
            ("<+1245>-12:45<+1345>,M9.5.0/2:45,M4.1.0/3:45", chrono_tz::Pacific::Chatham, 2008),
            // A transition at 24:00 on a Saturday.
            ("<-04>4<-03>,M9.1.6/24,M4.1.6/24", chrono_tz::America::Santiago, 2023),
            // A negative transition time.
            ("<-02>2<-01>,M3.5.0/-1,M10.5.0/0", chrono_tz::America::Nuuk, 2024),
            ("<+0545>-5:45", chrono_tz::Asia::Kathmandu, 2000),
            ("IST-2IDT,M3.4.4/26,M10.5.0", chrono_tz::Asia::Jerusalem, 2013),
        ];
        for (text, zone, from) in zones {
            let rule = Rule::parse(text).unwrap_or_else(|| panic!("{text} parses"));
            let start = chrono::NaiveDate::from_ymd_opt(*from, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
            let end = chrono::NaiveDate::from_ymd_opt(2040, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
            // Every 15 minutes: Chatham changes at :45.
            let mut t = start;
            while t < end {
                let want = zone.timestamp_opt(t, 0).unwrap().offset().fix().local_minus_utc();
                assert_eq!(rule.offset_at(t), want, "{text} at {t}");
                t += 900;
            }
        }
    }

    /// No zone in the database uses day numbers, so POSIX's own definition:
    /// `Jn` never counts February 29th (J60 is March 1st in every year), `n`
    /// counts from 0 and does (59 is February 29th in a leap year).
    #[test]
    fn day_numbers_count_the_leap_day_as_posix_says() {
        let j = Rule::parse("EST5EDT,J60/2,J300/2").unwrap();
        let n = Rule::parse("EST5EDT,59/2,300/2").unwrap();
        // 2024-03-01 07:00 UTC and 2024-02-29 07:00 UTC (02:00 EST).
        assert_eq!(j.offset_at(1_709_276_399), -5 * 3600);
        assert_eq!(j.offset_at(1_709_276_400), -4 * 3600);
        assert_eq!(n.offset_at(1_709_189_999), -5 * 3600);
        assert_eq!(n.offset_at(1_709_190_000), -4 * 3600);
        // 2023 has no February 29th: day 59 is March 1st too.
        assert_eq!(n.offset_at(1_677_654_000), -4 * 3600);
        assert_eq!(n.offset_at(1_677_653_999), -5 * 3600);
    }

    /// The moments either side of each change, exactly, for the rule the
    /// maintainer's units live under.
    #[test]
    fn zagreb_changes_to_the_second() {
        let rule = Rule::parse("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        // 2026-03-29 01:00:00 UTC and 2026-10-25 01:00:00 UTC.
        assert_eq!(rule.offset_at(1_774_745_999), 3600);
        assert_eq!(rule.offset_at(1_774_746_000), 7200);
        assert_eq!(rule.offset_at(1_792_889_999), 7200);
        assert_eq!(rule.offset_at(1_792_890_000), 3600);
    }
}
