//! Port of `fantasy_manager/providers/moneypuck.py`: MoneyPuck expected-goals season summaries
//! (free CSVs, credit required: show [`CREDIT`] wherever the numbers appear).
//!
//! Network access goes through an injected `fetch_text(url, params, ttl)` callable
//! ([`fm_http::FetchText`]); [`cached_fetch_text`] routes it through `HttpCache` with the
//! 12 h (current season) / 30 d (finished seasons) TTLs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use chrono::{Datelike, NaiveDate};
use fm_core::Map;
use fm_http::{FetchText, HttpCache, Params};

pub const CREDIT: &str = "Expected goals: MoneyPuck.com";
pub const DATA_URL: &str = "https://moneypuck.com/data.htm";
pub const BASE_URL: &str = "https://moneypuck.com/moneypuck/playerData/seasonSummary";

pub const HOUR: f64 = 3600.0;
pub const TTL_CURRENT: f64 = 12.0 * HOUR;
pub const TTL_PAST: f64 = 30.0 * 24.0 * HOUR;

pub const SITUATIONS: [&str; 5] = ["all", "5on5", "5on4", "4on5", "other"];
pub const PP_SITUATION: &str = "5on4";

/// Columns each signal reads (a renamed column must skip the signal, see [`missing_columns`]).
pub const CORE_COLUMNS: [&str; 3] = ["playerId", "situation", "games_played"];
pub const SKATER_XG_COLUMNS: [&str; 2] = ["I_F_xGoals", "I_F_goals"];
pub const SKATER_ONICE_COLUMNS: [&str; 2] = ["OnIce_F_goals", "OnIce_F_shotsOnGoal"];
pub const GOALIE_GSAX_COLUMNS: [&str; 3] = ["xGoals", "goals", "ongoal"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Skaters,
    Goalies,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Skaters => "skaters",
            Kind::Goalies => "goalies",
        }
    }
}

// --------------------------------------------------------------------------- helpers

pub fn season_url(season_start_year: i64, kind: Kind) -> String {
    format!("{BASE_URL}/{season_start_year}/regular/{}.csv", kind.as_str())
}

/// 20252026 -> 2025; 2025 -> 2025.
pub fn start_year(season: i64) -> i64 {
    if season > 9999 {
        season / 10000
    } else {
        season
    }
}

fn today_local() -> NaiveDate {
    chrono::Local::now().date_naive()
}

/// Start year of the current NHL season (from September on, the upcoming one).
pub fn current_start_year(today: Option<NaiveDate>) -> i64 {
    let t = today.unwrap_or_else(today_local);
    if t.month() >= 9 {
        t.year() as i64
    } else {
        t.year() as i64 - 1
    }
}

/// 12 h for the current (or a future) season's file, 30 d for finished seasons.
pub fn ttl_for_url(url: &str, today: Option<NaiveDate>) -> f64 {
    let cur = current_start_year(today);
    for part in url.split('/') {
        if part.len() == 4 && part.bytes().all(|b| b.is_ascii_digit()) {
            let y: i64 = part.parse().unwrap_or(0);
            return if y >= cur { TTL_CURRENT } else { TTL_PAST };
        }
    }
    TTL_CURRENT
}

/// Plain uncached GET returning the response text (`default_fetch_text`).
pub fn default_fetch_text(url: &str, params: &Params, _ttl: f64) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent("fantasy-manager/0.1")
        .build()?;
    let mut req = client.get(url);
    let q: Vec<(&str, &str)> = params
        .iter()
        .filter_map(|(k, v)| match v {
            fm_http::cache::ParamValue::Str(s) => Some((k.as_str(), s.as_str())),
            _ => None,
        })
        .collect();
    if !q.is_empty() {
        req = req.query(&q);
    }
    Ok(req.send()?.error_for_status()?.text()?)
}

/// `cached_fetch_text(cache, today)`: fetch_text through `HttpCache::get_text` with the
/// season-dependent TTL (the incoming ttl is ignored, like Python's two-argument callable).
pub fn cached_fetch_text<'a>(
    cache: &'a HttpCache,
    today: Option<NaiveDate>,
) -> impl Fn(&str, &Params, f64) -> Result<String> + Send + Sync + 'a {
    move |url: &str, params: &Params, _ttl: f64| cache.get_text(url, params, &[], &[], ttl_for_url(url, today))
}

