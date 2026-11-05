//! Python `datetime` formatting/parsing in the C locale, verified against CPython 3.12 by
//! `tests/golden.rs`.
//!
//! * [`strftime`] / [`strftime_date`]: chrono's formatter is compatible with CPython (Windows
//!   CRT / glibc, C locale) for `%a %A %b %B %d %m %Y %y %H %M %S %I %p %j %%`, which is every
//!   directive the Python code uses.
//! * [`strptime`]: a port of `Lib/_strptime.py` - the format becomes the same regex (directive
//!   patterns, whitespace runs -> `\s+`, case-insensitive, leftmost-first alternation matched
//!   from the start, then "unconverted data remains" if it did not consume everything), so it
//!   accepts and rejects exactly the same inputs. Supported directives: `a A b B d f H I j m M p
//!   S y Y w u %`; others (`%z %Z %U %W %c %x %X %G %V`) return `None`. Deviation: `\d` only
//!   matches ASCII digits (Python's also matches other scripts' digits).
//! * [`fromisoformat`] / [`date_fromisoformat`]: CPython 3.12's `datetime.fromisoformat` /
//!   `date.fromisoformat` rules (`_pydatetime.py`).

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use regex::Regex;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// `d.strftime(fmt)` for a date (English day/month names, `%d` zero padded). Unknown directives
/// (which Python would pass to the C library) give an empty string instead of panicking.
pub fn strftime_date(d: NaiveDate, fmt: &str) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    match write!(s, "{}", d.format(fmt)) {
        Ok(()) => s,
        Err(_) => String::new(),
    }
}

/// `dt.strftime(fmt)` for a naive/local datetime.
pub fn strftime(dt: NaiveDateTime, fmt: &str) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    match write!(s, "{}", dt.format(fmt)) {
        Ok(()) => s,
        Err(_) => String::new(),
    }
}

// ------------------------------------------------------------------------------------------
// fromisoformat

fn digits(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut v = 0u32;
    for c in b {
        v = v * 10 + (c - b'0') as u32;
    }
    Some(v)
}

/// `_find_isoformat_datetime_separator` (byte position; the date part is ASCII when valid).
fn find_separator(s: &[u8]) -> Option<usize> {
    let len = s.len();
    if len == 7 {
        return Some(7);
    }
    if len < 8 {
        return None;
    }
    let is_digit = |i: usize| s.get(i).is_some_and(|c| c.is_ascii_digit());
    if s[4] == b'-' {
        if s[5] == b'W' {
            if len > 8 && s[8] == b'-' {
                if len == 9 {
                    return None;
                }
                if len > 10 && is_digit(10) {
                    return Some(8);
                }
                return Some(10);
            }
            Some(8)
        } else {
            Some(10)
        }
    } else if s[4] == b'W' {
        let mut idx = 7;
        while idx < len && is_digit(idx) {
            idx += 1;
        }
        if idx < 9 {
            return Some(idx);
        }
        Some(if idx % 2 == 0 { 7 } else { 8 })
    } else {
        Some(8)
    }
}

/// `_parse_isoformat_date` for a 7/8/10-byte date string.
fn parse_iso_date(d: &[u8]) -> Option<NaiveDate> {
    if !matches!(d.len(), 7 | 8 | 10) {
        return None;
    }
    let year = digits(&d[0..4])? as i32;
    let has_sep = d[4] == b'-';
    let mut pos = 4 + has_sep as usize;
    if d.get(pos) == Some(&b'W') {
        pos += 1;
        let week = digits(d.get(pos..pos + 2)?)?;
        pos += 2;
        let mut day = 1;
        if d.len() > pos {
            if (d[pos] == b'-') != has_sep {
                return None;
            }
            pos += has_sep as usize;
            day = digits(d.get(pos..pos + 1)?)?;
            if d.len() != pos + 1 {
                return None;
            }
        }
        if !(1..=7).contains(&day) || year < 1 {
            return None;
        }
        let wd = chrono::Weekday::try_from((day - 1) as u8).ok()?;
        NaiveDate::from_isoywd_opt(year, week, wd)
    } else {
        let month = digits(d.get(pos..pos + 2)?)?;
        pos += 2;
        if (d.get(pos) == Some(&b'-')) != has_sep {
            return None;
        }
        pos += has_sep as usize;
        let day = digits(d.get(pos..pos + 2)?)?;
        if d.len() != pos + 2 || year < 1 {
            return None;
        }
        NaiveDate::from_ymd_opt(year, month, day)
    }
}

