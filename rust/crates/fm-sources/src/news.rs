//! Rust port of `fantasy_manager/providers/news.py` (RotoWire / ESPN RSS player news + the 30-day
//! `news_items.json` store). Owner: work package `W2-news-odds-llm`.
//!
//! Deviations (documented):
//! * feedparser is replaced by a small quick-xml reader that only extracts what the Python code
//!   reads (`title`, `description`/`summary`, `link`, `guid`/`id`, `pubDate`/`published`/
//!   `updated`). feedparser's `published_parsed` fallback (its own lenient date parser) is not
//!   ported: an item whose date `parse_rss_date` cannot read is undated.
//! * The clock is a parameter (`now`), see ARCHITECTURE.md 4.5.
//! * `email.utils.parsedate_to_datetime` is ported literally (`parsedate_tz`).
//! * `pyshim` holds small Python-compat helpers used by this package (`str()`/`repr()`/`int()`/
//!   `float()` of json.loads values, Python whitespace / `\w`, atomic writes); JSON output goes
//!   through `fm_pycompat::pyjson`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, TimeZone, Utc};
use fm_core::PyDateTime;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value};

/// `providers.news.NewsItem` (pydantic). `published` is timezone-aware UTC.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewsItem {
    /// "rotowire" | "espn"
    pub source: String,
    /// feed guid (for dedupe)
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub player_name: Option<String>,
    pub headline: String,
    #[serde(default)]
    pub blurb: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub published: Option<PyDateTime>,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl NewsItem {
    /// `model_dump(mode="json")`.
    pub fn model_dump_json(&self) -> JsonMap<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m,
            _ => JsonMap::new(),
        }
    }
}

pub const ROTOWIRE_URL: &str = "https://www.rotowire.com/rss/news.php?sport=NHL";
pub const ESPN_NEWS_URL: &str = "https://www.espn.com/espn/rss/nhl/news";

/// `FetchText = Callable[[str, dict | None], str]` (the news clients always pass `params=None`).
pub type FetchText<'a> = dyn Fn(&str, Option<&fm_http::Params>) -> anyhow::Result<String> + 'a;

// --------------------------------------------------------------------------- python shims

pub(crate) mod pyshim {
    //! Python-compat helpers private to this package.
    use serde_json::{Map as JsonMap, Value};

    /// Python `str.isspace()` for one char (Unicode White_Space plus \x1c-\x1f).
    pub fn is_py_space(c: char) -> bool {
        c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
    }

    /// `\s` in a Python `re` str pattern, as a Rust `regex` class.
    pub const WS: &str = r"[\s\x1C-\x1F]";

