//! Wall-clock time: the time zone (a POSIX TZ string, as glibc and ESP-IDF
//! take it: `EET-2EEST,M3.5.0/3,M10.5.0/4`), its daylight saving rules,
//! civil dates for display, and the Time cluster's view of them.
//!
//! The engine's clock stays monotonic (its timers compare it); the real
//! time reaches the API and the devices through an offset once the
//! platform knows UTC (SNTP, or a clock the S3 kept over a restart).

/// Unix time of 2024-01-01: a system clock before it has not been set.
pub const VALID_AFTER_UNIX: i64 = 1_704_067_200;

/// The default time zone (UTC, no daylight saving time).
pub const DEFAULT_TZ: &str = "UTC0";

/// The default SNTP server.
pub const DEFAULT_SERVER: &str = "pool.ntp.org";

/// A transition date of a POSIX TZ rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Date {
    /// `Mm.w.d`: day `d` (0 = Sunday) of week `w` (5 = last) of month `m`.
    Month(u8, u8, u8),
    /// `Jn`: day 1–365, February 29 never counted.
    Julian(u16),
    /// `n`: day 0–365, counting February 29.
    Day(u16),
}

/// A parsed time zone (default: UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tz {
    /// Standard time offset east of UTC, s.
    pub std_s: i32,
    /// Daylight saving: `(shift s, start, start time s, end, end time s)`,
    /// start in standard and end in daylight time, local.
    dst: Option<(i32, Date, i32, Date, i32)>,
}

struct P<'a>(&'a [u8]);

impl P<'_> {
    fn peek(&self) -> Option<u8> {
        self.0.first().copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        let ok = self.peek() == Some(c);
        if ok {
            self.0 = &self.0[1..];
        }
        ok
    }
    fn name(&mut self) -> Option<()> {
        if self.eat(b'<') {
            let end = self.0.iter().position(|&c| c == b'>')?;
            self.0 = &self.0[end + 1..];
            return Some(());
        }
        let n = self.0.iter().take_while(|c| c.is_ascii_alphabetic()).count();
        (n >= 3).then(|| self.0 = &self.0[n..])
    }
    fn num(&mut self) -> Option<i32> {
        let n = self.0.iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 || n > 3 {
            return None;
        }
        let v = std::str::from_utf8(&self.0[..n]).ok()?.parse().ok();
        self.0 = &self.0[n..];
        v
    }
    /// `[+-]hh[:mm[:ss]]` in seconds.
    fn hms(&mut self) -> Option<i32> {
        let neg = self.eat(b'-');
        if !neg {
            self.eat(b'+');
        }
        let mut s = self.num()? * 3600;
        if self.eat(b':') {
            s += self.num()? * 60;
            if self.eat(b':') {
                s += self.num()?;
            }
        }
        Some(if neg { -s } else { s })
    }
    fn date(&mut self) -> Option<(Date, i32)> {
        let d = if self.eat(b'M') {
            let m = self.num()?;
            self.eat(b'.').then_some(())?;
            let w = self.num()?;
            self.eat(b'.').then_some(())?;
            let d = self.num()?;
            ((1..=12).contains(&m) && (1..=5).contains(&w) && (0..=6).contains(&d)).then_some(())?;
            Date::Month(m as u8, w as u8, d as u8)
        } else if self.eat(b'J') {
            let n = self.num()?;
            (1..=365).contains(&n).then_some(Date::Julian(n as u16))?
        } else {
            let n = self.num()?;
            (0..=365).contains(&n).then_some(Date::Day(n as u16))?
        };
        let t = if self.eat(b'/') { self.hms()? } else { 7200 };
        Some((d, t))
    }
}

