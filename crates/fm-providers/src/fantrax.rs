//! Rust port of `fantasy_manager/providers/fantrax.py` (owner: work package `W3-fantrax`).
//!
//! Talks to Fantrax's private JSON endpoint (`POST https://www.fantrax.com/fxpa/req`) with a
//! logged-in cookie session, using fantraxapi's message format (`Method.msg_block`). Responses are
//! parsed here directly from the raw tables, the league Rules page (`getLeagueRulesOld`, HTML) is
//! parsed with regexes as in Python. Every response is cached as JSON under
//! `$FM_DATA_DIR/fantrax_cache` (shared with Python through [`fm_http::FxpaFileCache`]); a parity
//! [`Tape`] can answer every request instead. Cookie values are never logged or cached.
//!
//! Deliberate deviations from Python (documented, no effect on outputs):
//! * The session is the [`FxSession`] trait (a `requests.Session` stand-in) so tests can inject
//!   fakes; [`HttpSession`] is the reqwest implementation with a manual cookie jar (it keeps each
//!   cookie's domain / path / expiry, which `fantrax_session.json` stores).
//! * `lineup_lock_for` is not here (it lives in `fm_engine::recommend::lineup_lock`); this module
//!   exposes the parsed lock through `LeagueContext.lineup_lock` / `ProviderExtras.lineup_lock`.
//! * Python passes no `today` to `parse_roster` (so `date.today()` is used for a roster row's
//!   waiver date); the port passes the provider's `today`, which is the same day in practice.
//! * `DraftPickRef.round` is required in fm-core: a pick without a round gets 0.
//! * The `LeagueProvider` trait methods take `&self`; the ones Python lets load the league first
//!   (`activity`, `lineup_snapshot`, `raw_settings`, `describe_settings`, `fit_points`) have
//!   `&mut self` inherent versions that do; the trait versions need a prior `load()`.
//! * Warnings raised after `load()` through `&self` calls are kept in the provider and copied into
//!   `extras().warnings` by the next `&mut self` call ([`FantraxProvider::sync_extras`]).
//! * `_lstsq` uses a Householder QR solve (numpy uses SVD): both give the least-squares solution
//!   of the full-rank kept columns; results are rounded to 2 dp.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering as AtomicOrdering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone};
use regex::Regex;
use serde_json::Value;
use thiserror::Error;

use fm_core::config::{default_espn_year, format_points, secret_value, split_points, Settings};
use fm_core::models::{
    is_canonical_stat, normalize_name, ActivityAction, ActivityItem, DraftPickRef, FantasyTeam, LeagueContext,
    LineupDay, LineupLock, Player, RosterSlot, ScoringConfig, ScoringKind, Split, StatLine, Status, GOALIE_STATS,
    SKATER_STATS,
};
use fm_core::provider::LeagueProvider;
use fm_core::snapshot::ProviderExtras;
pub use fm_core::snapshot::{LeagueRules, PendingTradeInfo, TradeBlockInfo, TransactionInfo};
use fm_core::{Map, ProviderError, PyDateTime};
use fm_engine::scoring::PointsScoring;
use fm_http::{fxpa_key, CacheMiss, FxpaFileCache, Tape};
use fm_pycompat::fmt::{fixed, general};
use fm_pycompat::num::{py_round_i64, py_sum, round_to};
use fm_pycompat::text::py_strip;

use crate::fantrax_auth::FantraxAuth;

pub const FXPA_URL: &str = "https://www.fantrax.com/fxpa/req";
pub const FANTRAX_TTL: f64 = 15.0 * 60.0;
/// Requests that carry lineups (getTeamRosterInfo): short, so a lineup set on fantrax.com shows up
/// on the next run.
pub const LINEUP_TTL: f64 = 2.0 * 60.0;
pub const LINEUP_METHODS: &[&str] = &["getTeamRosterInfo"];
pub const CACHE_DIR_NAME: &str = "fantrax_cache";
pub const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
pub const FA_PAGE_SIZE: i64 = 200;
pub const FA_MAX_PAGES: i64 = 3;
pub const TAKEN_MAX_PAGES: i64 = 2;
pub const FA_GOALIE_PAGES: i64 = 1;
/// FP/G error above which `fm settings` suggests --fit-points.
pub const FIT_HINT_MAE: f64 = 0.1;
/// Fantrax's own total fantasy points, stored in the season StatLine.
pub const FPTS_KEY: &str = "FPTS";

pub const NON_STARTING: &[&str] = &["BN", "IR", "MIN", "TAXI"];

/// Roster row statusId -> slot for non-active rows (go-fantrax mapStatusID).
pub fn status_slot(status_id: &str) -> Option<&'static str> {
    match status_id {
        "2" => Some("BN"),
        "3" => Some("IR"),
        "9" => Some("MIN"),
        _ => None,
    }
}

/// Fantrax lineup-slot short names -> canonical slot names.
fn slot_alias(s: &str) -> Option<&'static str> {
    Some(match s {
        "C" => "C",
        "LW" => "LW",
        "RW" => "RW",
        "W" => "W",
        "F" | "FWD" => "F",
        "D" => "D",
        "G" => "G",
        "UTIL" | "UT" | "U" | "SKT" | "SK" | "SKATER" | "FLEX" => "UTIL",
        "RES" | "RESERVE" | "BN" | "BENCH" => "BN",
        "IR" | "INJ RES" | "INJ" | "IR+" => "IR",
        "MIN" | "MINORS" | "MINOR" => "MIN",
        "TAXI" => "TAXI",
        _ => return None,
    })
}

// Player icon typeIds (fantraxapi Player / go-fantrax PlayerIcon).
pub const ICON_DTD: &str = "1";
pub const ICON_IR: &str = "2";
pub const ICON_OUT: &str = "30";
pub const ICON_SUSP: &str = "6";

/// Table header shortName (upper-cased) -> canonical stat key.
pub fn stat_column(short: &str) -> Option<&'static str> {
    Some(match short {
        "GP" => "GP",
        "G" => "G",
        "A" => "A",
        "PTS" | "P" => "PTS",
        "+/-" | "PM" => "PM",
        "PIM" => "PIM",
        "PPG" => "PPG",
        "PPA" => "PPA",
        "PPP" => "PPP",
        "SHG" => "SHG",
        "SHA" => "SHA",
        "SHP" => "SHP",
        "GWG" => "GWG",
        "FOW" => "FOW",
        "FOL" => "FOL",
        "SOG" | "SHOTS" => "SOG",
        "HIT" | "HITS" => "HIT",
        "BLK" | "BKS" | "BLKS" => "BLK",
        "HAT" => "HAT",
        "GS" => "GS",
        "W" => "W",
        "L" => "L",
        "OTL" | "OTL/SOL" => "OTL",
        "SA" => "SA",
        "GA" => "GA",
        "SV" | "SVS" => "SV",
        "SO" | "SHO" => "SO",
        "GAA" => "GAA",
        "SV%" | "SVPCT" => "SVPCT",
        "ENG" => "ENG",
        "FT" | "FIGHTS" => "FT",
        _ => return None,
    })
}

pub const FPTS_IDS: &[&str] = &["SCORE", "fpts", "FPTS", "FPts"];
pub const FPG_IDS: &[&str] = &["FPTS_PER_GAME", "fptsPerGame", "FP/G"];
pub const AGE_IDS: &[&str] = &["age", "AGE", "Age"];
pub const OWNED_IDS: &[&str] = &["OVERVIEW_PERCENT_OWNED_2", "Ros", "%Ros", "Own", "%Own"];
/// Weekly change in % rostered (header shortName "+/-" is ambiguous with plus-minus).
pub const OWNED_CHANGE_IDS: &[&str] = &["OVERVIEW_PLUS_MINUS_PERCENT_OWNED_2"];
pub const STATUS_IDS: &[&str] = &["status", "STATUS", "Sta"];

pub const SPLIT_TIMEFRAMES: &[&str] = &["season", "projected"];
pub const SKATER_GROUP: &str = "HOCKEY_SKATING";
pub const GOALIE_GROUP_DEFAULT: &str = "POS_201";

static RULE_KEYWORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)keeper|minor|prospect|salary|contract|draft|dynasty|taxi|rookie|roster|reserve|injur|active|max|min(imum)?|limit|claim|waiver|trade|pick|budget|year",
    )
    .expect("regex")
});
pub const MAX_RULE_LINES: usize = 60;

static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("regex"));

/// `tuple(dict.fromkeys(SKATER_STATS + GOALIE_STATS))`
fn canonical_stats_order() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for s in SKATER_STATS.iter().chain(GOALIE_STATS.iter()) {
        if !out.contains(s) {
            out.push(s);
        }
    }
    out
}

// ------------------------------------------------------------------ errors

/// fantraxapi exceptions (`FantraxException`, `NotLoggedIn`, `NotMemberOfLeague`).
#[derive(Debug, Error)]
pub enum FantraxError {
    #[error("Not logged in")]
    NotLoggedIn,
    #[error("Not member of league")]
    NotMemberOfLeague,
    #[error("{0}")]
    Other(String),
}

/// `requests.RequestException` (network failure); `kind` is the Python class name it stands for.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct RequestError {
    pub kind: String,
    pub message: String,
}

impl RequestError {
    pub fn from_reqwest(e: &reqwest::Error) -> Self {
        let kind = if e.is_timeout() {
            "ReadTimeout"
        } else if e.is_connect() {
            "ConnectionError"
        } else {
            "RequestException"
        };
        // never include the URL: it carries the league id only, but keep messages short
        let mut message = e.to_string();
        if let Some(url) = e.url() {
            message = message.replace(&format!(" ({url})"), "").replace(url.as_str(), "<url>");
        }
        RequestError { kind: kind.into(), message }
    }
}

/// Python `type(e).__name__` for the error types this module raises.
pub fn error_type_name(e: &anyhow::Error) -> String {
    if let Some(f) = e.downcast_ref::<FantraxError>() {
        return match f {
            FantraxError::NotLoggedIn => "NotLoggedIn",
            FantraxError::NotMemberOfLeague => "NotMemberOfLeague",
            FantraxError::Other(_) => "FantraxException",
        }
        .into();
    }
    if e.downcast_ref::<CacheMiss>().is_some() {
        return "CacheMiss".into();
    }
    if let Some(r) = e.downcast_ref::<RequestError>() {
        return r.kind.clone();
    }
    if let Some(p) = e.downcast_ref::<ProviderError>() {
        return match p {
            ProviderError::LoginFailed(_) => "LoginFailed",
            ProviderError::Unavailable(_) => "ProviderUnavailable",
            ProviderError::Message(_) => "ProviderError",
        }
        .into();
    }
    if let Some(c) = e.downcast_ref::<fm_core::CoreError>() {
        return match c {
            fm_core::CoreError::Lookup(_) => "LookupError",
            fm_core::CoreError::Value(_) => "ValueError",
        }
        .into();
    }
    "Exception".into()
}

fn provider_err(msg: impl Into<String>) -> anyhow::Error {
    ProviderError::Message(msg.into()).into()
}

// ------------------------------------------------------------------ JSON helpers (Python idioms)

/// `d.get(k)` on a JSON object (None for non-objects).
pub(crate) fn jget<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    v.as_object()?.get(k)
}

/// Python truthiness of a JSON value.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `d.get(k)` filtered by truthiness (`d.get(k) or ...`).
pub(crate) fn jget_t<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    jget(v, k).filter(|x| truthy(x))
}

static EMPTY_OBJ: LazyLock<Value> = LazyLock::new(|| Value::Object(Default::default()));
static EMPTY_ARR: LazyLock<Value> = LazyLock::new(|| Value::Array(Vec::new()));

/// `(d.get(k) or {})`
fn jobj<'a>(v: &'a Value, k: &str) -> &'a Value {
    jget_t(v, k).unwrap_or(&EMPTY_OBJ)
}

/// `(d.get(k) or [])` as a slice (non-lists -> empty).
fn jlist<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    jget_t(v, k).unwrap_or(&EMPTY_ARR).as_array().map(|a| a.as_slice()).unwrap_or(&[])
}

/// Python `repr` of a JSON value (dicts / lists use Python's literal syntax).
pub(crate) fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => {
            let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
            let mut out = String::new();
            out.push(q);
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c == q => {
                        out.push('\\');
                        out.push(c);
                    }
                    c => out.push(c),
                }
            }
            out.push(q);
            out
        }
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter().map(|(k, v)| format!("{}: {}", py_repr(&Value::String(k.clone())), py_repr(v))).collect::<Vec<_>>().join(", ")
        ),
        other => py_str(other),
    }
}

/// Python `str(v)` of a JSON value.
pub(crate) fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                fm_pycompat::fmt::repr_f64(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// `str(d.get(k, default))`
fn get_str_or(v: &Value, k: &str, default: &str) -> String {
    jget(v, k).map(py_str).unwrap_or_else(|| default.to_string())
}

/// `x if x is None else str(x)` for an optional JSON value.
fn opt_str(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(x) => Some(py_str(x)),
    }
}

/// Python `float(s)` on a string (None where Python raises ValueError).
pub(crate) fn py_float_str(s: &str) -> Option<f64> {
    let t = py_strip(s);
    if t.is_empty() {
        return None;
    }
    let cleaned: String;
    let t = if t.contains('_') {
        // digits separated by single underscores only
        let b: Vec<char> = t.chars().collect();
        for (i, c) in b.iter().enumerate() {
            if *c == '_' {
                let ok = i > 0 && i + 1 < b.len() && b[i - 1].is_ascii_digit() && b[i + 1].is_ascii_digit();
                if !ok {
                    return None;
                }
            }
        }
        cleaned = t.replace('_', "");
        cleaned.as_str()
    } else {
        t
    };
    let lower = t.to_ascii_lowercase();
    let body = lower.trim_start_matches(['+', '-']);
    if matches!(body, "inf" | "infinity" | "nan") {
        let neg = lower.starts_with('-');
        return Some(match body {
            "nan" => f64::NAN,
            _ if neg => f64::NEG_INFINITY,
            _ => f64::INFINITY,
        });
    }
    if !t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Python `float(v)` of a JSON value.
pub(crate) fn py_float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => py_float_str(s),
        _ => None,
    }
}

/// Python `int(v)` of a JSON value (floats truncate, strings must be integers).
pub(crate) fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|x| x.is_finite()).map(|x| x.trunc() as i64)),
        Value::Bool(b) => Some(*b as i64),
        Value::String(s) => {
            let t = py_strip(s).replace('_', "");
            t.parse::<i64>().ok()
        }
        _ => None,
    }
}

fn py_upper(s: &str) -> String {
    fm_pycompat::text::py_upper(s)
}

fn py_lower(s: &str) -> String {
    fm_pycompat::text::py_lower(s)
}

/// Python `s[:n]` (code points).
fn char_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn split_of(name: &str) -> Option<Split> {
    Some(match name {
        "season" => Split::Season,
        "prior" => Split::Prior,
        "prior2" => Split::Prior2,
        "prior3" => Split::Prior3,
        "last7" => Split::Last7,
        "last15" => Split::Last15,
        "last30" => Split::Last30,
        "projected" => Split::Projected,
        _ => return None,
    })
}

fn local_today() -> NaiveDate {
    Local::now().date_naive()
}

pub(crate) fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ------------------------------------------------------------------ cookies

/// `'Cookie: a=b; c=d'` (prefix optional) -> {'a': 'b', 'c': 'd'}.
pub fn parse_cookie_header(raw: &str) -> Map<String, String> {
    let mut s = py_strip(raw);
    if py_lower(s).starts_with("cookie:") {
        s = &s["cookie:".len()..];
    }
    let mut out = Map::new();
    for part in s.replace('\n', ";").split(';') {
        let part = py_strip(part);
        if part.is_empty() || !part.contains('=') {
            continue;
        }
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        let (k, mut v) = (py_strip(k), py_strip(v));
        let chars: Vec<char> = v.chars().collect();
        if chars.len() >= 2 && chars[0] == '"' && chars[chars.len() - 1] == '"' {
            v = &v[1..v.len() - 1];
        }
        if !k.is_empty() {
            out.insert(k.to_string(), v.to_string());
        }
    }
    out
}

/// Netscape cookies.txt (tab separated, 7 fields) -> {name: value} for `domain`.
pub fn parse_netscape_cookies(text: &str, domain: &str) -> Map<String, String> {
    let mut out = Map::new();
    for line in py_splitlines(text) {
        let mut line = py_strip(line);
        if let Some(rest) = line.strip_prefix("#HttpOnly_") {
            line = rest;
        } else if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 7 {
            continue;
        }
        if !py_lower(fields[0]).contains(domain) {
            continue;
        }
        out.insert(fields[5].to_string(), fields[6].to_string());
    }
    out
}

/// `str.splitlines()` (the common line boundaries).
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < bytes.len() {
        let (pos, c) = bytes[i];
        if matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            out.push(&text[start..pos]);
            let mut next = pos + c.len_utf8();
            if c == '\r' && i + 1 < bytes.len() && bytes[i + 1].1 == '\n' {
                i += 1;
                next += 1;
            }
            start = next;
        }
        i += 1;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn looks_netscape(text: &str) -> bool {
    if text.contains("Netscape HTTP Cookie File") {
        return true;
    }
    py_splitlines(text).iter().any(|ln| !ln.is_empty() && !ln.starts_with("# ") && ln.split('\t').count() >= 7)
}

/// Cookie file contents: Netscape cookies.txt, a JSON cookie export, or a header string.
pub fn parse_cookie_text(text: &str) -> Map<String, String> {
    let t = py_strip(text).trim_start_matches('\u{feff}');
    if t.is_empty() {
        return Map::new();
    }
    if t.starts_with('[') || t.starts_with('{') {
        let mut data: Option<Value> = serde_json::from_str(t).ok();
        if let Some(Value::Object(o)) = &data {
            if let Some(c) = o.get("cookies") {
                data = Some(c.clone());
            }
        }
        match &data {
            Some(Value::Array(list)) => {
                let mut out = Map::new();
                for c in list {
                    let Some(o) = c.as_object() else { continue };
                    let Some(name) = o.get("name") else { continue };
                    let domain = o.get("domain").map(py_str).unwrap_or_else(|| "fantrax.com".into());
                    if !py_lower(&domain).contains("fantrax") {
                        continue;
                    }
                    out.insert(py_str(name), o.get("value").map(py_str).unwrap_or_default());
                }
                return out;
            }
            Some(Value::Object(o)) => {
                return o.iter().map(|(k, v)| (k.clone(), py_str(v))).collect();
            }
            _ => {}
        }
    }
    if looks_netscape(t) {
        return parse_netscape_cookies(t, "fantrax.com");
    }
    parse_cookie_header(t)
}

/// Cookies from FANTRAX_COOKIE, else FANTRAX_COOKIE_FILE. Empty map if neither is set.
pub fn load_cookies(settings: &Settings) -> anyhow::Result<Map<String, String>> {
    let raw = secret_value(settings.fantrax_cookie.as_ref());
    if !raw.is_empty() {
        return Ok(parse_cookie_header(&raw));
    }
    if let Some(path) = &settings.fantrax_cookie_file {
        let p = expand_user(path);
        if !p.is_file() {
            return Err(provider_err(format!("FANTRAX_COOKIE_FILE points to {}, which does not exist.", p.display())));
        }
        let bytes = std::fs::read(&p)?;
        let cookies = parse_cookie_text(&String::from_utf8_lossy(&bytes));
        if cookies.is_empty() {
            return Err(provider_err(format!(
                "No fantrax.com cookies found in FANTRAX_COOKIE_FILE ({}). Use a Cookie header string or a Netscape cookies.txt export.",
                p.display()
            )));
        }
        return Ok(cookies);
    }
    Ok(Map::new())
}