/// Python `float(str)` (whitespace allowed); None when it does not parse.
fn py_float(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Python `int(str)`.
fn py_int(s: &str) -> Result<i64> {
    s.trim()
        .parse::<i64>()
        .map_err(|_| anyhow!("invalid literal for int() with base 10: {s:?}"))
}

pub type Row = HashMap<String, String>;

/// `_f(row, key)`: missing / empty / invalid -> 0.0.
fn f(row: &Row, key: &str) -> f64 {
    match row.get(key) {
        None => 0.0,
        Some(v) if v.is_empty() => 0.0,
        Some(v) => py_float(v).unwrap_or(0.0),
    }
}

/// `csv.reader` rows of a text (blank lines skipped; ragged rows allowed).
fn csv_records(text: &str) -> Vec<Vec<String>> {
    let mut rdr = csv::ReaderBuilder::new().has_headers(false).flexible(true).from_reader(text.as_bytes());
    rdr.records().filter_map(|r| r.ok()).map(|r| r.iter().map(|c| c.to_string()).collect()).collect()
}

fn strip_bom(text: &str) -> &str {
    text.trim_start_matches('\u{feff}')
}

/// Header columns of a MoneyPuck CSV (None for an empty response without a header).
pub fn csv_columns(text: &str) -> Option<Vec<String>> {
    let text = strip_bom(text);
    if text.trim().is_empty() {
        return None;
    }
    // csv.reader yields [] for a leading blank line -> falsy
    if text.starts_with('\n') || text.starts_with('\r') {
        return None;
    }
    let first = csv_records(text).into_iter().next()?;
    if first.is_empty() {
        return None;
    }
    Some(first.iter().map(|c| c.trim().to_string()).collect())
}

/// The `required` columns absent from the CSV header ([] for an empty response).
pub fn missing_columns(text: &str, required: &[&str]) -> Vec<String> {
    let Some(cols) = csv_columns(text) else { return Vec::new() };
    required.iter().filter(|c| !cols.iter().any(|h| h == *c)).map(|c| c.to_string()).collect()
}

fn ratio(num: f64, den: f64) -> Option<f64> {
    if den > 0.0 {
        Some(num / den)
    } else {
        None
    }
}

fn team(code: Option<&str>) -> Option<String> {
    let c = code.unwrap_or("").trim();
    let fixed = match c {
        "L.A" => "LAK",
        "N.J" => "NJD",
        "S.J" => "SJS",
        "T.B" => "TBL",
        other => other,
    };
    if fixed.is_empty() {
        None
    } else {
        Some(fixed.to_string())
    }
}

/// `nhl.map_position` (private copy: C/L/LW/R/RW/D/G -> C/LW/RW/D/G).
fn map_position(code: Option<&str>) -> Option<String> {
    let c = code?;
    if c.is_empty() {
        return None;
    }
    match c.trim().to_uppercase().as_str() {
        "C" => Some("C"),
        "L" | "LW" => Some("LW"),
        "R" | "RW" => Some("RW"),
        "D" => Some("D"),
        "G" => Some("G"),
        _ => None,
    }
    .map(str::to_string)
}

// --------------------------------------------------------------------------- rows

/// One skater-season-situation row in canonical units (totals; TOI in minutes).
#[derive(Clone, Debug, PartialEq)]
pub struct MpSkater {
    pub nhl_id: i64,
    /// start year (2025 = 2025-26)
    pub season: i64,
    pub name: String,
    pub team: Option<String>,
    pub position: Option<String>,
    pub situation: String,
    pub gp: i64,
    pub toi_min: f64,
    pub ixg: f64,
    pub goals: f64,
    pub sog: f64,
    pub ixg_adj: f64,
    pub shot_attempts: f64,
    pub points: f64,
    pub primary_assists: f64,
    pub secondary_assists: f64,
    pub onice_xg_pct: Option<f64>,
    pub onice_corsi_pct: Option<f64>,
    pub onice_gf: f64,
    pub onice_sog_for: f64,
    pub onice_xgf: f64,
    pub onice_ga: f64,
    pub onice_sog_against: f64,
    pub onice_sh_pct: Option<f64>,
    pub onice_sv_pct: Option<f64>,
    pub pp_toi_min: Option<f64>,
    pub pp_ixg: Option<f64>,
    pub pp_goals: Option<f64>,
    pub pp_points: Option<f64>,
}

impl MpSkater {
    pub fn group(&self) -> &'static str {
        if self.position.as_deref() == Some("D") {
            "D"
        } else {
            "F"
        }
    }
    pub fn assists(&self) -> f64 {
        self.primary_assists + self.secondary_assists
    }
    pub fn goals_minus_ixg(&self) -> f64 {
        self.goals - self.ixg
    }
    pub fn ixg_per_game(&self) -> Option<f64> {
        ratio(self.ixg, self.gp as f64)
    }
    pub fn sh_pct(&self) -> Option<f64> {
        ratio(self.goals, self.sog)
    }
    /// ixG per shot on goal: the shooting % his shot quality implies.
    pub fn ixg_per_shot(&self) -> Option<f64> {
        ratio(self.ixg, self.sog)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MpGoalie {
    pub nhl_id: i64,
    pub season: i64,
    pub name: String,
    pub team: Option<String>,
    pub situation: String,
    pub gp: i64,
    pub toi_min: f64,
    /// xGoals against
    pub xga: f64,
    /// goals against
    pub ga: f64,
    /// shots on goal against (`ongoal`)
    pub sa: f64,
    pub unblocked_attempts: f64,
}

impl MpGoalie {
    pub fn sv_pct(&self) -> Option<f64> {
        if self.sa <= 0.0 {
            None
        } else {
            Some(1.0 - self.ga / self.sa)
        }
    }
    /// Save % an average goalie would post on the same shots.
    pub fn xsv_pct(&self) -> Option<f64> {
        if self.sa <= 0.0 {
            None
        } else {
            Some(1.0 - self.xga / self.sa)
        }
    }
    /// Goals saved above expected (positive = better than expected).
    pub fn gsax(&self) -> f64 {
        self.xga - self.ga
    }
}

fn opt_float(row: &Row, key: &str) -> Result<Option<f64>> {
    match row.get(key) {
        None => Ok(None),
        Some(v) if v.is_empty() => Ok(None),
        Some(v) => py_float(v)
            .map(Some)
            .ok_or_else(|| anyhow!("could not convert string to float: {v:?}")),
    }
}

fn req_id(row: &Row) -> Result<i64> {
    py_int(row.get("playerId").ok_or_else(|| anyhow!("KeyError: 'playerId'"))?)
}

fn text_of(row: &Row, key: &str) -> String {
    row.get(key).map(|s| s.trim().to_string()).unwrap_or_default()
}

pub fn parse_skater_row(row: &Row) -> Result<MpSkater> {
    let ogf = f(row, "OnIce_F_goals");
    let osf = f(row, "OnIce_F_shotsOnGoal");
    let oga = f(row, "OnIce_A_goals");
    let osa = f(row, "OnIce_A_shotsOnGoal");
    Ok(MpSkater {
        nhl_id: req_id(row)?,
        season: f(row, "season") as i64,
        name: text_of(row, "name"),
        team: team(row.get("team").map(|s| s.as_str())),
        position: map_position(row.get("position").map(|s| s.as_str())),
        situation: text_of(row, "situation"),
        gp: f(row, "games_played") as i64,
        toi_min: f(row, "icetime") / 60.0,
        ixg: f(row, "I_F_xGoals"),
        goals: f(row, "I_F_goals"),
        sog: f(row, "I_F_shotsOnGoal"),
        ixg_adj: f(row, "I_F_scoreVenueAdjustedxGoals"),
        shot_attempts: f(row, "I_F_shotAttempts"),
        points: f(row, "I_F_points"),
        primary_assists: f(row, "I_F_primaryAssists"),
        secondary_assists: f(row, "I_F_secondaryAssists"),
        onice_xg_pct: opt_float(row, "onIce_xGoalsPercentage")?,
        onice_corsi_pct: opt_float(row, "onIce_corsiPercentage")?,
        onice_gf: ogf,
        onice_sog_for: osf,
        onice_xgf: f(row, "OnIce_F_xGoals"),
        onice_ga: oga,
        onice_sog_against: osa,
        onice_sh_pct: ratio(ogf, osf),
        onice_sv_pct: if osa <= 0.0 { None } else { Some(1.0 - oga / osa) },
        pp_toi_min: None,
        pp_ixg: None,
        pp_goals: None,
        pp_points: None,
    })
}

pub fn parse_goalie_row(row: &Row) -> Result<MpGoalie> {
    Ok(MpGoalie {
        nhl_id: req_id(row)?,
        season: f(row, "season") as i64,
        name: text_of(row, "name"),
        team: team(row.get("team").map(|s| s.as_str())),
        situation: text_of(row, "situation"),
        gp: f(row, "games_played") as i64,
        toi_min: f(row, "icetime") / 60.0,
        xga: f(row, "xGoals"),
        ga: f(row, "goals"),
        sa: f(row, "ongoal"),
        unblocked_attempts: f(row, "unblocked_shot_attempts"),
    })
}

/// Rows of a MoneyPuck CSV (a header-only file -> []); rows without a `playerId` are dropped.
pub fn read_csv(text: &str) -> Vec<Row> {
    let text = strip_bom(text);
    let mut recs = csv_records(text).into_iter();
    let Some(header) = recs.next() else { return Vec::new() };
    recs.filter_map(|rec| {
        let mut row = Row::with_capacity(header.len());
        for (h, v) in header.iter().zip(rec) {
            row.insert(h.clone(), v);
        }
        match row.get("playerId") {
            Some(v) if !v.is_empty() => Some(row),
            _ => None,
        }
    })
    .collect()
}

/// Skater rows of one situation (`None` = every situation). `with_pp` fills the `pp_*` fields of
/// each row from the same player's 5on4 row.
pub fn parse_skaters(text: &str, situation: Option<&str>, with_pp: bool) -> Result<Vec<MpSkater>> {
    let rows = read_csv(text).iter().map(parse_skater_row).collect::<Result<Vec<_>>>()?;
    let pp: HashMap<i64, (f64, f64, f64, f64)> = if with_pp {
        rows.iter()
            .filter(|r| r.situation == PP_SITUATION)
            .map(|r| (r.nhl_id, (r.toi_min, r.ixg, r.goals, r.points)))
            .collect()
    } else {
        HashMap::new()
    };
    let mut out: Vec<MpSkater> =
        rows.into_iter().filter(|r| situation.is_none_or(|s| r.situation == s)).collect();
    if with_pp {
        for r in &mut out {
            let (a, b, c, d) = pp.get(&r.nhl_id).copied().unwrap_or((0.0, 0.0, 0.0, 0.0));
            r.pp_toi_min = Some(a);
            r.pp_ixg = Some(b);
            r.pp_goals = Some(c);
            r.pp_points = Some(d);
        }
    }
    Ok(out)
}

pub fn parse_goalies(text: &str, situation: Option<&str>) -> Result<Vec<MpGoalie>> {
    let mut out = Vec::new();
    for r in read_csv(text) {
        let g = parse_goalie_row(&r)?;
        if situation.is_none_or(|s| g.situation == s) {
            out.push(g);
        }
    }
    Ok(out)
}

/// Pooled on-ice shooting % by position group (F / D) over skaters with >= `min_toi_min`
/// minutes. Pass rows of one situation (normally `all`).
pub fn onice_sh_norms<'a>(rows: impl IntoIterator<Item = &'a MpSkater>, min_toi_min: f64) -> Map<String, f64> {
    let mut g: Map<String, f64> = Map::new();
    let mut s: Map<String, f64> = Map::new();
    for r in rows {
        if r.toi_min < min_toi_min {
            continue;
        }
        *g.entry(r.group().to_string()).or_insert(0.0) += r.onice_gf;
        *s.entry(r.group().to_string()).or_insert(0.0) += r.onice_sog_for;
    }
    g.iter()
        .filter(|(grp, _)| s.get(*grp).copied().unwrap_or(0.0) > 0.0)
        .map(|(grp, gv)| (grp.clone(), gv / s[grp]))
        .collect()
}

/// League goals / ixG by position group (F / D) over one season's rows (situation `all`).
pub fn xg_calibration<'a>(rows: impl IntoIterator<Item = &'a MpSkater>) -> Map<String, f64> {
    let mut g: Map<String, f64> = Map::new();
    let mut x: Map<String, f64> = Map::new();
    for r in rows {
        *g.entry(r.group().to_string()).or_insert(0.0) += r.goals;
        *x.entry(r.group().to_string()).or_insert(0.0) += r.ixg;
    }
    g.iter()
        .filter(|(grp, _)| x.get(*grp).copied().unwrap_or(0.0) > 0.0)
        .map(|(grp, gv)| (grp.clone(), gv / x[grp]))
        .collect()
}

