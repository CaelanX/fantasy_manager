//! The part of the third-party `espn_api` 0.46 package (`.venv/Lib/site-packages/espn_api`) that
//! `providers/espn.py` relies on: `requests/espn_requests.py` (`EspnFantasyRequests`),
//! `base_league.py` + `hockey/league.py` (`League`: fetch, teams, player map, matchup ids,
//! scoreboard, box scores), `base_settings.py`, `hockey/{team,player,box_player,box_score,
//! matchup}.py`, `hockey/constant.py` and `utils/utils.py::json_parsing`.
//! Owner: work package `W3-espn`.
//!
//! Every HTTP GET goes through [`fm_http::HttpCache::fetch`] with exactly the URL, params and
//! `x-fantasy-filter` header espn_api sends (Python routes espn_api's `requests.get` through the
//! cache with `cache.install_espn_cache`), so the shared `cache.db` and the parity tapes hit.
//!
//! Deviations (documented): only the attributes the provider and compute code read are kept
//! (draft picks, logos, division names, matchup category scores, box-score `game_played` /
//! `points_breakdown` are parsed for their errors only, or not at all). Python exceptions raised
//! while parsing (`KeyError`, `TypeError`, ...) become [`PyErr`] so callers that print
//! `type(e).__name__` can show the same name.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fm_core::Map;
use fm_http::{CachedResponse, HttpCache, ParamValue, Params};
use fm_pycompat::pyjson::{dumps, DumpOpts};
use serde_json::Value;
use thiserror::Error;

// ------------------------------------------------------------------ constants

/// `espn_api.requests.constant.FANTASY_BASE_ENDPOINT`
pub const FANTASY_BASE_ENDPOINT: &str = "https://lm-api-reads.fantasy.espn.com/apis/v3/games/";
/// `FANTASY_SPORTS["nhl"]`
pub const FANTASY_SPORT: &str = "fhl";

/// `hockey.constant.POSITION_MAP` (int -> name half).
pub const POSITION_NAMES: [&str; 9] =
    ["Center", "Left Wing", "Right Wing", "Forward", "Defense", "Goalie", "Util", "Bench", "IR"];

/// `POSITION_MAP[id]` for an int id (None when absent).
pub fn position_name(id: i64) -> Option<&'static str> {
    usize::try_from(id).ok().and_then(|i| POSITION_NAMES.get(i).copied())
}

/// `POSITION_MAP[name]` (name -> slot id half).
pub fn position_id(name: &str) -> Option<i64> {
    POSITION_NAMES.iter().position(|n| *n == name).map(|i| i as i64)
}

/// `hockey.constant.STATS_IDENTIFIER`
pub fn stats_identifier(prefix: &str) -> Option<&'static str> {
    Some(match prefix {
        "00" => "Total",
        "01" => "Last 7",
        "02" => "Last 15",
        "03" => "Last 30",
        "10" => "Projected",
        "20" => "20",
        _ => return None,
    })
}

/// `hockey.constant.PRO_TEAM_MAP` (Montreal keeps the package's mojibake).
pub fn pro_team_name(id: i64) -> Option<&'static str> {
    Some(match id {
        1 => "Boston Bruins",
        2 => "Buffalo Sabres",
        3 => "Calgary Flames",
        4 => "Chicago Blackhawks",
        5 => "Detroit Red Wings",
        6 => "Edmonton Oilers",
        7 => "Carolina Hurricanes",
        8 => "Los Angeles Kings",
        9 => "Dallas Stars",
        10 => "Montr\u{c3}\u{a9}al Canadiens",
        11 => "New Jersey Devils",
        12 => "New York Islanders",
        13 => "New York Rangers",
        14 => "Ottawa Senators",
        15 => "Philadelphia Flyers",
        16 => "Pittsburgh Penguins",
        17 => "Colorado Avalanche",
        18 => "San Jose Sharks",
        19 => "St. Louis Blues",
        20 => "Tampa Bay Lightning",
        21 => "Toronto Maple Leafs",
        22 => "Vancouver Canucks",
        23 => "Washington Capitals",
        24 => "Arizona Coyotes",
        25 => "Anaheim Ducks",
        26 => "Florida Panthers",
        27 => "Nashville Predators",
        28 => "Winnipeg Jets",
        29 => "Columbus Blue Jackets",
        30 => "Minnesota Wild",
        37 => "Vegas Golden Knights",
        124292 => "Seattle Kraken",
        129764 => "Utah Hockey Club",
        _ => return None,
    })
}

/// Every `PRO_TEAM_MAP` value (tests).
pub fn pro_team_names() -> Vec<&'static str> {
    [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
     37, 124292, 129764]
        .iter()
        .filter_map(|&i| pro_team_name(i))
        .collect()
}

/// `hockey.constant.STATS_MAP` (stat id string -> abbreviation; `''`-valued ids do not exist).
pub fn stats_map(id: &str) -> Option<&'static str> {
    Some(match id {
        "0" => "GS",
        "1" => "W",
        "2" => "L",
        "3" => "SA",
        "4" => "GA",
        "5" => "5",
        "6" => "SV",
        "7" => "SO",
        "8" => "MIN ?",
        "9" => "OTL",
        "10" => "GAA",
        "11" => "SV%",
        "12" => "12",
        "13" => "G",
        "14" => "A",
        "15" => "+/-",
        "16" => "16",
        "17" => "PIM",
        "18" => "PPG",
        "19" => "PPA",
        "20" => "SHG",
        "21" => "SHA",
        "22" => "GWG",
        "23" => "FOW",
        "24" => "FOL",
        "25" => "25",
        "26" => "TTOI ?",
        "27" => "ATOI",
        "28" => "HAT",
        "29" => "SOG",
        "30" => "30",
        "31" => "HIT",
        "32" => "BLK",
        "33" => "DEF",
        "34" => "GP",
        "35" => "STPG",
        "36" => "STPA",
        "37" => "STP",
        "38" => "PPP",
        "39" => "SHP",
        "40" => "40",
        "41" => "41",
        "42" => "42",
        "43" => "43",
        "44" => "44",
        "45" => "45",
        "99" => "99",
        _ => return None,
    })
}