    /// Python `\w` for one char (`str.isalnum()` or `_`; Rust's alphanumeric is a close match).
    pub fn is_py_word(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    pub fn strip(s: &str) -> &str {
        s.trim_matches(is_py_space)
    }
    pub fn rstrip(s: &str) -> &str {
        s.trim_end_matches(is_py_space)
    }

    /// `" ".join(s.split())`
    pub fn collapse_ws(s: &str) -> String {
        s.split(is_py_space).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ")
    }

    /// `s[:n]` on code points.
    pub fn take_chars(s: &str, n: usize) -> &str {
        match s.char_indices().nth(n) {
            Some((i, _)) => &s[..i],
            None => s,
        }
    }

    pub fn char_len(s: &str) -> usize {
        s.chars().count()
    }

    /// Python truthiness of a JSON value.
    pub fn truthy(v: &Value) -> bool {
        match v {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
        }
    }

    /// Python `repr(s)` for a str.
    pub fn repr_str(s: &str) -> String {
        let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
        let mut out = String::with_capacity(s.len() + 2);
        out.push(quote);
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c == quote => {
                    out.push('\\');
                    out.push(c);
                }
                c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
                c if c.is_control() || matches!(c as u32, 0x80..=0xa0 | 0xad) => {
                    let u = c as u32;
                    if u <= 0xff {
                        out.push_str(&format!("\\x{u:02x}"));
                    } else if u <= 0xffff {
                        out.push_str(&format!("\\u{u:04x}"));
                    } else {
                        out.push_str(&format!("\\U{u:08x}"));
                    }
                }
                c => out.push(c),
            }
        }
        out.push(quote);
        out
    }

    fn num_str(n: &serde_json::Number) -> String {
        if let Some(i) = n.as_i64() {
            i.to_string()
        } else if let Some(u) = n.as_u64() {
            u.to_string()
        } else {
            fm_pycompat::fmt::repr_f64(n.as_f64().unwrap_or(f64::NAN))
        }
    }

    /// Python `repr()` of a json.loads value.
    pub fn repr_value(v: &Value) -> String {
        match v {
            Value::Null => "None".into(),
            Value::Bool(true) => "True".into(),
            Value::Bool(false) => "False".into(),
            Value::Number(n) => num_str(n),
            Value::String(s) => repr_str(s),
            Value::Array(a) => format!("[{}]", a.iter().map(repr_value).collect::<Vec<_>>().join(", ")),
            Value::Object(o) => format!(
                "{{{}}}",
                o.iter().map(|(k, v)| format!("{}: {}", repr_str(k), repr_value(v))).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// Python `str()` of a json.loads value.
    pub fn str_value(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            other => repr_value(other),
        }
    }

    /// Python `int(str)` for ASCII input: surrounding whitespace, sign, `_` between digits.
    pub fn int_from_str(s: &str) -> Option<i64> {
        let t = strip(s);
        let (neg, body) = match t.as_bytes().first() {
            Some(b'-') => (true, &t[1..]),
            Some(b'+') => (false, &t[1..]),
            _ => (false, t),
        };
        if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
            return None;
        }
        let digits: String = body.chars().filter(|c| *c != '_').collect();
        if !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let v: i64 = digits.parse().ok()?;
        Some(if neg { -v } else { v })
    }

    /// Python `float(str)`: whitespace, sign, `inf`/`infinity`/`nan`, `_` between digits.
    pub fn float_from_str(s: &str) -> Option<f64> {
        let t = strip(s);
        if t.is_empty() || t.starts_with('_') || t.ends_with('_') || t.contains("__") {
            return None;
        }
        let cleaned: String = t.chars().filter(|c| *c != '_').collect();
        let lower = cleaned.to_ascii_lowercase();
        let body = lower.trim_start_matches(['+', '-']);
        if body == "inf" || body == "infinity" || body == "nan" {
            let neg = lower.starts_with('-');
            return Some(match (body, neg) {
                ("nan", _) => f64::NAN,
                (_, true) => f64::NEG_INFINITY,
                _ => f64::INFINITY,
            });
        }
        if !body.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | '+' | '-')) {
            return None;
        }
        cleaned.parse::<f64>().ok()
    }

    /// Python `int(v)` of a json.loads value (None = TypeError / ValueError).
    pub fn py_int(v: &Value) -> Option<i64> {
        match v {
            Value::Bool(b) => Some(*b as i64),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Some(i)
                } else {
                    let f = n.as_f64()?;
                    if f.is_finite() {
                        Some(f.trunc() as i64)
                    } else {
                        None
                    }
                }
            }
            Value::String(s) => int_from_str(s),
            _ => None,
        }
    }

    /// Python `float(v)` of a json.loads value (None = TypeError / ValueError).
    pub fn py_float(v: &Value) -> Option<f64> {
        match v {
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Number(n) => n.as_f64(),
            Value::String(s) => float_from_str(s),
            _ => None,
        }
    }

    /// `json.dumps(v, indent=indent, sort_keys=sort_keys, separators=separators)` with
    /// `ensure_ascii=True` (delegates to `fm_pycompat::pyjson`).
    pub fn dumps(v: &Value, indent: Option<usize>, sort_keys: bool, separators: Option<(&'static str, &'static str)>) -> String {
        let opts = fm_pycompat::pyjson::DumpOpts { indent, ensure_ascii: true, sort_keys, separators };
        fm_pycompat::pyjson::dumps(v, &opts)
    }

    /// Atomic `tmp.write_text(...)` + `tmp.replace(path)`.
    pub fn write_atomic(path: &std::path::Path, tmp_suffix: &str, text: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let tmp = with_suffix(path, tmp_suffix);
        std::fs::write(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, path)
    }

    /// `Path.with_suffix(suffix)` (replaces the last extension).
    pub fn with_suffix(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        path.with_file_name(format!("{stem}{suffix}"))
    }

    /// `json.loads(path.read_text())` as an object, else an empty map.
    pub fn read_json_object(path: &std::path::Path) -> JsonMap<String, Value> {
        match std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            Some(Value::Object(m)) => m,
            _ => JsonMap::new(),
        }
    }
}

use pyshim::WS;

fn re(p: &str) -> Regex {
    Regex::new(p).expect("static regex")
}

/// A Python `re` pattern with `\s` widened to Python whitespace.
fn pyre(p: &str) -> Regex {
    re(&p.replace(r"\s", WS))
}

// --------------------------------------------------------------------------- dates

const TZ_OFFSETS: &[(&str, i64)] = &[
    ("UTC", 0), ("GMT", 0), ("Z", 0),
    ("EST", -5), ("EDT", -4), ("CST", -6), ("CDT", -5), ("MST", -7), ("MDT", -6), ("PST", -8), ("PDT", -7),
    ("AKST", -9), ("AKDT", -8), ("HST", -10),
];
const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