/// League goals against / xGoals against over one season's goalie rows (situation `all`),
/// clamped to `bounds`; 1.0 when the rows hold fewer than `min_shots` shots on goal.
pub fn goalie_calibration<'a>(
    rows: impl IntoIterator<Item = &'a MpGoalie>,
    min_shots: f64,
    bounds: (f64, f64),
) -> f64 {
    let (mut ga, mut xga, mut sa) = (0.0, 0.0, 0.0);
    for r in rows {
        ga += r.ga;
        xga += r.xga;
        sa += r.sa;
    }
    if sa < min_shots || xga <= 0.0 {
        return 1.0;
    }
    let (lo, hi) = bounds;
    hi.min(lo.max(ga / xga))
}

// --------------------------------------------------------------------------- client

fn check_situation(situation: Option<&str>) -> Result<()> {
    if let Some(s) = situation {
        if !SITUATIONS.contains(&s) {
            return Err(anyhow!("unknown situation {s:?}; choose from {}", SITUATIONS.join(", ")));
        }
    }
    Ok(())
}

/// Season summaries from MoneyPuck. Raw CSV text is memoised per (kind, year) so asking for
/// several situations of one season downloads (or reads the cache) once.
pub struct MoneyPuckClient<'a> {
    fetch_text: Box<FetchText<'a>>,
    today: Option<NaiveDate>,
    text: Mutex<HashMap<(Kind, i64), Arc<String>>>,
}