fn expand_user(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s == "~" || s.starts_with("~/") || s.starts_with("~\\") {
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            let mut out = PathBuf::from(home);
            if s.len() > 2 {
                out.push(&s[2..]);
            }
            return out;
        }
    }
    p.to_path_buf()
}

// ------------------------------------------------------------------ session (requests.Session)

/// One cookie as `http.cookiejar.Cookie` keeps it (what `fantrax_session.json` stores).
#[derive(Clone, Debug, PartialEq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// epoch seconds; None = session cookie
    pub expires: Option<i64>,
}

/// A response: status and the parsed JSON body (`Err` = not JSON, like `resp.json()` raising).
#[derive(Clone, Debug)]
pub struct FxResponse {
    pub status: u16,
    pub body: Result<Value, String>,
}

/// The part of `requests.Session` the Fantrax code uses.
pub trait FxSession: Send {
    /// `session.post(url, params=query, json=payload, timeout=30)`.
    fn post(&mut self, url: &str, query: &[(String, String)], payload: &Value) -> Result<FxResponse, RequestError>;
    /// `session.get(url, timeout=30)`.
    fn get(&mut self, url: &str) -> Result<FxResponse, RequestError>;
    /// `session.headers.update({name: value})`.
    fn set_header(&mut self, name: &str, value: &str);
    fn header(&self, name: &str) -> Option<String>;
    /// `session.cookies` (every cookie in the jar).
    fn cookies(&self) -> Vec<Cookie>;
    /// `session.cookies.set(name, value, domain=, path=)`.
    fn set_cookie(&mut self, cookie: Cookie);
    /// `session.cookies.get(name)`.
    fn cookie(&self, name: &str) -> Option<String> {
        self.cookies().into_iter().find(|c| c.name == name).map(|c| c.value)
    }
}

/// `requests.Session` over reqwest (blocking) with a manual cookie jar.
pub struct HttpSession {
    client: Option<reqwest::blocking::Client>,
    headers: Vec<(String, String)>,
    jar: Vec<Cookie>,
}

impl HttpSession {
    pub fn new() -> Self {
        HttpSession { client: None, headers: Vec::new(), jar: Vec::new() }
    }

    fn client(&mut self) -> Result<&reqwest::blocking::Client, RequestError> {
        if self.client.is_none() {
            let c = reqwest::blocking::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| RequestError::from_reqwest(&e))?;
            self.client = Some(c);
        }
        Ok(self.client.as_ref().expect("client"))
    }

    fn cookie_header(&self, host: &str) -> Option<String> {
        let now = now_secs() as i64;
        let parts: Vec<String> = self
            .jar
            .iter()
            .filter(|c| c.expires.is_none_or(|e| e > now))
            .filter(|c| domain_matches(host, &c.domain))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
    }

    fn store_set_cookies(&mut self, host: &str, headers: &reqwest::header::HeaderMap) {
        for hv in headers.get_all(reqwest::header::SET_COOKIE) {
            let Ok(s) = hv.to_str() else { continue };
            if let Some(c) = parse_set_cookie(s, host) {
                let expired = c.expires.is_some_and(|e| e <= now_secs() as i64);
                self.jar.retain(|x| !(x.name == c.name && x.domain == c.domain && x.path == c.path));
                if !expired {
                    self.jar.push(c);
                }
            }
        }
    }

    fn send(
        &mut self,
        method: reqwest::Method,
        url: &str,
        query: &[(String, String)],
        payload: Option<&Value>,
    ) -> Result<FxResponse, RequestError> {
        let mut url = url.to_string();
        let mut method = method;
        let mut payload = payload.cloned();
        let mut query: Vec<(String, String)> = query.to_vec();
        for _ in 0..10 {
            let parsed = reqwest::Url::parse(&url)
                .map_err(|e| RequestError { kind: "InvalidURL".into(), message: e.to_string() })?;
            let host = parsed.host_str().unwrap_or("").to_lowercase();
            let cookie = self.cookie_header(&host);
            let headers = self.headers.clone();
            let client = self.client()?.clone();
            let mut req = client.request(method.clone(), parsed.clone());
            if !query.is_empty() {
                req = req.query(&query);
            }
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(c) = cookie {
                req = req.header(reqwest::header::COOKIE, c);
            }
            if let Some(p) = &payload {
                req = req.header(reqwest::header::CONTENT_TYPE, "application/json").body(
                    serde_json::to_vec(p).map_err(|e| RequestError { kind: "InvalidJSONError".into(), message: e.to_string() })?,
                );
            }
            let resp = req.send().map_err(|e| RequestError::from_reqwest(&e))?;
            self.store_set_cookies(&host, resp.headers());
            let status = resp.status();
            if status.is_redirection() {
                if let Some(loc) = resp.headers().get(reqwest::header::LOCATION).and_then(|l| l.to_str().ok()) {
                    let next = parsed.join(loc).map_err(|e| RequestError { kind: "InvalidURL".into(), message: e.to_string() })?;
                    url = next.to_string();
                    query.clear();
                    if status != reqwest::StatusCode::TEMPORARY_REDIRECT && status != reqwest::StatusCode::PERMANENT_REDIRECT {
                        method = reqwest::Method::GET;
                        payload = None;
                    }
                    continue;
                }
            }
            let code = status.as_u16();
            let text = resp.text().map_err(|e| RequestError::from_reqwest(&e))?;
            let body = serde_json::from_str::<Value>(&text).map_err(|e| e.to_string());
            return Ok(FxResponse { status: code, body });
        }
        Err(RequestError { kind: "TooManyRedirects".into(), message: "Exceeded 10 redirects.".into() })
    }
}

impl Default for HttpSession {
    fn default() -> Self {
        Self::new()
    }
}

fn domain_matches(host: &str, domain: &str) -> bool {
    let d = domain.trim_start_matches('.').to_lowercase();
    !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
}

/// One `Set-Cookie` header -> cookie (cookiejar defaults: host-only domain = request host, a
/// Domain attribute gets a leading dot, path "/").
pub fn parse_set_cookie(header: &str, host: &str) -> Option<Cookie> {
    let mut parts = header.split(';');
    let first = parts.next()?;
    let (name, value) = first.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut c = Cookie {
        name: name.to_string(),
        value: value.trim().trim_matches('"').to_string(),
        domain: host.to_string(),
        path: "/".into(),
        expires: None,
    };
    let mut max_age: Option<i64> = None;
    for attr in parts {
        let (k, v) = attr.split_once('=').unwrap_or((attr, ""));
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "domain" if !v.is_empty() => {
                let d = v.to_lowercase();
                c.domain = if d.starts_with('.') { d } else { format!(".{d}") };
            }
            "path" if !v.is_empty() => c.path = v.to_string(),
            "max-age" => max_age = v.parse::<i64>().ok(),
            "expires" => c.expires = parse_http_date(v),
            _ => {}
        }
    }
    if let Some(m) = max_age {
        c.expires = Some(now_secs() as i64 + m);
    }
    Some(c)
}

fn parse_http_date(s: &str) -> Option<i64> {
    if let Ok(d) = chrono::DateTime::parse_from_rfc2822(s) {
        return Some(d.timestamp());
    }
    let s = s.trim_end_matches(" GMT").trim_end_matches(" UTC");
    for f in ["%a, %d-%b-%Y %H:%M:%S", "%a, %d %b %Y %H:%M:%S", "%A, %d-%b-%y %H:%M:%S", "%a %b %e %H:%M:%S %Y"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, f) {
            return Some(d.and_utc().timestamp());
        }
    }
    None
}

impl FxSession for HttpSession {
    fn post(&mut self, url: &str, query: &[(String, String)], payload: &Value) -> Result<FxResponse, RequestError> {
        self.send(reqwest::Method::POST, url, query, Some(payload))
    }
    fn get(&mut self, url: &str) -> Result<FxResponse, RequestError> {
        self.send(reqwest::Method::GET, url, &[], None)
    }
    fn set_header(&mut self, name: &str, value: &str) {
        if let Some(h) = self.headers.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            h.1 = value.to_string();
        } else {
            self.headers.push((name.to_string(), value.to_string()));
        }
    }
    fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }
    fn cookies(&self) -> Vec<Cookie> {
        self.jar.clone()
    }
    fn set_cookie(&mut self, cookie: Cookie) {
        self.jar.retain(|x| !(x.name == cookie.name && x.domain == cookie.domain && x.path == cookie.path));
        self.jar.push(cookie);
    }
}

/// `build_session(cookies)`: browser-like headers + the cookies on `.fantrax.com`.
pub fn build_session(cookies: &Map<String, String>) -> HttpSession {
    let mut s = HttpSession::new();
    for (k, v) in [
        ("User-Agent", BROWSER_UA),
        ("Accept", "application/json, text/plain, */*"),
        ("Origin", "https://www.fantrax.com"),
        ("Referer", "https://www.fantrax.com/"),
    ] {
        s.set_header(k, v);
    }
    for (k, v) in cookies {
        s.set_cookie(Cookie { name: k.clone(), value: v.clone(), domain: ".fantrax.com".into(), path: "/".into(), expires: None });
    }
    s
}

// ------------------------------------------------------------------ raw fxpa client

/// The relogin hook: a new session to retry with, or None to give up.
pub type Relogin<'a> = dyn FnMut() -> anyhow::Result<Option<Box<dyn FxSession>>> + 'a;

/// One (method, params) message.
pub type FxCall = (String, Map<String, Value>);

/// fantraxapi `Method(name, **data).msg_block(league_id)`: values become `str(v)`, None dropped.
pub fn msg_block(league_id: &str, method: &str, data: &Map<String, Value>) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("leagueId".into(), Value::String(league_id.to_string()));
    for (k, v) in data {
        if v.is_null() {
            continue;
        }
        out.insert(k.clone(), Value::String(py_str(v)));
    }
    let mut m = serde_json::Map::new();
    m.insert("method".into(), Value::String(method.to_string()));
    m.insert("data".into(), Value::Object(out));
    Value::Object(m)
}

/// Build an [`FxCall`] from string pairs.
pub fn fx_call(method: &str, data: &[(&str, &str)]) -> FxCall {
    (method.to_string(), data.iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect())
}

fn raise_page_error(page_error: &Value) -> anyhow::Error {
    let code = jget(page_error, "code");
    match code.and_then(|c| c.as_str()) {
        Some("WARNING_NOT_LOGGED_IN") => return FantraxError::NotLoggedIn.into(),
        Some("NOT_MEMBER_OF_LEAGUE") => return FantraxError::NotMemberOfLeague.into(),
        _ => {}
    }
    let msg = jget_t(page_error, "title").or_else(|| code.filter(|c| truthy(c))).map(py_str).unwrap_or_else(|| py_str(page_error));
    FantraxError::Other(msg).into()
}

/// POSTs batched fxpa messages; caches successful responses as JSON files (or answers from a
/// parity tape).
pub struct FxpaClient {
    pub league_id: String,
    session: Option<Box<dyn FxSession>>,
    cache: Option<FxpaFileCache>,
    pub offline: bool,
    pub ttl: f64,
    tape: Option<Tape>,
    pub requests_made: i64,
}

impl FxpaClient {
    pub fn new(
        league_id: &str,
        session: Option<Box<dyn FxSession>>,
        data_dir: Option<&Path>,
        offline: bool,
        tape: Option<Tape>,
    ) -> Self {
        FxpaClient {
            league_id: league_id.to_string(),
            session,
            cache: data_dir.map(|d| FxpaFileCache::new(d, offline)),
            offline,
            ttl: FANTRAX_TTL,
            tape,
            requests_made: 0,
        }
    }

    pub fn key(&self, msgs: &[Value]) -> String {
        fxpa_key(&self.league_id, msgs)
    }

    pub fn set_session(&mut self, session: Box<dyn FxSession>) {
        self.session = Some(session);
    }

    fn cached_post(&mut self, msgs: &[Value], fresh: bool, relogin: Option<&mut Relogin<'_>>) -> anyhow::Result<Value> {
        let key = self.key(msgs);
        let methods = || {
            let names: Vec<Value> = msgs.iter().map(|m| jget(m, "method").cloned().unwrap_or(Value::Null)).collect();
            py_repr(&Value::Array(names))
        };
        if let Some(tape) = &self.tape {
            return match tape.fxpa(&key) {
                Some(rec) => Ok(rec.body.clone()),
                None => Err(CacheMiss(format!("offline and Fantrax request not cached (tape): {}", methods())).into()),
            };
        }
        let lineup = msgs.iter().any(|m| jget(m, "method").and_then(|x| x.as_str()).is_some_and(|x| LINEUP_METHODS.contains(&x)));
        let ttl = if lineup { self.ttl.min(LINEUP_TTL) } else { self.ttl };
        if let Some(cache) = &self.cache {
            if !(fresh && !self.offline) {
                if let Some(body) = cache.get(&key, ttl) {
                    return Ok(body);
                }
            }
        }
        if self.offline {
            return Err(CacheMiss(format!("offline and Fantrax request not cached: {}", methods())).into());
        }
        let body = self.post(msgs, relogin)?;
        if let Some(cache) = &self.cache {
            cache.put(&key, &body)?;
        }
        Ok(body)
    }

    fn post(&mut self, msgs: &[Value], relogin: Option<&mut Relogin<'_>>) -> anyhow::Result<Value> {
        match self.post_once(msgs) {
            Err(e) if matches!(e.downcast_ref::<FantraxError>(), Some(FantraxError::NotLoggedIn)) => {
                let Some(relogin) = relogin else { return Err(e) };
                match relogin()? {
                    None => Err(e),
                    Some(session) => {
                        self.session = Some(session);
                        self.post_once(msgs)
                    }
                }
            }
            other => other,
        }
    }

    fn post_once(&mut self, msgs: &[Value]) -> anyhow::Result<Value> {
        let mut payload = serde_json::Map::new();
        payload.insert("msgs".into(), Value::Array(msgs.to_vec()));
        payload.insert("uiv".into(), Value::from(3));
        payload.insert("refUrl".into(), Value::String(format!("https://www.fantrax.com/fantasy/league/{}", self.league_id)));
        payload.insert("dt".into(), Value::from(0));
        payload.insert("at".into(), Value::from(0));
        payload.insert("av".into(), Value::String("0.0".into()));
        payload.insert("tz".into(), Value::String(local_tz_name(None)));
        self.requests_made += 1;
        let league_id = self.league_id.clone();
        let Some(session) = self.session.as_mut() else {
            return Err(FantraxError::Other("no Fantrax session".into()).into());
        };
        let resp = session.post(FXPA_URL, &[("leagueId".to_string(), league_id)], &Value::Object(payload))?;
        let status = resp.status;
        let body = match resp.body {
            Ok(b) => b,
            Err(_) => return Err(FantraxError::Other(format!("Fantrax returned a non-JSON response (HTTP {status})")).into()),
        };
        if let Some(pe) = jget(&body, "pageError").filter(|p| p.is_object()) {
            return Err(raise_page_error(pe));
        }
        if status >= 400 {
            return Err(FantraxError::Other(format!("Fantrax HTTP {status}")).into());
        }
        if !jget(&body, "responses").is_some_and(|r| r.is_array()) {
            return Err(FantraxError::Other("Fantrax response has no 'responses' list".into()).into());
        }
        Ok(body)
    }

    /// `call(("getStandings", {}), ...)` -> one `data` object per message. `fresh` skips the cache.
    pub fn call(&mut self, calls: &[FxCall], fresh: bool, relogin: Option<&mut Relogin<'_>>) -> anyhow::Result<Vec<Value>> {
        let msgs: Vec<Value> = calls.iter().map(|(m, d)| msg_block(&self.league_id, m, d)).collect();
        let body = self.cached_post(&msgs, fresh, relogin)?;
        let out: Vec<Value> = jget(&body, "responses")
            .and_then(|r| r.as_array())
            .map(|rs| {
                rs.iter()
                    .map(|r| if r.is_object() { jget(r, "data").cloned().unwrap_or_else(|| EMPTY_OBJ.clone()) } else { EMPTY_OBJ.clone() })
                    .collect()
            })
            .unwrap_or_default();
        if out.len() != msgs.len() {
            return Err(FantraxError::Other(format!("expected {} responses, got {}", msgs.len(), out.len())).into());
        }
        Ok(out)
    }
}

/// Time zone sent with every Fantrax request (`tz`): FM_TZ, else TZ, else the system zone
/// (/etc/timezone, the /etc/localtime link), else the current UTC offset as `Etc/GMT+N`.
pub fn local_tz_name(offset_secs: Option<i32>) -> String {
    for var in ["FM_TZ", "TZ"] {
        let name = std::env::var(var).unwrap_or_default();
        let name = py_strip(&name);
        if name.contains('/') || name == "UTC" {
            return name.to_string();
        }
    }
    if let Ok(name) = std::fs::read_to_string("/etc/timezone") {
        let name = py_strip(&name);
        if name.contains('/') {
            return name.to_string();
        }
    }
    if let Ok(link) = std::fs::canonicalize("/etc/localtime") {
        let link = link.to_string_lossy().to_string();
        if let Some((_, rest)) = link.split_once("zoneinfo/") {
            return rest.to_string();
        }
    }
    let offset = offset_secs.unwrap_or_else(|| Local::now().offset().local_minus_utc());
    let minutes = fm_pycompat::num::floordiv_i64(offset as i64, 60);
    if minutes == 0 || fm_pycompat::num::pymod_i64(minutes, 60) != 0 {
        return "UTC".into();
    }
    let h = fm_pycompat::num::floordiv_i64(-minutes, 60);
    format!("Etc/GMT{}{}", if h >= 0 { "+" } else { "-" }, h.abs())
}

// ------------------------------------------------------------------ pure parsing helpers

/// `strip_html(s)` for a string.
pub fn strip_html(s: &str) -> String {
    py_strip(&TAG_RE.replace_all(s, "")).to_string()
}

/// `strip_html(v)` for any JSON value (`str(v or "")`).
pub fn strip_html_v(v: Option<&Value>) -> String {
    match v {
        Some(x) if truthy(x) => strip_html(&py_str(x)),
        _ => String::new(),
    }
}

/// `_num(content)`
pub fn num(content: Option<&Value>) -> Option<f64> {
    let content = content?;
    if content.is_null() {
        return None;
    }
    let s0 = strip_html_v(Some(content)).replace([',', '%'], "");
    let mut s = py_strip(&s0);
    if matches!(s, "" | "-" | "--" | "N/A") {
        return None;
    }
    if let Some(r) = s.strip_prefix('+') {
        s = r;
    }
    let owned;
    let s = if s.starts_with('.') {
        owned = format!("0{s}");
        owned.as_str()
    } else {
        s
    };
    py_float_str(s)
}

/// `_signed_num(content)`: keeps a leading Unicode minus / en dash.
pub fn signed_num(content: Option<&Value>) -> Option<f64> {
    match content {
        Some(Value::String(s)) => num(Some(&Value::String(s.replace(['\u{2212}', '\u{2013}'], "-")))),
        other => num(other),
    }
}

pub fn canonical_slot(short_name: &str) -> String {
    let s = py_upper(&strip_html(short_name));
    match slot_alias(&s) {
        Some(a) => a.to_string(),
        None if s.is_empty() => "BN".into(),
        None => s,
    }
}

static POS_SPLIT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[,/\s]+").expect("regex"));