/// `hockey.constant.ACTIVITY_MAP` (message type -> action).
pub fn activity_name(msg_type: i64) -> Option<&'static str> {
    Some(match msg_type {
        178 => "FA ADDED",
        180 => "WAIVER ADDED",
        179 | 181 | 239 => "DROPPED",
        244 => "TRADED",
        _ => return None,
    })
}

// ------------------------------------------------------------------ errors

/// `espn_api.requests.espn_requests.{ESPNAccessDenied, ESPNInvalidLeague, ESPNUnknownError}`.
#[derive(Debug, Error)]
pub enum EspnError {
    #[error("{0}")]
    AccessDenied(String),
    #[error("{0}")]
    InvalidLeague(String),
    #[error("{0}")]
    UnknownError(String),
}

/// A Python built-in exception raised while parsing ESPN JSON (`KeyError`, `TypeError`,
/// `IndexError`, `ValueError`). Display = `str(e)` (a KeyError shows the quoted key).
#[derive(Debug, Error)]
#[error("{msg}")]
pub struct PyErr {
    pub kind: &'static str,
    pub msg: String,
}

impl PyErr {
    pub fn key(k: &str) -> PyErr {
        PyErr { kind: "KeyError", msg: format!("'{k}'") }
    }
    pub fn type_err(msg: impl Into<String>) -> PyErr {
        PyErr { kind: "TypeError", msg: msg.into() }
    }
    pub fn index(msg: impl Into<String>) -> PyErr {
        PyErr { kind: "IndexError", msg: msg.into() }
    }
    pub fn value(msg: impl Into<String>) -> PyErr {
        PyErr { kind: "ValueError", msg: msg.into() }
    }
}

/// `type(e).__name__` for the errors this crate and fm-http raise.
pub fn exc_name(e: &anyhow::Error) -> &'static str {
    if let Some(p) = e.downcast_ref::<PyErr>() {
        return p.kind;
    }
    if let Some(x) = e.downcast_ref::<EspnError>() {
        return match x {
            EspnError::AccessDenied(_) => "ESPNAccessDenied",
            EspnError::InvalidLeague(_) => "ESPNInvalidLeague",
            EspnError::UnknownError(_) => "ESPNUnknownError",
        };
    }
    if e.downcast_ref::<fm_http::CacheMiss>().is_some() {
        return "CacheMiss";
    }
    if e.downcast_ref::<fm_http::HttpError>().is_some() {
        return "HttpError";
    }
    if e.downcast_ref::<fm_core::ProviderError>().is_some() {
        return "ProviderError";
    }
    if e.downcast_ref::<serde_json::Error>().is_some() {
        return "JSONDecodeError";
    }
    "Exception"
}

// ------------------------------------------------------------------ Python value helpers

/// `repr(v)` of the Python object a JSON value decodes to.
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => {
            if s.contains('\'') && !s.contains('"') {
                format!("\"{s}\"")
            } else {
                format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
            }
        }
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter().map(|(k, x)| format!("{}: {}", py_repr(&Value::String(k.clone())), py_repr(x))).collect::<Vec<_>>().join(", ")
        ),
        other => py_str(other),
    }
}

/// `str(v)` of the Python object a JSON value decodes to.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => i.to_string(),
            (_, Some(u)) => u.to_string(),
            _ => fm_pycompat::fmt::repr_f64(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Python truthiness.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// Truthiness of an optional (missing = falsy).
pub fn truthy_opt(v: Option<&Value>) -> bool {
    v.is_some_and(truthy)
}

/// `float(v)`: numbers, bools and numeric strings; None for anything Python rejects.
pub fn py_float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() || t.contains("__") || t.starts_with('_') || t.ends_with('_') {
                return None;
            }
            t.replace('_', "").parse::<f64>().ok()
        }
        _ => None,
    }
}

/// `int(v)`: ints, floats (truncated), bools and integer strings; None for anything Python
/// rejects with TypeError / ValueError.
pub fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|x| x.is_finite()).map(|x| x.trunc() as i64)),
        Value::Bool(b) => Some(*b as i64),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// An integral JSON number as i64 (Python dict lookups by int; `1.0 == 1`).
pub fn as_int_key(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|x| x.fract() == 0.0 && x.is_finite()).map(|x| x as i64)),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

/// `obj[key]` on a dict (KeyError when missing, TypeError when `obj` is not a dict).
pub fn item<'a>(obj: &'a Value, key: &str) -> Result<&'a Value, PyErr> {
    match obj {
        Value::Object(m) => m.get(key).ok_or_else(|| PyErr::key(key)),
        Value::Null => Err(PyErr::type_err("'NoneType' object is not subscriptable")),
        Value::Array(_) => Err(PyErr::type_err("list indices must be integers or slices, not str")),
        _ => Err(PyErr::type_err("object is not subscriptable")),
    }
}

/// `obj.get(key)` on a dict (AttributeError-like TypeError when not a dict).
pub fn get<'a>(obj: &'a Value, key: &str) -> Result<Option<&'a Value>, PyErr> {
    match obj {
        Value::Object(m) => Ok(m.get(key)),
        _ => Err(PyErr::type_err(format!("'{}' object has no attribute 'get'", type_name(obj)))),
    }
}

/// `obj.get(key)` when `obj` may not be a dict (treated as missing).
pub fn get_lenient<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    obj.as_object().and_then(|m| m.get(key))
}

pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn as_i64_req(v: &Value, what: &str) -> Result<i64, PyErr> {
    as_int_key(v).ok_or_else(|| PyErr::type_err(format!("{what}: expected an int, got {}", type_name(v))))
}

static EMPTY_LIST: Value = Value::Array(Vec::new());

