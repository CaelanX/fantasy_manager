//! `datetime.datetime` as pydantic serializes it. Python mixes naive (local) and aware datetimes;
//! this type keeps the distinction so comparisons and output match.

use std::cmp::Ordering;
use std::fmt;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// A Python `datetime`: the wall-clock part plus the UTC offset in seconds when aware.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PyDateTime {
    pub local: NaiveDateTime,
    /// `None` = naive datetime (local time in Python).
    pub offset_secs: Option<i32>,
}

impl PyDateTime {
    pub fn naive(local: NaiveDateTime) -> Self {
        PyDateTime { local, offset_secs: None }
    }

    pub fn from_utc(dt: DateTime<Utc>) -> Self {
        PyDateTime { local: dt.naive_utc(), offset_secs: Some(0) }
    }

    pub fn from_fixed(dt: DateTime<FixedOffset>) -> Self {
        PyDateTime { local: dt.naive_local(), offset_secs: Some(dt.offset().local_minus_utc()) }
    }

    pub fn is_aware(&self) -> bool {
        self.offset_secs.is_some()
    }

    /// The instant in UTC (aware only).
    pub fn to_utc(&self) -> Option<DateTime<Utc>> {
        let off = FixedOffset::east_opt(self.offset_secs?)?;
        off.from_local_datetime(&self.local).single().map(|d| d.with_timezone(&Utc))
    }

    /// `dt.date()` (wall-clock date).
    pub fn date(&self) -> NaiveDate {
        self.local.date()
    }

    /// Parse ISO 8601 as Python `datetime.fromisoformat` (3.11+) / pydantic accept it:
    /// `YYYY-MM-DD[T ]HH:MM[:SS[.ffffff]][Z|+HH:MM|-HH:MM]`, or a bare date (midnight).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return Some(PyDateTime::naive(d.and_hms_opt(0, 0, 0)?));
        }
        let (body, offset) = if let Some(b) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
            (b, Some(0))
        } else {
            // a trailing +HH:MM / -HH:MM / +HHMM after the time part
            let tpos = s.find(['T', ' ']).unwrap_or(0);
            match s[tpos..].rfind(['+', '-']) {
                Some(i) if tpos > 0 => {
                    let (b, o) = s.split_at(tpos + i);
                    (b, Some(parse_offset(o)?))
                }
                _ => (s, None),
            }
        };
        let body = body.replacen(' ', "T", 1);
        let local = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"]
            .iter()
            .find_map(|f| NaiveDateTime::parse_from_str(&body, f).ok())?;
        Some(PyDateTime { local, offset_secs: offset })
    }

    /// pydantic v2 JSON form: `2026-11-04T23:00:00Z`, `...+01:00`, naive without suffix,
    /// microseconds only when non-zero (`.123456`).
    pub fn to_pydantic(&self) -> String {
        let mut out = self.local.format("%Y-%m-%dT%H:%M:%S").to_string();
        let micros = self.local.and_utc().timestamp_subsec_micros();
        if micros != 0 {
            out.push_str(&format!(".{micros:06}"));
        }
        match self.offset_secs {
            None => {}
            Some(0) => out.push('Z'),
            Some(o) => out.push_str(&fmt_offset(o)),
        }
        out
    }

    /// `dt.isoformat()` (Python): like pydantic but UTC is `+00:00`.
    pub fn isoformat(&self) -> String {
        let mut out = self.local.format("%Y-%m-%dT%H:%M:%S").to_string();
        let micros = self.local.and_utc().timestamp_subsec_micros();
        if micros != 0 {
            out.push_str(&format!(".{micros:06}"));
        }
        if let Some(o) = self.offset_secs {
            out.push_str(&fmt_offset(o));
        }
        out
    }

    /// `str(dt)`: isoformat with a space separator.
    pub fn py_str(&self) -> String {
        self.isoformat().replacen('T', " ", 1)
    }
}

fn parse_offset(o: &str) -> Option<i32> {
    let sign = if o.starts_with('-') { -1 } else { 1 };
    let digits: String = o[1..].chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 4 {
        return None;
    }
    let h: i32 = digits[0..2].parse().ok()?;
    let m: i32 = digits[2..4].parse().ok()?;
    let s: i32 = if digits.len() >= 6 { digits[4..6].parse().ok()? } else { 0 };
    Some(sign * (h * 3600 + m * 60 + s))
}

fn fmt_offset(o: i32) -> String {
    let sign = if o < 0 { '-' } else { '+' };
    let a = o.abs();
    if a % 60 != 0 {
        format!("{sign}{:02}:{:02}:{:02}", a / 3600, (a % 3600) / 60, a % 60)
    } else {
        format!("{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60)
    }
}

impl PartialOrd for PyDateTime {
    /// Aware vs aware compares instants, naive vs naive compares wall clock; mixing them raises
    /// TypeError in Python (here: `None`).
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self.to_utc(), other.to_utc()) {
            (Some(a), Some(b)) => Some(a.cmp(&b)),
            (None, None) if !self.is_aware() && !other.is_aware() => Some(self.local.cmp(&other.local)),
            _ => None,
        }
    }
}

impl fmt::Display for PyDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.py_str())
    }
}

impl Serialize for PyDateTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_pydantic())
    }
}

impl<'de> Deserialize<'de> for PyDateTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        PyDateTime::parse(&s).ok_or_else(|| de::Error::custom(format!("bad datetime {s:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_forms() {
        for s in ["2026-10-03T02:00:00Z", "2026-11-04T15:21:43.406000+00:00", "2026-11-04T10:00:00",
                  "2026-11-04T10:00:00-05:00"] {
            let d = PyDateTime::parse(s).unwrap();
            assert_eq!(PyDateTime::parse(&d.to_pydantic()).unwrap(), d, "{s}");
        }
        assert_eq!(PyDateTime::parse("2026-10-03T02:00:00Z").unwrap().to_pydantic(), "2026-10-03T02:00:00Z");
        assert_eq!(PyDateTime::parse("2026-11-04T15:21:43.406+00:00").unwrap().isoformat(),
                   "2026-11-04T15:21:43.406000+00:00");
    }
}