/// `'<b>C</b>,RW'` -> `['C', 'RW', 'F']` (F added for forwards, like the ESPN provider).
pub fn player_positions(pos_short_names: &str) -> Vec<String> {
    const ORDER: [&str; 5] = ["C", "LW", "RW", "D", "G"];
    let mut found: HashSet<&str> = HashSet::new();
    let up = py_upper(&strip_html(pos_short_names));
    for part in POS_SPLIT_RE.split(&up) {
        let part = match part {
            "L" => "LW",
            "R" => "RW",
            p => p,
        };
        if part == "W" {
            found.insert("LW");
            found.insert("RW");
        } else if part == "F" {
            found.insert("F");
        } else if let Some(p) = ORDER.iter().find(|o| **o == part) {
            found.insert(p);
        }
    }
    let mut pos: Vec<String> = ORDER.iter().filter(|p| found.contains(**p)).map(|p| p.to_string()).collect();
    if ["C", "LW", "RW", "F"].iter().any(|p| found.contains(p)) {
        let at = pos.iter().filter(|p| matches!(p.as_str(), "C" | "LW" | "RW")).count();
        pos.insert(at, "F".into());
    }
    pos
}

pub fn status_from_icons(icons: Option<&Value>) -> (Status, Option<String>) {
    let mut ids: Map<String, String> = Map::new();
    if let Some(Value::Array(list)) = icons {
        for icon in list {
            let tid = get_str_or(icon, "typeId", "");
            let tip = jget_t(icon, "tooltip").or_else(|| jget_t(icon, "toolTip"));
            let text = strip_html_v(tip);
            ids.entry(tid).or_insert(text);
        }
    }
    for (tid, status) in [(ICON_IR, Status::Ir), (ICON_OUT, Status::Out), (ICON_SUSP, Status::Suspended), (ICON_DTD, Status::Dtd)] {
        if let Some(note) = ids.get(tid) {
            return (status, if note.is_empty() { None } else { Some(note.clone()) });
        }
    }
    (Status::Healthy, None)
}

pub fn fantrax_team_abbrev(short: &str) -> Option<String> {
    let s = py_upper(&strip_html(short));
    if s.is_empty() || matches!(s.as_str(), "FA" | "(N/A)" | "N/A" | "NA" | "-" | "FREE AGENT") {
        return None;
    }
    fm_core::normalize::normalize_team(Some(&s))
}

/// Index header cells by every identifier Fantrax exposes (key, sortKey, sortType, shortName).
pub fn header_index(cells: &[Value]) -> HashMap<String, usize> {
    let mut idx = HashMap::new();
    for (i, c) in cells.iter().enumerate() {
        for f in ["key", "sortKey", "sortType", "shortName"] {
            if let Some(v) = jget_t(c, f) {
                idx.entry(py_str(v)).or_insert(i);
            }
        }
    }
    idx
}

fn find_idx(idx: &HashMap<String, usize>, ids: &[&str]) -> Option<usize> {
    ids.iter().find_map(|i| idx.get(*i).copied())
}

/// Column index -> canonical stat key, by header shortName (insertion ordered).
pub fn stat_columns(header_cells: &[Value]) -> Vec<(usize, &'static str)> {
    let mut out: Vec<(usize, &'static str)> = Vec::new();
    for (i, c) in header_cells.iter().enumerate() {
        let ident = py_upper(&format!("{} {}", get_str_or(c, "sortType", ""), get_str_or(c, "sortKey", "")));
        if ident.contains("PERCENT") || ident.contains("OWNED") {
            continue;
        }
        let short = py_upper(&strip_html_v(jget(c, "shortName")));
        if let Some(key) = stat_column(&short) {
            if is_canonical_stat(key) && !out.iter().any(|(_, k)| *k == key) {
                out.push((i, key));
            }
        }
    }
    out
}

/// One table row's numbers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowStats {
    pub stats: Map<String, f64>,
    pub fpts: Option<f64>,
    pub fpg: Option<f64>,
    pub age: Option<f64>,
    pub owned: Option<f64>,
    /// "+/-": change in % rostered from the previous week
    pub owned_change: Option<f64>,
    pub status_cell: Option<String>,
    pub status_team_id: Option<String>,
    /// weekday (Mon=0) of a "W (Wed)" status cell
    pub waiver_day: Option<i64>,
}

static WAIVER_STATUS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^W\s*\(\s*([A-Za-z]{3})").expect("regex"));
const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// The next date (today included) falling on weekday `day`.
pub fn waiver_clear_date(day: Option<i64>, today: NaiveDate) -> Option<NaiveDate> {
    let day = day?;
    let wd = today.weekday().num_days_from_monday() as i64;
    Some(today + Duration::days(fm_pycompat::num::pymod_i64(day - wd, 7)))
}

pub fn parse_row_cells(header_cells: &[Value], cells: &[Value]) -> RowStats {
    let idx = header_index(header_cells);
    let mut out = RowStats::default();
    let cell = |i: Option<usize>| -> Option<&Value> { i.and_then(|i| cells.get(i)) };
    for (i, key) in stat_columns(header_cells) {
        let v = cell(Some(i)).and_then(|c| num(jget(c, "content")));
        if let Some(v) = v {
            out.stats.insert(key.to_string(), v);
        }
    }
    if let Some(c) = cell(find_idx(&idx, FPTS_IDS)) {
        out.fpts = num(jget(c, "content"));
    }
    if let Some(c) = cell(find_idx(&idx, FPG_IDS)) {
        out.fpg = num(jget(c, "content"));
    }
    if let Some(c) = cell(find_idx(&idx, AGE_IDS)) {
        out.age = num(jget(c, "content"));
    }
    if let Some(c) = cell(find_idx(&idx, OWNED_IDS)) {
        out.owned = num(jget(c, "content"));
    }
    if let Some(c) = cell(find_idx(&idx, OWNED_CHANGE_IDS)) {
        out.owned_change = signed_num(jget(c, "content"));
    }
    if let Some(c) = cell(find_idx(&idx, STATUS_IDS)) {
        let s = strip_html_v(jget(c, "content"));
        out.status_cell = if s.is_empty() { None } else { Some(s) };
        out.status_team_id = jget_t(c, "teamId").map(py_str);
        if let Some(m) = WAIVER_STATUS_RE.captures(out.status_cell.as_deref().unwrap_or("")) {
            let d = m[1].to_lowercase();
            if let Some(pos) = WEEKDAYS.iter().position(|w| *w == d) {
                out.waiver_day = Some(pos as i64);
            }
        }
    }
    out
}

/// StatLine (split `split`) from a table row; GP from the GP column, else FPts / FP/G.
pub fn season_line(rs: &RowStats, split: &str) -> Option<StatLine> {
    let mut stats = rs.stats.clone();
    let mut gp: Option<f64> = stats.get("GP").copied();
    if gp.is_none_or(|g| g == 0.0) {
        if let (Some(fpts), Some(fpg)) = (rs.fpts, rs.fpg.filter(|x| *x != 0.0)) {
            let g = py_round_i64(fpts / fpg);
            gp = Some(g as f64);
            if g > 0 {
                stats.insert("GP".into(), g as f64);
            }
        }
    }
    let gp = gp.filter(|g| *g != 0.0)?;
    if gp <= 0.0 {
        return None;
    }
    if !stats.contains_key("PTS") && (stats.contains_key("G") || stats.contains_key("A")) && !stats.contains_key("W") {
        let pts = stats.get("G").copied().unwrap_or(0.0) + stats.get("A").copied().unwrap_or(0.0);
        stats.insert("PTS".into(), pts);
    }
    if let Some(f) = rs.fpts {
        stats.insert(FPTS_KEY.into(), f);
    }
    Some(StatLine { split: split_of(split)?, gp: py_round_i64(gp), stats })
}

/// A Player with every optional field at its default.
pub(crate) fn new_player(cid: String, name: String, ids: Map<String, String>, team: Option<String>, positions: Vec<String>) -> Player {
    Player {
        name_norm: normalize_name(&name),
        cid,
        name,
        ids,
        team,
        positions,
        primary_position: None,
        birth_date: None,
        status: Status::Unknown,
        status_note: None,
        waiver_until: None,
        lines: Map::new(),
        pct_owned: None,
        draft_overall: None,
        draft_round: None,
        draft_year: None,
        career_gp: None,
        pct_owned_change: None,
        pct_started: None,
        adp: None,
        adp_change: None,
        toi_per_game: None,
        pp_toi_per_game: None,
        pp_share: None,
        toi_trend: None,
        pp_share_trend: None,
        line: None,
        pp_unit: None,
        pk_unit: None,
        line_change: None,
        usual_line: None,
        usual_pp_unit: None,
        confirmed_start: None,
        start_source: None,
        ixg_per_game: None,
        goals_minus_ixg: None,
        onice_sh_pct: None,
        onice_xg_pct: None,
        xg_split: None,
    }
}

pub fn player_from_scorer(scorer: &Value, rs: Option<&RowStats>, split: Option<&str>, today: Option<NaiveDate>) -> Player {
    let sid = get_str_or(scorer, "scorerId", "None");
    let mut name = strip_html_v(jget(scorer, "name"));
    if name.is_empty() {
        name = sid.clone();
    }
    let (status, note) = status_from_icons(jget(scorer, "icons"));
    let mut lines = Map::new();
    if let (Some(rs), Some(split)) = (rs, split.filter(|s| !s.is_empty())) {
        if let Some(ln) = season_line(rs, split) {
            lines.insert(split.to_string(), ln);
        }
    }
    let team_v = jget_t(scorer, "teamShortName").or_else(|| jget_t(scorer, "teamName"));
    let team = fantrax_team_abbrev(&strip_html_v(team_v));
    let positions = player_positions(&strip_html_v(jget(scorer, "posShortNames")));
    let mut ids = Map::new();
    ids.insert("fantrax".to_string(), sid.clone());
    let mut p = new_player(format!("fantrax:{sid}"), name, ids, team, positions);
    p.status = status;
    p.status_note = note;
    p.waiver_until = rs.and_then(|r| waiver_clear_date(r.waiver_day, today.unwrap_or_else(local_today)));
    p.lines = lines;
    p.pct_owned = rs.and_then(|r| r.owned);
    p.pct_owned_change = rs.and_then(|r| r.owned_change);
    p
}

/// cid -> (FPts, FP/G)
pub type FantasyPoints = Map<String, (Option<f64>, Option<f64>)>;

#[derive(Clone, Debug)]
pub struct ParsedRoster {
    pub slots: Vec<RosterSlot>,
    pub fantasy_points: FantasyPoints,
    pub ages: Map<String, f64>,
    /// status name -> (total, max)
    pub limits: Map<String, (i64, i64)>,
    /// starting slot -> count (incl. empty)
    pub slot_counts: Map<String, i64>,
    /// StatLine split of this view (None: period view)
    pub split: Option<String>,
}

pub fn parse_status_totals(data: &Value) -> Map<String, (i64, i64)> {
    let mut out = Map::new();
    for t in jlist(jobj(data, "miscData"), "statusTotals") {
        let name = jget_t(t, "name").or_else(|| jget_t(t, "statusId")).map(py_str).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let total = match jget_t(t, "total") {
            Some(v) => py_int(v),
            None => Some(0),
        };
        let max = match jget_t(t, "max") {
            Some(v) => py_int(v),
            None => Some(0),
        };
        if let (Some(a), Some(b)) = (total, max) {
            out.insert(name, (a, b));
        }
    }
    out
}

/// Parse a getTeamRosterInfo (view=STATS) response. `positions` maps posId -> shortName.
pub fn parse_roster(data: &Value, positions: &Map<String, String>, codes: Option<&Codes>, today: Option<NaiveDate>) -> ParsedRoster {
    let split = data_split(data, codes);
    let mut slots = Vec::new();
    let mut fp = FantasyPoints::new();
    let mut ages = Map::new();
    let mut counts: Map<String, i64> = Map::new();
    for table in jlist(data, "tables") {
        let header = jlist(jobj(table, "header"), "cells");
        for row in jlist(table, "rows") {
            let Some(ro) = row.as_object() else { continue };
            if !ro.contains_key("posId") && !ro.contains_key("scorer") {
                continue;
            }
            let status_id = get_str_or(row, "statusId", "1");
            let slot = match status_slot(&status_id) {
                Some(s) => s.to_string(),
                None => {
                    let pos_id = get_str_or(row, "posId", "None");
                    canonical_slot(positions.get(&pos_id).map(|s| s.as_str()).unwrap_or(""))
                }
            };
            let starting = !NON_STARTING.contains(&slot.as_str());
            let scorer = jget(row, "scorer").filter(|s| truthy(s) && jget_t(s, "scorerId").is_some());
            let Some(scorer) = scorer else {
                if status_id != "1" {
                    continue; // empty reserve rows carry no information
                }
                slots.push(RosterSlot { slot: slot.clone(), player: None, starting });
                *counts.entry(slot).or_insert(0) += 1;
                continue;
            };
            let rs = parse_row_cells(header, jlist(row, "cells"));
            let p = player_from_scorer(scorer, Some(&rs), split.as_deref(), today);
            if split.is_some() {
                fp.insert(p.cid.clone(), (rs.fpts, rs.fpg));
            }
            if let Some(a) = rs.age {
                ages.insert(p.cid.clone(), a);
            }
            slots.push(RosterSlot { slot: slot.clone(), player: Some(p), starting });
            if starting {
                *counts.entry(slot).or_insert(0) += 1;
            }
        }
    }
    ParsedRoster { slots, fantasy_points: fp, ages, limits: parse_status_totals(data), slot_counts: counts, split }
}

pub fn roster_shape(slot_counts: &Map<String, i64>, limits: &Map<String, (i64, i64)>) -> Map<String, i64> {
    let mut shape: Map<String, i64> = slot_counts.iter().filter(|(_, v)| **v > 0).map(|(k, v)| (k.clone(), *v)).collect();
    for (name, slot) in [("Reserve", "BN"), ("Inj Res", "IR"), ("Injured Reserve", "IR"), ("Minors", "MIN")] {
        if let Some((_, m)) = limits.get(name) {
            if *m > 0 {
                shape.insert(slot.to_string(), *m);
            }
        }
    }
    shape
}

pub fn parse_positions(position_map: Option<&Value>) -> Map<String, String> {
    let mut out = Map::new();
    if let Some(Value::Object(m)) = position_map {
        for (pid, v) in m {
            if v.is_object() {
                let id = jget(v, "id").map(py_str).unwrap_or_else(|| pid.clone());
                let short = jget_t(v, "shortName").or_else(|| jget_t(v, "name")).map(py_str).unwrap_or_else(|| pid.clone());
                out.insert(id, short);
            }
        }
    }
    out
}

/// `{id: {'name':..., 'short':...}}`
#[derive(Clone, Debug, PartialEq)]
pub struct TeamMeta {
    pub name: String,
    pub short: String,
}

/// fantasyTeams (list or id->dict) -> {id: TeamMeta}.
pub fn parse_teams(data: &Value) -> Map<String, TeamMeta> {
    let raw = jget_t(data, "fantasyTeams");
    let mut pairs: Vec<(String, &Value)> = Vec::new();
    match raw {
        Some(Value::Array(list)) => {
            let mut m: Map<String, &Value> = Map::new();
            for t in list.iter().filter(|t| t.is_object()) {
                m.insert(get_str_or(t, "id", "None"), t);
            }
            pairs.extend(m);
        }
        Some(Value::Object(o)) => pairs.extend(o.iter().map(|(k, v)| (k.clone(), v))),
        _ => {}
    }
    let mut out = Map::new();
    for (tid, t) in pairs {
        if !t.is_object() {
            continue;
        }
        out.insert(tid.clone(), TeamMeta { name: get_str_or(t, "name", &tid), short: get_str_or(t, "shortName", "") });
    }
    out
}

/// getStandings -> {team_id: {'rank', 'win', 'loss', 'tie', 'points_for', 'points_against', 'waiver_order'}}.
pub fn parse_standings(data: &Value) -> Map<String, Map<String, Value>> {
    let mut out = Map::new();
    let tables = jlist(data, "tableList");
    let Some(table) = tables.first() else { return out };
    let mut fields: HashMap<String, usize> = HashMap::new();
    for (i, c) in jlist(jobj(table, "header"), "cells").iter().enumerate() {
        fields.insert(jget(c, "key").map(py_str).unwrap_or_else(|| "\u{0}None".into()), i);
    }
    let val = |cells: &[Value], key: &str| -> Option<f64> {
        let i = *fields.get(key)?;
        num(jget(cells.get(i)?, "content"))
    };
    let int_v = |x: Option<f64>| -> Value { x.map(|v| Value::from(v.trunc() as i64)).unwrap_or(Value::Null) };
    let int_or0 = |x: Option<f64>| -> Value { Value::from(x.map(|v| v.trunc() as i64).unwrap_or(0)) };
    let float_v = |x: Option<f64>| -> Value { x.map(Value::from).unwrap_or(Value::Null) };
    for row in jlist(table, "rows") {
        let fixed = jlist(row, "fixedCells");
        let Some(team_id) = fixed.iter().find_map(|c| jget_t(c, "teamId")) else { continue };
        let cells = jlist(row, "cells");
        let rank = fixed.first().and_then(|f| num(jget(f, "content")));
        let mut m = Map::new();
        m.insert("rank".to_string(), int_v(rank));
        m.insert("win".to_string(), int_or0(val(cells, "win")));
        m.insert("loss".to_string(), int_or0(val(cells, "loss")));
        m.insert("tie".to_string(), int_or0(val(cells, "tie")));
        m.insert("points_for".to_string(), float_v(val(cells, "pointsFor")));
        m.insert("points_against".to_string(), float_v(val(cells, "pointsAgainst")));
        m.insert("waiver_order".to_string(), int_v(val(cells, "wwOrder")));
        out.insert(py_str(team_id), m);
    }
    out
}

pub fn parse_trade_blocks(blocks: &[Value], positions: &Map<String, String>, teams: &Map<String, TeamMeta>) -> Vec<TradeBlockInfo> {
    let mut out = Vec::new();
    for b in blocks {
        let Some(bo) = b.as_object() else { continue };
        if bo.len() <= 2 || !bo.contains_key("teamId") {
            continue;
        }
        let tid = py_str(&bo["teamId"]);
        let scorers = |key: &str| -> Vec<String> {
            let mut v = Vec::new();
            if let Some(Value::Object(groups)) = jget_t(jobj(b, key), "scorers") {
                for g in groups.values() {
                    for s in g.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                        if let Some(id) = jget_t(s, "scorerId") {
                            v.push(format!("fantrax:{}", py_str(id)));
                        }
                    }
                }
            }
            v
        };
        let poss = |key: &str| -> Vec<String> {
            jlist(jobj(b, key), "positions")
                .iter()
                .map(|p| {
                    let ps = py_str(p);
                    canonical_slot(positions.get(&ps).map(|s| s.as_str()).unwrap_or(&ps))
                })
                .collect()
        };
        let stats = |key: &str| -> Vec<String> {
            jlist(jobj(b, key), "stats").iter().filter_map(|s| jget_t(s, "shortName").map(py_str)).collect()
        };
        let note_v = jget(jobj(b, "comment"), "body").cloned().unwrap_or(Value::String(String::new()));
        out.push(TradeBlockInfo {
            team_name: teams.get(&tid).map(|t| t.name.clone()).unwrap_or_else(|| tid.clone()),
            note: strip_html_v(Some(&note_v)),
            players_offered: scorers("scorersOffered"),
            players_wanted: scorers("scorersWanted"),
            positions_offered: poss("positionsOffered"),
            positions_wanted: poss("positionsWanted"),
            stats_offered: stats("statsOffered"),
            stats_wanted: stats("statsWanted"),
            team_id: tid,
        });
    }
    out
}