/// `espn_api.utils.utils.json_parsing(obj, key)`: the first value (depth-first, dict order) of
/// `key`; nested dicts and lists of dicts/lists are searched, the key only matches a leaf
/// value. Not found = Python's `[]` (an empty JSON array).
pub fn json_parsing<'a>(obj: &'a Value, key: &str) -> &'a Value {
    fn extract<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
        match obj {
            Value::Object(m) => {
                for (k, v) in m {
                    let nested = match v {
                        Value::Object(_) => true,
                        Value::Array(a) => matches!(a.first(), Some(Value::Array(_)) | Some(Value::Object(_))),
                        _ => false,
                    };
                    if nested {
                        if let Some(x) = extract(v, key) {
                            return Some(x);
                        }
                    } else if k == key {
                        return Some(v);
                    }
                }
                None
            }
            Value::Array(a) => a.iter().find_map(|x| extract(x, key)),
            _ => None,
        }
    }
    extract(obj, key).unwrap_or(&EMPTY_LIST)
}

/// `POSITION_MAP.get(v, '')` for a hashable JSON value.
fn position_get(v: &Value) -> Result<String, PyErr> {
    match v {
        Value::Array(_) | Value::Object(_) => Err(PyErr::type_err(format!("unhashable type: '{}'", type_name(v)))),
        Value::String(s) => Ok(match position_id(s) {
            // POSITION_MAP['Center'] is an int; nothing reads it as a name
            Some(i) => i.to_string(),
            None => String::new(),
        }),
        _ => Ok(as_int_key(v).and_then(position_name).unwrap_or("").to_string()),
    }
}

/// `json.dumps(v)` with Python's defaults (header values).
pub fn py_json(v: &Value) -> String {
    dumps(v, &DumpOpts::default())
}

// ------------------------------------------------------------------ requests

/// `espn_api.requests.espn_requests.EspnFantasyRequests` (hockey), over [`HttpCache`].
pub struct EspnFantasyRequests {
    pub year: i64,
    pub league_id: i64,
    /// `ENDPOINT`: `.../games/fhl/seasons/{year}`
    pub endpoint: String,
    /// `LEAGUE_ENDPOINT` (switches to the alternate form after a 401 that the other form answers)
    league_endpoint: Mutex<String>,
    /// `{'espn_s2': ..., 'SWID': ...}` (empty = None: both are needed)
    pub cookies: Vec<(String, String)>,
    pub cache: Arc<HttpCache>,
    /// TTL of every request (`install_espn_cache(cache, ttl=ESPN_TTL)`)
    pub ttl: f64,
}

/// A header list with one `x-fantasy-filter` JSON value.
pub fn filter_header(filters: &Value) -> Vec<(String, String)> {
    vec![("x-fantasy-filter".to_string(), py_json(filters))]
}

/// `{"view": v}`
pub fn view_param(v: &str) -> Params {
    vec![("view".to_string(), ParamValue::Str(v.to_string()))]
}

impl EspnFantasyRequests {
    pub fn new(year: i64, league_id: i64, espn_s2: Option<&str>, swid: Option<&str>, cache: Arc<HttpCache>, ttl: f64) -> Self {
        let base = format!("{FANTASY_BASE_ENDPOINT}{FANTASY_SPORT}");
        let endpoint = format!("{base}/seasons/{year}");
        let league_endpoint = if year < 2018 {
            format!("{base}/leagueHistory/{league_id}?seasonId={year}")
        } else {
            format!("{base}/seasons/{year}/segments/0/leagues/{league_id}")
        };
        let cookies = match (espn_s2.filter(|s| !s.is_empty()), swid.filter(|s| !s.is_empty())) {
            (Some(a), Some(b)) => vec![("espn_s2".to_string(), a.to_string()), ("SWID".to_string(), b.to_string())],
            _ => vec![],
        };
        EspnFantasyRequests { year, league_id, endpoint, league_endpoint: Mutex::new(league_endpoint), cookies, cache, ttl }
    }

