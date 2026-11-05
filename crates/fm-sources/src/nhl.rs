//! Rust port of `fantasy_manager/providers/nhl.py`: NHL data provider - season stats (stats REST
//! API) and schedules, rosters, game logs, player pages and search (api-web.nhle.com).
//! Owner: work package `W2-nhl-matching` (wave 2).
//!
//! All network access goes through an injected `fetch_json(url, params, ttl)` ([`FetchJson`]) so the
//! integrator can swap in a caching fetcher and tests can replay recorded fixtures.
//!
//! Deviations from Python (API shape only):
//! * `fetch_json` also receives a TTL (fm-http's fixed `FetchJson` shape). Python's
//!   `CachedNhlFetch` chose the TTL inside the fetch callable; here [`NhlTtl`] (held by the client)
//!   makes the same choice and [`cached_nhl_fetch`] adapts an `HttpCache`.
//! * `date.today()` defaults become explicit `today` parameters (`current_season`,
//!   `NhlClient::new`); `NhlRosterPlayer::age_on` takes the date.
//! * Parsers that can raise in Python (`KeyError`, pydantic `ValidationError`) return `Result`.
//! * `NHLE_FACTORS` is [`nhle_factors()`]; the JSON is `include_str!`-ed from `crates/fm-sources/data/nhle_factors.json`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};
use chrono::{Datelike, Duration, NaiveDate};
use fm_core::{Map, PyDateTime};
use fm_http::ParamValue;
use fm_http::{FetchJson, HttpCache, Params};
use fm_pycompat::num::round_to;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const STATS_BASE: &str = "https://api.nhle.com/stats/rest/en";
pub const WEB_BASE: &str = "https://api-web.nhle.com/v1";
pub const SEARCH_URL: &str = "https://search.d3.nhle.com/api/v1/search/player";

pub const NHL_TEAMS: [&str; 32] = [
    "ANA", "BOS", "BUF", "CGY", "CAR", "CHI", "COL", "CBJ", "DAL", "DET", "EDM", "FLA", "LAK", "MIN", "MTL", "NSH",
    "NJD", "NYI", "NYR", "OTT", "PHI", "PIT", "SJS", "SEA", "STL", "TBL", "TOR", "UTA", "VAN", "VGK", "WSH", "WPG",
];

/// Canonical skater stat keys produced by this module.
pub const SKATER_STAT_KEYS: [&str; 14] =
    ["GP", "G", "A", "PTS", "PM", "PIM", "PPG", "PPP", "SHG", "SHP", "GWG", "SOG", "HIT", "BLK"];
pub const GOALIE_STAT_KEYS: [&str; 11] = ["GP", "GS", "W", "L", "OTL", "GA", "SA", "SV", "SO", "GAA", "SVPCT"];

const SUMMARY_MAP: [(&str, &str); 13] = [
    ("gamesPlayed", "GP"),
    ("goals", "G"),
    ("assists", "A"),
    ("points", "PTS"),
    ("plusMinus", "PM"),
    ("penaltyMinutes", "PIM"),
    ("ppGoals", "PPG"),
    ("ppPoints", "PPP"),
    ("shGoals", "SHG"),
    ("shPoints", "SHP"),
    ("gameWinningGoals", "GWG"),
    ("otGoals", "OTG"),
    ("shots", "SOG"),
];
const REALTIME_MAP: [(&str, &str); 7] = [
    ("hits", "HIT"),
    ("blockedShots", "BLK"),
    ("giveaways", "GIVE"),
    ("takeaways", "TAKE"),
    ("missedShots", "MISS"),
    ("emptyNetGoals", "ENG"),
    ("emptyNetPoints", "ENP"),
];
const FACEOFF_MAP: [(&str, &str); 2] = [("totalFaceoffWins", "FOW"), ("totalFaceoffLosses", "FOL")];
const GOALIE_MAP: [(&str, &str); 11] = [
    ("gamesPlayed", "GP"),
    ("gamesStarted", "GS"),
    ("wins", "W"),
    ("losses", "L"),
    ("otLosses", "OTL"),
    ("goalsAgainst", "GA"),
    ("shotsAgainst", "SA"),
    ("saves", "SV"),
    ("shutouts", "SO"),
    ("goalsAgainstAverage", "GAA"),
    ("savePct", "SVPCT"),
];
const GAMELOG_SKATER_MAP: [(&str, &str); 13] = [
    ("goals", "G"),
    ("assists", "A"),
    ("points", "PTS"),
    ("plusMinus", "PM"),
    ("pim", "PIM"),
    ("powerPlayGoals", "PPG"),
    ("powerPlayPoints", "PPP"),
    ("shorthandedGoals", "SHG"),
    ("shorthandedPoints", "SHP"),
    ("gameWinningGoals", "GWG"),
    ("otGoals", "OTG"),
    ("shots", "SOG"),
    ("shifts", "SHIFTS"),
];

// --------------------------------------------------------------------------- JSON helpers

/// `row.get(key)` with JSON null treated as missing (Python `None`).
fn g<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|x| !x.is_null())
}

/// Python truthiness of an optional JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `_num(v)`: floats/ints (not bools) as f64, else 0.0.
fn num(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// A JSON number (not bool) as f64.
fn opt_f64(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

fn opt_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Python `int(x)` on a JSON value (numbers truncate, numeric strings parse).
fn py_int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(Value::Bool(b)) => Some(*b as i64),
        _ => None,
    }
}

/// `int(row["key"])` raising KeyError / ValueError as an error.
fn req_int(row: &Value, key: &str) -> Result<i64> {
    py_int(g(row, key)).ok_or_else(|| anyhow!("missing or non-integer {key:?}"))
}

fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}

// --------------------------------------------------------------------------- helpers

/// Season id like 20262027. From September on, the upcoming season is current.
pub fn current_season(today: NaiveDate) -> i64 {
    let y = if today.month() >= 9 { today.year() } else { today.year() - 1 } as i64;
    y * 10000 + (y + 1)
}

pub fn prior_season(season: i64) -> i64 {
    season - 10001
}

pub fn map_position(code: Option<&str>) -> Option<String> {
    let code = code.filter(|c| !c.is_empty())?;
    let up = code.trim().to_uppercase();
    match up.as_str() {
        "C" => Some("C"),
        "L" | "LW" => Some("LW"),
        "R" | "RW" => Some("RW"),
        "D" => Some("D"),
        "G" => Some("G"),
        _ => None,
    }
    .map(str::to_string)
}

/// `'24:49'` -> 1489.0 seconds; numbers are passed through as seconds.
pub fn parse_toi(value: Option<&Value>) -> Option<f64> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => parse_toi_str(s),
        Some(Value::Bool(b)) => Some(*b as i64 as f64),
        _ => None,
    }
}

pub fn parse_toi_str(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    let int = |p: &str| p.trim().parse::<i64>().ok();
    let flt = |p: &str| p.trim().parse::<f64>().ok();
    match parts.len() {
        2 => Some((int(parts[0])? * 60) as f64 + flt(parts[1])?),
        3 => Some((int(parts[0])? * 3600 + int(parts[1])? * 60) as f64 + flt(parts[2])?),
        _ => flt(s),
    }
}

/// NHL localized strings look like `{"default": "Connor", "fr": ...}`.
fn default_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::Object(o)) => o.get("default").and_then(|x| x.as_str()).map(str::to_string),
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// `date.fromisoformat(value[:10])`; None on anything unparsable.
fn parse_date(v: Option<&str>) -> Option<NaiveDate> {
    let s = v.filter(|s| !s.is_empty())?;
    let head: String = s.chars().take(10).collect();
    if head.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(&head, "%Y-%m-%d").ok()
}

fn parse_date_v(v: Option<&Value>) -> Option<NaiveDate> {
    parse_date(v.and_then(|x| x.as_str()))
}

/// `datetime.fromisoformat(value.replace("Z", "+00:00"))`.
fn parse_dt(v: Option<&str>) -> Option<PyDateTime> {
    let s = v.filter(|s| !s.is_empty())?;
    PyDateTime::parse(s)
}

/// `'SJS, TOR'` -> `['SJS', 'TOR']` (last entry is the most recent team).
pub fn split_teams(team_abbrevs: Option<&str>) -> Vec<String> {
    match team_abbrevs {
        None => vec![],
        Some(s) => s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect(),
    }
}