pub fn parse_transactions(data: &Value) -> Vec<TransactionInfo> {
    let mut out = Vec::new();
    for row in jlist(jobj(data, "table"), "rows") {
        let cells = jlist(row, "cells");
        let scorer = jobj(row, "scorer");
        let code = jget_t(row, "transactionCode").map(py_str).unwrap_or_default();
        let kind = if code == "CLAIM" { jget_t(row, "claimType").map(py_str).unwrap_or_else(|| code.clone()) } else { code.clone() };
        let name = strip_html_v(jget(scorer, "name"));
        out.push(TransactionInfo {
            tx_id: get_str_or(row, "txSetId", ""),
            team_id: cells.first().and_then(|c| opt_str(jget(c, "teamId"))),
            when: if cells.len() > 1 { Some(strip_html_v(jget(&cells[1], "content"))) } else { None },
            kind: if kind.is_empty() { "UNKNOWN".into() } else { kind },
            player_cid: jget_t(scorer, "scorerId").map(|s| format!("fantrax:{}", py_str(s))),
            player_name: if name.is_empty() { None } else { Some(name) },
        });
    }
    out
}

pub fn parse_pending_trades(data: &Value) -> Vec<PendingTradeInfo> {
    let mut out = Vec::new();
    for t in jlist(data, "tradeInfoList") {
        let mut moves = Vec::new();
        for m in jlist(t, "moves") {
            let mut mv: Map<String, Value> = Map::new();
            mv.insert("from".into(), jget(jobj(m, "from"), "teamId").cloned().unwrap_or(Value::Null));
            mv.insert("to".into(), jget(jobj(m, "to"), "teamId").cloned().unwrap_or(Value::Null));
            if let Some(dp) = jget(m, "draftPick") {
                let mut pick = serde_json::Map::new();
                pick.insert("year".into(), jget(dp, "year").cloned().unwrap_or(Value::Null));
                pick.insert("round".into(), jget(dp, "round").cloned().unwrap_or(Value::Null));
                pick.insert("original_owner".into(), jget(jobj(dp, "origOwnerTeam"), "id").cloned().unwrap_or(Value::Null));
                mv.insert("pick".into(), Value::Object(pick));
            } else if let Some(sc) = jget_t(m, "scorer") {
                mv.insert("player_cid".into(), Value::String(format!("fantrax:{}", get_str_or(sc, "scorerId", "None"))));
                mv.insert("player_name".into(), jget(sc, "name").cloned().unwrap_or(Value::Null));
            }
            moves.push(mv);
        }
        let mut info: Map<String, Value> = Map::new();
        for i in jlist(t, "usefulInfo") {
            if i.is_object() {
                info.insert(get_str_or(i, "name", "None"), jget(i, "value").cloned().unwrap_or(Value::Null));
            }
        }
        out.push(PendingTradeInfo {
            trade_id: get_str_or(t, "txSetId", ""),
            proposed_by: opt_str(jget(t, "creatorTeamId")),
            moves,
            proposed_at: info.get("Proposed").filter(|v| truthy(v)).map(py_str),
        });
    }
    out
}

// ------------------------------------------------------------------ activity feed (harness capture)

pub const WHEN_FORMATS_WITH_YEAR: &[&str] = &[
    "%a %b %d, %Y, %I:%M%p",
    "%a %b %d, %Y, %I:%M %p",
    "%b %d, %Y, %I:%M%p",
    "%b %d, %Y, %I:%M %p",
    "%a %b %d, %Y",
    "%b %d, %Y",
];
pub const WHEN_FORMATS_NO_YEAR: &[&str] =
    &["%a %b %d, %I:%M%p", "%a %b %d, %I:%M %p", "%b %d, %I:%M%p", "%b %d, %I:%M %p", "%a %b %d", "%b %d"];
static TZ_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s+(E[DS]T|C[DS]T|M[DS]T|P[DS]T|ET|UTC|GMT)\b").expect("regex"));
static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("regex"));
const FANTRAX_ADD_KINDS: &[&str] = &["CLAIM", "FA", "WW", "WAIVER", "ADD", "FREE_AGENT"];
const FANTRAX_DROP_KINDS: &[&str] = &["DROP"];
const FANTRAX_TRADE_KINDS: &[&str] = &["TRADE"];
const FANTRAX_IR_KINDS: &[&str] = &["IR", "INJURED_RESERVE", "RESERVE_IR"];
const FANTRAX_ACTIVATE_KINDS: &[&str] = &["ACTIVATE", "ACTIVATED"];

/// Start year of the NHL season `day` belongs to (August on = the upcoming season).
pub fn season_start_year_for(day: NaiveDate) -> i64 {
    if day.month() >= 8 {
        day.year() as i64
    } else {
        day.year() as i64 - 1
    }
}

/// Parse Fantrax's display timestamps; without a year the season is inferred (the date must fall
/// in the season starting `season_start_year`). None when unparseable.
pub fn parse_fantrax_when(text: Option<&str>, season_start_year: Option<i64>) -> Option<NaiveDateTime> {
    let text = text.filter(|t| !t.is_empty())?;
    let stripped = strip_html(text);
    let t0 = TZ_RE.replace_all(&stripped, "");
    let t1 = py_strip(&t0).replace('\u{a0}', " ");
    let t = WS_RE.replace_all(&t1, " ").to_string();
    for fmt in WHEN_FORMATS_WITH_YEAR {
        if let Some(d) = fm_pycompat::time::strptime(&t, fmt) {
            return Some(d);
        }
    }
    let y0 = season_start_year.unwrap_or_else(|| season_start_year_for(local_today()));
    for fmt in WHEN_FORMATS_NO_YEAR {
        for year in [y0, y0 + 1] {
            let Some(d) = fm_pycompat::time::strptime(&format!("{t} {year}"), &format!("{fmt} %Y")) else { continue };
            if (d.month() >= 8 && year == y0) || (d.month() < 8 && year == y0 + 1) {
                return Some(d);
            }
        }
    }
    None
}

/// TransactionInfo rows -> ActivityItems (claims ADD, drops DROP, trades TRADE_IN / TRADE_OUT).
pub fn activity_from_transactions(
    txs: &[TransactionInfo],
    team_names: &Map<String, String>,
    season_start_year: Option<i64>,
) -> Vec<ActivityItem> {
    let mut trade_teams: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for t in txs {
        if FANTRAX_TRADE_KINDS.contains(&py_upper(&t.kind).as_str()) {
            if let Some(team) = t.team_id.as_deref().filter(|x| !x.is_empty()) {
                trade_teams.entry(t.tx_id.as_str()).or_default().insert(team);
            }
        }
    }
    let mut out = Vec::new();
    for t in txs {
        let Some(ts) = parse_fantrax_when(t.when.as_deref(), season_start_year) else { continue };
        let kind = py_upper(&t.kind);
        let item = |action: ActivityAction, team: Option<&str>, other: Option<&str>| ActivityItem {
            source: "fantrax".into(),
            tx_id: t.tx_id.clone(),
            ts: PyDateTime::naive(ts),
            team_id: team.map(str::to_string),
            team_name: team_names.get(team.unwrap_or("")).cloned(),
            action,
            cid: t.player_cid.clone(),
            player_name: t.player_name.clone(),
            nhl_id: None,
            group_id: Some(t.tx_id.clone()),
            counterparty_id: other.map(str::to_string),
        };
        let k = kind.as_str();
        if FANTRAX_TRADE_KINDS.contains(&k) {
            let others: Vec<&str> = trade_teams
                .get(t.tx_id.as_str())
                .map(|s| s.iter().copied().filter(|x| Some(*x) != t.team_id.as_deref()).collect())
                .unwrap_or_default();
            let other = if others.len() == 1 { Some(others[0]) } else { None };
            out.push(item(ActivityAction::TradeIn, t.team_id.as_deref(), other));
            if let Some(o) = other {
                out.push(item(ActivityAction::TradeOut, Some(o), t.team_id.as_deref()));
            }
        } else if FANTRAX_DROP_KINDS.contains(&k) {
            out.push(item(ActivityAction::Drop, t.team_id.as_deref(), None));
        } else if FANTRAX_IR_KINDS.contains(&k) {
            out.push(item(ActivityAction::Ir, t.team_id.as_deref(), None));
        } else if FANTRAX_ACTIVATE_KINDS.contains(&k) {
            out.push(item(ActivityAction::Activate, t.team_id.as_deref(), None));
        } else if FANTRAX_ADD_KINDS.contains(&k) || k.ends_with("CLAIM") {
            out.push(item(ActivityAction::Add, t.team_id.as_deref(), None));
        }
    }
    out
}

/// Pending trade proposals -> PROPOSED items, one per player move (`team_id` receives,
/// `counterparty_id` gives; draft picks skipped). Timestamp: the "Proposed" info, else `now`.
pub fn activity_from_pending(
    trades: &[PendingTradeInfo],
    team_names: &Map<String, String>,
    season_start_year: Option<i64>,
    now: Option<NaiveDateTime>,
) -> Vec<ActivityItem> {
    let mut out = Vec::new();
    for tr in trades {
        let ts = parse_fantrax_when(tr.proposed_at.as_deref(), season_start_year)
            .or(now)
            .unwrap_or_else(|| Local::now().naive_local());
        for mv in &tr.moves {
            let Some(cid) = mv.get("player_cid").filter(|v| truthy(v)) else { continue };
            let to = opt_str(mv.get("to"));
            let key = to.clone().unwrap_or_default();
            out.push(ActivityItem {
                source: "fantrax".into(),
                tx_id: tr.trade_id.clone(),
                ts: PyDateTime::naive(ts),
                team_name: team_names.get(&key).cloned(),
                team_id: to,
                action: ActivityAction::Proposed,
                cid: Some(py_str(cid)),
                player_name: opt_str(mv.get("player_name")),
                nhl_id: None,
                group_id: Some(tr.trade_id.clone()),
                counterparty_id: opt_str(mv.get("from")),
            });
        }
    }
    out
}

/// Roster snapshot -> LineupDay rows (starting = slot not in BN / IR / MIN / TAXI).
pub fn lineup_days_from_teams(teams: &[FantasyTeam], day: NaiveDate) -> Vec<LineupDay> {
    let mut out = Vec::new();
    for t in teams {
        for sl in &t.slots {
            let Some(p) = &sl.player else { continue };
            out.push(LineupDay {
                team_id: t.team_id.clone(),
                date: day,
                cid: p.cid.clone(),
                slot: sl.slot.clone(),
                starting: !NON_STARTING.contains(&sl.slot.as_str()),
                provider_pts: None,
            });
        }
    }
    out
}

/// getPlayerStats -> (available players, row stats by cid, total pages).
pub fn parse_player_pool(
    data: &Value,
    codes: Option<&Codes>,
    today: Option<NaiveDate>,
) -> (Vec<Player>, Map<String, RowStats>, i64) {
    let split = data_split(data, codes);
    let header = jlist(jobj(data, "tableHeader"), "cells");
    let mut players = Vec::new();
    let mut rows = Map::new();
    for entry in jlist(data, "statsTable") {
        let scorer = jobj(entry, "scorer");
        if jget_t(scorer, "scorerId").is_none() {
            continue;
        }
        let rs = parse_row_cells(header, jlist(entry, "cells"));
        if rs.status_team_id.is_some() {
            continue; // rostered by a fantasy team: not a free agent
        }
        let p = player_from_scorer(scorer, Some(&rs), split.as_deref(), today);
        rows.insert(p.cid.clone(), rs);
        players.push(p);
    }
    let pages = total_pages(data);
    (players, rows, pages)
}

fn total_pages(data: &Value) -> i64 {
    match jget_t(jobj(data, "paginatedResultSet"), "totalNumPages") {
        Some(v) => py_int(v).unwrap_or(1),
        None => 1,
    }
}

/// getPlayerStats (statusOrTeamFilter=ALL_TAKEN) -> {cid: row stats} for players on a fantasy team.
pub fn parse_taken_rows(data: &Value) -> Map<String, RowStats> {
    let header = jlist(jobj(data, "tableHeader"), "cells");
    let mut out = Map::new();
    for entry in jlist(data, "statsTable") {
        let Some(sid) = jget_t(jobj(entry, "scorer"), "scorerId") else { continue };
        let rs = parse_row_cells(header, jlist(entry, "cells"));
        if rs.status_team_id.is_none() {
            continue;
        }
        out.insert(format!("fantrax:{}", py_str(sid)), rs);
    }
    out
}

/// {cid: (% rostered, age)} for players on a fantasy team.
pub fn parse_taken_ownership(data: &Value) -> Map<String, (Option<f64>, Option<f64>)> {
    parse_taken_rows(data).into_iter().map(|(cid, rs)| (cid, (rs.owned, rs.age))).collect()
}

// ------------------------------------------------------------------ timeframes

/// split -> {"seasonOrProjection", "timeframeTypeCode"}
pub type Codes = Map<String, Map<String, String>>;

/// The seasonOrProjection a roster / player-stats response is showing (None if absent).
pub fn displayed_timeframe(data: &Value) -> Option<&Value> {
    let ds = jget_t(data, "displayedSelections");
    let tf = ds.filter(|d| d.is_object()).and_then(|d| jget(d, "displayedSeasonOrProjection"));
    let tf = tf.filter(|t| truthy(t)).or_else(|| jget(data, "displayedSeasonOrProjection"));
    tf.filter(|t| t.is_object())
}

pub fn timeframe_options(data: &Value) -> Vec<&Value> {
    let lists = jobj(data, "displayedLists");
    for opts in [jget(lists, "seasonOrProjections"), jget(lists, "displayedSeasonOrProjections"), jget(data, "seasonOrProjections")] {
        if let Some(Value::Array(a)) = opts {
            if !a.is_empty() {
                return a.iter().filter(|o| o.is_object() && jget_t(o, "code").is_some()).collect();
            }
        }
    }
    Vec::new()
}

/// {'season'|'prior'|'projected': {'seasonOrProjection', 'timeframeTypeCode'}} from a response.
pub fn timeframe_codes(data: &Value) -> Codes {
    let opts = timeframe_options(data);
    let is_code = |o: &Value, c: &str| jget(o, "timeframeTypeCode").and_then(|x| x.as_str()) == Some(c);
    let ytd: Vec<&Value> = opts.iter().copied().filter(|o| is_code(o, "YEAR_TO_DATE")).collect();
    let mut reg: Vec<&Value> = ytd.iter().copied().filter(|o| py_upper(&get_str_or(o, "name", "")).contains("REG")).collect();
    if reg.is_empty() {
        reg = ytd.clone();
    }
    let end = |o: &Value| -> f64 { jget_t(o, "endDate").and_then(py_float).unwrap_or(0.0) };
    fm_pycompat::order::sort_desc_by(&mut reg, |a, b| fm_pycompat::order::cmp_f64(end(a), end(b)));
    let mut chosen: Vec<(&str, &Value)> = Vec::new();
    if let Some(r) = reg.first() {
        chosen.push(("season", r));
    }
    if reg.len() > 1 {
        chosen.push(("prior", reg[1]));
    }
    if let Some(p) = opts.iter().copied().find(|o| is_code(o, "PROJECTED_SEASON")) {
        chosen.push(("projected", p));
    }
    chosen
        .into_iter()
        .map(|(k, o)| {
            let mut m = Map::new();
            m.insert("seasonOrProjection".to_string(), get_str_or(o, "code", "None"));
            m.insert("timeframeTypeCode".to_string(), get_str_or(o, "timeframeTypeCode", ""));
            (k.to_string(), m)
        })
        .collect()
}

/// StatLine split for a response's rows ('season' / 'prior' / 'projected'), None for per-period
/// views. Responses without timeframe info are treated as season-to-date.
pub fn data_split(data: &Value, codes: Option<&Codes>) -> Option<String> {
    let Some(tf) = displayed_timeframe(data) else { return Some("season".into()) };
    let code = get_str_or(tf, "code", "");
    let computed;
    let codes = match codes.filter(|c| !c.is_empty()) {
        Some(c) => c,
        None => {
            computed = timeframe_codes(data);
            &computed
        }
    };
    for (split, c) in codes {
        if c.get("seasonOrProjection").map(|s| s.as_str()) == Some(code.as_str()) {
            return Some(split.clone());
        }
    }
    if jget(tf, "timeframeTypeCode").and_then(|x| x.as_str()) == Some("PROJECTED_SEASON") {
        return Some("projected".into());
    }
    None
}

/// Copy stat lines `target` lacks from `other` (same player, another timeframe).
pub fn merge_lines(target: &mut Player, other: &Player) {
    for (split, line) in &other.lines {
        target.lines.entry(split.clone()).or_insert_with(|| line.clone());
    }
    if target.pct_owned.is_none() && other.pct_owned.is_some() {
        target.pct_owned = other.pct_owned;
    }
    if target.pct_owned_change.is_none() && other.pct_owned_change.is_some() {
        target.pct_owned_change = other.pct_owned_change;
    }
}

/// (FPts, FP/G): season-to-date once the player has games, else Fantrax's projection.
pub fn pick_fantasy_points(
    by_split: &Map<String, (Option<f64>, Option<f64>)>,
    player: &Player,
) -> Option<(Option<f64>, Option<f64>)> {
    for split in ["season", "projected", "prior"] {
        if let Some(v) = by_split.get(split) {
            if player.lines.contains_key(split) {
                return Some(*v);
            }
        }
    }
    by_split.values().next().copied()
}

/// Merge the same roster parsed from several timeframes into the first one.
pub fn merge_rosters(parsed: Vec<ParsedRoster>) -> ParsedRoster {
    let mut it = parsed.into_iter();
    let mut main = it.next().expect("at least one parsed roster");
    let rest: Vec<ParsedRoster> = it.collect();
    let mut others: Map<String, Player> = Map::new();
    let mut fp_by: Map<String, FantasyPoints> = Map::new();
    for (cid, v) in &main.fantasy_points {
        fp_by.entry(cid.clone()).or_default().insert(main.split.clone().unwrap_or_else(|| "?".into()), *v);
    }
    for pr in &rest {
        for (cid, v) in &pr.fantasy_points {
            fp_by.entry(cid.clone()).or_default().insert(pr.split.clone().unwrap_or_else(|| "?".into()), *v);
        }
        for sl in &pr.slots {
            if let Some(p) = &sl.player {
                others.entry(p.cid.clone()).or_insert_with(|| p.clone());
            }
        }
        for (cid, age) in &pr.ages {
            main.ages.entry(cid.clone()).or_insert(*age);
        }
    }
    let mut fp = FantasyPoints::new();
    for sl in main.slots.iter_mut() {
        let Some(p) = sl.player.as_mut() else { continue };
        if let Some(o) = others.get(&p.cid) {
            merge_lines(p, o);
        }
        if let Some(by) = fp_by.get(&p.cid) {
            if let Some(v) = pick_fantasy_points(by, p) {
                fp.insert(p.cid.clone(), v);
            }
        }
    }
    main.fantasy_points = fp;
    main
}

/// draftPicksData (roster response) -> {year: [{'round', 'original_owner'}]}.
pub fn parse_draft_picks(data: &Value) -> Map<i64, Vec<DraftPickRef>> {
    let mut out = Map::new();
    for y in jlist(jobj(data, "draftPicksData"), "draftPicksPerYear") {
        let Some(year) = jget(y, "year").and_then(py_int) else { continue };
        let picks = jlist(y, "draftPickList")
            .iter()
            .filter(|pk| pk.is_object())
            .map(|pk| DraftPickRef {
                round: jget(pk, "round").and_then(py_int).unwrap_or(0),
                original_owner: opt_str(jget(pk, "origOwnerTeamId")),
                extra: Map::new(),
            })
            .collect();
        out.insert(year, picks);
    }
    out
}

// ------------------------------------------------------------------ league rules page (getLeagueRulesOld)