    pub fn league_endpoint(&self) -> String {
        self.league_endpoint.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_league_endpoint(&self, s: String) {
        *self.league_endpoint.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    fn fetch(&self, url: &str, params: &Params, headers: &[(String, String)]) -> anyhow::Result<CachedResponse> {
        self.cache.fetch(url, params, headers, &self.cookies, self.ttl)
    }

    /// `checkRequestStatus`: 401 tries the other endpoint form (kept when it answers 200).
    fn check_request_status(
        &self,
        status: u16,
        extend: &str,
        params: &Params,
        headers: &[(String, String)],
    ) -> anyhow::Result<Option<Value>> {
        match status {
            401 => {
                let original = self.league_endpoint();
                let alternate = if original.contains("/leagueHistory/") {
                    let base = original.split("/leagueHistory/").next().unwrap_or("");
                    format!("{base}/seasons/{}/segments/0/leagues/{}", self.year, self.league_id)
                } else {
                    let base = original.split("/seasons/").next().unwrap_or("");
                    format!("{base}/leagueHistory/{}?seasonId={}", self.league_id, self.year)
                };
                self.set_league_endpoint(alternate.clone());
                let r = self.fetch(&format!("{alternate}{extend}"), params, headers)?;
                if r.status_code == 200 {
                    return Ok(Some(r.json()?));
                }
                self.set_league_endpoint(original);
                let has = |k: &str| self.cookies.iter().any(|(n, _)| n == k);
                if self.cookies.is_empty() || !has("espn_s2") || !has("SWID") {
                    return Err(EspnError::AccessDenied("espn_s2 and swid are required".into()).into());
                }
                Err(EspnError::AccessDenied(format!(
                    "League {} cannot be accessed with the provided credentials",
                    self.league_id
                ))
                .into())
            }
            404 => Err(EspnError::InvalidLeague(format!("League {} does not exist", self.league_id)).into()),
            200 => Ok(None),
            s => Err(EspnError::UnknownError(format!("ESPN returned an HTTP {s}")).into()),
        }
    }

    /// `league_get(params, headers, extend)`: a list response yields its first element.
    pub fn league_get(&self, params: &Params, headers: &[(String, String)], extend: &str) -> anyhow::Result<Value> {
        let endpoint = format!("{}{}", self.league_endpoint(), extend);
        let r = self.fetch(&endpoint, params, headers)?;
        let alt = self.check_request_status(r.status_code, extend, params, headers)?;
        let response = match alt {
            Some(v) if truthy(&v) => v,
            _ => r.json()?,
        };
        match response {
            Value::Array(a) => {
                a.into_iter().next().ok_or_else(|| PyErr::index("list index out of range").into())
            }
            other => Ok(other),
        }
    }

    /// `get(params, headers, extend)` on the season endpoint.
    pub fn get(&self, params: &Params, headers: &[(String, String)], extend: &str) -> anyhow::Result<Value> {
        let endpoint = format!("{}{}", self.endpoint, extend);
        let r = self.fetch(&endpoint, params, headers)?;
        self.check_request_status(r.status_code, "", &Vec::new(), &[])?;
        r.json()
    }

    /// `get_league()`: views mTeam, mRoster, mMatchup, mSettings, mStandings.
    pub fn get_league(&self) -> anyhow::Result<Value> {
        let params: Params = vec![(
            "view".into(),
            ParamValue::List(["mTeam", "mRoster", "mMatchup", "mSettings", "mStandings"].iter().map(|s| s.to_string()).collect()),
        )];
        self.league_get(&params, &[], "")
    }

    /// `get_pro_schedule()`: view proTeamSchedules_wl.
    pub fn get_pro_schedule(&self) -> anyhow::Result<Value> {
        self.get(&view_param("proTeamSchedules_wl"), &[], "")
    }

    /// `get_pro_players()`: every active NHL player (`/players`, view players_wl).
    pub fn get_pro_players(&self) -> anyhow::Result<Value> {
        let filters = serde_json::json!({"filterActive": {"value": true}});
        self.get(&view_param("players_wl"), &filter_header(&filters), "/players")
    }

    /// `get_league_draft()`: view mDraftDetail.
    pub fn get_league_draft(&self) -> anyhow::Result<Value> {
        self.league_get(&view_param("mDraftDetail"), &[], "")
    }
}

// ------------------------------------------------------------------ Player

/// `espn_api.hockey.player.Player` (the attributes `providers/espn.py` reads). Values that
/// `json_parsing` did not find are Python's `[]` (an empty JSON array), as in espn_api.
#[derive(Clone, Debug)]
pub struct Player {
    /// `fullName` (any JSON value; the provider requires a string)
    pub name: Value,
    /// `id`
    pub player_id: Value,
    /// POSITION_MAP name of the default position ('' when unknown)
    pub position: String,
    /// POSITION_MAP name of `lineupSlotId` ('' when absent)
    pub lineup_slot: String,
    pub eligible_slots: Vec<String>,
    /// PRO_TEAM_MAP name ('Unknown Team' when unknown)
    pub pro_team: String,
    pub injury_status: Value,
    pub injured: bool,
    /// 'Total 2027' / 'Last 7 2027' / 'Projected 2027' ... -> {abbr: value} ("total")
    pub stats: Map<String, Map<String, Value>>,
}

/// `get_stat_key(id)`
pub fn get_stat_key(id: &str) -> String {
    let prefix: String = id.chars().take(2).collect();
    match stats_identifier(&prefix) {
        Some(t) => format!("{t} {}", id.chars().skip(2).collect::<String>()),
        None => id.to_string(),
    }
}

impl Player {
    /// `Player(data)`
    pub fn new(data: &Value) -> anyhow::Result<Player> {
        let name = json_parsing(data, "fullName").clone();
        let player_id = json_parsing(data, "id").clone();
        let dpid = json_parsing(data, "defaultPositionId");
        let position = match dpid {
            Value::Null => String::new(),
            Value::Array(_) | Value::Object(_) => {
                return Err(PyErr::type_err(format!("unhashable type: '{}'", type_name(dpid))).into())
            }
            Value::String(s) if s.is_empty() => String::new(),
            Value::String(_) => {
                return Err(PyErr::type_err("'<=' not supported between instances of 'str' and 'int'").into())
            }
            _ => {
                let x = dpid.as_f64().unwrap_or(0.0);
                let key = if x != 0.0 && x <= 3.0 { x - 1.0 } else { x };
                if key.fract() == 0.0 { position_name(key as i64).unwrap_or("").to_string() } else { String::new() }
            }
        };
        let lineup_slot = match data.get("lineupSlotId") {
            None => String::new(),
            Some(v) => position_get(v)?,
        };
        let eligible_slots = match json_parsing(data, "eligibleSlots") {
            Value::Array(a) => a.iter().map(position_get).collect::<Result<Vec<_>, _>>()?,
            Value::String(s) => s.chars().map(|c| position_get(&Value::String(c.to_string()))).collect::<Result<Vec<_>, _>>()?,
            Value::Object(m) => m.keys().map(|k| position_get(&Value::String(k.clone()))).collect::<Result<Vec<_>, _>>()?,
            other => return Err(PyErr::type_err(format!("'{}' object is not iterable", type_name(other))).into()),
        };
        let pt = json_parsing(data, "proTeamId");
        let pro_team = match pt {
            Value::Array(_) | Value::Object(_) => {
                return Err(PyErr::type_err(format!("unhashable type: '{}'", type_name(pt))).into())
            }
            _ => as_int_key(pt).and_then(pro_team_name).unwrap_or("Unknown Team").to_string(),
        };
        let first_injury = json_parsing(data, "injuryStatus").clone();

        // player = data.get('playerPoolEntry', {}).get('player') or data['player']
        let ppe_player = match data.get("playerPoolEntry") {
            None => None,
            Some(ppe) => get(ppe, "player")?,
        };
        let player = match ppe_player {
            Some(p) if truthy(p) => p,
            _ => item(data, "player")?,
        };
        let injury_status = get(player, "injuryStatus")?.cloned().unwrap_or(first_injury);
        let injured = get(player, "injured")?.is_some_and(truthy);

        let mut stats: Map<String, Map<String, Value>> = Map::new();
        if let Some(splits) = get(player, "stats")? {
            let list: &[Value] = match splits {
                Value::Array(a) => a,
                other => return Err(PyErr::type_err(format!("'{}' object is not iterable", type_name(other))).into()),
            };
            for split in list {
                if !get(split, "stats")?.is_some_and(truthy) {
                    continue;
                }
                let id = match item(split, "id")? {
                    Value::String(s) => s.clone(),
                    other => return Err(PyErr::type_err(format!("'{}' object is not subscriptable", type_name(other))).into()),
                };
                let stat_key = get_stat_key(&id);
                let raw = item(split, "stats")?;
                let Value::Object(m) = raw else {
                    return Err(PyErr::type_err("stats is not a dict").into());
                };
                let mut total: Map<String, Value> = Map::new();
                for (i, v) in m {
                    let abbr = stats_map(i).ok_or_else(|| PyErr::key(i))?;
                    total.insert(abbr.to_string(), v.clone());
                }
                // self.stats[stat_key] = {} keeps the key's first position
                stats.insert(stat_key, total);
            }
        }
        Ok(Player {
            name,
            player_id,
            position,
            lineup_slot,
            eligible_slots,
            pro_team,
            injury_status,
            injured,
            stats,
        })
    }
}

// ------------------------------------------------------------------ BoxPlayer / BoxScore

/// `round(x, 2)` as espn_api calls it (TypeError on a non-number).
fn round2(v: &Value) -> Result<f64, PyErr> {
    match v {
        Value::Number(n) if n.is_f64() => Ok(fm_pycompat::num::round_to(n.as_f64().unwrap_or(0.0), 2)),
        Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => Ok(*b as i64 as f64),
        other => Err(PyErr::type_err(format!("type {} doesn't define __round__ method", type_name(other)))),
    }
}

/// `espn_api.hockey.box_player.BoxPlayer`: a Player plus its lineup slot and points.
#[derive(Clone, Debug)]
pub struct BoxPlayer {
    pub player: Player,
    /// POSITION_MAP name of `lineupSlotId`, 'FA' when absent
    pub slot_position: String,
    pub points: f64,
}

impl BoxPlayer {
    pub fn new(data: &Value, pro_schedule: &HashMap<i64, (Value, Value)>) -> anyhow::Result<BoxPlayer> {
        let player = Player::new(data)?;
        let mut slot_position = "FA".to_string();
        if let Some(v) = data.get("lineupSlotId") {
            slot_position = as_int_key(v).and_then(position_name).ok_or_else(|| PyErr::key(&py_str(v)))?.to_string();
        }
        let p = if data.get("playerPoolEntry").is_some() {
            item(item(data, "playerPoolEntry")?, "player")?
        } else {
            item(data, "player")?
        };
        let pro_team_id = item(p, "proTeamId")?;
        if let Some((_, date)) = as_int_key(pro_team_id).and_then(|k| pro_schedule.get(&k)) {
            if py_float(date).is_none() || date.is_string() {
                return Err(PyErr::type_err("unsupported operand type(s) for /").into());
            }
        }
        let mut points = 0.0;
        if let Some(list) = get(p, "stats")? {
            for stats in list.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                points = match get(stats, "appliedTotal")? {
                    None => 0.0,
                    Some(v) => round2(v)?,
                };
            }
        }
        Ok(BoxPlayer { player, slot_position, points })
    }
}

/// `espn_api.hockey.box_score.BoxScore` (teams and lineups).
#[derive(Clone, Debug)]
pub struct BoxScore {
    /// team id (JSON value as ESPN sent it)
    pub home_team: Value,
    /// team id, `0` for a bye
    pub away_team: Value,
    pub home_lineup: Vec<BoxPlayer>,
    pub away_lineup: Vec<BoxPlayer>,
}

impl BoxScore {
    pub fn new(data: &Value, pro_schedule: &HashMap<i64, (Value, Value)>, by_matchup: bool) -> anyhow::Result<BoxScore> {
        item(data, "winner")?;
        let roster_key = if by_matchup { "rosterForMatchupPeriod" } else { "rosterForCurrentScoringPeriod" };
        let side = |s: &Value| -> anyhow::Result<(Value, Vec<BoxPlayer>)> {
            let team = item(s, "teamId")?.clone();
            let empty = Value::Object(Default::default());
            let roster = get(s, roster_key)?.unwrap_or(&empty);
            if let Some(v) = get(s, "totalPointsLive")? {
                round2(v)?;
                if let Some(p) = get(s, "totalProjectedPointsLive")? {
                    round2(p)?;
                }
            } else if let Some(v) = get(roster, "appliedStatTotal")? {
                round2(v)?;
            }
            let mut lineup = Vec::new();
            if let Some(entries) = get(roster, "entries")? {
                for e in entries.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                    lineup.push(BoxPlayer::new(e, pro_schedule)?);
                }
            }
            Ok((team, lineup))
        };
        let (home_team, home_lineup) = side(item(data, "home")?)?;
        let (away_team, away_lineup) = match data.get("away") {
            Some(a) => side(a)?,
            None => (Value::from(0), Vec::new()),
        };
        Ok(BoxScore { home_team, away_team, home_lineup, away_lineup })
    }
}