/// `_parse_hh_mm_ss_ff`: `HH[:?MM[:?SS[{.,}fff[fff]]]]` -> (h, m, s, us).
#[allow(clippy::needless_range_loop)] // mirrors the Python loop
fn parse_hh_mm_ss_ff(t: &[u8]) -> Option<[u32; 4]> {
    let len = t.len();
    let mut comps = [0u32; 4];
    let mut pos = 0;
    let mut has_sep = false;
    for comp in 0..3 {
        if len - pos < 2 {
            return None;
        }
        comps[comp] = digits(&t[pos..pos + 2])?;
        pos += 2;
        let next = t.get(pos);
        if comp == 0 {
            has_sep = next == Some(&b':');
        }
        if next.is_none() || comp >= 2 {
            break;
        }
        if has_sep && next != Some(&b':') {
            return None;
        }
        pos += has_sep as usize;
    }
    if pos < len {
        if t[pos] != b'.' && t[pos] != b',' {
            return None;
        }
        pos += 1;
        let rem = len - pos;
        let n = rem.min(6);
        let mut us = digits(&t[pos..pos + n])?;
        for _ in n..6 {
            us *= 10;
        }
        comps[3] = us;
        if rem > n && !t[pos + n..].iter().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    Some(comps)
}

/// `_parse_isoformat_time` -> (time, offset seconds).
fn parse_iso_time(t: &[u8]) -> Option<(NaiveTime, Option<i32>)> {
    let len = t.len();
    if len < 2 {
        return None;
    }
    let find = |c: u8| t.iter().position(|x| *x == c).map(|p| p + 1).unwrap_or(0);
    let tz_pos = {
        let a = find(b'-');
        if a > 0 { a } else { let b = find(b'+'); if b > 0 { b } else { find(b'Z') } }
    };
    let timestr = if tz_pos > 0 { &t[..tz_pos - 1] } else { t };
    let c = parse_hh_mm_ss_ff(timestr)?;
    let mut off = None;
    if tz_pos == len && t[len - 1] == b'Z' {
        off = Some(0);
    } else if tz_pos > 0 {
        let tzstr = &t[tz_pos..];
        if matches!(tzstr.len(), 0 | 1 | 3) {
            return None;
        }
        let z = parse_hh_mm_ss_ff(tzstr)?;
        // timezone(timedelta) requires |offset| < 24h; minutes/seconds must form a valid
        // timedelta (any values add up)
        let total_us = (z[0] as i64 * 3600 + z[1] as i64 * 60 + z[2] as i64) * 1_000_000 + z[3] as i64;
        if total_us >= 86_400 * 1_000_000 {
            return None;
        }
        let sign = if t[tz_pos - 1] == b'-' { -1 } else { 1 };
        off = Some(sign * (total_us / 1_000_000) as i32);
    }
    let time = NaiveTime::from_hms_micro_opt(c[0], c[1], c[2], c[3])?;
    if c[2] > 59 {
        return None;
    }
    Some((time, off))
}

/// `datetime.fromisoformat(s)` (Python 3.11+: `Z`, offsets, fractional seconds, basic format,
/// ISO week dates, date only, any single separator character). Returns the naive local part and
/// the UTC offset in whole seconds if present (sub-second offsets are truncated). `None` where
/// Python raises `ValueError`.
pub fn fromisoformat(s: &str) -> Option<(NaiveDateTime, Option<i32>)> {
    let b = s.as_bytes();
    if s.chars().count() < 7 {
        return None;
    }
    let sep = find_separator(b)?;
    if sep > b.len() || !s.is_char_boundary(sep.min(b.len())) {
        return None;
    }
    let date = parse_iso_date(&b[..sep])?;
    // skip the separator character (any single code point)
    let rest = &s[sep..];
    let tstr = match rest.chars().next() {
        Some(c) => &rest[c.len_utf8()..],
        None => "",
    };
    if tstr.is_empty() {
        return Some((date.and_hms_opt(0, 0, 0)?, None));
    }
    let (time, off) = parse_iso_time(tstr.as_bytes())?;
    Some((NaiveDateTime::new(date, time), off))
}

/// `date.fromisoformat(s)` (`YYYY-MM-DD`, `YYYYMMDD`, `YYYY-Www[-D]`, `YYYYWww[D]`).
pub fn date_fromisoformat(s: &str) -> Option<NaiveDate> {
    if !s.is_ascii() {
        return None;
    }
    parse_iso_date(s.as_bytes())
}

// ------------------------------------------------------------------------------------------
// strptime

const A_WEEKDAY: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const F_WEEKDAY: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];
const A_MONTH: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const F_MONTH: [&str; 12] = ["january", "february", "march", "april", "may", "june", "july", "august",
                             "september", "october", "november", "december"];