fn date_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        pyre(
            r"(?:\w{3},\s*)?(\d{1,2})\s+(\w{3})\w*\s+(\d{4})\s+(\d{1,2}):(\d{2})(?::(\d{2}))?\s*([AaPp][Mm])?\s*([A-Za-z]{1,5}|[+-]\d{4})?",
        )
    })
}

fn ampm_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"\b[AaPp][Mm]\b"))
}

/// `datetime(y, m, d, H, M, S, tzinfo=timezone(offset_secs))` -> UTC, None on ValueError.
fn make_utc(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64, offset_secs: i64) -> Option<DateTime<Utc>> {
    if offset_secs.abs() >= 86400 || !(1..=9999).contains(&y) {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(y as i32, u32::try_from(mo).ok()?, u32::try_from(d).ok()?)?;
    let naive = date.and_hms_opt(u32::try_from(h).ok()?, u32::try_from(mi).ok()?, u32::try_from(s).ok()?)?;
    let off = FixedOffset::east_opt(offset_secs as i32)?;
    Some(off.from_local_datetime(&naive).single()?.with_timezone(&Utc))
}

const PARSEDATE_MONTHS: [&str; 24] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec", "january", "february",
    "march", "april", "may", "june", "july", "august", "september", "october", "november", "december",
];
const PARSEDATE_DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const PARSEDATE_ZONES: &[(&str, i64)] = &[
    ("UT", 0), ("UTC", 0), ("GMT", 0), ("Z", 0), ("AST", -400), ("ADT", -300), ("EST", -500), ("EDT", -400),
    ("CST", -600), ("CDT", -500), ("MST", -700), ("MDT", -600), ("PST", -800), ("PDT", -700),
];

/// `email._parseaddr._parsedate_tz` -> (y, m, d, H, M, S, tz offset seconds or None).
type ParsedDate = (i64, i64, i64, i64, i64, i64, Option<i64>);

fn parsedate_tz(data: &str) -> Option<ParsedDate> {
    let mut data: Vec<String> = data.split(pyshim::is_py_space).filter(|t| !t.is_empty()).map(String::from).collect();
    if data.is_empty() {
        return None;
    }
    if data[0].ends_with(',') || PARSEDATE_DAYS.contains(&data[0].to_lowercase().as_str()) {
        data.remove(0);
    } else if let Some(i) = data[0].rfind(',') {
        data[0] = data[0][i + 1..].to_string();
    }
    if data.len() == 3 {
        let stuff: Vec<String> = data[0].split('-').map(String::from).collect();
        if stuff.len() == 3 {
            let mut d = stuff;
            d.extend(data[1..].iter().cloned());
            data = d;
        }
    }
    if data.len() == 4 {
        let s = data[3].clone();
        let i = s.find('+').or_else(|| s.find('-'));
        match i {
            Some(i) if i > 0 => {
                data.truncate(3);
                data.push(s[..i].to_string());
                data.push(s[i..].to_string());
            }
            _ => data.push(String::new()),
        }
    }
    if data.len() < 5 {
        return None;
    }
    data.truncate(5);
    let (mut dd, mut mm, mut yy, mut tm, mut tz) =
        (data[0].clone(), data[1].clone(), data[2].clone(), data[3].clone(), data[4].clone());
    if dd.is_empty() || mm.is_empty() || yy.is_empty() {
        return None;
    }
    mm = mm.to_lowercase();
    if !PARSEDATE_MONTHS.contains(&mm.as_str()) {
        let tmp = dd.to_lowercase();
        dd = mm;
        mm = tmp;
        if !PARSEDATE_MONTHS.contains(&mm.as_str()) {
            return None;
        }
    }
    let mut mon = PARSEDATE_MONTHS.iter().position(|m| *m == mm)? as i64 + 1;
    if mon > 12 {
        mon -= 12;
    }
    if dd.ends_with(',') {
        dd.pop();
    }
    if let Some(i) = yy.find(':') {
        if i > 0 {
            std::mem::swap(&mut yy, &mut tm);
        }
    }
    if yy.ends_with(',') {
        yy.pop();
        if yy.is_empty() {
            return None;
        }
    }
    if !yy.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        std::mem::swap(&mut yy, &mut tz);
    }
    if tm.ends_with(',') {
        tm.pop();
    }
    let mut parts: Vec<String> = tm.split(':').map(String::from).collect();
    let (thh, tmm, tss) = match parts.len() {
        2 => (parts[0].clone(), parts[1].clone(), "0".to_string()),
        3 => (parts[0].clone(), parts[1].clone(), parts[2].clone()),
        1 if parts[0].contains('.') => {
            parts = parts[0].split('.').map(String::from).collect();
            match parts.len() {
                2 => (parts[0].clone(), parts[1].clone(), "0".to_string()),
                3 => (parts[0].clone(), parts[1].clone(), parts[2].clone()),
                _ => return None,
            }
        }
        _ => return None,
    };
    let mut y = pyshim::int_from_str(&yy)?;
    let d = pyshim::int_from_str(&dd)?;
    let h = pyshim::int_from_str(&thh)?;
    let mi = pyshim::int_from_str(&tmm)?;
    let s = pyshim::int_from_str(&tss)?;
    if y < 100 {
        y += if y > 68 { 1900 } else { 2000 };
    }
    let tzu = tz.to_uppercase();
    let mut tzoffset: Option<i64> = None;
    if let Some((_, v)) = PARSEDATE_ZONES.iter().find(|(k, _)| *k == tzu) {
        tzoffset = Some(*v);
    } else {
        tzoffset = pyshim::int_from_str(&tzu).or(tzoffset);
        if tzoffset == Some(0) && tzu.starts_with('-') {
            tzoffset = None;
        }
    }
    if let Some(off) = tzoffset {
        if off != 0 {
            let (sign, a) = if off < 0 { (-1, -off) } else { (1, off) };
            tzoffset = Some(sign * ((a / 100) * 3600 + (a % 100) * 60));
        }
    }
    Some((y, mon, d, h, mi, s, tzoffset))
}