fn secs(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Bool(_)) => None,
        Some(Value::Number(n)) => n.as_f64(),
        other => parse_toi(other),
    }
}

fn home_flag(flag: Option<&Value>) -> Option<bool> {
    match flag.and_then(|f| f.as_str()) {
        Some("H") => Some(true),
        Some("R") => Some(false),
        _ => None,
    }
}

// --------------------------------------------------------------------------- models

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlSkaterSeason {
    pub player_id: i64,
    pub name: String,
    pub last_name: Option<String>,
    /// most recent team
    pub team: Option<String>,
    pub teams: Vec<String>,
    /// C / LW / RW / D
    pub position: Option<String>,
    pub shoots: Option<String>,
    pub season: i64,
    pub game_type: i64,
    /// seconds
    pub toi_per_game: Option<f64>,
    pub shooting_pct: Option<f64>,
    pub faceoff_win_pct: Option<f64>,
    pub stats: Map<String, f64>,
}

impl NhlSkaterSeason {
    pub fn games_played(&self) -> i64 {
        self.stats.get("GP").copied().unwrap_or(0.0).trunc() as i64
    }

    pub fn per_game(&self) -> Map<String, f64> {
        let gp = self.stats.get("GP").copied().unwrap_or(0.0);
        if gp == 0.0 {
            return self.stats.keys().filter(|k| *k != "GP").map(|k| (k.clone(), 0.0)).collect();
        }
        self.stats.iter().filter(|(k, _)| *k != "GP").map(|(k, v)| (k.clone(), v / gp)).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlGoalieSeason {
    pub player_id: i64,
    pub name: String,
    pub last_name: Option<String>,
    pub team: Option<String>,
    pub teams: Vec<String>,
    pub position: String,
    pub catches: Option<String>,
    pub season: i64,
    pub game_type: i64,
    pub toi_seconds: Option<f64>,
    pub stats: Map<String, f64>,
}

impl NhlGoalieSeason {
    pub fn games_played(&self) -> i64 {
        self.stats.get("GP").copied().unwrap_or(0.0).trunc() as i64
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlGameLogEntry {
    pub game_id: Option<i64>,
    pub date: NaiveDate,
    pub team: Option<String>,
    pub opponent: Option<String>,
    pub home: Option<bool>,
    pub game_type: Option<i64>,
    pub toi_seconds: Option<f64>,
    pub is_goalie: bool,
    pub decision: Option<String>,
    pub stats: Map<String, f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlGame {
    pub game_id: i64,
    pub season: Option<i64>,
    pub game_type: i64,
    pub date: NaiveDate,
    pub start_time_utc: Option<PyDateTime>,
    pub away: String,
    pub home: String,
    pub state: Option<String>,
}

impl NhlGame {
    pub fn teams(&self) -> (&str, &str) {
        (&self.away, &self.home)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct NhlWeekSchedule {
    pub days: Map<NaiveDate, Vec<NhlGame>>,
    pub regular_season_start: Option<NaiveDate>,
    pub regular_season_end: Option<NaiveDate>,
    pub next_start_date: Option<NaiveDate>,
    pub previous_start_date: Option<NaiveDate>,
}

impl NhlWeekSchedule {
    pub fn games_per_team(&self) -> Map<String, i64> {
        let mut c: Map<String, i64> = Map::new();
        for games in self.days.values() {
            for g in games {
                *c.entry(g.away.clone()).or_insert(0) += 1;
                *c.entry(g.home.clone()).or_insert(0) += 1;
            }
        }
        c
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlRosterPlayer {
    pub player_id: i64,
    pub first_name: String,
    pub last_name: String,
    pub team: String,
    /// C / LW / RW / D / G
    pub position: Option<String>,
    /// raw NHL code
    pub position_code: Option<String>,
    pub birth_date: Option<NaiveDate>,
    pub sweater_number: Option<i64>,
    pub shoots_catches: Option<String>,
}

impl NhlRosterPlayer {
    pub fn name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name).trim().to_string()
    }

    /// Age in fractional years on the given date.
    pub fn age_on(&self, on: NaiveDate) -> Option<f64> {
        let b = self.birth_date?;
        let years = on.year() - b.year() - (((on.month(), on.day()) < (b.month(), b.day())) as i32);
        let y = b.year() + years;
        let last_bday = b
            .with_year(y)
            .unwrap_or_else(|| NaiveDate::from_ymd_opt(y, 2, 28).expect("valid Feb 28"));
        Some(years as f64 + (on - last_bday).num_days() as f64 / 365.25)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlPlayerLanding {
    pub player_id: i64,
    pub first_name: String,
    pub last_name: String,
    pub team: Option<String>,
    pub position: Option<String>,
    pub birth_date: Option<NaiveDate>,
    pub shoots_catches: Option<String>,
    pub is_active: Option<bool>,
    pub last5: Vec<NhlGameLogEntry>,
    pub featured_regular_season: serde_json::Map<String, Value>,
    pub season_totals: Vec<Value>,
    pub draft_year: Option<i64>,
    pub draft_round: Option<i64>,
    pub draft_overall: Option<i64>,
    pub draft_team: Option<String>,
    /// careerTotals.regularSeason.gamesPlayed (0 when absent)
    pub career_gp: Option<i64>,
}

impl NhlPlayerLanding {
    pub fn name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name).trim().to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlSearchResult {
    pub player_id: i64,
    pub name: String,
    pub position: Option<String>,
    pub position_code: Option<String>,
    pub team: Option<String>,
    pub active: Option<bool>,
    pub sweater_number: Option<i64>,
    pub birth_country: Option<String>,
}

// --------------------------------------------------------------------------- row parsers

pub fn skater_from_rows(
    summary: &Value,
    realtime: Option<&Value>,
    faceoffs: Option<&Value>,
    game_type: i64,
) -> Result<NhlSkaterSeason> {
    let mut stats: Map<String, f64> = SUMMARY_MAP.iter().map(|(src, canon)| (canon.to_string(), num(g(summary, src)))).collect();
    if let Some(rt) = realtime.filter(|r| truthy(Some(r))) {
        for (src, canon) in REALTIME_MAP {
            stats.insert(canon.to_string(), num(g(rt, src)));
        }
    }
    if let Some(fo) = faceoffs.filter(|r| truthy(Some(r))) {
        for (src, canon) in FACEOFF_MAP {
            stats.insert(canon.to_string(), num(g(fo, src)));
        }
    }
    let teams = split_teams(g(summary, "teamAbbrevs").and_then(|v| v.as_str()));
    Ok(NhlSkaterSeason {
        player_id: req_int(summary, "playerId")?,
        name: opt_string(g(summary, "skaterFullName")).filter(|s| !s.is_empty()).unwrap_or_default(),
        last_name: opt_string(g(summary, "lastName")),
        team: teams.last().cloned(),
        teams,
        position: map_position(g(summary, "positionCode").and_then(|v| v.as_str())),
        shoots: opt_string(g(summary, "shootsCatches")),
        season: py_int(g(summary, "seasonId")).unwrap_or(0),
        game_type,
        toi_per_game: opt_f64(g(summary, "timeOnIcePerGame")),
        shooting_pct: opt_f64(g(summary, "shootingPct")),
        faceoff_win_pct: opt_f64(g(summary, "faceoffWinPct")),
        stats,
    })
}

pub fn goalie_from_row(row: &Value, game_type: i64) -> Result<NhlGoalieSeason> {
    let stats: Map<String, f64> = GOALIE_MAP.iter().map(|(src, canon)| (canon.to_string(), num(g(row, src)))).collect();
    let teams = split_teams(g(row, "teamAbbrevs").and_then(|v| v.as_str()));
    Ok(NhlGoalieSeason {
        player_id: req_int(row, "playerId")?,
        name: opt_string(g(row, "goalieFullName")).unwrap_or_default(),
        last_name: opt_string(g(row, "lastName")),
        team: teams.last().cloned(),
        teams,
        position: "G".to_string(),
        catches: opt_string(g(row, "shootsCatches")),
        season: py_int(g(row, "seasonId")).unwrap_or(0),
        game_type,
        toi_seconds: opt_f64(g(row, "timeOnIce")),
        stats,
    })
}

pub fn game_log_entry(row: &Value) -> Result<NhlGameLogEntry> {
    let has = |k: &str| row.as_object().map(|o| o.contains_key(k)).unwrap_or(false);
    let is_goalie = has("shotsAgainst") || has("savePctg") || has("decision");
    let flag = g(row, "homeRoadFlag").and_then(|v| v.as_str());
    let mut stats: Map<String, f64> = Map::new();
    let mut decision: Option<String> = None;
    if is_goalie {
        let sa = num(g(row, "shotsAgainst"));
        let ga = num(g(row, "goalsAgainst"));
        decision = opt_string(g(row, "decision"));
        let d = decision.as_deref();
        let svpct = match g(row, "savePctg") {
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
            _ => {
                if sa != 0.0 {
                    (sa - ga) / sa
                } else {
                    0.0
                }
            }
        };
        let b = |x: bool| if x { 1.0 } else { 0.0 };
        stats.insert("GP".into(), 1.0);
        stats.insert("GS".into(), num(g(row, "gamesStarted")));
        stats.insert("W".into(), b(d == Some("W")));
        stats.insert("L".into(), b(d == Some("L")));
        stats.insert("OTL".into(), b(matches!(d, Some("O" | "OT" | "OTL" | "SO"))));
        stats.insert("GA".into(), ga);
        stats.insert("SA".into(), sa);
        stats.insert("SV".into(), sa - ga);
        stats.insert("SO".into(), num(g(row, "shutouts")));
        stats.insert("SVPCT".into(), svpct);
        stats.insert("G".into(), num(g(row, "goals")));
        stats.insert("A".into(), num(g(row, "assists")));
        stats.insert("PIM".into(), num(g(row, "pim")));
    } else {
        stats.insert("GP".into(), 1.0);
        for (src, canon) in GAMELOG_SKATER_MAP {
            if has(src) {
                stats.insert(canon.to_string(), num(g(row, src)));
            }
        }
    }
    Ok(NhlGameLogEntry {
        game_id: py_int(g(row, "gameId")),
        date: parse_date_v(g(row, "gameDate")).ok_or_else(|| anyhow!("game log row without a valid gameDate"))?,
        team: opt_string(g(row, "teamAbbrev")),
        opponent: opt_string(g(row, "opponentAbbrev")),
        home: match flag {
            Some("H") => Some(true),
            Some("R") => Some(false),
            _ => None,
        },
        game_type: py_int(g(row, "gameTypeId")),
        toi_seconds: parse_toi(g(row, "toi")),
        is_goalie,
        decision,
        stats,
    })
}

pub fn game_from_row(row: &Value, day: Option<NaiveDate>) -> Result<NhlGame> {
    let start = parse_dt(g(row, "startTimeUTC").and_then(|v| v.as_str()));
    let date = parse_date_v(g(row, "gameDate"))
        .or(day)
        .or_else(|| start.map(|s| s.date()))
        .ok_or_else(|| anyhow!("game row without a date"))?;
    let abbrev = |side: &str| -> Result<String> {
        row.get(side)
            .and_then(|t| t.get("abbrev"))
            .and_then(|a| a.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("game row without {side}.abbrev"))
    };
    Ok(NhlGame {
        game_id: req_int(row, "id")?,
        season: py_int(g(row, "season")),
        game_type: py_int(g(row, "gameType")).unwrap_or(0),
        date,
        start_time_utc: start,
        away: abbrev("awayTeam")?,
        home: abbrev("homeTeam")?,
        state: opt_string(g(row, "gameState")),
    })
}

// --------------------------------------------------------------------------- schedule helpers

/// Number of game dates in the inclusive window [start, end].
pub fn games_in_window(dates: &[NaiveDate], start: NaiveDate, end: NaiveDate) -> usize {
    dates.iter().filter(|d| start <= **d && **d <= end).count()
}

/// Number of NHL games on each date given team -> game dates (each game counted once).
pub fn games_per_day(all_schedules: &Map<String, Vec<NaiveDate>>) -> Map<NaiveDate, i64> {
    let mut teams_playing: std::collections::BTreeMap<NaiveDate, i64> = Default::default();
    for dates in all_schedules.values() {
        let uniq: BTreeSet<&NaiveDate> = dates.iter().collect();
        for d in uniq {
            *teams_playing.entry(*d).or_insert(0) += 1;
        }
    }
    teams_playing.into_iter().map(|(d, n)| (d, (n + 1) / 2)).collect()
}

/// Dates with fewer than `threshold` games (more free agents / bench players available).
pub fn off_nights(per_day: &Map<NaiveDate, i64>, threshold: i64) -> BTreeSet<NaiveDate> {
    per_day.iter().filter(|(_, n)| **n > 0 && **n < threshold).map(|(d, _)| *d).collect()
}

/// Consecutive-day game pairs (relevant for goalie starts).
pub fn back_to_backs(dates: &[NaiveDate]) -> Vec<(NaiveDate, NaiveDate)> {
    let ds: Vec<NaiveDate> = dates.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
    ds.windows(2).filter(|w| w[1] - w[0] == Duration::days(1)).map(|w| (w[0], w[1])).collect()
}

// --------------------------------------------------------------------------- deployment (per-game reports)
//
// Per-game rows of the stats REST reports (`isGame=true`) over a date window. See the Python
// module comment for the verified field names of skater/timeonice, skater/powerplay,
// goalie/summary and team/powerplay. Season reports (`isGame=false`) carry the same player
// fields with `teamAbbrevs`.

pub const HOUR_S: f64 = 3600.0;
/// window reaching today: games may still be in progress
pub const TTL_GAME_WINDOW_OPEN: f64 = 12.0 * HOUR_S;
/// finished days do not change
pub const TTL_GAME_WINDOW_CLOSED: f64 = 30.0 * 24.0 * HOUR_S;
pub const TTL_SEASON_REPORT: f64 = 6.0 * HOUR_S;

fn param_str<'a>(params: &'a Params, key: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
        ParamValue::Str(s) => Some(s.as_str()),
        _ => None,
    })
}

/// TTL for a per-game (`isGame=true`) date-window request: 12h when the window reaches `today`
/// (or has no end), 30 days for past days; None for any other request.
pub fn game_window_ttl(params: &Params, today: NaiveDate) -> Option<f64> {
    if param_str(params, "isGame").unwrap_or("").to_lowercase() != "true" {
        return None;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"gameDate<="?(\d{4}-\d{2}-\d{2})"#).expect("static regex"));
    let Some(m) = re.captures(param_str(params, "cayenneExp").unwrap_or("")) else {
        return Some(TTL_GAME_WINDOW_OPEN);
    };
    let end = NaiveDate::parse_from_str(&m[1], "%Y-%m-%d").ok()?;
    Some(if end < today { TTL_GAME_WINDOW_CLOSED } else { TTL_GAME_WINDOW_OPEN })
}

/// The TTL policy of Python's `CachedNhlFetch`: per-game windows use [`game_window_ttl`];
/// everything else `ttl_for(url, params)` when given (e.g. `providers.enrich.ttl_for`), else
/// `default_ttl`.
pub struct NhlTtl {
    pub today: NaiveDate,
    pub default_ttl: f64,
    pub ttl_for: Option<Box<dyn Fn(&str, &Params) -> f64 + Send + Sync>>,
}

impl NhlTtl {
    pub fn new(today: NaiveDate) -> Self {
        NhlTtl { today, default_ttl: TTL_SEASON_REPORT, ttl_for: None }
    }

    pub fn ttl(&self, url: &str, params: &Params) -> f64 {
        if let Some(t) = game_window_ttl(params, self.today) {
            return t;
        }
        if let Some(f) = &self.ttl_for {
            return f(url, params);
        }
        self.default_ttl
    }
}

/// A `fetch_json` that goes through an `HttpCache` (`cache.get_json(url, params, ttl=...)`).
pub fn cached_nhl_fetch(cache: Arc<HttpCache>) -> Box<FetchJson<'static>> {
    Box::new(move |url, params, ttl| cache.get_json(url, params, &[], &[], ttl))
}

/// Plain uncached GET returning parsed JSON (follows the API's 307 redirects); the `ttl` is ignored.
pub fn default_fetch_json(url: &str, params: &Params, _ttl: f64) -> Result<Value> {
    let mut query: Vec<(String, String)> = Vec::new();
    for (k, v) in params {
        match v {
            ParamValue::Str(s) => query.push((k.clone(), s.clone())),
            ParamValue::List(l) => query.extend(l.iter().map(|x| (k.clone(), x.clone()))),
            ParamValue::None => {}
        }
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("fantasy-manager/0.1")
        .build()?;
    let resp = client.get(url).query(&query).send()?.error_for_status()?;
    Ok(resp.json()?)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlSkaterToiGame {
    pub player_id: i64,
    pub game_id: Option<i64>,
    pub date: NaiveDate,
    pub team: Option<String>,
    pub opponent: Option<String>,
    pub home: Option<bool>,
    pub name: Option<String>,
    pub position: Option<String>,
    pub toi: Option<f64>,
    pub ev_toi: Option<f64>,
    pub pp_toi: Option<f64>,
    pub sh_toi: Option<f64>,
    pub ot_toi: Option<f64>,
    pub shifts: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlSkaterPpGame {
    pub player_id: i64,
    pub game_id: Option<i64>,
    pub date: NaiveDate,
    pub team: Option<String>,
    pub opponent: Option<String>,
    /// seconds
    pub pp_toi: Option<f64>,
    /// NHL ppTimeOnIcePctPerGame (0..1)
    pub pp_share: Option<f64>,
    pub pp_goals: f64,
    pub pp_points: f64,
    pub pp_shots: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlGoalieGame {
    pub player_id: i64,
    pub game_id: Option<i64>,
    pub date: NaiveDate,
    pub team: Option<String>,
    pub opponent: Option<String>,
    pub home: Option<bool>,
    pub name: Option<String>,
    pub started: bool,
    pub sa: f64,
    pub sv: f64,
    pub ga: f64,
    /// seconds
    pub toi: Option<f64>,
    /// W / L / OTL
    pub decision: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlTeamPpGame {
    /// abbreviation (from the opponent's row of the same game)
    pub team: Option<String>,
    pub team_id: Option<i64>,
    pub team_name: Option<String>,
    pub game_id: Option<i64>,
    pub date: NaiveDate,
    pub opponent: Option<String>,
    /// seconds of power play in the game
    pub pp_toi: Option<f64>,
    pub pp_opportunities: f64,
    pub pp_goals: f64,
}

/// Season (`isGame=false`) deployment of one skater: per-game seconds and PP share.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlSkaterDeploymentSeason {
    pub player_id: i64,
    pub season: i64,
    pub name: Option<String>,
    pub team: Option<String>,
    pub teams: Vec<String>,
    pub gp: i64,
    pub toi_per_game: Option<f64>,
    pub ev_toi_per_game: Option<f64>,
    pub pp_toi_per_game: Option<f64>,
    pub sh_toi_per_game: Option<f64>,
    pub pp_share: Option<f64>,
}

fn row_date(r: &Value) -> Result<NaiveDate> {
    parse_date_v(g(r, "gameDate")).ok_or_else(|| anyhow!("row without a valid gameDate"))
}

fn toi_game(r: &Value) -> Result<NhlSkaterToiGame> {
    Ok(NhlSkaterToiGame {
        player_id: req_int(r, "playerId")?,
        game_id: py_int(g(r, "gameId")),
        date: row_date(r)?,
        team: opt_string(g(r, "teamAbbrev")),
        opponent: opt_string(g(r, "opponentTeamAbbrev")),
        home: home_flag(g(r, "homeRoad")),
        name: opt_string(g(r, "skaterFullName")),
        position: map_position(g(r, "positionCode").and_then(|v| v.as_str())),
        toi: secs(g(r, "timeOnIce")),
        ev_toi: secs(g(r, "evTimeOnIce")),
        pp_toi: secs(g(r, "ppTimeOnIce")),
        sh_toi: secs(g(r, "shTimeOnIce")),
        ot_toi: secs(g(r, "otTimeOnIce")),
        shifts: match g(r, "shifts") {
            Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
            _ => None,
        },
    })
}

fn pp_game(r: &Value) -> Result<NhlSkaterPpGame> {
    Ok(NhlSkaterPpGame {
        player_id: req_int(r, "playerId")?,
        game_id: py_int(g(r, "gameId")),
        date: row_date(r)?,
        team: opt_string(g(r, "teamAbbrev")),
        opponent: opt_string(g(r, "opponentTeamAbbrev")),
        pp_toi: secs(g(r, "ppTimeOnIce")),
        pp_share: opt_f64(g(r, "ppTimeOnIcePctPerGame")),
        pp_goals: num(g(r, "ppGoals")),
        pp_points: num(g(r, "ppPoints")),
        pp_shots: num(g(r, "ppShots")),
    })
}

fn goalie_game(r: &Value) -> Result<NhlGoalieGame> {
    let decision = if num(g(r, "wins")) != 0.0 {
        Some("W")
    } else if num(g(r, "losses")) != 0.0 {
        Some("L")
    } else if num(g(r, "otLosses")) != 0.0 {
        Some("OTL")
    } else {
        None
    };
    let (sa, ga) = (num(g(r, "shotsAgainst")), num(g(r, "goalsAgainst")));
    Ok(NhlGoalieGame {
        player_id: req_int(r, "playerId")?,
        game_id: py_int(g(r, "gameId")),
        date: row_date(r)?,
        team: opt_string(g(r, "teamAbbrev")),
        opponent: opt_string(g(r, "opponentTeamAbbrev")),
        home: home_flag(g(r, "homeRoad")),
        name: opt_string(g(r, "goalieFullName")),
        started: num(g(r, "gamesStarted")) != 0.0,
        sa,
        sv: if g(r, "saves").is_some() { num(g(r, "saves")) } else { sa - ga },
        ga,
        toi: secs(g(r, "timeOnIce")),
        decision: decision.map(str::to_string),
    })
}

/// Team rows carry no abbreviation: take it from the other team's opponentTeamAbbrev.
fn team_pp_games(rows: &[Value]) -> Result<Vec<NhlTeamPpGame>> {
    let mut by_game: HashMap<String, Vec<&Value>> = HashMap::new();
    for r in rows {
        by_game.entry(g(r, "gameId").map(|v| v.to_string()).unwrap_or_default()).or_default().push(r);
    }
    let mut out = Vec::new();
    for r in rows {
        let key = g(r, "gameId").map(|v| v.to_string()).unwrap_or_default();
        let tid = g(r, "teamId");
        let others: Vec<&&Value> = by_game.get(&key).map(|v| v.iter().filter(|o| g(o, "teamId") != tid).collect()).unwrap_or_default();
        let abbrev = opt_string(g(r, "teamAbbrev")).filter(|s| !s.is_empty()).or_else(|| {
            if others.len() == 1 {
                opt_string(g(others[0], "opponentTeamAbbrev"))
            } else {
                None
            }
        });
        out.push(NhlTeamPpGame {
            team: abbrev,
            team_id: py_int(tid),
            team_name: opt_string(g(r, "teamFullName")),
            game_id: py_int(g(r, "gameId")),
            date: row_date(r)?,
            opponent: opt_string(g(r, "opponentTeamAbbrev")),
            pp_toi: secs(g(r, "ppTimeOnIcePerGame")),
            pp_opportunities: num(g(r, "ppOpportunities")),
            pp_goals: num(g(r, "powerPlayGoalsFor")),
        });
    }
    Ok(out)
}

/// `gameDate>="start" and gameDate<="end"` (either side optional).
pub fn game_window_filter(start: Option<NaiveDate>, end: Option<NaiveDate>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(s) = start {
        parts.push(format!("gameDate>=\"{}\"", s.format("%Y-%m-%d")));
    }
    if let Some(e) = end {
        parts.push(format!("gameDate<=\"{}\"", e.format("%Y-%m-%d")));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" and "))
    }
}

// --------------------------------------------------------------------------- preseason (gameType 1)
//
// The NHL publishes NO preseason data through the stats REST reports nor the player game logs;
// preseason games appear in `club-schedule-season` (gameType 1) and their
// `gamecenter/{id}/boxscore` carries per-skater lines; `gamecenter/{id}/landing` lists every goal
// with its strength (see the Python module comment).

pub const PRESEASON: i64 = 1;
pub const FINAL_STATES: [&str; 2] = ["FINAL", "OFF"];
const BOX_SKATER_MAP: [(&str, &str); 10] = [
    ("goals", "G"),
    ("assists", "A"),
    ("points", "PTS"),
    ("plusMinus", "PM"),
    ("pim", "PIM"),
    ("hits", "HIT"),
    ("blockedShots", "BLK"),
    ("powerPlayGoals", "PPG"),
    ("sog", "SOG"),
    ("shorthandedGoals", "SHG"),
];

/// One skater's box-score line in one game (built for preseason games, gameType 1). `stats`
/// uses canonical keys (GP = 1). PPA / PPP / SHA / SHP are only present when the game's scoring
/// summary was parsed (`has_strength`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NhlSkaterGameLine {
    pub player_id: i64,
    pub game_id: i64,
    pub game_type: Option<i64>,
    pub date: NaiveDate,
    pub team: Option<String>,
    pub opponent: Option<String>,
    pub home: Option<bool>,
    pub name: Option<String>,
    pub position: Option<String>,
    /// seconds
    pub toi: Option<f64>,
    pub shifts: Option<i64>,
    pub has_strength: bool,
    pub stats: Map<String, f64>,
}

fn box_skater(row: &Value, game: &Value, side: &str) -> Result<Option<NhlSkaterGameLine>> {
    if g(row, "playerId").is_none() {
        return Ok(None);
    }
    let empty = Value::Object(Default::default());
    let home = g(game, "homeTeam").unwrap_or(&empty);
    let away = g(game, "awayTeam").unwrap_or(&empty);
    let (me, other) = if side == "homeTeam" { (home, away) } else { (away, home) };
    let mut stats: Map<String, f64> = Map::new();
    stats.insert("GP".into(), 1.0);
    for (src, canon) in BOX_SKATER_MAP {
        if row.get(src).is_some() {
            stats.insert(canon.to_string(), num(g(row, src)));
        }
    }
    Ok(Some(NhlSkaterGameLine {
        player_id: req_int(row, "playerId")?,
        game_id: py_int(g(game, "id")).unwrap_or(0),
        game_type: py_int(g(game, "gameType")),
        date: parse_date_v(g(game, "gameDate")).ok_or_else(|| anyhow!("box score without a valid gameDate"))?,
        team: opt_string(g(me, "abbrev")),
        opponent: opt_string(g(other, "abbrev")),
        home: Some(side == "homeTeam"),
        name: default_str(g(row, "name")),
        position: map_position(g(row, "position").and_then(|v| v.as_str())),
        toi: secs(g(row, "toi")),
        shifts: match g(row, "shifts") {
            Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
            _ => None,
        },
        has_strength: false,
        stats,
    }))
}

/// Skater lines (forwards + defense, both teams) of a `gamecenter/{id}/boxscore` payload; empty
/// when the game is not final or the box score carries no player stats (limited scoring).
pub fn boxscore_skater_lines(boxscore: &Value) -> Result<Vec<NhlSkaterGameLine>> {
    let state = g(boxscore, "gameState").and_then(|v| v.as_str()).unwrap_or("");
    if !FINAL_STATES.contains(&state) {
        return Ok(vec![]);
    }
    let empty = Value::Object(Default::default());
    let by_team = g(boxscore, "playerByGameStats").unwrap_or(&empty);
    let mut out = Vec::new();
    for side in ["awayTeam", "homeTeam"] {
        let team = g(by_team, side).unwrap_or(&empty);
        for group in ["forwards", "defense"] {
            for row in arr(team, group) {
                if let Some(ln) = box_skater(row, boxscore, side)? {
                    out.push(ln);
                }
            }
        }
    }
    Ok(out)
}

/// player_id -> {PPG, PPA, SHG, SHA}
pub type GoalStrength = Map<i64, Map<String, f64>>;

/// player_id -> {PPG, PPA, SHG, SHA} from a `gamecenter/{id}/landing` scoring summary (goals
/// with strength "pp" / "sh"); None when the payload has no scoring summary.
pub fn goal_strength_points(landing: &Value) -> Option<GoalStrength> {
    let periods = g(landing, "summary").and_then(|s| s.get("scoring")).and_then(|p| p.as_array())?;
    let mut out: GoalStrength = Map::new();
    for per in periods {
        for goal in arr(per, "goals") {
            let strength = goal.get("strength").and_then(|s| s.as_str()).unwrap_or("").to_lowercase();
            if strength != "pp" && strength != "sh" {
                continue;
            }
            let tag = strength.to_uppercase();
            if let Some(pid) = py_int(g(goal, "playerId")) {
                *out.entry(pid).or_default().entry(format!("{tag}G")).or_insert(0.0) += 1.0;
            }
            for a in arr(goal, "assists") {
                if let Some(pid) = py_int(g(a, "playerId")) {
                    *out.entry(pid).or_default().entry(format!("{tag}A")).or_insert(0.0) += 1.0;
                }
            }
        }
    }
    Some(out)
}

/// Add PPA / PPP / SHA / SHP from a scoring summary (goals keep the box score's PPG / SHG when
/// it has them, else the summary's).
pub fn apply_goal_strength(lines: &mut [NhlSkaterGameLine], strength: Option<&GoalStrength>) {
    let Some(strength) = strength else { return };
    let none = Map::new();
    for ln in lines.iter_mut() {
        let s = strength.get(&ln.player_id).unwrap_or(&none);
        for tag in ["PP", "SH"] {
            let gk = format!("{tag}G");
            let goals = ln.stats.get(&gk).copied().unwrap_or_else(|| s.get(&gk).copied().unwrap_or(0.0));
            let assists = s.get(&format!("{tag}A")).copied().unwrap_or(0.0);
            ln.stats.insert(gk, goals);
            ln.stats.insert(format!("{tag}A"), assists);
            ln.stats.insert(format!("{tag}P"), goals + assists);
        }
        ln.has_strength = true;
    }
}

// --------------------------------------------------------------------------- league history / NHLe

/// One regular-season line in any league (`landing.seasonTotals`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeagueSeason {
    /// e.g. 20252026
    pub season: i64,
    /// leagueAbbrev: "SHL", "OHL", "AHL", "NHL", "WJC-20", ...
    pub league: String,
    pub team: Option<String>,
    pub gp: i64,
    pub g: i64,
    pub a: i64,
    pub pts: i64,
    /// age on Oct 1 of the season's start year
    pub age_at_season: Option<f64>,
}

impl LeagueSeason {
    pub fn start_year(&self) -> i64 {
        self.season / 10000
    }

    pub fn pts_per_game(&self) -> f64 {
        if self.gp != 0 {
            self.pts as f64 / self.gp as f64
        } else {
            0.0
        }
    }
}

fn age_oct1(birth: Option<NaiveDate>, start_year: i64) -> Option<f64> {
    let b = birth?;
    let d = NaiveDate::from_ymd_opt(start_year as i32, 10, 1)?;
    Some(round_to((d - b).num_days() as f64 / 365.25, 2))
}

/// `seasonTotals` rows of `game_type` (2 = regular season) with games played, oldest first.
/// Points default to goals + assists when missing (goalies carry neither and get 0).
pub fn parse_season_totals(rows: &[Value], birth_date: Option<NaiveDate>, game_type: i64) -> Vec<LeagueSeason> {
    let mut out: Vec<LeagueSeason> = Vec::new();
    for r in rows {
        let truthy_int = |k: &str| -> Option<i64> {
            if truthy(g(r, k)) {
                py_int(g(r, k))
            } else {
                Some(0)
            }
        };
        let Some(gt) = truthy_int("gameTypeId") else { continue };
        if gt != game_type {
            continue;
        }
        let Some(gp) = truthy_int("gamesPlayed") else { continue };
        let Some(season) = py_int(g(r, "season")) else { continue };
        if gp <= 0 {
            continue;
        }
        let goals = truthy_int("goals").unwrap_or(0);
        let assists = truthy_int("assists").unwrap_or(0);
        let pts = g(r, "points").and_then(|v| py_int(Some(v))).unwrap_or(goals + assists);
        let league = match g(r, "leagueAbbrev") {
            Some(v) if truthy(Some(v)) => v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()),
            _ => "?".to_string(),
        };
        out.push(LeagueSeason {
            season,
            league,
            team: default_str(g(r, "teamName")),
            gp,
            g: goals,
            a: assists,
            pts,
            age_at_season: age_oct1(birth_date, season / 10000),
        });
    }
    out.sort_by_key(|x| (x.season, (x.league != "NHL") as i64));
    out
}

static NHLE_JSON: &str = include_str!("../data/nhle_factors.json");
static NHLE_CACHE: OnceLock<Mutex<HashMap<String, Arc<Value>>>> = OnceLock::new();

/// The packaged NHLe table (`valuation/nhle_factors.json`): `{"leagues": {abbrev: {factor,
/// class, reliability}}, "norm_age": {...}, "age_adjustment": {...}, "version": ...}`.
pub fn nhle_factors() -> &'static Value {
    static V: OnceLock<Value> = OnceLock::new();
    V.get_or_init(|| serde_json::from_str(NHLE_JSON).expect("nhle_factors.json is valid JSON"))
}

/// `load_nhle_factors(path)`: the packaged table when `path` is None, else that file (cached per path).
pub fn load_nhle_factors(path: Option<&std::path::Path>) -> Result<Arc<Value>> {
    let key = path.map(|p| p.display().to_string()).unwrap_or_default();
    let cache = NHLE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().map_err(|_| anyhow!("nhle cache poisoned"))?;
    if let Some(v) = guard.get(&key) {
        return Ok(v.clone());
    }
    let v: Value = match path {
        None => nhle_factors().clone(),
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
    };
    let v = Arc::new(v);
    guard.insert(key, v.clone());
    Ok(v)
}

// --------------------------------------------------------------------------- client

pub struct NhlClient {
    fetch: Box<FetchJson<'static>>,
    pub season: i64,
    pub ttl: NhlTtl,
}

impl NhlClient {
    /// `NhlClient(fetch_json, season)`; `season=None` -> `current_season(today)`.
    pub fn new(fetch_json: Box<FetchJson<'static>>, season: Option<i64>, today: NaiveDate) -> Self {
        NhlClient { fetch: fetch_json, season: season.unwrap_or_else(|| current_season(today)), ttl: NhlTtl::new(today) }
    }

    /// A client over the plain uncached [`default_fetch_json`].
    pub fn with_default_fetch(season: Option<i64>, today: NaiveDate) -> Self {
        Self::new(Box::new(default_fetch_json), season, today)
    }

    /// Install a `ttl_for(url, params)` (e.g. `providers.enrich.ttl_for`) for non-window requests.
    pub fn with_ttl_for(mut self, f: Box<dyn Fn(&str, &Params) -> f64 + Send + Sync>) -> Self {
        self.ttl.ttl_for = Some(f);
        self
    }

    /// `self.fetch_json(url, params)`; JSON null / missing -> `Value::Null` (Python `or {}`).
    fn fetch_json(&self, url: &str, params: &Params) -> Result<Value> {
        let ttl = self.ttl.ttl(url, params);
        (self.fetch)(url, params, ttl)
    }

    fn fetch_web(&self, url: &str) -> Result<Value> {
        self.fetch_json(url, &Vec::new())
    }

    // ---- stats REST -----------------------------------------------------------
    pub fn report(&self, kind: &str, report: &str, season: Option<i64>, game_type: i64, extra_filter: Option<&str>) -> Result<Vec<Value>> {
        let season = season.unwrap_or(self.season);
        let mut cay = format!("seasonId={season} and gameTypeId={game_type}");
        if let Some(f) = extra_filter.filter(|f| !f.is_empty()) {
            cay.push_str(&format!(" and {f}"));
        }
        let params: Params = vec![
            ("isAggregate".into(), "false".into()),
            ("isGame".into(), "false".into()),
            // sort must reference a field present in the report; playerId is in all of them
            ("sort".into(), r#"[{"property": "playerId", "direction": "ASC"}]"#.into()),
            ("start".into(), 0i64.into()),
            ("limit".into(), (-1i64).into()),
            ("cayenneExp".into(), cay.into()),
        ];
        let url = format!("{STATS_BASE}/{kind}/{report}");
        self.paged_rows(&url, &params, |r| g(r, "playerId").map(|v| v.to_string()), |_| true)
    }

    /// Shared `limit=-1` fetch + defensive paging used by the season and per-game reports.
    fn paged_rows(
        &self,
        url: &str,
        params: &Params,
        ident: impl Fn(&Value) -> Option<String>,
        keep: impl Fn(&Value) -> bool,
    ) -> Result<Vec<Value>> {
        let data = self.fetch_json(url, params)?;
        let mut rows: Vec<Value> = arr(&data, "data").iter().filter(|r| keep(r)).cloned().collect();
        let total = data.get("total").and_then(|t| if t.is_i64() || t.is_u64() { t.as_i64() } else { None });
        let mut seen: HashSet<Option<String>> = rows.iter().map(&ident).collect();
        while let Some(total) = total {
            if !(0 < rows.len() as i64 && (rows.len() as i64) < total) {
                break;
            }
            let mut p = params.clone();
            for (k, v) in p.iter_mut() {
                if k == "start" {
                    *v = ParamValue::Str(rows.len().to_string());
                } else if k == "limit" {
                    *v = ParamValue::Str("100".to_string());
                }
            }
            let page = self.fetch_json(url, &p)?;
            let more: Vec<Value> = arr(&page, "data")
                .iter()
                .filter(|r| keep(r) && !seen.contains(&ident(r)))
                .cloned()
                .collect();
            if more.is_empty() {
                break;
            }
            seen.extend(more.iter().map(&ident));
            rows.extend(more);
        }
        Ok(rows)
    }

    /// Raw rows of the skater summary report.
    pub fn skater_summary(&self, season: Option<i64>, game_type: i64) -> Result<Vec<Value>> {
        self.report("skater", "summary", season, game_type, None)
    }

    /// Raw rows of the realtime report (hits, blockedShots, giveaways, takeaways, missedShots...).
    pub fn skater_realtime(&self, season: Option<i64>, game_type: i64) -> Result<Vec<Value>> {
        self.report("skater", "realtime", season, game_type, None)
    }

    /// Raw rows of the faceoffwins report (totalFaceoffWins, totalFaceoffLosses...).
    pub fn skater_faceoffs(&self, season: Option<i64>, game_type: i64) -> Result<Vec<Value>> {
        self.report("skater", "faceoffwins", season, game_type, None)
    }

    /// Skater season lines merged across summary + realtime (+ faceoffs) by playerId.
    pub fn all_skaters(
        &self,
        season: Option<i64>,
        game_type: i64,
        include_realtime: bool,
        include_faceoffs: bool,
    ) -> Result<Vec<NhlSkaterSeason>> {
        let summary = self.skater_summary(season, game_type)?;
        let by_pid = |rows: &[Value]| -> Result<HashMap<i64, Value>> {
            let mut m = HashMap::new();
            for r in rows {
                m.insert(req_int(r, "playerId")?, r.clone());
            }
            Ok(m)
        };
        let realtime = if include_realtime { by_pid(&self.skater_realtime(season, game_type)?)? } else { HashMap::new() };
        let mut faceoffs: HashMap<i64, Value> = HashMap::new();
        if include_faceoffs {
            // optional report; FOW simply missing if it fails
            faceoffs = self.skater_faceoffs(season, game_type).and_then(|rows| by_pid(&rows)).unwrap_or_default();
        }
        let mut out = Vec::with_capacity(summary.len());
        for row in &summary {
            let pid = req_int(row, "playerId")?;
            let mut s = skater_from_rows(
                row,
                if include_realtime { realtime.get(&pid) } else { None },
                if include_faceoffs { faceoffs.get(&pid) } else { None },
                game_type,
            )?;
            if include_realtime && !realtime.contains_key(&pid) {
                for k in ["HIT", "BLK"] {
                    s.stats.entry(k.to_string()).or_insert(0.0);
                }
            }
            out.push(s);
        }
        Ok(out)
    }

    pub fn goalie_summary(&self, season: Option<i64>, game_type: i64) -> Result<Vec<NhlGoalieSeason>> {
        self.report("goalie", "summary", season, game_type, None)?.iter().map(|r| goalie_from_row(r, game_type)).collect()
    }

    pub fn all_goalies(&self, season: Option<i64>, game_type: i64) -> Result<Vec<NhlGoalieSeason>> {
        self.goalie_summary(season, game_type)
    }

    // ---- web API: players ---------------------------------------------------------
    pub fn player_landing(&self, player_id: i64) -> Result<NhlPlayerLanding> {
        let d = self.fetch_web(&format!("{WEB_BASE}/player/{player_id}/landing"))?;
        let d = if truthy(Some(&d)) { d } else { Value::Object(Default::default()) };
        let empty = Value::Object(Default::default());
        let sub = |v: Option<&Value>, k: &str| -> Value {
            match v.and_then(|x| x.get(k)) {
                Some(x) if truthy(Some(x)) => x.clone(),
                _ => empty.clone(),
            }
        };
        let featured_stats = sub(Some(&d), "featuredStats");
        let featured_rs = sub(Some(&featured_stats), "regularSeason");
        let featured = sub(Some(&featured_rs), "subSeason");
        let draft = sub(Some(&d), "draftDetails");
        let career_totals = sub(Some(&d), "careerTotals");
        let career = sub(Some(&career_totals), "regularSeason");
        let int = |v: Option<&Value>| -> Option<i64> {
            match v {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if s.is_empty() => None,
                other => py_int(other),
            }
        };
        let last5 = arr(&d, "last5Games").iter().map(game_log_entry).collect::<Result<Vec<_>>>()?;
        Ok(NhlPlayerLanding {
            player_id: if truthy(d.get("playerId")) { py_int(d.get("playerId")).unwrap_or(player_id) } else { player_id },
            first_name: default_str(g(&d, "firstName")).unwrap_or_default(),
            last_name: default_str(g(&d, "lastName")).unwrap_or_default(),
            team: opt_string(g(&d, "currentTeamAbbrev")),
            position: map_position(g(&d, "position").and_then(|v| v.as_str())),
            birth_date: parse_date_v(g(&d, "birthDate")),
            shoots_catches: opt_string(g(&d, "shootsCatches")),
            is_active: g(&d, "isActive").and_then(|v| v.as_bool()),
            last5,
            featured_regular_season: featured.as_object().cloned().unwrap_or_default(),
            season_totals: arr(&d, "seasonTotals").to_vec(),
            draft_year: int(draft.get("year")),
            draft_round: int(draft.get("round")),
            draft_overall: int(draft.get("overallPick")),
            draft_team: opt_string(g(&draft, "teamAbbrev")),
            career_gp: if truthy(Some(&career)) {
                int(career.get("gamesPlayed"))
            } else if truthy(Some(&d)) {
                Some(0)
            } else {
                None
            },
        })
    }

    /// Regular-season lines in every league the player played in (`landing.seasonTotals`),
    /// oldest first, with his age on Oct 1 of each season. `landing` reuses an already parsed page.
    pub fn player_history(&self, player_id: i64, landing: Option<&NhlPlayerLanding>) -> Result<Vec<LeagueSeason>> {
        match landing {
            Some(l) => Ok(parse_season_totals(&l.season_totals, l.birth_date, 2)),
            None => {
                let l = self.player_landing(player_id)?;
                Ok(parse_season_totals(&l.season_totals, l.birth_date, 2))
            }
        }
    }

    /// Per-game lines, most recent first (as returned by the API).
    pub fn game_log(&self, player_id: i64, season: Option<i64>, game_type: i64) -> Result<Vec<NhlGameLogEntry>> {
        let season = season.unwrap_or(self.season);
        let d = self.fetch_web(&format!("{WEB_BASE}/player/{player_id}/game-log/{season}/{game_type}"))?;
        let mut entries = Vec::new();
        for row in arr(&d, "gameLog") {
            let mut e = game_log_entry(row)?;
            if e.game_type.is_none() {
                e.game_type = Some(py_int(g(&d, "gameTypeId")).unwrap_or(game_type));
            }
            entries.push(e);
        }
        Ok(entries)
    }

    pub fn search_player(&self, q: &str, active: Option<bool>, limit: i64) -> Result<Vec<NhlSearchResult>> {
        let mut params: Params = vec![("culture".into(), "en-us".into()), ("limit".into(), limit.into()), ("q".into(), q.into())];
        if let Some(a) = active {
            params.push(("active".into(), if a { "true" } else { "false" }.into()));
        }
        let rows = self.fetch_json(SEARCH_URL, &params)?;
        let mut out = Vec::new();
        for r in rows.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let Some(pid) = py_int(r.get("playerId")) else { continue };
            out.push(NhlSearchResult {
                player_id: pid,
                name: opt_string(g(r, "name")).unwrap_or_default(),
                position: map_position(g(r, "positionCode").and_then(|v| v.as_str())),
                position_code: opt_string(g(r, "positionCode")),
                team: opt_string(g(r, "teamAbbrev")).filter(|s| !s.is_empty()).or_else(|| opt_string(g(r, "lastTeamAbbrev"))),
                active: g(r, "active").and_then(|v| v.as_bool()),
                sweater_number: py_int(g(r, "sweaterNumber")),
                birth_country: opt_string(g(r, "birthCountry")),
            });
        }
        Ok(out)
    }

    // ---- web API: schedules ---------------------------------------------------------
    /// Season schedule for one club; `game_type=None` keeps preseason/playoffs too.
    pub fn team_schedule(&self, team: &str, season: Option<i64>, game_type: Option<i64>) -> Result<Vec<NhlGame>> {
        let season = season.unwrap_or(self.season);
        let d = self.fetch_web(&format!("{WEB_BASE}/club-schedule-season/{team}/{season}"))?;
        let mut games = arr(&d, "games").iter().map(|r| game_from_row(r, None)).collect::<Result<Vec<_>>>()?;
        if let Some(gt) = game_type {
            games.retain(|g| g.game_type == gt);
        }
        games.sort_by_key(|g| (g.date, g.game_id));
        Ok(games)
    }

    pub fn league_games(&self, season: Option<i64>, teams: &[&str], game_type: Option<i64>) -> Result<Map<String, Vec<NhlGame>>> {
        let mut out = Map::new();
        for t in teams {
            out.insert(t.to_string(), self.team_schedule(t, season, game_type)?);
        }
        Ok(out)
    }

    /// team -> sorted game dates for all clubs (32 requests; cache upstream).
    pub fn league_schedule(&self, season: Option<i64>, teams: &[&str], game_type: Option<i64>) -> Result<Map<String, Vec<NaiveDate>>> {
        Ok(self
            .league_games(season, teams, game_type)?
            .into_iter()
            .map(|(t, games)| (t, games.into_iter().map(|g| g.date).collect()))
            .collect())
    }

    /// League schedule for the week starting at `day` (None = "now").
    pub fn week_schedule(&self, day: Option<NaiveDate>) -> Result<NhlWeekSchedule> {
        match day {
            Some(d) => self.week_schedule_key(&d.format("%Y-%m-%d").to_string()),
            None => self.week_schedule_key("now"),
        }
    }

    /// `week_schedule` with a raw key string ("now" or an ISO date).
    pub fn week_schedule_key(&self, key: &str) -> Result<NhlWeekSchedule> {
        let d = self.fetch_web(&format!("{WEB_BASE}/schedule/{key}"))?;
        let mut days: Map<NaiveDate, Vec<NhlGame>> = Map::new();
        for gw in arr(&d, "gameWeek") {
            let Some(dd) = parse_date_v(g(gw, "date")) else { continue };
            let games = arr(gw, "games").iter().map(|r| game_from_row(r, Some(dd))).collect::<Result<Vec<_>>>()?;
            days.insert(dd, games);
        }
        Ok(NhlWeekSchedule {
            days,
            regular_season_start: parse_date_v(g(&d, "regularSeasonStartDate")),
            regular_season_end: parse_date_v(g(&d, "regularSeasonEndDate")),
            next_start_date: parse_date_v(g(&d, "nextStartDate")),
            previous_start_date: parse_date_v(g(&d, "previousStartDate")),
        })
    }

    // ---- web API: rosters ---------------------------------------------------------
    pub fn team_roster(&self, team: &str, season: Option<i64>) -> Result<Vec<NhlRosterPlayer>> {
        let season = season.unwrap_or(self.season);
        let d = self.fetch_web(&format!("{WEB_BASE}/roster/{team}/{season}"))?;
        let mut out = Vec::new();
        for group in ["forwards", "defensemen", "goalies"] {
            for p in arr(&d, group) {
                let code = opt_string(g(p, "positionCode"));
                out.push(NhlRosterPlayer {
                    player_id: req_int(p, "id")?,
                    first_name: default_str(g(p, "firstName")).unwrap_or_default(),
                    last_name: default_str(g(p, "lastName")).unwrap_or_default(),
                    team: team.to_string(),
                    position: map_position(code.as_deref()),
                    position_code: code,
                    birth_date: parse_date_v(g(p, "birthDate")),
                    sweater_number: py_int(g(p, "sweaterNumber")),
                    shoots_catches: opt_string(g(p, "shootsCatches")),
                });
            }
        }
        Ok(out)
    }

    pub fn league_rosters(&self, season: Option<i64>, teams: &[&str]) -> Result<Map<String, Vec<NhlRosterPlayer>>> {
        let mut out = Map::new();
        for t in teams {
            out.insert(t.to_string(), self.team_roster(t, season)?);
        }
        Ok(out)
    }

    // ---- stats REST: per-game deployment (date windows) --------------------------------
    /// Per-game rows (`isGame=true`) of a stats report for games in [start, end].
    pub fn game_rows(
        &self,
        kind: &str,
        report: &str,
        season: Option<i64>,
        start: Option<NaiveDate>,
        end: Option<NaiveDate>,
        game_type: i64,
    ) -> Result<Vec<Value>> {
        let season = season.unwrap_or(self.season);
        let mut cay = format!("seasonId={season} and gameTypeId={game_type}");
        if let Some(extra) = game_window_filter(start, end) {
            cay.push_str(&format!(" and {extra}"));
        }
        let id_field = if kind == "team" { "teamId" } else { "playerId" };
        let sort = format!(
            r#"[{{"property": "gameDate", "direction": "ASC"}}, {{"property": "gameId", "direction": "ASC"}}, {{"property": "{id_field}", "direction": "ASC"}}]"#
        );
        let params: Params = vec![
            ("isAggregate".into(), "false".into()),
            ("isGame".into(), "true".into()),
            // every sort property must be a field of the report: gameDate / gameId / the row id are
            ("sort".into(), sort.into()),
            ("start".into(), 0i64.into()),
            ("limit".into(), (-1i64).into()),
            ("cayenneExp".into(), cay.into()),
        ];
        let url = format!("{STATS_BASE}/{kind}/{report}");
        self.paged_rows(
            &url,
            &params,
            |r| Some(format!("{}|{}", g(r, "gameId").map(|v| v.to_string()).unwrap_or_default(), g(r, id_field).map(|v| v.to_string()).unwrap_or_default())),
            |r| truthy(g(r, "gameDate")) && g(r, id_field).is_some(),
        )
    }

    /// Per-game skater ice time (all / EV / PP / SH seconds, shifts) for games in [start, end].
    pub fn skater_toi_games(&self, season: Option<i64>, start: Option<NaiveDate>, end: Option<NaiveDate>, game_type: i64) -> Result<Vec<NhlSkaterToiGame>> {
        self.game_rows("skater", "timeonice", season, start, end, game_type)?.iter().map(toi_game).collect()
    }

    /// Per-game skater power play (PP seconds, share of the team's PP time, PP goals / points).
    pub fn skater_pp_games(&self, season: Option<i64>, start: Option<NaiveDate>, end: Option<NaiveDate>, game_type: i64) -> Result<Vec<NhlSkaterPpGame>> {
        self.game_rows("skater", "powerplay", season, start, end, game_type)?.iter().map(pp_game).collect()
    }

    /// Per-game goalie lines (started, SA, SV, GA, TOI) for games in [start, end].
    pub fn goalie_games(&self, season: Option<i64>, start: Option<NaiveDate>, end: Option<NaiveDate>, game_type: i64) -> Result<Vec<NhlGoalieGame>> {
        self.game_rows("goalie", "summary", season, start, end, game_type)?.iter().map(goalie_game).collect()
    }

    /// Per-game team power-play time (seconds) and opportunities for games in [start, end].
    pub fn team_pp_toi_games(&self, season: Option<i64>, start: Option<NaiveDate>, end: Option<NaiveDate>, game_type: i64) -> Result<Vec<NhlTeamPpGame>> {
        team_pp_games(&self.game_rows("team", "powerplay", season, start, end, game_type)?)
    }

    /// player_id -> season TOI / PP deployment from the season (`isGame=false`) timeonice and
    /// powerplay reports; empty before the season's first game.
    pub fn skater_deployment_season(&self, season: Option<i64>, game_type: i64) -> Result<Map<i64, NhlSkaterDeploymentSeason>> {
        let season = season.unwrap_or(self.season);
        let toi = self.report("skater", "timeonice", Some(season), game_type, None)?;
        // PP share is optional
        let pp: HashMap<i64, Value> = self
            .report("skater", "powerplay", Some(season), game_type, None)
            .map(|rows| rows.into_iter().filter_map(|r| py_int(g(&r, "playerId")).map(|id| (id, r))).collect())
            .unwrap_or_default();
        let none = Value::Object(Default::default());
        let mut out = Map::new();
        for r in &toi {
            let Some(pid) = py_int(g(r, "playerId")) else { continue };
            let p = pp.get(&pid).unwrap_or(&none);
            let teams = split_teams(g(r, "teamAbbrevs").and_then(|v| v.as_str()));
            out.insert(
                pid,
                NhlSkaterDeploymentSeason {
                    player_id: pid,
                    season: if truthy(g(r, "seasonId")) { py_int(g(r, "seasonId")).unwrap_or(season) } else { season },
                    name: opt_string(g(r, "skaterFullName")),
                    team: teams.last().cloned(),
                    teams,
                    gp: num(g(r, "gamesPlayed")).trunc() as i64,
                    toi_per_game: secs(g(r, "timeOnIcePerGame")),
                    ev_toi_per_game: secs(g(r, "evTimeOnIcePerGame")),
                    pp_toi_per_game: secs(g(r, "ppTimeOnIcePerGame")),
                    sh_toi_per_game: secs(g(r, "shTimeOnIcePerGame")),
                    pp_share: opt_f64(g(p, "ppTimeOnIcePctPerGame")),
                },
            );
        }
        Ok(out)
    }

    // ---- preseason (gameType 1) ---------------------------------------------------------
    /// The player's preseason game log (`/game-log/{season}/1`); [] in practice as of 2026-09.
    pub fn preseason_game_logs(&self, player_id: i64, season: Option<i64>) -> Result<Vec<NhlGameLogEntry>> {
        self.game_log(player_id, season, PRESEASON)
    }

    /// Unique preseason games (gameType 1) of `teams` dated before `before` (all when None).
    pub fn preseason_games(&self, season: Option<i64>, teams: &[&str], before: Option<NaiveDate>) -> Result<Vec<NhlGame>> {
        let mut seen: Map<i64, NhlGame> = Map::new();
        for t in teams {
            for game in self.team_schedule(t, season, Some(PRESEASON))? {
                if before.map(|b| game.date < b).unwrap_or(true) {
                    seen.entry(game.game_id).or_insert(game);
                }
            }
        }
        let mut v: Vec<NhlGame> = seen.into_values().collect();
        v.sort_by_key(|g| (g.date, g.game_id));
        Ok(v)
    }

    /// Skater box-score lines of one finished game; with `strength` the game's scoring summary
    /// adds PP / SH assists and points (a failing summary only drops those).
    pub fn game_skater_lines(&self, game_id: i64, strength: bool) -> Result<Vec<NhlSkaterGameLine>> {
        let box_ = self.fetch_web(&format!("{WEB_BASE}/gamecenter/{game_id}/boxscore"))?;
        let mut lines = boxscore_skater_lines(&box_)?;
        if !lines.is_empty() && strength {
            let landing = self.fetch_web(&format!("{WEB_BASE}/gamecenter/{game_id}/landing")).unwrap_or(Value::Null);
            apply_goal_strength(&mut lines, goal_strength_points(&landing).as_ref());
        }
        Ok(lines)
    }

    /// Per-game skater lines of every finished preseason game: 1 box score + 1 scoring summary
    /// per game. Games whose box score fails are skipped.
    pub fn preseason_skater_games(&self, season: Option<i64>, teams: &[&str], before: Option<NaiveDate>) -> Result<Vec<NhlSkaterGameLine>> {
        let mut out = Vec::new();
        for game in self.preseason_games(season, teams, before)? {
            if let Ok(lines) = self.game_skater_lines(game.game_id, true) {
                out.extend(lines);
            }
        }
        Ok(out)
    }
}