/// `TimeRE.__seqToRE`: alternatives sorted by length descending (stable).
fn seq_to_re(items: &[&str], name: &str) -> String {
    let mut v: Vec<&str> = items.to_vec();
    v.sort_by_key(|s| std::cmp::Reverse(s.len()));
    format!("(?P<{name}>{})", v.join("|"))
}

fn directive_re(c: char) -> Option<String> {
    Some(match c {
        'd' => r"(?P<d>3[0-1]|[1-2][0-9]|0[1-9]|[1-9]| [1-9])".into(),
        'f' => r"(?P<f>[0-9]{1,6})".into(),
        'H' => r"(?P<H>2[0-3]|[0-1][0-9]|[0-9])".into(),
        'I' => r"(?P<I>1[0-2]|0[1-9]|[1-9])".into(),
        'j' => r"(?P<j>36[0-6]|3[0-5][0-9]|[1-2][0-9][0-9]|0[1-9][0-9]|00[1-9]|[1-9][0-9]|0[1-9]|[1-9])".into(),
        'm' => r"(?P<m>1[0-2]|0[1-9]|[1-9])".into(),
        'M' => r"(?P<M>[0-5][0-9]|[0-9])".into(),
        'S' => r"(?P<S>6[0-1]|[0-5][0-9]|[0-9])".into(),
        'w' => r"(?P<w>[0-6])".into(),
        'u' => r"(?P<u>[1-7])".into(),
        'y' => r"(?P<y>[0-9][0-9])".into(),
        'Y' => r"(?P<Y>[0-9][0-9][0-9][0-9])".into(),
        'A' => seq_to_re(&F_WEEKDAY, "A"),
        'a' => seq_to_re(&A_WEEKDAY, "a"),
        'B' => seq_to_re(&F_MONTH, "B"),
        'b' => seq_to_re(&A_MONTH, "b"),
        'p' => seq_to_re(&["am", "pm"], "p"),
        '%' => "%".into(),
        _ => return None,
    })
}

/// `TimeRE.pattern(format)` -> regex source, or None for a bad directive / stray '%'.
/// Python's regex raises on a repeated group name (e.g. "%d %d"); so does `Regex::new`.
fn strptime_pattern(fmt: &str) -> Option<String> {
    let mut out = String::from("(?i)^");
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let d = chars.next()?;
            out.push_str(&directive_re(d)?);
        } else if c.is_whitespace() || crate::text::is_space(c) {
            while chars.peek().is_some_and(|n| n.is_whitespace() || crate::text::is_space(*n)) {
                chars.next();
            }
            out.push_str(r"\s+");
        } else {
            out.push_str(&regex::escape(&c.to_string()));
        }
    }
    Some(out)
}

fn compiled(fmt: &str) -> Option<Regex> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Regex>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = guard.get(fmt) {
        return r.clone();
    }
    let r = strptime_pattern(fmt).and_then(|p| Regex::new(&p).ok());
    if guard.len() > 256 {
        guard.clear();
    }
    guard.insert(fmt.to_string(), r.clone());
    r
}