// ------------------------------------------------------------------ Matchup

/// `espn_api.hockey.matchup.Matchup` (teams and final scores).
#[derive(Clone, Debug)]
pub struct Matchup {
    pub home_team: Value,
    pub home_final_score: Value,
    pub away_team: Value,
    pub away_final_score: Value,
    pub winner: Value,
}

impl Matchup {
    pub fn new(data: &Value) -> anyhow::Result<Matchup> {
        let home = item(data, "home")?;
        let home_team = item(home, "teamId")?.clone();
        let home_final_score = item(home, "totalPoints")?.clone();
        let away = item(data, "away")?;
        let away_team = item(away, "teamId")?.clone();
        let away_final_score = item(away, "totalPoints")?.clone();
        let winner = item(data, "winner")?.clone();
        if let Some(cs) = home.get("cumulativeScore") {
            if truthy(item(cs, "scoreByStat")?) {
                for side in [home, away] {
                    let c = item(side, "cumulativeScore")?;
                    for k in ["wins", "ties"] {
                        if py_float(item(c, k)?).is_none() {
                            return Err(PyErr::type_err("unsupported operand type(s) for +").into());
                        }
                    }
                    if let Value::Object(m) = item(c, "scoreByStat")? {
                        for (i, v) in m {
                            stats_map(i).ok_or_else(|| PyErr::key(i))?;
                            item(v, "score")?;
                            item(v, "result")?;
                        }
                    }
                }
            }
        }
        Ok(Matchup { home_team, home_final_score, away_team, away_final_score, winner })
    }
}