/// Parse RSS pubDate, including RotoWire's 'Mon, 28 Sep 2026 2:38:00 PM PDT'. Returns UTC.
pub fn parse_rss_date(value: Option<&str>) -> Option<PyDateTime> {
    let value = pyshim::strip(value.filter(|v| !v.is_empty())?);
    if !ampm_re().is_match(value) {
        if let Some((y, mo, d, h, mi, s, tz)) = parsedate_tz(value) {
            if let Some(dt) = make_utc(y, mo, d, h, mi, s, tz.unwrap_or(0)) {
                return Some(PyDateTime::from_utc(dt));
            }
        }
    }
    let caps = date_re().captures(value)?;
    let g = |i: usize| caps.get(i).map(|m| m.as_str());
    let mon3: String = g(2)?.chars().take(3).collect::<String>().to_lowercase();
    let month = MONTHS.iter().position(|m| *m == mon3)? as i64 + 1;
    let mut hour = pyshim::int_from_str(g(4)?)?;
    if let Some(ampm) = g(7) {
        let ampm = ampm.to_uppercase();
        if ampm == "PM" && hour != 12 {
            hour += 12;
        } else if ampm == "AM" && hour == 12 {
            hour = 0;
        }
    }
    let mut offset = 0i64;
    if let Some(tz) = g(8) {
        let tzc: Vec<char> = tz.chars().collect();
        let first = tzc[0];
        if (first == '+' || first == '-') && tzc.len() == 5 && tzc[1..].iter().all(|c| c.is_ascii_digit()) {
            let sign = if first == '+' { 1 } else { -1 };
            let hh: i64 = tz[1..3].parse().ok()?;
            let mm: i64 = tz[3..5].parse().ok()?;
            offset = sign * (hh * 3600 + mm * 60);
        } else if first == '+' || first == '-' {
            return None; // non-ASCII digits in the offset: not ported
        } else {
            let up = tz.to_uppercase();
            offset = TZ_OFFSETS.iter().find(|(k, _)| *k == up).map(|(_, v)| *v).unwrap_or(0) * 3600;
        }
    }
    let year = pyshim::int_from_str(g(3)?)?;
    let day = pyshim::int_from_str(g(1)?)?;
    let minute = pyshim::int_from_str(g(5)?)?;
    let sec = match g(6) {
        Some(s) => pyshim::int_from_str(s)?,
        None => 0,
    };
    make_utc(year, month, day, hour, minute, sec, offset).map(PyDateTime::from_utc)
}

// --------------------------------------------------------------------------- tags / cleaning