static RULE_HEAD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<h[36][^>]*>(.*?)</h[36]>").expect("regex"));
static RULE_SPAN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<span[^>]*>(.*?)</span>").expect("regex"));
static RULE_VALUE_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<span|<li|</li|</ul|<br|<h\d|<div|<table|<b>\s*&bull;").expect("regex"));
static SCORING_ROW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<td class="scGroup">(.*?)</td>\s*<td class="scoringPts">(.*?)</td>\s*<td class="points">(.*?)</td>"#)
        .expect("regex")
});
static ROW_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<tr[^>]*>(.*?)</tr>").expect("regex"));
static CELL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<t[dh][^>]*>(.*?)</t[dh]>").expect("regex"));
static ABBR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(([^()]+)\)\s*$").expect("regex"));
static INT_CELL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-?\d*$").expect("regex"));
static INT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-?\d+$").expect("regex"));
/// Labels not repeated by describe_settings (other managers' identities, ids).
pub const RULE_SKIP_LABELS: &[&str] = &["League Creator Username", "Commissioner Team Name", "League ID", "Draft Order"];

/// `_text(fragment)`: tags -> spaces, HTML entities unescaped, whitespace collapsed.
fn html_text(fragment: &str) -> String {
    let no_tags = TAG_RE.replace_all(fragment, " ");
    let un = htmlize::unescape(no_tags.as_ref());
    py_strip(&WS_RE.replace_all(&un, " ")).to_string()
}

/// `rules.empty`
pub fn rules_empty(rules: &LeagueRules) -> bool {
    rules.settings.is_empty() && rules.scoring.is_empty() && rules.positions.is_empty()
}

/// Parse the HTML of getLeagueRulesOld (the league's Rules page).
pub fn parse_league_rules(content: &str) -> LeagueRules {
    let mut rules = LeagueRules::default();
    if content.is_empty() {
        return rules;
    }
    let heads: Vec<(usize, String)> =
        RULE_HEAD_RE.captures_iter(content).map(|c| (c.get(0).map_or(0, |m| m.start()), html_text(&c[1]))).collect();
    let section = |pos: usize| -> String {
        let mut name = "General".to_string();
        for (start, title) in &heads {
            if *start > pos {
                break;
            }
            if !title.is_empty() {
                name = title.clone();
            }
        }
        name
    };
    for c in RULE_SPAN_RE.captures_iter(content) {
        let m = c.get(0).expect("match");
        let label_t = html_text(&c[1]);
        let label = py_strip(label_t.trim_end_matches(':')).to_string();
        if label.is_empty() || fm_pycompat::text::py_len(&label) > 120 {
            continue;
        }
        let rest = &content[m.end()..];
        let value = match RULE_VALUE_END.find(rest) {
            Some(end) => html_text(&rest[..end.start()]),
            None => html_text(char_prefix(rest, 300)),
        };
        rules.settings.entry(section(m.start())).or_default().insert(label, value);
    }
    for c in SCORING_ROW_RE.captures_iter(content) {
        let name = html_text(&c[2]);
        let points = num(Some(&Value::String(html_text(&c[3]))));
        let Some(abbr_m) = ABBR_RE.captures(&name) else { continue };
        let Some(points) = points else { continue };
        if name.is_empty() {
            continue;
        }
        let abbr = py_strip(&abbr_m[1]).to_string();
        let mut group = html_text(&c[1]);
        if group.is_empty() {
            group = "All".into();
        }
        rules.scoring.entry(group).or_default().insert(abbr.clone(), points);
        rules.scoring_names.entry(abbr).or_insert(name);
    }
    // Rosters table: Pos | Min Active | Max Active | Total Max; columns by header text when present.
    let keys = ["min_active", "max_active", "max"];
    let mut cols: [Option<usize>; 3] = [Some(1), Some(2), Some(3)];
    for row in ROW_RE.captures_iter(content) {
        let cells: Vec<String> = CELL_RE.captures_iter(&row[1]).map(|c| html_text(&c[1])).collect();
        let low: Vec<String> = cells.iter().map(|c| py_lower(c)).collect();
        if low.iter().any(|c| c == "max active" || c == "total max") {
            let find = |label: &str| low.iter().position(|c| c == label);
            cols = [find("min active"), find("max active"), find("total max")];
            continue;
        }
        if cells.len() >= 3 {
            let Some(pm) = ABBR_RE.captures(&cells[0]) else { continue };
            if !cells[1..cells.len().min(4)].iter().all(|c| INT_CELL_RE.is_match(c)) {
                continue;
            }
            let cell = |i: Option<usize>| -> Option<i64> {
                let v = i.and_then(|i| cells.get(i)).map(|s| s.as_str()).unwrap_or("");
                if INT_RE.is_match(v) {
                    v.parse::<i64>().ok()
                } else {
                    None
                }
            };
            let entry: Map<String, Option<i64>> = keys.iter().zip(cols.iter()).map(|(k, i)| (k.to_string(), cell(*i))).collect();
            rules.positions.insert(py_upper(py_strip(&pm[1])), entry);
        }
    }
    rules
}

/// {position: "Total Max"} from the Rules page Rosters table (positions with a limit only).
pub fn rules_position_limits(rules: Option<&LeagueRules>) -> Map<String, i64> {
    let mut out = Map::new();
    if let Some(r) = rules {
        for (pos, v) in &r.positions {
            if let Some(Some(cap)) = v.get("max") {
                if *cap > 0 {
                    out.insert(canonical_slot(pos), *cap);
                }
            }
        }
    }
    out
}

static STAT_LIMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)games? played|\bgp\b|goalie.*(minimum|min\b|start)|(minimum|min\b).*goalie").expect("regex")
});

/// Games-played caps / goalie minimums stated on the Rules page; None when it was not read.
pub fn rules_stat_limits(rules: Option<&LeagueRules>) -> Option<Vec<String>> {
    let rules = rules.filter(|r| !rules_empty(r))?;
    let mut out = Vec::new();
    for sect in rules.settings.values() {
        for (k, v) in sect {
            let vl = py_lower(py_strip(v));
            if STAT_LIMIT_RE.is_match(k) && !matches!(vl.as_str(), "" | "not used" | "no" | "none") {
                out.push(format!("{k}: {v}"));
            }
        }
    }
    Some(out)
}

/// "Maximum Total Players" (active + reserve).
pub fn rules_max_roster(rules: Option<&LeagueRules>) -> Option<i64> {
    rules.and_then(|r| r.get_int("Maximum Total Players")).filter(|n| *n > 0)
}

pub const CLAIMS_SEASON_LABEL: &str = "Max # of claims per season";
pub const CLAIMS_WEEK_LABEL: &str = "Max # of claims per week";

static LEADING_INT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\d+)").expect("regex"));
static CLAIM_SUB_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z][A-Za-z ]*:\s*(\d+|Unlimited)").expect("regex"));

/// "Unlimited" / blank / "Not Used" -> None; "10" -> 10; "3 (Free Agents:Unlimited, ...)" -> 3;
/// "Unlimited (Free Agents:2, Waiver Wire:1)" -> 3.
pub fn parse_claim_limit(text: Option<&str>) -> Option<i64> {
    let text = text.unwrap_or("");
    if let Some(m) = LEADING_INT_RE.captures(text) {
        return m[1].parse().ok();
    }
    let subs: Vec<String> = CLAIM_SUB_RE.captures_iter(text).map(|c| c[1].to_string()).collect();
    if !subs.is_empty() && subs.iter().all(|v| fm_pycompat::text::is_digit(v)) {
        return Some(subs.iter().filter_map(|v| v.parse::<i64>().ok()).sum());
    }
    None
}

/// (max claims per season, max claims per week); None = Unlimited (or not stated).
pub fn rules_claim_limits(rules: Option<&LeagueRules>) -> (Option<i64>, Option<i64>) {
    match rules {
        None => (None, None),
        Some(r) => (parse_claim_limit(r.get(CLAIMS_SEASON_LABEL)), parse_claim_limit(r.get(CLAIMS_WEEK_LABEL))),
    }
}

/// Fantrax category abbreviation -> canonical stat key (None if not tracked).
pub fn stat_key(abbr: &str) -> Option<&'static str> {
    stat_column(&py_upper(py_strip(abbr))).filter(|k| is_canonical_stat(k))
}

/// (base weights, goalie-group overrides, notes) from the Rules page scoring table.
pub fn rules_weights(rules: &LeagueRules) -> (Map<String, f64>, Map<String, f64>, Vec<String>) {
    let mut base: Map<String, f64> = Map::new();
    let mut goalie: Map<String, f64> = Map::new();
    let mut notes = Vec::new();
    let mut groups: Vec<(&String, &Map<String, f64>)> = rules.scoring.iter().collect();
    groups.sort_by_key(|(g, _)| py_lower(g).contains("goal")); // goalies last (stable)
    for (group, cats) in groups {
        let is_goalie = py_lower(group).contains("goal");
        for (abbr, pts) in cats {
            let Some(key) = stat_key(abbr) else {
                let name = rules.scoring_names.get(abbr).unwrap_or(abbr);
                notes.push(format!("scoring category {name} = {} is not tracked", general(*pts, None, false)));
                continue;
            };
            if *pts == 0.0 {
                continue;
            }
            if !is_goalie || !base.contains_key(key) {
                base.entry(key.to_string()).or_insert(*pts);
            } else if base[key] != *pts {
                goalie.insert(key.to_string(), *pts);
            }
        }
    }
    (base, goalie, notes)
}

// ------------------------------------------------------------------ scoring weights: check and fit

pub const FIT_EXCLUDE: &[&str] = &["GP", FPTS_KEY, "GAA", "SVPCT"];
/// Aggregates that are sums of other columns: fitted last so they drop out when collinear.
pub const FIT_LAST: &[&str] = &["PTS", "PPP", "SHP", "SA"];

/// Fantrax lines usable for scoring checks: GP > 0 and Fantrax FPts present.
pub fn fit_lines(p: &Player) -> Vec<&StatLine> {
    ["season", "projected", "prior"]
        .iter()
        .filter_map(|s| p.lines.get(*s))
        .filter(|ln| ln.gp > 0 && ln.stats.contains_key(FPTS_KEY))
        .collect()
}

/// (mean |FP/G error| of sum(weight*stat) vs Fantrax's FP/G, players compared); NaN when none.
pub fn weights_check<'a, I>(weights: &Map<String, f64>, players: I, goalie_weights: Option<&Map<String, f64>>) -> (f64, i64)
where
    I: IntoIterator<Item = &'a Player>,
{
    let w: Map<String, f64> = weights.iter().filter(|(k, _)| k.as_str() != FPTS_KEY).map(|(k, v)| (k.clone(), *v)).collect();
    let scoring = PointsScoring::new(w, Some(goalie_weights.cloned().unwrap_or_default()));
    let mut errs: Vec<f64> = Vec::new();
    for p in players {
        for line in fit_lines(p) {
            if !scoring.weights.keys().any(|k| line.stats.contains_key(k)) {
                continue;
            }
            errs.push((scoring.value(&line.per_game()) - line.stats[FPTS_KEY] / line.gp as f64).abs());
            break;
        }
    }
    let mae = if errs.is_empty() { f64::NAN } else { py_sum(errs.iter().copied()) / errs.len() as f64 };
    (mae, errs.len() as i64)
}

/// Mean absolute error (FP/G) of the weights against Fantrax's own FPts; NaN if nothing to compare.
pub fn validate_weights<'a, I>(weights: &Map<String, f64>, players: I) -> f64
where
    I: IntoIterator<Item = &'a Player>,
{
    weights_check(weights, players, None).0
}

fn itemized(p: &Player, weights: &Map<String, f64>) -> bool {
    fit_lines(p).iter().any(|line| weights.keys().any(|k| k != FPTS_KEY && line.stats.contains_key(k)))
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitResult {
    /// merged (skater weights + goalie-only stats)
    pub weights: Map<String, f64>,
    /// mean |error| in FP/G of the rounded weights
    pub mae: f64,
    /// lines fitted
    pub n: i64,
    /// worst |error| in FP/G
    pub residual_max: f64,
    /// goalie values differing from skaters
    pub goalie_weights: Map<String, f64>,
    pub groups: Map<String, Map<String, f64>>,
}

impl FitResult {
    pub fn points_line(&self) -> String {
        format!("FANTRAX_POINTS={}", format_points(&self.weights, Some(&self.goalie_weights)))
    }

    /// The JSON `fm settings --fit-points --json` prints under "fit".
    pub fn to_json(&self) -> Value {
        let nan = |x: f64| if x.is_nan() { Value::Null } else { Value::from(x) };
        let wmap = |m: &Map<String, f64>| Value::Object(m.iter().map(|(k, v)| (k.clone(), Value::from(*v))).collect());
        let mut o = serde_json::Map::new();
        o.insert("weights".into(), wmap(&self.weights));
        o.insert("goalie_weights".into(), wmap(&self.goalie_weights));
        o.insert("groups".into(), Value::Object(self.groups.iter().map(|(k, v)| (k.clone(), wmap(v))).collect()));
        o.insert("mae".into(), nan(self.mae));
        o.insert("n".into(), Value::from(self.n));
        o.insert("residual_max".into(), nan(self.residual_max));
        o.insert("points_line".into(), Value::String(self.points_line()));
        Value::Object(o)
    }
}

/// Least squares; columns that are all zero or a linear combination of earlier columns are
/// dropped first (Gram-Schmidt) and get weight 0.
pub fn lstsq(rows: &[Vec<f64>], y: &[f64], ncols: usize) -> Vec<f64> {
    let cols: Vec<Vec<f64>> = (0..ncols).map(|j| rows.iter().map(|r| r[j]).collect()).collect();
    let mut basis: Vec<Vec<f64>> = Vec::new();
    let mut keep: Vec<usize> = Vec::new();
    for (j, c) in cols.iter().enumerate() {
        let mut v = c.clone();
        for b in &basis {
            let d = py_sum(v.iter().zip(b).map(|(x, z)| x * z));
            v = v.iter().zip(b).map(|(x, z)| x - d * z).collect();
        }
        let norm = py_sum(v.iter().map(|x| x * x)).sqrt();
        let scale = py_sum(c.iter().map(|x| x * x)).sqrt();
        if norm > 1e-12 && norm > 1e-9 * scale {
            basis.push(v.iter().map(|x| x / norm).collect());
            keep.push(j);
        }
    }
    let mut out = vec![0.0; ncols];
    if keep.is_empty() {
        return out;
    }
    let x: Vec<Vec<f64>> = rows.iter().map(|r| keep.iter().map(|&j| r[j]).collect()).collect();
    for (j, w) in keep.iter().zip(householder_lstsq(x, y.to_vec())) {
        out[*j] = w;
    }
    out
}

/// min ||A x - b|| for a full-column-rank A (n x k, n >= k) by Householder QR.
fn householder_lstsq(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Vec<f64> {
    let n = a.len();
    let k = a.first().map_or(0, |r| r.len());
    for j in 0..k.min(n) {
        let norm = a[j..].iter().map(|r| r[j] * r[j]).sum::<f64>().sqrt();
        if norm == 0.0 {
            continue;
        }
        let alpha = if a[j][j] > 0.0 { -norm } else { norm };
        let mut v: Vec<f64> = a[j..].iter().map(|r| r[j]).collect();
        v[0] -= alpha;
        let vnorm2: f64 = v.iter().map(|x| x * x).sum();
        if vnorm2 == 0.0 {
            continue;
        }
        for c in j..k {
            let dot: f64 = v.iter().zip(&a[j..]).map(|(vi, r)| vi * r[c]).sum();
            let f = 2.0 * dot / vnorm2;
            for (vi, r) in v.iter().zip(a[j..].iter_mut()) {
                r[c] -= f * vi;
            }
        }
        let dot: f64 = v.iter().zip(&b[j..]).map(|(vi, bi)| vi * bi).sum();
        let f = 2.0 * dot / vnorm2;
        for (vi, bi) in v.iter().zip(b[j..].iter_mut()) {
            *bi -= f * vi;
        }
    }
    let mut x = vec![0.0; k];
    for i in (0..k.min(n)).rev() {
        if a[i][i].abs() < 1e-300 {
            continue;
        }
        let s: f64 = (i + 1..k).map(|c| a[i][c] * x[c]).sum();
        x[i] = (b[i] - s) / a[i][i];
    }
    x
}

fn fit_group(lines: &[&StatLine]) -> Map<String, f64> {
    let mut present: BTreeSet<&str> = BTreeSet::new();
    for ln in lines {
        for (k, v) in &ln.stats {
            if !FIT_EXCLUDE.contains(&k.as_str()) && *v != 0.0 {
                present.insert(k.as_str());
            }
        }
    }
    let mut order: Vec<&str> =
        canonical_stats_order().into_iter().filter(|k| present.contains(k) && !FIT_LAST.contains(k)).collect();
    let extra: Vec<&str> = present.iter().copied().filter(|k| !order.contains(k) && !FIT_LAST.contains(k)).collect();
    order.extend(extra);
    order.extend(FIT_LAST.iter().copied().filter(|k| present.contains(k)));
    let rows: Vec<Vec<f64>> = lines.iter().map(|ln| order.iter().map(|k| ln.stats.get(*k).copied().unwrap_or(0.0)).collect()).collect();
    let y: Vec<f64> = lines.iter().map(|ln| ln.stats[FPTS_KEY]).collect();
    let mut out = Map::new();
    for (k, v) in order.iter().zip(lstsq(&rows, &y, order.len())) {
        let v = round_to(v, 2);
        if v.abs() >= 0.05 {
            out.insert(k.to_string(), v);
        }
    }
    out
}

/// Least-squares fit of Fantrax FPts totals on per-stat totals (skaters and goalies separately).
pub fn fit_weights<'a, I>(players: I) -> FitResult
where
    I: IntoIterator<Item = &'a Player>,
{
    let mut skaters: Vec<&StatLine> = Vec::new();
    let mut goalies: Vec<&StatLine> = Vec::new();
    for p in players {
        for ln in fit_lines(p) {
            if ln.stats.iter().any(|(k, v)| !FIT_EXCLUDE.contains(&k.as_str()) && *v != 0.0) {
                if p.is_goalie() {
                    goalies.push(ln);
                } else {
                    skaters.push(ln);
                }
            }
        }
    }
    let mut fitted: Map<String, Map<String, f64>> = Map::new();
    fitted.insert("skaters".into(), if skaters.is_empty() { Map::new() } else { fit_group(&skaters) });
    fitted.insert("goalies".into(), if goalies.is_empty() { Map::new() } else { fit_group(&goalies) });
    let mut base = fitted["skaters"].clone();
    let mut goalie = Map::new();
    for (k, v) in &fitted["goalies"] {
        match base.get(k) {
            None => {
                base.insert(k.clone(), *v);
            }
            Some(b) if b != v => {
                goalie.insert(k.clone(), *v);
            }
            _ => {}
        }
    }
    let scoring = PointsScoring::new(base.clone(), Some(goalie.clone()));
    let errs: Vec<f64> = skaters
        .iter()
        .chain(goalies.iter())
        .map(|ln| (scoring.value(&ln.per_game()) - ln.stats[FPTS_KEY] / ln.gp as f64).abs())
        .collect();
    let mae = if errs.is_empty() { f64::NAN } else { py_sum(errs.iter().copied()) / errs.len() as f64 };
    let residual_max = errs.iter().copied().fold(None, |acc: Option<f64>, x| Some(acc.map_or(x, |a| if x > a { x } else { a })));
    FitResult {
        weights: base,
        mae,
        n: errs.len() as i64,
        residual_max: residual_max.unwrap_or(f64::NAN),
        goalie_weights: goalie,
        groups: fitted,
    }
}

static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(20\d\d)").expect("regex"));
static SEASON_SPAN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"20\d\d\s*[-/]\s*(\d\d)\b").expect("regex"));