// ------------------------------------------------------------------ Team / Settings

/// `espn_api.hockey.team.Team` (the attributes the provider reads).
#[derive(Clone, Debug)]
pub struct Team {
    pub team_id: i64,
    pub team_abbrev: String,
    pub team_name: String,
    pub division_id: Value,
    pub wins: i64,
    pub losses: i64,
    pub ties: i64,
    pub standing: Value,
    pub final_standing: Value,
    pub roster: Vec<Player>,
    /// league members owning the team (`[m for m in members if m.get('id') in team['owners']]`)
    pub owners: Vec<Value>,
}

fn record_int(v: &Value, what: &str) -> Result<i64, PyErr> {
    as_i64_req(v, what)
}

impl Team {
    pub fn new(data: &Value, roster: &Value, schedule: &Value, owners: Vec<Value>) -> anyhow::Result<Team> {
        let team_id = as_i64_req(item(data, "id")?, "team id")?;
        let team_abbrev = py_str(item(data, "abbrev")?);
        let mut team_name = get(data, "name")?.map(py_str).unwrap_or_else(|| "Unknown".into());
        if team_name == "Unknown" {
            let loc = get(data, "location")?.map(py_str).unwrap_or_else(|| "Unknown".into());
            let nick = get(data, "nickname")?.map(py_str).unwrap_or_else(|| "Unknown".into());
            team_name = format!("{loc} {nick}");
        }
        let division_id = item(data, "divisionId")?.clone();
        let overall = item(item(data, "record")?, "overall")?;
        let wins = record_int(item(overall, "wins")?, "wins")?;
        let losses = record_int(item(overall, "losses")?, "losses")?;
        let ties = record_int(item(overall, "ties")?, "ties")?;
        let standing = item(data, "playoffSeed")?.clone();
        let final_standing = item(data, "rankCalculatedFinal")?.clone();
        if let Some(Value::Object(m)) = data.get("valuesByStat") {
            for k in m.keys() {
                stats_map(k).ok_or_else(|| PyErr::key(k))?;
            }
        }
        let mut players = Vec::new();
        let entries = item(roster, "entries")?;
        for p in entries.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            players.push(Player::new(p)?);
        }
        // _fetch_schedule: Matchup(match) for this team's games (parsed for its errors)
        for m in schedule.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if m.get("away").is_some() {
                let mine = as_int_key(item(item(m, "away")?, "teamId")?) == Some(team_id)
                    || as_int_key(item(item(m, "home")?, "teamId")?) == Some(team_id);
                if mine {
                    Matchup::new(m)?;
                }
            }
        }
        Ok(Team {
            team_id,
            team_abbrev,
            team_name,
            division_id,
            wins,
            losses,
            ties,
            standing,
            final_standing,
            roster: players,
            owners,
        })
    }
}

/// `espn_api.base_settings.BaseSettings`.
#[derive(Clone, Debug)]
pub struct Settings {
    pub reg_season_count: Value,
    /// matchup period -> scoring periods (`scheduleSettings.matchupPeriods`)
    pub matchup_periods: Value,
    pub team_count: Value,
    pub playoff_team_count: Value,
    pub keeper_count: Value,
    pub trade_deadline: Value,
    pub name: Value,
    pub scoring_type: Option<Value>,
    pub raw_scoring_settings: Value,
    pub raw_schedule_settings: Value,
    pub playoff_matchup_period_length: Value,
    pub faab: Value,
    pub acquisition_budget: Value,
}

impl Settings {
    pub fn new(data: &Value) -> anyhow::Result<Settings> {
        let sched = item(data, "scheduleSettings")?;
        let reg_season_count = item(sched, "matchupPeriodCount")?.clone();
        let matchup_periods = item(sched, "matchupPeriods")?.clone();
        item(item(data, "tradeSettings")?, "vetoVotesRequired")?;
        let team_count = item(data, "size")?.clone();
        let playoff_team_count = item(sched, "playoffTeamCount")?.clone();
        let keeper_count = item(item(data, "draftSettings")?, "keeperCount")?.clone();
        let trade_deadline = item(data, "tradeSettings")?.get("deadlineDate").cloned().unwrap_or(Value::from(0));
        let name = item(data, "name")?.clone();
        let scoring = item(data, "scoringSettings")?;
        item(scoring, "matchupTieRule")?;
        item(scoring, "playoffMatchupTieRule")?;
        let playoff_matchup_period_length = sched.get("playoffMatchupPeriodLength").cloned().unwrap_or(Value::from(0));
        item(sched, "playoffSeedingRule")?;
        let scoring_type = scoring.get("scoringType").cloned();
        let acq = item(data, "acquisitionSettings")?;
        let faab = item(acq, "isUsingAcquisitionBudget")?.clone();
        let acquisition_budget = acq.get("acquisitionBudget").cloned().unwrap_or(Value::from(0));
        Ok(Settings {
            reg_season_count,
            matchup_periods,
            team_count,
            playoff_team_count,
            keeper_count,
            trade_deadline,
            name,
            scoring_type,
            raw_scoring_settings: scoring.clone(),
            raw_schedule_settings: sched.clone(),
            playoff_matchup_period_length,
            faab,
            acquisition_budget,
        })
    }
}