impl<'a> MoneyPuckClient<'a> {
    pub fn new(fetch_text: Box<FetchText<'a>>) -> Self {
        MoneyPuckClient { fetch_text, today: None, text: Mutex::new(HashMap::new()) }
    }

    /// Uncached client (`MoneyPuckClient()`).
    pub fn with_default_fetch() -> Self {
        Self::new(Box::new(default_fetch_text))
    }

    /// `MoneyPuckClient.from_cache(cache, today)`.
    pub fn from_cache(cache: &'a HttpCache, today: Option<NaiveDate>) -> Self {
        let mut c = Self::new(Box::new(cached_fetch_text(cache, today)));
        c.today = today;
        c
    }

    pub fn raw(&self, season_start_year: i64, kind: Kind) -> Result<Arc<String>> {
        let key = (kind, start_year(season_start_year));
        if let Some(t) = self.text.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return Ok(t.clone());
        }
        let url = season_url(key.1, kind);
        let t = Arc::new((self.fetch_text)(&url, &Vec::new(), ttl_for_url(&url, self.today))?);
        self.text.lock().unwrap_or_else(|e| e.into_inner()).insert(key, t.clone());
        Ok(t)
    }

    /// Skater rows of season `season_start_year` (2025 = 2025-26; NHL ids like 20252026 are
    /// accepted too) for one `situation` (`None` = all situations).
    pub fn skaters(&self, season_start_year: i64, situation: Option<&str>, with_pp: bool) -> Result<Vec<MpSkater>> {
        check_situation(situation)?;
        parse_skaters(&self.raw(season_start_year, Kind::Skaters)?, situation, with_pp)
    }

    pub fn goalies(&self, season_start_year: i64, situation: Option<&str>) -> Result<Vec<MpGoalie>> {
        check_situation(situation)?;
        parse_goalies(&self.raw(season_start_year, Kind::Goalies)?, situation)
    }

    /// Required columns missing from one season file's header.
    pub fn missing(&self, season_start_year: i64, kind: Kind, required: &[&str]) -> Result<Vec<String>> {
        Ok(missing_columns(&self.raw(season_start_year, kind)?, required))
    }

    pub fn skaters_by_id(&self, season_start_year: i64, situation: &str, with_pp: bool) -> Result<Map<i64, MpSkater>> {
        Ok(self.skaters(season_start_year, Some(situation), with_pp)?.into_iter().map(|r| (r.nhl_id, r)).collect())
    }
}
