//! Wall-clock helpers of the automations: a 5-field cron expression
//! (`minute hour day-of-month month day-of-week`) and the local calendar
//! time of a Unix timestamp.

/// Local calendar time (the hub's fixed UTC offset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Local {
    /// Minutes since 1970-01-01 00:00 local time (the schedule's tick).
    pub minute: i64,
    /// Minute of the day, 0–1439.
    pub minute_of_day: u16,
    /// ISO weekday: 1 = Monday … 7 = Sunday.
    pub weekday: u8,
    /// Day of the month, 1–31.
    pub day: u8,
    /// Month, 1–12.
    pub month: u8,
    /// Year.
    pub year: i32,
}

impl Local {
    /// Local time of Unix time `unix_s` at UTC offset `offset_s`.
    pub fn at(unix_s: i64, offset_s: i64) -> Self {
        let t = unix_s + offset_s;
        let days = t.div_euclid(86_400);
        let secs = t.rem_euclid(86_400);
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Self {
            minute: t.div_euclid(60),
            minute_of_day: (secs / 60) as u16,
            // 1970-01-01 was a Thursday.
            weekday: ((days + 3).rem_euclid(7) + 1) as u8,
            day,
            month,
            year,
        }
    }

    /// `YYYY-MM-DD HH:MM`.
    pub fn text(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year,
            self.month,
            self.day,
            self.minute_of_day / 60,
            self.minute_of_day % 60
        )
    }
}

/// A parsed cron expression (bit sets per field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    /// Bit 0 = Sunday … bit 6 = Saturday.
    weekdays: u8,
    days_any: bool,
    weekdays_any: bool,
}

fn field(text: &str, lo: u32, hi: u32, names: &[&str]) -> Result<(u64, bool), String> {
    let num = |s: &str| -> Result<u32, String> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(lo + i as u32);
        }
        s.parse::<u32>().ok().filter(|v| (lo..=hi).contains(v)).ok_or_else(|| format!("{s:?} is not in {lo}–{hi}"))
    };
    let mut bits = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0).ok_or_else(|| format!("bad step in {part:?}"))?),
            None => (part, 1),
        };
        let (a, b) = match range {
            "*" => (lo, hi),
            r => match r.split_once('-') {
                Some((a, b)) => (num(a)?, num(b)?),
                None if step > 1 => (num(r)?, hi),
                None => (num(r)?, num(r)?),
            },
        };
        if a > b {
            return Err(format!("empty range {part:?}"));
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, text == "*"))
}

impl Cron {
    /// Parses `minute hour day month weekday` (numbers, `*`, `a-b`, lists,
    /// `/step`; weekday 0–7 with 0 and 7 = Sunday, or `sun`…`sat`; months
    /// `jan`…`dec`).
    pub fn parse(text: &str) -> Result<Self, String> {
        let f: Vec<&str> = text.split_whitespace().collect();
        let [mi, h, d, mo, wd] = f[..] else {
            return Err("cron needs 5 fields: minute hour day month weekday".into());
        };
        const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
        const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
        let (minutes, _) = field(mi, 0, 59, &[])?;
        let (hours, _) = field(h, 0, 23, &[])?;
        let (days, days_any) = field(d, 1, 31, &[])?;
        let (months, _) = field(mo, 1, 12, &MONTHS)?;
        let (wds, weekdays_any) = field(wd, 0, 7, &DAYS)?;
        Ok(Self {
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: ((wds & 0x7F) | (wds >> 7)) as u8,
            days_any,
            weekdays_any,
        })
    }

    /// Whether the minute `t` matches (day of month and weekday as in
    /// cron: either one when both are restricted).
    pub fn matches(&self, t: &Local) -> bool {
        let (h, m) = (t.minute_of_day / 60, t.minute_of_day % 60);
        let dom = self.days & (1 << t.day) != 0;
        let dow = self.weekdays & (1 << (t.weekday % 7)) != 0;
        let day = match (self.days_any, self.weekdays_any) {
            (true, true) => true,
            (false, true) => dom,
            (true, false) => dow,
            (false, false) => dom || dow,
        };
        self.minutes & (1 << m) != 0 && self.hours & (1 << h) != 0 && self.months & (1 << t.month) != 0 && day
    }
}