/// Season labelled by the year it ends (2026-27 -> 2027), like the ESPN provider.
pub fn season_from_settings(fs: &Value, today: NaiveDate) -> i64 {
    if let Some(Value::Number(n)) = jget(jobj(fs, "season"), "endDate") {
        if let Some(end) = n.as_f64().filter(|e| *e > 0.0) {
            let secs = end / 1e3;
            let whole = secs.floor();
            let nanos = ((secs - whole) * 1e9).round() as u32;
            if let Some(dt) = Local.timestamp_opt(whole as i64, nanos.min(999_999_999)).earliest() {
                return dt.year() as i64;
            }
        }
    }
    let subtitle = jget(fs, "subtitle").map(py_str).filter(|_| jget_t(fs, "subtitle").is_some()).unwrap_or_default();
    let years: Vec<i64> = YEAR_RE.captures_iter(&subtitle).filter_map(|c| c[1].parse().ok()).collect();
    if let Some(y) = years.iter().max() {
        if let Some(m) = SEASON_SPAN_RE.captures(&subtitle) {
            return 2000 + m[1].parse::<i64>().unwrap_or(0);
        }
        return *y;
    }
    default_espn_year(today)
}

/// Collect scalar settings whose key path mentions roster/dynasty keywords.
pub fn find_rule_settings(obj: &Value, path: &str, out: &mut Map<String, Value>) {
    if out.len() >= MAX_RULE_LINES * 3 {
        return;
    }
    match obj {
        Value::Object(m) => {
            for (k, v) in m {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                if v.is_object() || v.is_array() {
                    find_rule_settings(v, &p, out);
                } else if RULE_KEYWORDS.is_match(k)
                    && !v.is_null()
                    && v.as_str() != Some("")
                    && !v.as_str().is_some_and(|s| fm_pycompat::text::py_len(s) > 200)
                {
                    out.insert(p, v.clone());
                }
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().take(50).enumerate() {
                find_rule_settings(v, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

/// `analysis.schedule_grid.parse_fantrax_period`: "(Mar 22/27 - Mar 28/27)" -> dates.
fn parse_fantrax_period(name: &str) -> Option<(NaiveDate, NaiveDate)> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\(?\s*([A-Za-z]{3} \d{1,2}/\d{2})\s*-\s*([A-Za-z]{3} \d{1,2}/\d{2})\s*\)?").expect("regex")
    });
    let m = RE.captures(name)?;
    let a = fm_pycompat::time::strptime(&m[1], "%b %d/%y")?.date();
    let b = fm_pycompat::time::strptime(&m[2], "%b %d/%y")?.date();
    Some((a, b))
}

/// `analysis.schedule_grid.fantrax_periods`: scoringPeriodList -> (number, start, end) sorted by number.
fn fantrax_periods(list: &[Value]) -> Vec<(i64, NaiveDate, NaiveDate)> {
    let mut out = Vec::new();
    for item in list {
        let Some(num) = jget(item, "value").and_then(py_int) else { continue };
        let name = jget_t(item, "name").map(py_str).unwrap_or_default();
        let Some((a, b)) = parse_fantrax_period(&name) else { continue };
        if num >= 9999 {
            continue;
        }
        out.push((num, a, b));
    }
    out.sort_by_key(|p| p.0);
    out
}

pub const LINEUP_RULE: &str = "Lineup changes are executed";

/// "daily" or "weekly" from the Rules page's "Lineup changes are executed" value; unknown /
/// missing -> "weekly" (the conservative assumption).
pub fn parse_lineup_lock(text: Option<&str>) -> &'static str {
    let t = py_lower(&fm_pycompat::text::split_ws(text.unwrap_or("")).join(" "));
    if t.contains("dai") {
        "daily"
    } else {
        "weekly"
    }
}

// ------------------------------------------------------------------ provider

/// Loads a Fantrax league into a LeagueContext. After `load()`: `fantasy_points`, `ages`,
/// `rules`, `draft_picks`, `standings`, `trade_blocks`, `pending_trades`, `transactions` and the
/// warnings are populated (also mirrored in [`LeagueProvider::extras`]).
pub struct FantraxProvider {
    pub settings: Settings,
    pub fetch_free_agents: bool,
    pub today: Option<NaiveDate>,
    cache_offline: bool,
    session: Mutex<Option<Box<dyn FxSession>>>,
    auth: Mutex<Option<FantraxAuth>>,
    relogged: AtomicBool,
    tape: Option<Tape>,
    warnings: Mutex<Vec<String>>,
    pub fantasy_points: FantasyPoints,
    pub ages: Map<String, f64>,
    pub standings: Map<String, Map<String, Value>>,
    pub trade_blocks: Vec<TradeBlockInfo>,
    pub pending_trades: Vec<PendingTradeInfo>,
    pub transactions: Vec<TransactionInfo>,
    pub weights_mae: f64,
    pub weights_checked: i64,
    pub scoring_source: String,
    raw_info: Value,
    limits: Map<String, (i64, i64)>,
    ctx: Option<LeagueContext>,
    fa_ids: HashSet<String>,
    client: Mutex<Option<FxpaClient>>,
    requests_made: AtomicI64,
    /// parsed Rules page (getLeagueRulesOld)
    pub rules: Option<LeagueRules>,
    /// my picks by year
    pub draft_picks: Map<i64, Vec<DraftPickRef>>,
    /// every team's, by team id
    pub team_draft_picks: Map<String, Map<i64, Vec<DraftPickRef>>>,
    /// FANTRAX_POINTS vs Fantrax FPts
    pub config_mae: f64,
    pub config_checked: i64,
    codes: Codes,
    misc: Map<String, Value>,
    teams_meta: Map<String, TeamMeta>,
    my_id: Option<String>,
    extras: ProviderExtras,
}

impl FantraxProvider {
    /// `FantraxProvider(settings, cache)`; `cache_offline` = `cache.offline`.
    pub fn new(settings: Settings, cache_offline: bool) -> Self {
        FantraxProvider {
            settings,
            fetch_free_agents: true,
            today: None,
            cache_offline,
            session: Mutex::new(None),
            auth: Mutex::new(None),
            relogged: AtomicBool::new(false),
            tape: None,
            warnings: Mutex::new(Vec::new()),
            fantasy_points: Map::new(),
            ages: Map::new(),
            standings: Map::new(),
            trade_blocks: Vec::new(),
            pending_trades: Vec::new(),
            transactions: Vec::new(),
            weights_mae: f64::NAN,
            weights_checked: 0,
            scoring_source: String::new(),
            raw_info: Value::Object(Default::default()),
            limits: Map::new(),
            ctx: None,
            fa_ids: HashSet::new(),
            client: Mutex::new(None),
            requests_made: AtomicI64::new(0),
            rules: None,
            draft_picks: Map::new(),
            team_draft_picks: Map::new(),
            config_mae: f64::NAN,
            config_checked: 0,
            codes: Codes::new(),
            misc: Map::new(),
            teams_meta: Map::new(),
            my_id: None,
            extras: ProviderExtras { kind: "FantraxProvider".into(), ..Default::default() },
        }
    }

    /// Replay every Fantrax request from a parity tape (no session, no network, no file cache).
    pub fn from_tape(settings: Settings, tape: Tape, today: NaiveDate) -> Self {
        let mut p = FantraxProvider::new(settings, true);
        p.tape = Some(tape);
        p.today = Some(today);
        p
    }

    pub fn with_session(mut self, session: Box<dyn FxSession>) -> Self {
        *self.session.get_mut().unwrap_or_else(|e| e.into_inner()) = Some(session);
        self
    }

    pub fn with_auth(mut self, auth: FantraxAuth) -> Self {
        *self.auth.get_mut().unwrap_or_else(|e| e.into_inner()) = Some(auth);
        self
    }

    pub fn with_today(mut self, today: NaiveDate) -> Self {
        self.today = Some(today);
        self
    }

    pub fn with_tape(mut self, tape: Tape) -> Self {
        self.tape = Some(tape);
        self
    }

    pub fn with_free_agents(mut self, fetch: bool) -> Self {
        self.fetch_free_agents = fetch;
        self
    }

    // -- plumbing ----------------------------------------------------------
    pub fn offline(&self) -> bool {
        self.settings.fm_offline || self.cache_offline
    }

    fn effective_today(&self) -> NaiveDate {
        self.today.unwrap_or_else(local_today)
    }

    /// Every warning so far (load + later calls).
    pub fn all_warnings(&self) -> Vec<String> {
        lock(&self.warnings).clone()
    }

    fn warn(&self, msg: String) {
        lock(&self.warnings).push(msg);
    }

    /// Run `f` with the FantraxAuth (created from the settings on first use).
    pub fn with_auth_mut<R>(&self, f: impl FnOnce(&mut FantraxAuth) -> R) -> R {
        let mut g = lock(&self.auth);
        let auth = g.get_or_insert_with(|| FantraxAuth::new(self.settings.clone()));
        f(auth)
    }

    fn league_id(&self) -> String {
        self.settings.fantrax_league_id.clone().unwrap_or_default()
    }

    fn ensure_client(&self) -> anyhow::Result<()> {
        let mut g = lock(&self.client);
        if g.is_some() {
            return Ok(());
        }
        let lid = self.settings.fantrax_league_id.clone().filter(|s| !s.is_empty()).ok_or_else(|| {
            provider_err(
                "FANTRAX_LEAGUE_ID is not set. It is the id in your league URL: fantrax.com/fantasy/league/<FANTRAX_LEAGUE_ID>/home.",
            )
        })?;
        let mut session = lock(&self.session).take();
        if session.is_none() && self.tape.is_none() {
            session = Some(if self.offline() {
                match self.with_auth_mut(|a| a.ensure_session(false)) {
                    Ok(s) => s,
                    Err(e) if e.downcast_ref::<ProviderError>().is_some() => Box::new(build_session(&Map::new())),
                    Err(e) => return Err(e),
                }
            } else {
                self.with_auth_mut(|a| a.ensure_session(true))?
            });
        }
        *g = Some(FxpaClient::new(&lid, session, Some(&self.settings.fm_data_dir), self.offline(), self.tape.clone()));
        Ok(())
    }

    /// FxpaClient hook for WARNING_NOT_LOGGED_IN: at most one recovery per provider.
    fn relogin(&self) -> anyhow::Result<Option<Box<dyn FxSession>>> {
        if self.relogged.swap(true, AtomicOrdering::SeqCst) {
            return Ok(None);
        }
        let (session, source) = self.with_auth_mut(|a| a.recover().map(|s| (s, a.source.clone())))?;
        if session.is_some() && source.as_deref() == Some("login") {
            let msg = "Fantrax session refreshed by login".to_string();
            let mut w = lock(&self.warnings);
            if !w.contains(&msg) {
                w.push(msg);
            }
        }
        Ok(session)
    }

    /// `provider._get_client().call(*calls, fresh=)`: raw client errors (FantraxError, CacheMiss,
    /// RequestError, LoginFailed from a relogin).
    pub fn client_call(&self, calls: &[FxCall], fresh: bool) -> anyhow::Result<Vec<Value>> {
        self.ensure_client()?;
        let offline = self.offline();
        let mut g = lock(&self.client);
        let client = g.as_mut().expect("client");
        let before = client.requests_made;
        let mut relogin = || self.relogin();
        let res = client.call(calls, fresh, if offline { None } else { Some(&mut relogin) });
        self.requests_made.fetch_add(client.requests_made - before, AtomicOrdering::SeqCst);
        res
    }

    /// Fantrax requests sent over the network so far.
    pub fn requests_made(&self) -> i64 {
        self.requests_made.load(AtomicOrdering::SeqCst)
    }

    /// `_call`: calls that must succeed; Fantrax errors become ProviderError with advice.
    pub fn call(&self, calls: &[FxCall], fresh: bool) -> anyhow::Result<Vec<Value>> {
        let e = match self.client_call(calls, fresh) {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        let lid = self.league_id();
        if let Some(f) = e.downcast_ref::<FantraxError>() {
            return Err(match f {
                FantraxError::NotLoggedIn => {
                    let login = self.relogged.load(AtomicOrdering::SeqCst)
                        && self.with_auth_mut(|a| a.source.as_deref() == Some("login"));
                    if login {
                        provider_err(
                            "Fantrax accepted the login (FANTRAX_USERNAME / FANTRAX_PASSWORD) but still says you are not logged in. \
                             Use a browser cookie instead: log in at fantrax.com, open DevTools > Network, click any 'req?leagueId=' \
                             request and copy the whole Cookie request header into FANTRAX_COOKIE. See 'Fantrax setup' in the README.",
                        )
                    } else {
                        provider_err(
                            "Fantrax says you are not logged in: the saved session or FANTRAX_COOKIE is missing, expired or \
                             incomplete. Recommended: set FANTRAX_USERNAME and FANTRAX_PASSWORD in .env so fm logs in itself and \
                             refreshes the session. Or log in at fantrax.com, open DevTools > Network, click any 'req?leagueId=' \
                             request and copy the whole Cookie request header into FANTRAX_COOKIE (or save it to the file named by \
                             FANTRAX_COOKIE_FILE). See 'Fantrax setup' in the README.",
                        )
                    }
                }
                FantraxError::NotMemberOfLeague => provider_err(format!(
                    "The logged-in Fantrax account is not a member of league {lid}. Check FANTRAX_LEAGUE_ID and that the cookie belongs to your account."
                )),
                FantraxError::Other(msg) => provider_err(format!(
                    "Fantrax request failed for league {lid}: {msg}. If this persists, run `fm auth fantrax --login` (or refresh FANTRAX_COOKIE) and check FANTRAX_LEAGUE_ID."
                )),
            });
        }
        if let Some(c) = e.downcast_ref::<CacheMiss>() {
            return Err(provider_err(format!("Offline mode and Fantrax data not cached: {c}")));
        }
        if let Some(r) = e.downcast_ref::<RequestError>() {
            return Err(provider_err(format!("Could not reach Fantrax: {}", r.kind)));
        }
        Err(e)
    }

    /// `_try`: best-effort calls, failures are recorded in the warnings.
    fn try_call(&self, label: &str, calls: &[FxCall]) -> Option<Vec<Value>> {
        match self.call(calls, false) {
            Ok(v) => Some(v),
            Err(e) => {
                if e.downcast_ref::<ProviderError>().is_some() {
                    self.warn(format!("{label} unavailable: {e}"));
                } else {
                    self.warn(format!("{label} unavailable: {}: {e}", error_type_name(&e)));
                }
                None
            }
        }
    }

    // -- public API --------------------------------------------------------
    pub fn load(&mut self) -> anyhow::Result<LeagueContext> {
        if let Some(ctx) = &self.ctx {
            return Ok(ctx.clone());
        }
        let res = self.load_inner();
        self.sync_extras();
        res
    }

    fn load_inner(&mut self) -> anyhow::Result<LeagueContext> {
        let today = self.effective_today();
        let r = self.call(&[fx_call("getFantasyLeagueInfo", &[]), fx_call("getTeamRosterInfo", &[("view", "STATS")])], false)?;
        let (info, my_roster_raw) = (r[0].clone(), r[1].clone());
        self.raw_info = info.clone();
        self.codes = timeframe_codes(&my_roster_raw);
        self.draft_picks = parse_draft_picks(&my_roster_raw);
        self.misc = jobj(&my_roster_raw, "miscData").as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
        let fs = jobj(&info, "fantasySettings").clone();
        let positions = parse_positions(jget(&info, "positionMap"));
        let mut teams_meta = parse_teams(&my_roster_raw);
        if teams_meta.is_empty() {
            teams_meta = parse_teams(&info);
        }
        if teams_meta.is_empty() {
            return Err(provider_err("Fantrax returned no teams for this league. Check FANTRAX_LEAGUE_ID."));
        }
        let my_id = self.my_team_id(&teams_meta, jlist(&my_roster_raw, "myTeamIds"))?;
        self.teams_meta = teams_meta.clone();
        self.my_id = Some(my_id.clone());

        let team_ids: Vec<String> = teams_meta.keys().cloned().collect();
        let parsed_rosters = self.rosters(&team_ids, &positions)?;

        if let Some(st) = self.try_call("Standings", &[fx_call("getStandings", &[])]) {
            if !st.is_empty() {
                self.standings = parse_standings(&st[0]);
            }
        }

        let mut teams: Vec<FantasyTeam> = Vec::new();
        let mut slot_counts: Map<String, i64> = Map::new();
        for (tid, parsed) in team_ids.iter().zip(parsed_rosters) {
            for (k, v) in &parsed.fantasy_points {
                self.fantasy_points.insert(k.clone(), *v);
            }
            for (k, v) in &parsed.ages {
                self.ages.insert(k.clone(), *v);
            }
            if *tid == my_id {
                slot_counts = parsed.slot_counts.clone();
                self.limits = parsed.limits.clone();
            }
            let record = self.standings.get(tid).map(|st| {
                let g = |k: &str| st.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
                (g("win"), g("loss"), g("tie"))
            });
            teams.push(FantasyTeam {
                team_id: tid.clone(),
                name: teams_meta[tid].name.clone(),
                owner_is_me: *tid == my_id,
                slots: parsed.slots,
                record,
            });
        }

        let rostered: HashSet<String> = teams.iter().flat_map(|t| t.players_iter().map(|p| p.cid.clone())).collect();
        if self.fetch_free_agents {
            self.rostered_ownership(&mut teams, &positions);
        }
        let free_agents = if self.fetch_free_agents { self.free_agents(&rostered, &positions) } else { Vec::new() };
        self.fa_ids = free_agents.iter().map(|p| p.cid.clone()).collect();
        if !self.fetch_free_agents {
            self.warn("Free-agent pool disabled; replacement level uses rostered players.".into());
        }

        self.load_extras(&positions, &teams_meta);
        self.load_rules();
        let all_players: Vec<&Player> = teams.iter().flat_map(|t| t.players_iter()).chain(free_agents.iter()).collect();
        let scoring = self.scoring(&all_players);

        let rules = self.rules.as_ref();
        let mut draft_picks = self.team_draft_picks.clone();
        if !self.draft_picks.is_empty() {
            draft_picks.insert(my_id.clone(), self.draft_picks.clone());
        }
        let lid = self.league_id();
        let ctx = LeagueContext {
            provider: "fantrax".into(),
            league_id: lid.clone(),
            season: season_from_settings(&fs, today),
            name: jget_t(&fs, "leagueName").map(py_str).unwrap_or_else(|| format!("Fantrax {lid}")),
            scoring,
            roster_shape: self.roster_shape_for(&slot_counts),
            teams,
            position_limits: rules_position_limits(rules),
            max_roster_size: rules_max_roster(rules),
            stat_limits: rules_stat_limits(rules),
            free_agents,
            matchup_period: None,
            dynasty: self.settings.fantrax_dynasty || self.rules_dynasty(),
            keeper_horizon_years: self.settings.fantrax_keeper_horizon_years,
            keepers_per_team: self.settings.fantrax_keepers,
            dynasty_mode: self.settings.fantrax_mode,
            dynasty_mode_source: None,
            as_of: today,
            schedule: Map::new(),
            games_per_day: Map::new(),
            opponents: Map::new(),
            game_starts: Map::new(),
            season_start: None,
            lineup_lock: Some(if self.lineup_lock() == "daily" { LineupLock::Daily } else { LineupLock::Weekly }),
            goalie_actual_starts: Map::new(),
            b2b_second_night: Map::new(),
            nhl_goalies: Map::new(),
            deployment_details: Map::new(),
            moves_limit_per_period: None,
            moves_used_this_period: None,
            moves_period_label: None,
            moves_limit_season: None,
            moves_used_season: None,
            period_start: None,
            period_end: None,
            recent_adds: Map::new(),
            faab_remaining: None,
            draft_picks,
            source_notes: Vec::new(),
            warnings: Vec::new(),
            sources: Vec::new(),
            enrich: Default::default(),
        };
        self.ctx = Some(ctx);
        self.apply_budget(&my_id);
        Ok(self.ctx.clone().expect("ctx"))
    }

    /// The scoring period containing `today` (Fantrax's periods when `need_exact`, else Mon-Sun).
    fn budget_period(&self, today: NaiveDate, need_exact: bool) -> (NaiveDate, NaiveDate) {
        if need_exact {
            if let Some(r) = self.try_call("Fantrax scoring periods", &[fx_call("getTeamRosterInfo", &[("view", "GAMES_PER_POS")])]) {
                if let Some(first) = r.first() {
                    let lst = jlist(jobj(first, "displayedLists"), "scoringPeriodList");
                    for (_, start, end) in fantrax_periods(lst) {
                        if start <= today && today <= end {
                            return (start, end);
                        }
                    }
                }
            }
        }
        let start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
        (start, start + Duration::days(6))
    }

    /// Claim limits from the Rules page, my claims this scoring period and in the last 14 days.
    fn apply_budget(&mut self, my_id: &str) {
        let Some(mut ctx) = self.ctx.take() else { return };
        let (season_cap, week_cap) = rules_claim_limits(self.rules.as_ref());
        ctx.moves_limit_per_period = week_cap;
        ctx.moves_limit_season = season_cap;
        ctx.moves_period_label = Some("week".into());
        let (ps, pe) = self.budget_period(ctx.as_of, week_cap.is_some());
        ctx.period_start = Some(ps);
        ctx.period_end = Some(pe);
        let names: Map<String, String> = ctx.teams.iter().map(|t| (t.team_id.clone(), t.name.clone())).collect();
        let items = activity_from_transactions(&self.transactions, &names, Some(season_start_year_for(ctx.as_of)));
        ctx.recent_adds = fm_core::activity::recent_adds_from(&items, my_id, ctx.as_of, fm_core::activity::RECENT_ADDS_WINDOW);
        ctx.moves_used_this_period = Some(fm_core::activity::count_adds(&items, my_id, Some(ps), Some(pe)) as i64);
        if season_cap.is_some() {
            let y0 = season_start_year_for(ctx.as_of);
            let start = NaiveDate::from_ymd_opt(y0 as i32, 8, 1);
            ctx.moves_used_season = Some(fm_core::activity::count_adds(&items, my_id, start, None) as i64);
            if self.transactions.len() >= 50 {
                ctx.warnings.push("Fantrax claims this season are counted from the last 50 league transactions only".into());
            }
        }
        self.ctx = Some(ctx);
    }

    /// Every team's roster, with season-to-date and full-season projection lines.
    fn rosters(&mut self, team_ids: &[String], positions: &Map<String, String>) -> anyhow::Result<Vec<ParsedRoster>> {
        let today = Some(self.effective_today());
        let views: Vec<(String, Map<String, String>)> =
            SPLIT_TIMEFRAMES.iter().filter_map(|s| self.codes.get(*s).map(|c| (s.to_string(), c.clone()))).collect();
        let call_for = |tid: &str, params: Option<&Map<String, String>>| -> FxCall {
            let mut d: Map<String, Value> = Map::new();
            d.insert("view".into(), Value::String("STATS".into()));
            d.insert("teamId".into(), Value::String(tid.to_string()));
            if let Some(p) = params {
                for (k, v) in p {
                    d.insert(k.clone(), Value::String(v.clone()));
                }
            }
            ("getTeamRosterInfo".to_string(), d)
        };
        if views.is_empty() {
            let calls: Vec<FxCall> = team_ids.iter().map(|t| call_for(t, None)).collect();
            let raw = self.call(&calls, false)?;
            self.record_draft_picks(team_ids, Some(&raw));
            return Ok(raw.iter().map(|r| parse_roster(r, positions, None, today)).collect());
        }
        let mut per_team: Vec<Vec<ParsedRoster>> = team_ids.iter().map(|_| Vec::new()).collect();
        for (i, (split, params)) in views.iter().enumerate() {
            let calls: Vec<FxCall> = team_ids.iter().map(|t| call_for(t, Some(params))).collect();
            let raw = if i == 0 { Some(self.call(&calls, false)?) } else { self.try_call(&format!("Fantrax {split} roster stats"), &calls) };
            if i == 0 {
                self.record_draft_picks(team_ids, raw.as_deref());
            }
            for (j, r) in raw.iter().flatten().enumerate() {
                if let Some(pt) = per_team.get_mut(j) {
                    pt.push(parse_roster(r, positions, Some(&self.codes), today));
                }
            }
        }
        Ok(per_team.into_iter().map(merge_rosters).collect())
    }

    /// Each team's draft picks from its roster response (draftPicksData), by team id.
    fn record_draft_picks(&mut self, team_ids: &[String], raw: Option<&[Value]>) {
        for (tid, r) in team_ids.iter().zip(raw.unwrap_or(&[])) {
            if r.is_object() {
                let picks = parse_draft_picks(r);
                if !picks.is_empty() {
                    self.team_draft_picks.insert(tid.clone(), picks);
                }
            }
        }
    }

    /// Starting slots from my roster, bench/IR/minors from status limits; the Rules page wins.
    fn roster_shape_for(&self, slot_counts: &Map<String, i64>) -> Map<String, i64> {
        let mut shape = roster_shape(slot_counts, &self.limits);
        let Some(r) = self.rules.as_ref().filter(|r| !rules_empty(r)) else { return shape };
        let pos_max: Vec<(&String, i64)> = r
            .positions
            .iter()
            .filter_map(|(p, v)| v.get("max_active").copied().flatten().filter(|n| *n != 0).map(|n| (p, n)))
            .collect();
        if !pos_max.is_empty() {
            shape.retain(|k, _| NON_STARTING.contains(&k.as_str()));
            for (p, n) in pos_max {
                shape.insert(canonical_slot(p), n);
            }
        }
        for (label, slot) in
            [("Maximum Reserve Players", "BN"), ("Maximum Injury Reserve Players", "IR"), ("Maximum Minor League Players", "MIN")]
        {
            if r.get(label).is_none() {
                continue;
            }
            match r.get_int(label) {
                Some(n) if n > 0 => {
                    shape.insert(slot.to_string(), n);
                }
                _ => {
                    shape.shift_remove(slot); // "Not Used"
                }
            }
        }
        shape
    }

    /// "daily" or "weekly", parsed from the Rules page ("weekly" when unavailable).
    pub fn lineup_lock(&self) -> &'static str {
        parse_lineup_lock(self.rules.as_ref().and_then(|r| r.get(LINEUP_RULE)))
    }

    pub fn rules_dynasty(&self) -> bool {
        self.rules.as_ref().is_some_and(|r| py_lower(r.get("Keeper league Type").unwrap_or("")).contains("dynasty"))
    }

    fn my_team_id(&self, teams: &Map<String, TeamMeta>, my_ids: &[Value]) -> anyhow::Result<String> {
        let hint_raw = self.settings.fantrax_team.clone().unwrap_or_default();
        let hint = py_strip(&hint_raw).to_string();
        if !hint.is_empty() {
            if teams.contains_key(&hint) {
                return Ok(hint);
            }
            let low = py_lower(&hint);
            for (tid, t) in teams {
                if low == py_lower(&t.short) || py_lower(&t.name).contains(&low) {
                    return Ok(tid.clone());
                }
            }
        } else {
            for tid in my_ids {
                let t = py_str(tid);
                if teams.contains_key(&t) {
                    return Ok(t);
                }
            }
        }
        let listing = teams.iter().map(|(tid, t)| format!("{tid}={}", t.name)).collect::<Vec<_>>().join(", ");
        let what = if hint.is_empty() {
            "Could not identify your Fantrax team".to_string()
        } else {
            format!("FANTRAX_TEAM={} matches no team", py_repr(&Value::String(hint.clone())))
        };
        Err(provider_err(format!("{what}. Set FANTRAX_TEAM to one of: {listing}")))
    }

    fn load_rules(&mut self) {
        let Some(r) = self.try_call("League rules", &[fx_call("getLeagueRulesOld", &[])]) else { return };
        let Some(first) = r.first() else { return };
        let content = jget_t(first, "content").map(py_str).unwrap_or_default();
        self.rules = Some(parse_league_rules(&content));
    }

    /// Point values: the league's Rules page, else FANTRAX_POINTS, else Fantrax's own FP/G.
    fn scoring(&mut self, players: &[&Player]) -> ScoringConfig {
        let (cfg_base, cfg_goalie) = split_points(&self.settings.fantrax_points);
        if !cfg_base.is_empty() {
            let (m, n) = weights_check(&cfg_base, players.iter().copied(), Some(&cfg_goalie));
            self.config_mae = m;
            self.config_checked = n;
        }
        let has_fpts = players.iter().any(|p| !fit_lines(p).is_empty());
        let mut weights = Map::new();
        let mut goalie = Map::new();
        if let Some(rules) = self.rules.as_ref().filter(|r| !r.scoring.is_empty()) {
            let (w, g, notes) = rules_weights(rules);
            weights = w;
            goalie = g;
            for n in notes {
                self.warn(format!("Fantrax {n}"));
            }
            self.scoring_source = "Fantrax league rules".into();
        }
        if weights.is_empty() && !cfg_base.is_empty() {
            weights = cfg_base;
            goalie = cfg_goalie;
            self.scoring_source = "FANTRAX_POINTS".into();
        }
        if weights.is_empty() {
            self.scoring_source = "Fantrax FP/G (FANTRAX_POINTS not set)".into();
            self.warn(
                "FANTRAX_POINTS is not set: using Fantrax's own FP/G. Prior-season and projected stats cannot be valued; \
                 set FANTRAX_POINTS for full valuations."
                    .into(),
            );
            let mut w = Map::new();
            w.insert(FPTS_KEY.to_string(), 1.0);
            return ScoringConfig { kind: ScoringKind::Points, weights: w, categories: Vec::new(), goalie_weights: Map::new() };
        }
        let (m, n) = weights_check(&weights, players.iter().copied(), Some(&goalie));
        self.weights_mae = m;
        self.weights_checked = n;
        let fas: Vec<&&Player> = players.iter().filter(|p| self.fa_ids.contains(&p.cid) && !fit_lines(p).is_empty()).collect();
        if !fas.is_empty() && !fas.iter().any(|p| itemized(p, &weights)) {
            self.warn(
                "The Fantrax free-agent list has no per-stat columns, so free agents are valued from other stat sources only \
                 (Fantrax FP/G is in FantraxProvider.fantasy_points)."
                    .into(),
            );
        }
        if self.weights_mae.is_nan() && has_fpts {
            self.warn(format!(
                "Fantrax showed no per-stat columns, so the {} point values could not be checked against Fantrax FPts.",
                self.scoring_source
            ));
        } else if !self.weights_mae.is_nan() && self.weights_mae > 0.25 {
            self.warn(format!(
                "{} point values differ from Fantrax's FP/G by {} per game on average; run 'fm --league fantrax settings --fit-points'.",
                self.scoring_source,
                fixed(self.weights_mae, 2, false)
            ));
        }
        ScoringConfig { kind: ScoringKind::Points, weights, categories: Vec::new(), goalie_weights: goalie }
    }

    fn load_extras(&mut self, positions: &Map<String, String>, teams: &Map<String, TeamMeta>) {
        if let Some(r) = self.try_call("Trade blocks", &[fx_call("getTradeBlocks", &[])]) {
            if let Some(first) = r.first() {
                self.trade_blocks = parse_trade_blocks(jlist(first, "tradeBlocks"), positions, teams);
            }
        }
        if let Some(r) = self.try_call("Pending trades", &[fx_call("getPendingTransactions", &[])]) {
            if let Some(first) = r.first() {
                self.pending_trades = parse_pending_trades(first);
            }
        }
        if let Some(r) = self.try_call("Transactions", &[fx_call("getTransactionDetailsHistory", &[("maxResultsPerPage", "50")])]) {
            if let Some(first) = r.first() {
                self.transactions = parse_transactions(first);
            }
        }
    }

    // -- harness capture (best effort: failures become warnings) ----------------------
    fn warn_ctx(&mut self, msg: String) {
        self.warn(msg.clone());
        if let Some(ctx) = self.ctx.as_mut() {
            ctx.warnings.push(msg);
        }
    }

    fn activity_items(&self, ctx: &LeagueContext, since: Option<NaiveDate>) -> Vec<ActivityItem> {
        let names: Map<String, String> = ctx.teams.iter().map(|t| (t.team_id.clone(), t.name.clone())).collect();
        let y0 = season_start_year_for(ctx.as_of);
        let mut items = activity_from_transactions(&self.transactions, &names, Some(y0));
        let midnight = ctx.as_of.and_hms_opt(0, 0, 0);
        items.extend(activity_from_pending(&self.pending_trades, &names, Some(y0), midnight));
        items.into_iter().filter(|i| since.is_none_or(|s| i.ts.date() >= s)).collect()
    }

    /// `activity(since)`: recent transactions (last 50) plus pending trade proposals as
    /// ActivityItems, filtered to `since`. Loads the league first when needed.
    pub fn activity_load(&mut self, since: Option<NaiveDate>) -> Vec<ActivityItem> {
        let ctx = match self.load() {
            Ok(c) => c,
            Err(e) => {
                self.warn_ctx(format!("Fantrax activity unavailable: {e}"));
                self.sync_extras();
                return Vec::new();
            }
        };
        self.activity_items(&ctx, since)
    }

    /// `lineup_snapshot(day)`: every team's current lineup labelled `day` (default as_of).
    pub fn lineup_snapshot_load(&mut self, day: Option<NaiveDate>) -> Vec<LineupDay> {
        match self.load() {
            Ok(ctx) => lineup_days_from_teams(&ctx.teams, day.unwrap_or(ctx.as_of)),
            Err(e) => {
                self.warn_ctx(format!("Fantrax lineup snapshot unavailable: {e}"));
                self.sync_extras();
                Vec::new()
            }
        }
    }

    /// (label, getPlayerStats params, max pages) for the free-agent pool.
    pub fn pool_queries(&self, positions: &Map<String, String>) -> Vec<(String, Map<String, Value>, i64)> {
        let gid = positions.iter().find(|(_, short)| canonical_slot(short) == "G").map(|(pid, _)| pid.clone());
        let goalie_group = gid.map(|g| format!("POS_{g}")).unwrap_or_else(|| GOALIE_GROUP_DEFAULT.to_string());
        let groups = [("skaters", SKATER_GROUP.to_string(), FA_MAX_PAGES), ("goalies", goalie_group, FA_GOALIE_PAGES)];
        let mut timeframes: Vec<(String, Map<String, String>)> =
            ["projected", "season"].iter().filter_map(|s| self.codes.get(*s).map(|c| (s.to_string(), c.clone()))).collect();
        if timeframes.is_empty() {
            timeframes.push(("default".into(), Map::new()));
        }
        let mut out = Vec::new();
        for (split, tf) in &timeframes {
            for (label, group, pages) in &groups {
                let mut d: Map<String, Value> = Map::new();
                d.insert("statusOrTeamFilter".into(), Value::String("ALL_AVAILABLE".into()));
                d.insert("view".into(), Value::String("STATS".into()));
                d.insert("positionOrGroup".into(), Value::String(group.clone()));
                for (k, v) in tf {
                    d.insert(k.clone(), Value::String(v.clone()));
                }
                out.push((format!("{label} {split}"), d, *pages));
            }
        }
        out
    }

    /// Available players via raw getPlayerStats, merged across queries, plus {cid: {split: row}}.
    #[allow(clippy::type_complexity)]
    fn player_pool(&self, positions: &Map<String, String>) -> anyhow::Result<(Vec<Player>, Map<String, Map<String, RowStats>>)> {
        self.ensure_client()?;
        let today = Some(self.effective_today());
        let codes = if self.codes.is_empty() { None } else { Some(&self.codes) };
        let mut players: Map<String, Player> = Map::new();
        let mut rows: Map<String, Map<String, RowStats>> = Map::new();
        let mut errors: Vec<String> = Vec::new();
        let mut last_err: Option<anyhow::Error> = None;
        let mut ok = 0;
        for (label, params, max_pages) in self.pool_queries(positions) {
            let (mut page, mut pages) = (1i64, 1i64);
            let res: anyhow::Result<()> = (|| {
                while page <= pages.min(max_pages) {
                    let mut d = params.clone();
                    d.insert("maxResultsPerPage".into(), Value::String(FA_PAGE_SIZE.to_string()));
                    d.insert("pageNumber".into(), Value::String(page.to_string()));
                    let data = self.client_call(&[("getPlayerStats".to_string(), d)], false)?.remove(0);
                    let split = data_split(&data, codes);
                    let (got, got_rows, n) = parse_player_pool(&data, codes, today);
                    pages = n;
                    ok += 1;
                    let mut useful = 0;
                    for p in got {
                        if p.lines.is_empty() && split.is_some() {
                            continue;
                        }
                        useful += 1;
                        let cid = p.cid.clone();
                        if let Some(existing) = players.get_mut(&cid) {
                            merge_lines(existing, &p);
                        } else {
                            players.insert(cid.clone(), p);
                        }
                        if let Some(rs) = got_rows.get(&cid) {
                            rows.entry(cid).or_default().insert(split.clone().unwrap_or_else(|| "?".into()), rs.clone());
                        }
                    }
                    if useful == 0 {
                        break;
                    }
                    page += 1;
                }
                Ok(())
            })();
            if let Err(e) = res {
                errors.push(format!("{label}: {}: {e}", error_type_name(&e)));
                last_err = Some(e);
            }
        }
        if ok == 0 {
            return Err(last_err.unwrap_or_else(|| fm_core::CoreError::Lookup("getPlayerStats returned no available players".into()).into()));
        }
        if !errors.is_empty() {
            self.warn(format!("Some Fantrax free-agent queries failed: {}", errors.join("; ")));
        }
        if players.is_empty() {
            return Err(fm_core::CoreError::Lookup("getPlayerStats returned no available players".into()).into());
        }
        Ok((players.into_values().collect(), rows))
    }

    /// Fill `pct_owned`, `pct_owned_change` and missing ages for rostered players from
    /// getPlayerStats ALL_TAKEN. Best effort.
    fn rostered_ownership(&mut self, teams: &mut [FantasyTeam], positions: &Map<String, String>) {
        let mut by_cid: HashMap<String, (usize, usize)> = HashMap::new();
        for (ti, t) in teams.iter().enumerate() {
            for (si, sl) in t.slots.iter().enumerate() {
                if let Some(p) = &sl.player {
                    by_cid.insert(p.cid.clone(), (ti, si));
                }
            }
        }
        if by_cid.is_empty() {
            return;
        }
        let gid = positions.iter().find(|(_, short)| canonical_slot(short) == "G").map(|(pid, _)| pid.clone());
        let tf = self.codes.get("projected").or_else(|| self.codes.get("season")).cloned().unwrap_or_default();
        let mut found = 0;
        for group in [SKATER_GROUP.to_string(), gid.map(|g| format!("POS_{g}")).unwrap_or_else(|| GOALIE_GROUP_DEFAULT.to_string())] {
            let mut params: Map<String, Value> = Map::new();
            params.insert("statusOrTeamFilter".into(), Value::String("ALL_TAKEN".into()));
            params.insert("view".into(), Value::String("STATS".into()));
            params.insert("positionOrGroup".into(), Value::String(group));
            for (k, v) in &tf {
                params.insert(k.clone(), Value::String(v.clone()));
            }
            let (mut page, mut pages) = (1i64, 1i64);
            while page <= pages.min(TAKEN_MAX_PAGES) {
                let mut d = params.clone();
                d.insert("maxResultsPerPage".into(), Value::String(FA_PAGE_SIZE.to_string()));
                d.insert("pageNumber".into(), Value::String(page.to_string()));
                let Some(r) = self.try_call("Fantrax rostered-player ownership", &[("getPlayerStats".to_string(), d)]) else { break };
                let Some(first) = r.first() else { break };
                for (cid, rs) in parse_taken_rows(first) {
                    let Some(&(ti, si)) = by_cid.get(&cid) else { continue };
                    let Some(p) = teams[ti].slots[si].player.as_mut() else { continue };
                    if rs.owned.is_some() && p.pct_owned.is_none() {
                        p.pct_owned = rs.owned;
                        found += 1;
                    }
                    if rs.owned_change.is_some() && p.pct_owned_change.is_none() {
                        p.pct_owned_change = rs.owned_change;
                    }
                    if let Some(a) = rs.age {
                        self.ages.entry(cid.clone()).or_insert(a);
                    }
                }
                pages = total_pages(first);
                page += 1;
            }
        }
        if found == 0 {
            self.warn("Fantrax % rostered unavailable for rostered players (market prior uses free agents only).".into());
        }
    }

    fn free_agents(&mut self, rostered: &HashSet<String>, positions: &Map<String, String>) -> Vec<Player> {
        let (players, rows) = match self.player_pool(positions) {
            Ok(x) => x,
            Err(e) => {
                let reason = if e.downcast_ref::<CacheMiss>().is_some() {
                    "not cached (offline)".to_string()
                } else {
                    format!("{}: {e}", error_type_name(&e))
                };
                self.warn(format!(
                    "Fantrax free-agent pool unavailable ({reason}); replacement level falls back to rostered players."
                ));
                return Vec::new();
            }
        };
        let mut out: Map<String, Player> = Map::new();
        for p in players {
            if rostered.contains(&p.cid) || out.contains_key(&p.cid) {
                continue;
            }
            let empty = Map::new();
            let by_split = rows.get(&p.cid).unwrap_or(&empty);
            let fp_map: Map<String, (Option<f64>, Option<f64>)> = by_split.iter().map(|(k, v)| (k.clone(), (v.fpts, v.fpg))).collect();
            if let Some(fp) = pick_fantasy_points(&fp_map, &p) {
                self.fantasy_points.insert(p.cid.clone(), fp);
            }
            if let Some(age) = by_split.values().find_map(|v| v.age) {
                self.ages.insert(p.cid.clone(), age);
            }
            out.insert(p.cid.clone(), p);
        }
        out.into_values().collect()
    }

    /// Least-squares scoring weights fitted on every loaded player's Fantrax lines.
    pub fn fit_points_load(&mut self) -> anyhow::Result<FitResult> {
        let ctx = self.load()?;
        Ok(fit_weights(ctx.all_players()))
    }

    /// League settings as Fantrax returned them plus roster/dynasty rules (loads first).
    pub fn raw_settings_load(&mut self) -> anyhow::Result<Value> {
        self.load()?;
        Ok(self.raw_settings_value())
    }

    fn raw_settings_value(&self) -> Value {
        let fs = jobj(&self.raw_info, "fantasySettings").clone();
        let mut out = serde_json::Map::new();
        out.insert("fantasySettings".into(), fs);
        out.insert(
            "roster_limits".into(),
            Value::Object(
                self.limits
                    .iter()
                    .map(|(k, (t, m))| {
                        let mut o = serde_json::Map::new();
                        o.insert("total".into(), Value::from(*t));
                        o.insert("max".into(), Value::from(*m));
                        (k.clone(), Value::Object(o))
                    })
                    .collect(),
            ),
        );
        let mut rule_src = serde_json::Map::new();
        if let Some(o) = self.raw_info.as_object() {
            for (k, v) in o {
                if k != "positionMap" {
                    rule_src.insert(k.clone(), v.clone());
                }
            }
        }
        let mut found = Map::new();
        find_rule_settings(&Value::Object(rule_src), "", &mut found);
        out.insert("rules".into(), Value::Object(found.into_iter().collect()));
        out.insert("draft_picks".into(), serde_json::to_value(&self.draft_picks).unwrap_or(Value::Null));
        out.insert("timeframes".into(), serde_json::to_value(&self.codes).unwrap_or(Value::Null));
        for k in ["draftPickTradingAllowed", "tradesAllowed", "claimsDropsAllowed"] {
            if let Some(v) = self.misc.get(k) {
                out.insert(k.into(), v.clone());
            }
        }
        if let Some(r) = self.rules.as_ref().filter(|r| !rules_empty(r)) {
            let lr: serde_json::Map<String, Value> = r
                .settings
                .iter()
                .map(|(sect, kv)| {
                    let inner: serde_json::Map<String, Value> = kv
                        .iter()
                        .filter(|(k, v)| !RULE_SKIP_LABELS.contains(&k.as_str()) && !v.is_empty())
                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                        .collect();
                    (sect.clone(), Value::Object(inner))
                })
                .collect();
            out.insert("league_rules".into(), Value::Object(lr));
            out.insert("scoring_rules".into(), serde_json::to_value(&r.scoring).unwrap_or(Value::Null));
            out.insert("position_limits".into(), serde_json::to_value(&r.positions).unwrap_or(Value::Null));
        }
        Value::Object(out)
    }

    /// Headline roster / dynasty rules from the Rules page (empty if it was not read).
    pub fn rule_summary(&self) -> Vec<String> {
        let Some(r) = self.rules.as_ref().filter(|r| !rules_empty(r)) else { return Vec::new() };
        let mut lines = Vec::new();
        let val = |label: &str| -> Option<String> { r.get(label).filter(|v| !v.is_empty()).map(str::to_string) };
        let parts: Vec<String> = [
            ("total", "Maximum Total Players"),
            ("active", "Maximum Active Players"),
            ("reserve", "Maximum Reserve Players"),
            ("IR", "Maximum Injury Reserve Players"),
            ("minors", "Maximum Minor League Players"),
        ]
        .iter()
        .filter_map(|(lab, key)| val(key).map(|v| format!("{lab} {v}")))
        .collect();
        if !parts.is_empty() {
            lines.push(format!("Roster: max {}", parts.join(", ")));
        }
        if !r.positions.is_empty() {
            let act: Vec<String> = r
                .positions
                .iter()
                .filter_map(|(p, v)| v.get("max_active").copied().flatten().map(|n| format!("{p} {n}")))
                .collect();
            lines.push(format!("Active by position (max): {}", act.join(", ")));
            let totals: Vec<String> = r
                .positions
                .iter()
                .filter_map(|(p, v)| v.get("max").copied().flatten().filter(|n| *n != 0).map(|n| format!("{p} {n}")))
                .collect();
            lines.push(format!(
                "Whole roster by position (Total Max): {}",
                if totals.is_empty() { "no limits".to_string() } else { totals.join(", ") }
            ));
        }
        if let Some(v) = val("Keeper league Type") {
            lines.push(format!("Keeper league type: {v}"));
        }
        if let Some(v) = val("Allow trading of draft Picks") {
            let mut extra = Vec::new();
            if let Some(y) = val("Number of future years for draft pick trading") {
                extra.push(format!("{y} future years"));
            }
            if let Some(n) = val("Number of rounds available for draft pick trading") {
                extra.push(format!("{n} rounds"));
            }
            let suffix = if extra.is_empty() { String::new() } else { format!(" ({})", extra.join(", ")) };
            lines.push(format!("Draft picks tradeable: {v}{suffix}"));
        }
        if let Some(v) = val(LINEUP_RULE) {
            lines.push(format!("Lineup changes: {} (Rules page: {v})", self.lineup_lock()));
        }
        if let Some(v) = val("Playoffs will begin in this Scoring Period") {
            let teams = val("Number of teams qualifying for playoffs");
            lines.push(format!("Playoffs: start scoring period {v}{}", teams.map(|t| format!(", {t} teams")).unwrap_or_default()));
        }
        let (cs, cw) = (val(CLAIMS_SEASON_LABEL), val(CLAIMS_WEEK_LABEL));
        if cs.is_some() || cw.is_some() {
            lines.push(format!(
                "Claims: max per season {}, per week {}",
                cs.unwrap_or_else(|| "not stated".into()),
                cw.unwrap_or_else(|| "not stated".into())
            ));
        }
        for label in [
            "Draft Type",
            "# of rounds",
            "Trade Deadline Date",
            "Trade Voting System",
            "Waiver Wire claim system",
            "# of days players remain on waivers for",
        ] {
            if let Some(v) = val(label) {
                lines.push(format!("{label}: {v}"));
            }
        }
        lines
    }

    /// Human-readable lines for `fm settings` (loads first).
    pub fn describe_settings_load(&mut self) -> anyhow::Result<Vec<String>> {
        self.load()?;
        Ok(self.describe_loaded())
    }

    fn describe_loaded(&self) -> Vec<String> {
        let Some(ctx) = self.ctx.as_ref() else { return Vec::new() };
        let raw = self.raw_settings_value();
        let g = |x: f64| general(x, None, false);
        let mut lines =
            vec![format!("League: {} (Fantrax {}), season {}, {} teams", ctx.name, ctx.league_id, ctx.season, ctx.teams.len())];
        if let Ok(me) = ctx.my_team() {
            lines.push(format!("My team: {} ({})", me.name, me.team_id));
        }
        if !self.limits.is_empty() {
            lines.push(format!(
                "Roster limits: {}",
                self.limits.iter().map(|(k, (t, m))| format!("{k} {t}/{m}")).collect::<Vec<_>>().join(", ")
            ));
        }
        if !ctx.roster_shape.is_empty() {
            lines.push(format!(
                "Roster shape: {}",
                ctx.roster_shape.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")
            ));
        }
        let mut w = ctx.scoring.weights.iter().map(|(k, v)| format!("{k}={}", g(*v))).collect::<Vec<_>>().join(", ");
        if !ctx.scoring.goalie_weights.is_empty() {
            w.push_str(&format!(
                "; goalies: {}",
                ctx.scoring.goalie_weights.iter().map(|(k, v)| format!("{k}={}", g(*v))).collect::<Vec<_>>().join(", ")
            ));
        }
        let src = if self.scoring_source.is_empty() { "points" } else { self.scoring_source.as_str() };
        lines.push(format!("Scoring ({src}): {w}"));
        let label = if self.scoring_source == "FANTRAX_POINTS" { "FANTRAX_POINTS check" } else { "Scoring check" };
        if !self.weights_mae.is_nan() {
            lines.push(format!(
                "{label}: mean abs error {} FP/G vs Fantrax FPts over {} players with per-stat columns",
                fixed(self.weights_mae, 3, false),
                self.weights_checked
            ));
        }
        if self.scoring_source != "FANTRAX_POINTS" && !self.config_mae.is_nan() {
            lines.push(format!(
                "FANTRAX_POINTS (not used: league rules win) check: mean abs error {} FP/G over {} players",
                fixed(self.config_mae, 3, false),
                self.config_checked
            ));
            if self.config_mae > FIT_HINT_MAE && !ctx.scoring.weights.is_empty() {
                lines.push(format!(
                    "  To match the league rules set: FANTRAX_POINTS={}",
                    format_points(&ctx.scoring.weights, Some(&ctx.scoring.goalie_weights))
                ));
            }
        }
        let mae = if self.scoring_source == "FANTRAX_POINTS" { self.weights_mae } else { f64::NAN };
        if !mae.is_nan() && mae > FIT_HINT_MAE {
            lines.push(
                "Hint: run 'fm --league fantrax settings --fit-points' to fit point values from Fantrax's own FPts and print a \
                 ready-to-paste FANTRAX_POINTS line."
                    .into(),
            );
        }
        let dyn_src = if self.rules_dynasty() { "Fantrax keeper league type" } else { "FANTRAX_DYNASTY" };
        lines.push(format!(
            "Dynasty: {}{}",
            if ctx.dynasty { "yes" } else { "no" },
            if ctx.dynasty { format!(" (from {dyn_src}; keeper horizon {} years)", ctx.keeper_horizon_years) } else { String::new() }
        ));
        let summary = self.rule_summary();
        if !summary.is_empty() {
            lines.push("League rules (Fantrax Rules page):".into());
            lines.extend(summary.iter().map(|x| format!("  {x}")));
        }
        if !self.draft_picks.is_empty() {
            let mine = self.my_id.clone().unwrap_or_default();
            let mut years: Vec<(&i64, &Vec<DraftPickRef>)> = self.draft_picks.iter().collect();
            years.sort_by_key(|(y, _)| **y);
            let mut parts = Vec::new();
            for (year, picks) in years {
                let mut rounds: Vec<i64> = picks.iter().map(|p| p.round).collect();
                rounds.sort();
                let acquired: Vec<&DraftPickRef> =
                    picks.iter().filter(|p| p.original_owner.as_deref().is_some_and(|o| !o.is_empty() && o != mine)).collect();
                let mut s = format!("{year}: {} picks", picks.len());
                if let (Some(a), Some(b)) = (rounds.first(), rounds.last()) {
                    s.push_str(&format!(" (rounds {a}-{b})"));
                }
                if !acquired.is_empty() {
                    let names: Vec<String> = acquired
                        .iter()
                        .map(|pk| {
                            let o = pk.original_owner.clone().unwrap_or_default();
                            let name = self.teams_meta.get(&o).map(|t| t.name.clone()).unwrap_or(o);
                            format!("R{} from {name}", pk.round)
                        })
                        .collect();
                    s.push_str(&format!(", acquired: {}", names.join(", ")));
                }
                parts.push(s);
            }
            lines.push(format!("My draft picks: {}", parts.join("; ")));
        }
        if let Some(v) = raw.get("draftPickTradingAllowed").filter(|_| summary.is_empty()) {
            lines.push(format!("Draft pick trading allowed: {}", if truthy(v) { "yes" } else { "no" }));
        }
        if summary.is_empty() {
            match raw.get("rules").and_then(|r| r.as_object()).filter(|r| !r.is_empty()) {
                Some(rules) => {
                    lines.push("League rules reported by Fantrax:".into());
                    for (k, v) in rules.iter().take(MAX_RULE_LINES) {
                        lines.push(format!("  {k}: {}", py_str(v)));
                    }
                }
                None => lines.push(
                    "Fantrax league info exposed no keeper/minors/contract/draft settings; set FANTRAX_DYNASTY / \
                     FANTRAX_KEEPER_HORIZON_YEARS by hand."
                        .into(),
                ),
            }
        }
        if !self.trade_blocks.is_empty() {
            lines.push(format!("Trade blocks: {} teams listed", self.trade_blocks.len()));
        }
        if !self.pending_trades.is_empty() {
            lines.push(format!("Pending trades: {}", self.pending_trades.len()));
        }
        let n_g = ctx.free_agents.iter().filter(|p| p.is_goalie()).count();
        lines.push(format!("Free agents loaded: {} ({n_g} goalies)", ctx.free_agents.len()));
        lines.extend(self.all_warnings().into_iter().map(|w| format!("Warning: {w}")));
        lines
    }

    /// `ping()`: one uncached authenticated request to check / keep the session alive.
    pub fn ping_value(&self) -> anyhow::Result<Value> {
        let info = match self.call(&[fx_call("getFantasyLeagueInfo", &[])], true) {
            Ok(mut v) => v.remove(0),
            Err(e) => {
                if e.downcast_ref::<ProviderError>().is_some() {
                    let name = error_type_name(&e);
                    self.with_auth_mut(|a| a.record_ping(false, Some(&name)));
                }
                return Err(e);
            }
        };
        self.with_auth_mut(|a| a.record_ping(true, None));
        let name = jget(&info, "fantasySettings").filter(|s| s.is_object()).and_then(|s| jget(s, "leagueName")).cloned();
        let source = self.with_auth_mut(|a| a.source.clone());
        let mut o = serde_json::Map::new();
        o.insert("ok".into(), Value::Bool(true));
        o.insert("league_id".into(), Value::String(self.league_id()));
        o.insert("league_name".into(), name.unwrap_or(Value::Null));
        o.insert("session_source".into(), source.clone().map(Value::String).unwrap_or(Value::Null));
        o.insert(
            "refreshed".into(),
            Value::Bool(self.relogged.load(AtomicOrdering::SeqCst) && source.as_deref() == Some("login")),
        );
        let made = lock(&self.client).as_ref().map(|c| c.requests_made).unwrap_or(0);
        o.insert("requests_made".into(), Value::from(made));
        Ok(Value::Object(o))
    }

    /// Copy the provider's state into [`ProviderExtras`] (what `extras()` returns).
    pub fn sync_extras(&mut self) {
        let nan = |x: f64| if x.is_nan() { None } else { Some(x) };
        let warnings = self.all_warnings();
        let e = &mut self.extras;
        e.kind = "FantraxProvider".into();
        e.warnings = warnings;
        e.ages = self.ages.clone();
        e.standings = self.standings.clone();
        e.trade_blocks = self.trade_blocks.clone();
        e.pending_trades = self.pending_trades.clone();
        e.transactions = self.transactions.clone();
        e.scoring_source = Some(self.scoring_source.clone());
        e.weights_mae = nan(self.weights_mae);
        e.weights_checked = Some(self.weights_checked);
        e.config_mae = nan(self.config_mae);
        e.config_checked = Some(self.config_checked);
        e.draft_picks = self.draft_picks.clone();
        e.team_draft_picks = self.team_draft_picks.clone();
        e.fantasy_points = self.fantasy_points.clone();
        e.rules = self.rules.clone();
        e.lineup_lock = Some(parse_lineup_lock(self.rules.as_ref().and_then(|r| r.get(LINEUP_RULE))).to_string());
        e.rules_dynasty = Some(self.rules.as_ref().is_some_and(|r| {
            py_lower(r.get("Keeper league Type").unwrap_or("")).contains("dynasty")
        }));
    }

    /// The loaded context, if `load()` succeeded.
    pub fn context(&self) -> Option<&LeagueContext> {
        self.ctx.as_ref()
    }

    pub fn timeframe_codes(&self) -> &Codes {
        &self.codes
    }

    pub fn roster_limits(&self) -> &Map<String, (i64, i64)> {
        &self.limits
    }

    pub fn my_team_id_loaded(&self) -> Option<&str> {
        self.my_id.as_deref()
    }
}

impl LeagueProvider for FantraxProvider {
    fn name(&self) -> &str {
        "fantrax"
    }

    fn load(&mut self) -> anyhow::Result<LeagueContext> {
        FantraxProvider::load(self)
    }

    fn extras(&self) -> &ProviderExtras {
        &self.extras
    }

    /// Needs a prior `load()` (see [`FantraxProvider::activity_load`]); otherwise logs and
    /// returns no items, like Python's failure path.
    fn activity(&self, since: Option<NaiveDate>) -> anyhow::Result<Vec<ActivityItem>> {
        match self.ctx.as_ref() {
            Some(ctx) => Ok(self.activity_items(ctx, since)),
            None => {
                self.warn("Fantrax activity unavailable: league not loaded".into());
                Ok(Vec::new())
            }
        }
    }

    fn lineup_snapshot(&self, day: Option<NaiveDate>) -> anyhow::Result<Vec<LineupDay>> {
        match self.ctx.as_ref() {
            Some(ctx) => Ok(lineup_days_from_teams(&ctx.teams, day.unwrap_or(ctx.as_of))),
            None => {
                self.warn("Fantrax lineup snapshot unavailable: league not loaded".into());
                Ok(Vec::new())
            }
        }
    }

    fn fantrax_call(&self, calls: &[(String, Map<String, Value>)], fresh: bool) -> anyhow::Result<Vec<Value>> {
        self.client_call(calls, fresh)
    }

    fn raw_settings(&self) -> anyhow::Result<Value> {
        if self.ctx.is_none() {
            return Err(ProviderError::Unavailable("Fantrax league not loaded".into()).into());
        }
        Ok(self.raw_settings_value())
    }

    fn describe_settings(&self) -> Vec<String> {
        self.describe_loaded()
    }

    fn fit_points(&self) -> anyhow::Result<Value> {
        let ctx = self.ctx.as_ref().ok_or_else(|| ProviderError::Unavailable("Fantrax league not loaded".into()))?;
        Ok(fit_weights(ctx.all_players()).to_json())
    }

    fn ping(&self) -> anyhow::Result<Value> {
        self.ping_value()
    }
}