/// Parse a POSIX TZ string (None when it is not one).
pub fn parse(s: &str) -> Option<Tz> {
    let mut p = P(s.trim().as_bytes());
    p.name()?;
    // POSIX counts west of UTC as positive.
    let std_s = -p.hms()?;
    if p.0.is_empty() {
        return Some(Tz { std_s, dst: None });
    }
    p.name()?;
    let dst_s = if matches!(p.peek(), Some(b'+' | b'-' | b'0'..=b'9')) { -p.hms()? } else { std_s + 3600 };
    // Without rules: the US rules (POSIX leaves them to the system).
    let (start, end) = if p.eat(b',') {
        let a = p.date()?;
        p.eat(b',').then_some(())?;
        (a, p.date()?)
    } else {
        ((Date::Month(3, 2, 0), 7200), (Date::Month(11, 1, 0), 7200))
    };
    p.0.is_empty().then_some(Tz { std_s, dst: Some((dst_s - std_s, start.0, start.1, end.0, end.1)) })
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day since 1970-01-01: `(year, month, day)`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Day since 1970 of a rule date in year `y`.
fn rule_day(y: i64, d: Date) -> i64 {
    let jan1 = days_from_civil(y, 1, 1);
    match d {
        Date::Month(m, w, wd) => {
            let first = days_from_civil(y, u32::from(m), 1);
            let dow = (first + 4).rem_euclid(7);
            let mut day = first + (i64::from(wd) - dow).rem_euclid(7) + 7 * i64::from(w - 1);
            let next = if m == 12 { days_from_civil(y + 1, 1, 1) } else { days_from_civil(y, u32::from(m) + 1, 1) };
            while day >= next {
                day -= 7;
            }
            day
        }
        Date::Julian(n) => jan1 + i64::from(n) - 1 + i64::from(leap(y) && n >= 60),
        Date::Day(n) => jan1 + i64::from(n),
    }
}

impl Tz {
    /// Daylight saving time of year `y` in Unix seconds: `(start, end,
    /// shift)`.
    pub fn dst_utc(&self, y: i64) -> Option<(i64, i64, i32)> {
        let (shift, a, at, b, bt) = self.dst?;
        let start = rule_day(y, a) * 86_400 + i64::from(at) - i64::from(self.std_s);
        let end = rule_day(y, b) * 86_400 + i64::from(bt) - i64::from(self.std_s + shift);
        Some((start, end, shift))
    }

    /// Offset east of UTC at Unix time `t`, s, and whether it is daylight
    /// saving time.
    pub fn offset_at(&self, t: i64) -> (i32, bool) {
        let y = civil_from_days(t.div_euclid(86_400)).0;
        match self.dst_utc(y) {
            Some((s, e, shift)) if (s <= e && t >= s && t < e) || (s > e && (t >= s || t < e)) => (self.std_s + shift, true),
            _ => (self.std_s, false),
        }
    }
}

/// `YYYY-MM-DD HH:MM:SS` of Unix time `t` shifted by `offset_s`.
pub fn civil(t: i64, offset_s: i32) -> String {
    let l = t + i64::from(offset_s);
    let (y, m, d) = civil_from_days(l.div_euclid(86_400));
    let s = l.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// The Time cluster's clock at Unix time `t`.
pub fn zcl_clock(tz: &Tz, t: i64, synced: bool, last_set: Option<i64>) -> hub_devices::hub::Clock {
    let zcl = |u: i64| (u - hub_devices::hub::ZCL_EPOCH_UNIX).clamp(0, 0xFFFF_FFFE) as u32;
    let y = civil_from_days(t.div_euclid(86_400)).0;
    hub_devices::hub::Clock {
        synced,
        tz_s: tz.std_s,
        dst: tz.dst_utc(y).map(|(s, e, shift)| (zcl(s), zcl(e), shift)),
        last_set: last_set.map(zcl),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix(y: i64, m: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60
    }

    #[test]
    fn kyiv_switches_on_the_last_sundays_at_03_and_04_local() {
        let tz = parse("EET-2EEST,M3.5.0/3,M10.5.0/4").unwrap();
        assert_eq!(tz.std_s, 7200);
        // 2026: 29 March 01:00 UTC, 25 October 01:00 UTC.
        let (s, e, shift) = tz.dst_utc(2026).unwrap();
        assert_eq!((s, e, shift), (unix(2026, 3, 29, 1, 0), unix(2026, 10, 25, 1, 0), 3600));
        assert_eq!(tz.offset_at(unix(2026, 7, 1, 12, 0)), (10_800, true));
        assert_eq!(tz.offset_at(unix(2026, 11, 22, 12, 0)), (7200, false));
        assert_eq!(civil(unix(2026, 11, 22, 21, 30), 7200), "2026-11-22 23:30:00");
    }

    #[test]
    fn other_forms() {
        assert_eq!(parse("UTC0"), Some(Tz { std_s: 0, dst: None }));
        let ny = parse("EST5EDT,M3.2.0,M11.1.0").unwrap();
        assert_eq!(ny.dst_utc(2026).unwrap().0, unix(2026, 3, 8, 7, 0));
        // Southern hemisphere: DST over the new year.
        let syd = parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(syd.offset_at(unix(2026, 1, 10, 0, 0)), (39_600, true));
        assert_eq!(syd.offset_at(unix(2026, 6, 10, 0, 0)), (36_000, false));
        assert_eq!(parse("<+0530>-5:30"), Some(Tz { std_s: 19_800, dst: None }));
        assert_eq!(parse("CET-1CEST").unwrap().dst_utc(2026).unwrap().2, 3600);
        for bad in ["", "E1", "EET-2EEST,M13.5.0", "EET-2EEST,M3.5.0", "EET-2x"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
    }

    #[test]
    fn forms_the_web_page_writes() {
        // A half-hour shift (Lord Howe), minutes in offsets and times
        // (Chatham), a time before midnight (Nuuk) and past 24 h (Jerusalem,
        // Santiago): (zone, standard offset, shift, 2026 start, end).
        let cases = [
            ("<+1030>-10:30<+11>-11,M10.1.0,M4.1.0", 37_800, 1800, unix(2026, 10, 3, 15, 30), unix(2026, 4, 4, 15, 0)),
            ("<+1245>-12:45<+1345>,M9.5.0/2:45,M4.1.0/3:45", 45_900, 3600, unix(2026, 9, 26, 14, 0), unix(2026, 4, 4, 14, 0)),
            ("<-02>2<-01>,M3.5.0/-1,M10.5.0/0", -7200, 3600, unix(2026, 3, 29, 1, 0), unix(2026, 10, 25, 1, 0)),
            ("<+02>-2<+03>,M3.4.4/26,M10.5.0", 7200, 3600, unix(2026, 3, 27, 0, 0), unix(2026, 10, 24, 23, 0)),
            ("<-04>4<-03>,M9.1.6/24,M4.1.6/24", -14_400, 3600, unix(2026, 9, 6, 4, 0), unix(2026, 4, 5, 3, 0)),
        ];
        for (s, std_s, shift, start, end) in cases {
            let tz = parse(s).unwrap_or_else(|| panic!("{s}"));
            assert_eq!((tz.std_s, tz.dst_utc(2026)), (std_s, Some((start, end, shift))), "{s}");
        }
        // Jerusalem 2028: the Friday before the last Sunday, not the last Friday.
        assert_eq!(parse("<+02>-2<+03>,M3.4.4/26,M10.5.0").unwrap().dst_utc(2028).unwrap().0, unix(2028, 3, 24, 0, 0));
    }

    #[test]
    fn zcl_clock_counts_from_2000() {
        let tz = parse("EET-2EEST,M3.5.0/3,M10.5.0/4").unwrap();
        let c = zcl_clock(&tz, unix(2026, 11, 22, 0, 0), true, Some(unix(2026, 11, 22, 0, 0)));
        assert_eq!(c.last_set, Some((unix(2026, 11, 22, 0, 0) - 946_684_800) as u32));
        assert_eq!(c.dst.unwrap().0, (unix(2026, 3, 29, 1, 0) - 946_684_800) as u32);
    }
}