// ------------------------------------------------------------------ League

/// `espn_api.hockey.League` (fetched on construction like `League(...)` with
/// `fetch_league=True`).
pub struct League {
    pub league_id: i64,
    pub year: i64,
    pub espn_request: EspnFantasyRequests,
    pub current_matchup_period: Value,
    pub scoring_period_id: i64,
    pub first_scoring_period: Value,
    pub final_scoring_period: i64,
    pub previous_seasons: Vec<Value>,
    /// `scoringPeriodId`, capped at `finalScoringPeriod`
    pub current_week: i64,
    pub settings: Settings,
    pub members: Vec<Value>,
    /// sorted by team id
    pub teams: Vec<Team>,
    /// NHL player id -> full name (`player_map`'s int half)
    pub player_map: HashMap<i64, String>,
    /// matchup period -> scoring period ids (strings, sorted as strings), schedule order
    pub matchup_ids: Vec<(Value, Vec<String>)>,
}

impl League {
    /// `League(league_id, year, espn_s2, swid)`: league, pro players and draft requests.
    pub fn fetch(req: EspnFantasyRequests) -> anyhow::Result<League> {
        let data = req.get_league()?;
        // BaseLeague._fetch_league reads status / settings before the pro-player request
        let mut league = League::from_data(req, &data, None)?;
        let players = league.espn_request.get_pro_players()?;
        league.set_player_map(&players)?;
        league.map_matchup_ids_and_teams(&data)?;
        // _fetch_draft: the picks are not used by the provider; the request (and its errors) are
        let draft = league.espn_request.get_league_draft()?;
        if let Some(dd) = get(&draft, "draftDetail")? {
            let _ = get(dd, "drafted")?;
        }
        Ok(league)
    }

    /// The parsing half of [`League::fetch`] (tests build leagues from JSON with it). With
    /// `players` the player map, matchup ids and teams are filled too.
    pub fn from_data(req: EspnFantasyRequests, data: &Value, players: Option<&Value>) -> anyhow::Result<League> {
        let status = item(data, "status")?;
        let current_matchup_period = item(status, "currentMatchupPeriod")?.clone();
        let scoring_period_id = as_i64_req(item(data, "scoringPeriodId")?, "scoringPeriodId")?;
        let first_scoring_period = item(status, "firstScoringPeriod")?.clone();
        let final_scoring_period = as_i64_req(item(status, "finalScoringPeriod")?, "finalScoringPeriod")?;
        let year = req.year;
        let previous_seasons = match item(status, "previousSeasons")? {
            Value::Array(a) => a
                .iter()
                .filter(|y| as_int_key(y).is_some_and(|y| y < year))
                .cloned()
                .collect(),
            other => return Err(PyErr::type_err(format!("'{}' object is not iterable", type_name(other))).into()),
        };
        let current_week = if year < 2018 || scoring_period_id <= final_scoring_period {
            scoring_period_id
        } else {
            final_scoring_period
        };
        let settings = Settings::new(item(data, "settings")?)?;
        let members = data.get("members").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut league = League {
            league_id: req.league_id,
            year,
            espn_request: req,
            current_matchup_period,
            scoring_period_id,
            first_scoring_period,
            final_scoring_period,
            previous_seasons,
            current_week,
            settings,
            members,
            teams: Vec::new(),
            player_map: HashMap::new(),
            matchup_ids: Vec::new(),
        };
        if let Some(p) = players {
            league.set_player_map(p)?;
            league.map_matchup_ids_and_teams(data)?;
        }
        Ok(league)
    }

    fn set_player_map(&mut self, players: &Value) -> anyhow::Result<()> {
        let list = match players {
            Value::Array(a) => a.as_slice(),
            _ => return Err(PyErr::type_err("string indices must be integers").into()),
        };
        for p in list {
            let id = item(p, "id")?;
            let name = item(p, "fullName")?;
            if let Some(i) = as_int_key(id) {
                self.player_map.insert(i, py_str(name));
            }
        }
        Ok(())
    }