/// `datetime.strptime(s, fmt)` (C locale, English names, case-insensitive like Python). `None`
/// where Python raises `ValueError`. Missing fields default like Python (year 1900, month 1,
/// day 1, 00:00:00); `%a`/`%A`/`%w`/`%u` are matched but do not change the date (Python only
/// uses the weekday to fill a missing day with `%U`/`%W`); `%j` sets the date within the year.
pub fn strptime(s: &str, fmt: &str) -> Option<NaiveDateTime> {
    let re = compiled(fmt)?;
    let caps = re.captures(s)?;
    if caps.get(0)?.end() != s.len() {
        return None; // "unconverted data remains"
    }
    let num = |name: &str| -> Option<Option<u32>> {
        match caps.name(name) {
            None => Some(None),
            Some(m) => m.as_str().trim().parse::<u32>().ok().map(Some),
        }
    };
    let mut year: Option<i32> = None;
    let (mut month, mut day) = (1u32, 1u32);
    let (mut hour, mut minute, mut second, mut micro) = (0u32, 0u32, 0u32, 0u32);
    if let Some(y) = num("y")? {
        year = Some(if y <= 68 { 2000 + y as i32 } else { 1900 + y as i32 });
    }
    if let Some(y) = num("Y")? {
        year = Some(y as i32);
    }
    if let Some(m) = num("m")? {
        month = m;
    }
    if let Some(b) = caps.name("B") {
        month = F_MONTH.iter().position(|x| *x == b.as_str().to_lowercase())? as u32 + 1;
    }
    if let Some(b) = caps.name("b") {
        month = A_MONTH.iter().position(|x| *x == b.as_str().to_lowercase())? as u32 + 1;
    }
    if let Some(d) = num("d")? {
        day = d;
    }
    if let Some(h) = num("H")? {
        hour = h;
    }
    if let Some(h) = num("I")? {
        hour = h;
        let ampm = caps.name("p").map(|p| p.as_str().to_lowercase()).unwrap_or_default();
        if ampm.is_empty() || ampm == "am" {
            if hour == 12 {
                hour = 0;
            }
        } else if ampm == "pm" && hour != 12 {
            hour += 12;
        }
    }
    if let Some(m) = num("M")? {
        minute = m;
    }
    if let Some(sv) = num("S")? {
        second = sv;
    }
    if let Some(f) = caps.name("f") {
        let mut t = f.as_str().to_string();
        while t.len() < 6 {
            t.push('0');
        }
        micro = t.parse().ok()?;
    }
    let julian = num("j")?;
    // Python: a year-less Feb 29 is computed in 1904 and then put back to 1900 -> ValueError.
    let year = year.unwrap_or(1900);
    let date = match julian {
        // Python: date.fromordinal(julian - 1 + date(year, 1, 1).toordinal()) - a julian past
        // the year end moves into the next year
        Some(j) => NaiveDate::from_ymd_opt(year, 1, 1)?.checked_add_days(chrono::Days::new((j - 1) as u64))?,
        None => NaiveDate::from_ymd_opt(year, month, day)?,
    };
    if second > 59 {
        return None; // datetime() rejects second=60/61
    }
    let time = NaiveTime::from_hms_micro_opt(hour, minute, second, micro)?;
    Some(NaiveDateTime::new(date, time))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let d = strptime("Thu Mar 4, 2027", "%a %b %d, %Y").unwrap();
        assert_eq!(d.to_string(), "2027-03-04 00:00:00");
        let d = strptime("Mon Sep 21, 2026, 10:15PM", "%a %b %d, %Y, %I:%M%p").unwrap();
        assert_eq!(d.to_string(), "2026-09-21 22:15:00");
        assert!(strptime("Feb 29 1900", "%b %d %Y").is_none());
        let (dt, off) = fromisoformat("2026-09-27T13:59+00:00").unwrap();
        assert_eq!((dt.to_string().as_str(), off), ("2026-09-27 13:59:00", Some(0)));
        assert_eq!(date_fromisoformat("20261104").unwrap().to_string(), "2026-11-04");
        assert_eq!(strftime_date(NaiveDate::from_ymd_opt(2026, 11, 4).unwrap(), "%a %b %d"), "Wed Nov 04");
    }
}