fn tag_patterns() -> &'static [(&'static str, Regex)] {
    static R: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    R.get_or_init(|| {
        let ci = |p: &str| pyre(&format!("(?i:{p})"));
        vec![
            ("injury", ci(r"injur|upper[- ]body|lower[- ]body|\((?:hip|knee|shoulder|ankle|groin|back|head|concussion|hand|wrist|foot|leg|arm|illness|undisclosed|neck|face|elbow|finger|lower body|upper body)\)")),
            ("ir", pyre(r"\bIR\b|injured reserve|\bLTIR\b")),
            ("day-to-day", ci(r"day[- ]to[- ]day")),
            ("upper-body", ci(r"upper[- ]body")),
            ("lower-body", ci(r"lower[- ]body")),
            ("out", ci(r"\bout\b|\bsidelined\b")),
            ("return", ci(r"\breturn|\bactivated\b|\bback in (?:the )?lineup")),
            ("line", ci(r"\bline(?:s|mates?|up)?\b")),
            ("top-line", ci(r"\btop[- ](?:line|six|pairing)")),
            ("power-play", ci(r"power[- ]play|\bPP1\b|\bPP2\b")),
            ("scratch", ci(r"\bscratch")),
            ("waivers", ci(r"\bwaiv")),
            ("trade", ci(r"\btrad(?:e|ed|es|ing)\b|\bshipped\b|\bdealt\b")),
            ("acquired", ci(r"\bacquir")),
            ("signing", ci(r"\bsign(?:s|ed|ing)?\b|\bcontract\b|\bextension\b")),
            ("recall", ci(r"\brecall|\bcalled up\b|\bpromoted\b")),
            ("demotion", ci(r"\bdemot|\bassigned to\b|\bsent down\b|\breassigned\b")),
            ("goalie", ci(r"\bgoalie|\bgoaltend|\bnetminder|\bcrease\b")),
            ("start", ci(r"\bstart(?:s|ed|ing)?\b|\bget the nod\b|\bgets the nod\b")),
        ]
    })
}

/// `TAG_PATTERNS` names in order.
pub fn tag_names() -> Vec<&'static str> {
    tag_patterns().iter().map(|(t, _)| *t).collect()
}

/// `tag_text(*texts)`: tags whose pattern matches the non-empty texts joined by a space.
pub fn tag_text(texts: &[Option<&str>]) -> Vec<String> {
    let blob = texts.iter().filter_map(|t| t.filter(|s| !s.is_empty())).collect::<Vec<_>>().join(" ");
    tag_patterns().iter().filter(|(_, p)| p.is_match(&blob)).map(|(t, _)| t.to_string()).collect()
}

fn tag_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"<[^>]+>"))
}

fn ws_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(&format!("{WS}+")))
}

fn rotowire_trailer() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| pyre(r"(?i)\s*Visit RotoWire\.com for more analysis on this update\.?\s*\n?\z"))
}

/// `_clean(text)`: tags -> space, `html.unescape`, collapse whitespace, strip.
pub(crate) fn clean(text: Option<&str>) -> String {
    let Some(text) = text.filter(|t| !t.is_empty()) else { return String::new() };
    let no_tags = tag_re().replace_all(text, " ");
    let unescaped = htmlize::unescape(no_tags.as_ref());
    pyshim::strip(&ws_re().replace_all(&unescaped, " ")).to_string()
}