    fn map_matchup_ids_and_teams(&mut self, data: &Value) -> anyhow::Result<()> {
        // _map_matchup_ids(data['schedule'])
        let schedule = item(data, "schedule")?;
        self.matchup_ids.clear();
        for m in schedule.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let mp = get(m, "matchupPeriodId")?.cloned().unwrap_or(Value::Null);
            let keys: Vec<String> = get_lenient(m, "home")
                .and_then(|h| get_lenient(h, "pointsByScoringPeriod"))
                .and_then(Value::as_object)
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            if keys.is_empty() {
                continue;
            }
            match self.matchup_ids.iter_mut().find(|(k, _)| *k == mp) {
                None => {
                    let mut s = keys;
                    s.sort();
                    self.matchup_ids.push((mp, s));
                }
                Some((_, v)) => {
                    let mut s: Vec<String> = v.iter().cloned().chain(keys).collect();
                    s.sort();
                    s.dedup();
                    *v = s;
                }
            }
        }
        // _fetch_teams(data, TeamClass=Team)
        let teams = item(data, "teams")?;
        item(data, "seasonId")?;
        let members = data.get("members").and_then(Value::as_array).cloned().unwrap_or_default();
        let list = teams.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
        let mut rosters: Vec<(Value, Value)> = Vec::new();
        for t in list {
            let id = item(t, "id")?.clone();
            let roster = get(t, "roster")?.cloned().unwrap_or(Value::Object(Default::default()));
            match rosters.iter_mut().find(|(k, _)| *k == id) {
                Some(slot) => slot.1 = roster,
                None => rosters.push((id, roster)),
            }
        }
        let mut out = Vec::new();
        for t in list {
            let id = item(t, "id")?;
            let roster = rosters.iter().find(|(k, _)| k == id).map(|(_, r)| r.clone()).unwrap_or(Value::Null);
            let owner_ids = t.get("owners").and_then(Value::as_array).cloned().unwrap_or_default();
            let owners: Vec<Value> = members
                .iter()
                .filter(|m| get_lenient(m, "id").is_some_and(|i| owner_ids.contains(i)))
                .cloned()
                .collect();
            out.push(Team::new(t, &roster, schedule, owners)?);
        }
        out.sort_by_key(|t| t.team_id);
        self.teams = out;
        Ok(())
    }

    /// `get_team_data(team_id)`
    pub fn get_team_data(&self, team_id: &Value) -> Option<&Team> {
        let id = as_int_key(team_id)?;
        self.teams.iter().find(|t| t.team_id == id)
    }

    /// `_get_pro_schedule(scoringPeriodId)`: pro team id -> (opponent id, game date ms).
    fn get_pro_schedule_for(&self, scoring_period: &Value) -> anyhow::Result<HashMap<i64, (Value, Value)>> {
        let data = self.espn_request.get_pro_schedule()?;
        let pro_teams = item(item(&data, "settings")?, "proTeams")?;
        let sp = py_str(scoring_period);
        let mut out = HashMap::new();
        for team in pro_teams.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let empty = Value::Object(Default::default());
            let pro_game = get(team, "proGamesByScoringPeriod")?.unwrap_or(&empty);
            let tid = item(team, "id")?;
            let games = get_lenient(pro_game, &sp);
            if as_int_key(tid) != Some(0) && games.is_some_and(truthy) {
                let g = games.and_then(|g| g.as_array()).and_then(|a| a.first()).ok_or_else(|| PyErr::key("0"))?;
                let home = item(g, "homeProTeamId")?.clone();
                let date = item(g, "date")?.clone();
                let away = item(g, "awayProTeamId")?.clone();
                let opp = if tid == &away { home } else { away };
                if let Some(k) = as_int_key(tid) {
                    out.insert(k, (opp, date));
                }
            }
        }
        Ok(out)
    }

    /// `scoreboard(matchupPeriod)`: matchups of a period (view mMatchup).
    pub fn scoreboard(&self, matchup_period: Option<i64>) -> anyhow::Result<Vec<Matchup>> {
        let mp: Value = match matchup_period.filter(|&m| m != 0) {
            Some(m) => Value::from(m),
            None => self.current_matchup_period.clone(),
        };
        let data = self.espn_request.league_get(&view_param("mMatchup"), &[], "")?;
        let schedule = item(&data, "schedule")?;
        let mut out = Vec::new();
        for m in schedule.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if item(m, "matchupPeriodId")? == &mp {
                out.push(Matchup::new(m)?);
            }
        }
        Ok(out)
    }

    /// `box_scores(matchup_period, scoring_period, matchup_total)`.
    pub fn box_scores(
        &self,
        matchup_period: Option<i64>,
        scoring_period: Option<i64>,
        matchup_total: bool,
    ) -> anyhow::Result<Vec<BoxScore>> {
        if self.year < 2019 {
            return Err(anyhow::anyhow!("Cant use box score before 2019"));
        }
        let mut matchup_id = self.current_matchup_period.clone();
        let mut scoring_id = Value::from(self.current_week);
        let cur_mp = as_int_key(&self.current_matchup_period);
        match (matchup_period.filter(|&m| m != 0), scoring_period.filter(|&s| s != 0)) {
            (Some(m), Some(s)) => {
                matchup_id = Value::from(m);
                scoring_id = Value::from(s);
            }
            (Some(m), None) if cur_mp.is_some_and(|c| m < c) => {
                matchup_id = Value::from(m);
                scoring_id = match self.matchup_ids.iter().find(|(k, _)| as_int_key(k) == Some(m)) {
                    Some((_, v)) => v.last().map(|s| Value::from(s.clone())).unwrap_or(Value::from(1)),
                    None => Value::from(1),
                };
            }
            (None, Some(s)) if s <= self.current_week => {
                scoring_id = Value::from(s);
                let key = s.to_string();
                if let Some((k, _)) = self.matchup_ids.iter().find(|(_, v)| v.contains(&key)) {
                    matchup_id = k.clone();
                }
            }
            _ => {}
        }
        let params: Params = vec![
            ("view".into(), ParamValue::List(vec!["mMatchupScore".into(), "mScoreboard".into()])),
            ("scoringPeriodId".into(), ParamValue::Str(py_str(&scoring_id))),
        ];
        let filters = serde_json::json!({"schedule": {"filterMatchupPeriodIds": {"value": [matchup_id]}}});
        let data = self.espn_request.league_get(&params, &filter_header(&filters), "")?;
        let schedule = item(&data, "schedule")?;
        let pro_schedule = self.get_pro_schedule_for(&scoring_id)?;
        let mut out = Vec::new();
        for m in schedule.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            out.push(BoxScore::new(m, &pro_schedule, matchup_total)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_parsing_first_leaf_depth_first() {
        let v = json!({"a": {"id": 1}, "id": 2, "list": [{"id": 3}]});
        assert_eq!(json_parsing(&v, "id"), &json!(1));
        let v = json!({"eligibleSlots": [0, 3], "x": {"eligibleSlots": [9]}});
        assert_eq!(json_parsing(&v, "eligibleSlots"), &json!([0, 3]));
        assert_eq!(json_parsing(&v, "missing"), &json!([]));
    }

    #[test]
    fn stat_keys() {
        assert_eq!(get_stat_key("002027"), "Total 2027");
        assert_eq!(get_stat_key("012027"), "Last 7 2027");
        assert_eq!(get_stat_key("102027"), "Projected 2027");
        assert_eq!(get_stat_key("992027"), "992027");
    }
}