/// `re.sub(r"(?<!:)//+", "/", url)`.
fn collapse_slashes(url: &str) -> String {
    let chars: Vec<char> = url.chars().collect();
    let mut out = String::with_capacity(url.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '/' && chars.get(i + 1) == Some(&'/') && (i == 0 || chars[i - 1] != ':') {
            while i < chars.len() && chars[i] == '/' {
                i += 1;
            }
            out.push('/');
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

// --------------------------------------------------------------------------- RSS

/// The fields of a feedparser entry this module reads.
#[derive(Clone, Debug, Default)]
pub(crate) struct FeedEntry {
    title: Option<String>,
    summary: Option<String>,
    link: Option<String>,
    id: Option<String>,
    published: Option<String>,
    updated: Option<String>,
}

impl FeedEntry {
    fn date(&self) -> Option<PyDateTime> {
        let raw = self.published.as_deref().filter(|s| !s.is_empty()).or(self.updated.as_deref());
        parse_rss_date(raw)
    }
}

fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name).to_lowercase();
    s
}

/// Minimal RSS 2.0 / Atom entry reader (the feedparser replacement).
pub(crate) fn parse_feed(xml: &str) -> Vec<FeedEntry> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut entries = Vec::new();
    let mut cur: Option<FeedEntry> = None;
    // element path inside the current item
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut guid_is_link = false;
    let mut link_from_guid = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if cur.is_none() {
                    if name == "item" || name == "entry" {
                        cur = Some(FeedEntry::default());
                        path.clear();
                        guid_is_link = false;
                        link_from_guid = false;
                    }
                    continue;
                }
                if path.is_empty() {
                    text.clear();
                    if name == "guid" {
                        // feedparser: a permalink guid doubles as the link when there is none
                        guid_is_link = !e
                            .try_get_attribute("isPermaLink")
                            .ok()
                            .flatten()
                            .and_then(|a| a.unescape_value().ok().map(|v| v.trim().eq_ignore_ascii_case("false")))
                            .unwrap_or(false);
                    }
                    if name == "link" {
                        // Atom: <link href="..."/> (as a non-empty element)
                        if let Some(href) = e.try_get_attribute("href").ok().flatten() {
                            if let (Some(c), Ok(v)) = (cur.as_mut(), href.unescape_value()) {
                                if c.link.is_none() {
                                    c.link = Some(v.into_owned());
                                }
                            }
                        }
                    }
                }
                path.push(name);
            }
            Ok(Event::Empty(e)) => {
                if let Some(c) = cur.as_mut() {
                    if path.is_empty() && local_name(e.name().as_ref()) == "link" {
                        if let Some(href) = e.try_get_attribute("href").ok().flatten() {
                            if let Ok(v) = href.unescape_value() {
                                if c.link.is_none() {
                                    c.link = Some(v.into_owned());
                                }
                            }
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if cur.is_some() && !path.is_empty() {
                    match t.unescape() {
                        Ok(s) => text.push_str(&s),
                        Err(_) => text.push_str(&String::from_utf8_lossy(t.as_ref())),
                    }
                }
            }
            Ok(Event::CData(t)) => {
                if cur.is_some() && !path.is_empty() {
                    text.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                let Some(c) = cur.as_mut() else { continue };
                if path.is_empty() {
                    if name == "item" || name == "entry" {
                        entries.push(cur.take().unwrap_or_default());
                    }
                    continue;
                }
                path.pop();
                if path.is_empty() {
                    let val = text.trim().to_string();
                    let val_raw = std::mem::take(&mut text);
                    match name.as_str() {
                        "title" => c.title = Some(val_raw),
                        "description" | "summary" => {
                            if c.summary.is_none() {
                                c.summary = Some(val_raw);
                            }
                        }
                        "link" => {
                            if c.link.is_none() || !val.is_empty() || link_from_guid {
                                c.link = Some(val);
                                link_from_guid = false;
                            }
                        }
                        "guid" => {
                            if guid_is_link && c.link.is_none() {
                                c.link = Some(val.clone());
                                link_from_guid = true;
                            }
                            c.id = Some(val);
                        }
                        "id" => c.id = Some(val),
                        "pubdate" | "published" | "issued" => c.published = Some(val),
                        "updated" | "dc:date" | "modified" => c.updated = Some(val),
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    entries
}

fn nonempty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|v| !v.is_empty())
}

pub fn parse_rotowire(xml: &str) -> Vec<NewsItem> {
    let mut items = Vec::new();
    for e in parse_feed(xml) {
        let title = clean(e.title.as_deref());
        if title.is_empty() {
            continue;
        }
        let (player, headline): (Option<String>, String) = match title.split_once(": ") {
            Some((p, h)) => (Some(p.to_string()), h.to_string()),
            None => (None, title.clone()),
        };
        let summary = clean(nonempty(&e.summary));
        let blurb = rotowire_trailer().replace(&summary, "").into_owned();
        let url = nonempty(&e.link).map(collapse_slashes).or_else(|| e.link.clone());
        let tags = tag_text(&[Some(&headline), Some(&blurb)]);
        let headline_out = pyshim::strip(if headline.is_empty() { &title } else { &headline }).to_string();
        items.push(NewsItem {
            source: "rotowire".into(),
            id: e.id.clone(),
            player_name: player.as_deref().filter(|p| !p.is_empty()).map(|p| pyshim::strip(p).to_string()),
            headline: headline_out,
            blurb,
            url,
            published: e.date(),
            tags,
        });
    }
    items
}

pub fn parse_espn_news(xml: &str) -> Vec<NewsItem> {
    let mut items = Vec::new();
    for e in parse_feed(xml) {
        let title = clean(e.title.as_deref());
        if title.is_empty() {
            continue;
        }
        let blurb = clean(nonempty(&e.summary));
        let tags = tag_text(&[Some(&title), Some(&blurb)]);
        items.push(NewsItem {
            source: "espn".into(),
            id: e.id.clone(),
            player_name: None,
            headline: title,
            blurb,
            url: e.link.clone(),
            published: e.date(),
            tags,
        });
    }
    items
}

/// `default_fetch_text(url)`: GET with the news User-Agent, redirects followed, 20 s timeout.
pub fn default_fetch_text(url: &str, params: Option<&fm_http::Params>) -> anyhow::Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("Mozilla/5.0 (fantasy-manager/0.1)")
        .build()?;
    let mut req = client.get(url);
    if let Some(ps) = params {
        let mut q: Vec<(String, String)> = Vec::new();
        for (k, v) in ps {
            match v {
                fm_http::cache::ParamValue::Str(s) => q.push((k.clone(), s.clone())),
                fm_http::cache::ParamValue::List(l) => q.extend(l.iter().map(|s| (k.clone(), s.clone()))),
                fm_http::cache::ParamValue::None => {}
            }
        }
        req = req.query(&q);
    }
    let resp = req.send()?.error_for_status()?;
    Ok(resp.text()?)
}

fn fetch_with(fetch_text: Option<&FetchText>, url: &str) -> anyhow::Result<String> {
    match fetch_text {
        Some(f) => f(url, None),
        None => default_fetch_text(url, None),
    }
}

pub fn fetch_rotowire(fetch_text: Option<&FetchText>) -> anyhow::Result<Vec<NewsItem>> {
    Ok(parse_rotowire(&fetch_with(fetch_text, ROTOWIRE_URL)?))
}

pub fn fetch_espn_news(fetch_text: Option<&FetchText>) -> anyhow::Result<Vec<NewsItem>> {
    Ok(parse_espn_news(&fetch_with(fetch_text, ESPN_NEWS_URL)?))
}

/// All sources, newest first. A failing source is logged and skipped.
pub fn fetch_all_news(fetch_text: Option<&FetchText>) -> Vec<NewsItem> {
    let mut items = Vec::new();
    for name in ["fetch_rotowire", "fetch_espn_news"] {
        let res = if name == "fetch_rotowire" { fetch_rotowire(fetch_text) } else { fetch_espn_news(fetch_text) };
        match res {
            Ok(v) => items.extend(v),
            Err(exc) => tracing::warn!("news source {} failed: {}", name, exc),
        }
    }
    newest_first(items)
}

// --------------------------------------------------------------------------- 30-day store

pub const STORE_NAME: &str = "news_items.json";
/// rookie role-signal items only (before the shared store)
pub const LEGACY_STORE_NAME: &str = "news_role_items.json";
pub const STORE_MAX_AGE_DAYS: i64 = 30;
/// ~30 days of both feeds fetched several times a day
pub const STORE_MAX_ITEMS: usize = 5000;

/// `datetime.min.replace(tzinfo=timezone.utc)`
fn epoch() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(1, 1, 1, 0, 0, 0).single().unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// An aware datetime's instant (a naive one is read as UTC).
fn instant(dt: &PyDateTime) -> DateTime<Utc> {
    dt.to_utc().unwrap_or_else(|| dt.local.and_utc())
}

fn newest_first(items: impl IntoIterator<Item = NewsItem>) -> Vec<NewsItem> {
    let mut v: Vec<NewsItem> = items.into_iter().collect();
    let e = epoch();
    v.sort_by(|a, b| {
        let ka = a.published.as_ref().map(instant).unwrap_or(e);
        let kb = b.published.as_ref().map(instant).unwrap_or(e);
        kb.cmp(&ka)
    });
    v
}

/// Dedupe key: the feed guid, else source + headline + publish time.
pub fn item_key(it: &NewsItem) -> String {
    match it.id.as_deref().filter(|s| !s.is_empty()) {
        Some(id) => id.to_string(),
        None => format!(
            "{}|{}|{}",
            it.source,
            it.headline,
            it.published.as_ref().map(|p| p.isoformat()).unwrap_or_default()
        ),
    }
}

/// `as_of: date | datetime | None`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AsOf {
    Date(NaiveDate),
    DateTime(PyDateTime),
}

/// Reference time for the age cut-off: now, or the end of day `as_of` (UTC).
fn ref_time(as_of: Option<AsOf>, now: DateTime<Utc>) -> DateTime<Utc> {
    match as_of {
        None => now,
        Some(AsOf::DateTime(dt)) => instant(&dt),
        Some(AsOf::Date(d)) => d.and_hms_opt(0, 0, 0).map(|n| n.and_utc()).unwrap_or(now) + Duration::days(1),
    }
}

fn write_store(store_dir: &Path, raw: &JsonMap<String, Value>) -> bool {
    let path = store_dir.join(STORE_NAME);
    let text = pyshim::dumps(&Value::Object(raw.clone()), Some(1), true, None);
    match pyshim::write_atomic(&path, ".json.tmp", &text) {
        Ok(()) => true,
        Err(e) => {
            tracing::info!("news store not saved: {}", e);
            false
        }
    }
}

/// The store's {key: stored item}, with the old rookie-signal item file merged in once.
fn load_raw(store_dir: &Path) -> JsonMap<String, Value> {
    let mut raw = pyshim::read_json_object(&store_dir.join(STORE_NAME));
    let legacy = store_dir.join(LEGACY_STORE_NAME);
    if legacy.exists() {
        for (key, d) in pyshim::read_json_object(&legacy) {
            raw.entry(key).or_insert(d);
        }
        if write_store(store_dir, &raw) {
            let _ = std::fs::remove_file(&legacy);
        }
    }
    raw
}

/// When a stored item was published, else first stored.
fn seen(d: &JsonMap<String, Value>) -> Option<DateTime<Utc>> {
    for k in ["published", "seen"] {
        if let Some(v) = d.get(k) {
            if pyshim::truthy(v) {
                match PyDateTime::parse(&pyshim::str_value(v)) {
                    Some(dt) => return Some(instant(&dt)),
                    None => continue,
                }
            }
        }
    }
    None
}

fn prune(raw: &JsonMap<String, Value>, reft: DateTime<Utc>) -> JsonMap<String, Value> {
    let cutoff = reft - Duration::days(STORE_MAX_AGE_DAYS);
    let mut keep: Vec<(&String, &Value, Option<DateTime<Utc>>)> = raw
        .iter()
        .filter_map(|(k, d)| {
            let obj = d.as_object()?;
            let s = seen(obj);
            (s.unwrap_or(reft) >= cutoff).then_some((k, d, s))
        })
        .collect();
    let e = epoch();
    keep.sort_by_key(|k| std::cmp::Reverse(k.2.unwrap_or(e)));
    keep.into_iter().take(store_max_items()).map(|(k, d, _)| (k.clone(), d.clone())).collect()
}

thread_local! {
    static MAX_ITEMS_OVERRIDE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn store_max_items() -> usize {
    MAX_ITEMS_OVERRIDE.with(|c| c.get()).unwrap_or(STORE_MAX_ITEMS)
}

/// Test hook (Python tests monkeypatch `STORE_MAX_ITEMS`): per-thread override.
#[doc(hidden)]
pub fn set_store_max_items_for_tests(n: Option<usize>) {
    MAX_ITEMS_OVERRIDE.with(|c| c.set(n));
}

fn items_of(raw: &JsonMap<String, Value>) -> Vec<NewsItem> {
    raw.values().filter_map(|d| serde_json::from_value::<NewsItem>(d.clone()).ok()).collect()
}

/// Items in the store no older than STORE_MAX_AGE_DAYS before `as_of` (default now), newest first.
pub fn stored_news(store_dir: Option<&Path>, as_of: Option<AsOf>, now: DateTime<Utc>) -> Vec<NewsItem> {
    let Some(dir) = store_dir else { return vec![] };
    newest_first(items_of(&prune(&load_raw(dir), ref_time(as_of, now))))
}

/// Merge `items` into the store (a fresh copy replaces a stored one with the same key), prune it
/// to the last STORE_MAX_AGE_DAYS / STORE_MAX_ITEMS, save it, and return every stored item no
/// older than that before `as_of`, newest first. Without `store_dir`: just `items`.
pub fn store_news(store_dir: Option<&Path>, items: &[NewsItem], as_of: Option<AsOf>, now: DateTime<Utc>) -> Vec<NewsItem> {
    let Some(path) = store_dir else {
        let mut by_key: fm_core::Map<String, NewsItem> = fm_core::Map::new();
        for it in items {
            by_key.insert(item_key(it), it.clone());
        }
        return newest_first(by_key.into_values());
    };
    let mut raw = load_raw(path);
    let now_iso = PyDateTime::from_utc(now).isoformat();
    for it in items {
        let key = item_key(it);
        let mut d = it.model_dump_json();
        let prev_seen = raw
            .get(&key)
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("seen"))
            .filter(|v| pyshim::truthy(v))
            .cloned();
        d.insert("seen".into(), prev_seen.unwrap_or_else(|| Value::String(now_iso.clone())));
        raw.insert(key, Value::Object(d));
    }
    let reft = ref_time(as_of, now);
    // Retention follows the earlier of now and as_of, so a past as_of (a replay) keeps its window.
    let raw = prune(&raw, now.min(reft));
    write_store(path, &raw);
    newest_first(items_of(&prune(&raw, reft)))
}

/// Fetch every source, merge the fresh items into the 30-day store and return (fresh + stored
/// items newest first, [(source label, error)] for sources that failed).
pub fn fetch_news(
    fetch_text: Option<&FetchText>,
    store_dir: Option<&Path>,
    as_of: Option<AsOf>,
    now: DateTime<Utc>,
) -> (Vec<NewsItem>, Vec<(String, anyhow::Error)>) {
    let mut fresh = Vec::new();
    let mut failed = Vec::new();
    for label in ["RotoWire news", "ESPN news"] {
        let res = if label == "RotoWire news" { fetch_rotowire(fetch_text) } else { fetch_espn_news(fetch_text) };
        match res {
            Ok(v) => fresh.extend(v),
            Err(e) => failed.push((label.to_string(), e)),
        }
    }
    (store_news(store_dir, &fresh, as_of, now), failed)
}

/// `<store_dir>/news_items.json`.
pub fn store_path(store_dir: &Path) -> PathBuf {
    store_dir.join(STORE_NAME)
}
