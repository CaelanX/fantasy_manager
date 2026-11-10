//! Rust port of `fantasy_manager/harness/deployment.py`: NHL deployment in the ledger (per-game
//! ice time, power-play share and goalie starts), skater summaries and goalie start counts.
//! Owner: work package `W3-harness-deploy-data`.
//!
//! Deviations:
//! * `pull_deployment` takes any [`DeploymentSource`] (implemented for `NhlClient`) so tests can
//!   feed synthetic rows like Python's `FakeNhl`.
//! * The three back-to-back constants are copied from `valuation/schedule.py`.
//!   TODO(W3-valuation-schedule merge): re-point to `fm_engine::valuation::schedule`.
//! * `summarize_rows` works on ledger `Row`s (JSON maps); `prior` is a `Map<String, Option<f64>>`
//!   so "a dict with None values" stays truthy like in Python.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use chrono::{Datelike, Duration, NaiveDate};
use fm_core::Map;
use fm_pycompat::num::{py_sum, round_to};
use fm_sources::nhl::{
    current_season, NhlClient, NhlGoalieGame, NhlSkaterPpGame, NhlSkaterToiGame, NhlTeamPpGame, NhlWeekSchedule,
};
use rusqlite::types::ToSql;
use serde_json::Value;

use crate::ledger::{now_iso, Ledger, Row};

pub const DEFAULT_LAST_N: usize = 5;
pub const MIN_BASE_GP: usize = 5;
pub const PP_UNITS_ON_ICE: f64 = 5.0;

// valuation/schedule.py
pub use fm_engine::valuation::schedule::{B2B_MIN_SAMPLE, B2B_SECOND_NIGHT_P, B2B_SHRINK_K};

pub const SUMMARY_KEYS: [&str; 13] = [
    "gp",
    "toi_avg",
    "pp_toi_avg",
    "pp_share_avg",
    "season_toi",
    "season_pp_toi",
    "season_pp_share",
    "baseline_toi",
    "baseline_pp_share",
    "baseline_source",
    "toi_trend",
    "pp_share_trend",
    "last_n",
];

/// `{"toi": minutes, "pp_share": 0..1}` (e.g. last season): the trend baseline.
pub type Prior = Map<String, Option<f64>>;

/// team -> game dates (`ctx.schedule`).
pub type Schedule = Map<String, Vec<NaiveDate>>;

pub fn season_start_date(as_of: NaiveDate) -> NaiveDate {
    let y = if as_of.month() >= 9 { as_of.year() } else { as_of.year() - 1 };
    NaiveDate::from_ymd_opt(y, 9, 1).expect("valid date")
}

fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// `_min(seconds)`: minutes rounded to 4 places.
fn minutes(seconds: Option<f64>) -> Option<f64> {
    seconds.map(|s| round_to(s / 60.0, 4))
}

fn days(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let n = (end - start).num_days();
    (0..=n).map(|i| start + Duration::days(i)).collect()
}

fn opt_f(x: Option<f64>) -> Value {
    x.map(Value::from).unwrap_or(Value::Null)
}

fn opt_s(x: &Option<String>) -> Value {
    x.as_ref().map(|s| Value::from(s.clone())).unwrap_or(Value::Null)
}

fn opt_i(x: Option<i64>) -> Value {
    x.map(Value::from).unwrap_or(Value::Null)
}

/// Python truthiness of an optional team string.
fn team_of(t: &Option<String>) -> Option<&str> {
    t.as_deref().filter(|s| !s.is_empty())
}

// --------------------------------------------------------------------------- pull

/// What `pull_deployment` needs from the NHL client.
pub trait DeploymentSource {
    fn skater_toi_games(&self, season: i64, start: NaiveDate, end: NaiveDate) -> Result<Vec<NhlSkaterToiGame>>;
    fn skater_pp_games(&self, season: i64, start: NaiveDate, end: NaiveDate) -> Result<Vec<NhlSkaterPpGame>>;
    fn team_pp_toi_games(&self, season: i64, start: NaiveDate, end: NaiveDate) -> Result<Vec<NhlTeamPpGame>>;
    fn goalie_games(&self, season: i64, start: NaiveDate, end: NaiveDate) -> Result<Vec<NhlGoalieGame>>;
    fn week_schedule(&self, day: NaiveDate) -> Result<NhlWeekSchedule>;
}

impl DeploymentSource for NhlClient {
    fn skater_toi_games(&self, season: i64, start: NaiveDate, end: NaiveDate) -> Result<Vec<NhlSkaterToiGame>> {
        NhlClient::skater_toi_games(self, Some(season), Some(start), Some(end), 2)
    }
    fn skater_pp_games(&self, season: i64, start: NaiveDate, end: NaiveDate) -> Result<Vec<NhlSkaterPpGame>> {
        NhlClient::skater_pp_games(self, Some(season), Some(start), Some(end), 2)
    }
    fn team_pp_toi_games(&self, season: i64, start: NaiveDate, end: NaiveDate) -> Result<Vec<NhlTeamPpGame>> {
        NhlClient::team_pp_toi_games(self, Some(season), Some(start), Some(end), 2)
    }
    fn goalie_games(&self, season: i64, start: NaiveDate, end: NaiveDate) -> Result<Vec<NhlGoalieGame>> {
        NhlClient::goalie_games(self, Some(season), Some(start), Some(end), 2)
    }
    fn week_schedule(&self, day: NaiveDate) -> Result<NhlWeekSchedule> {
        NhlClient::week_schedule(self, Some(day))
    }
}

/// Teams with a goalie row on `day` when that date was pulled, else None (unknown).
fn teams_on(ledger: &Ledger, day: NaiveDate) -> Result<Option<BTreeSet<String>>> {
    let d = iso(day);
    if ledger.count("deployment_pulls", "WHERE game_date=?", &[&d])? == 0 {
        return Ok(None);
    }
    let rows = ledger.query("SELECT DISTINCT team FROM goalie_starts WHERE game_date=?", &[&d])?;
    Ok(Some(
        rows.iter().filter_map(|r| r.get("team").and_then(|t| t.as_str())).filter(|t| !t.is_empty()).map(String::from).collect(),
    ))
}

/// Teams with a regular-season game on `day`: the given schedule, rows pulled in this call, the
/// ledger (a pulled date), else the NHL week schedule (best effort; None if unknown).
fn teams_playing(
    day: NaiveDate,
    ledger: &Ledger,
    nhl: &dyn DeploymentSource,
    schedule: Option<&Schedule>,
    pulled: &BTreeMap<NaiveDate, BTreeSet<String>>,
) -> Result<Option<BTreeSet<String>>> {
    if let Some(s) = schedule.filter(|s| !s.is_empty()) {
        return Ok(Some(s.iter().filter(|(_, ds)| ds.contains(&day)).map(|(t, _)| t.clone()).collect()));
    }
    if let Some(p) = pulled.get(&day) {
        return Ok(Some(p.clone()));
    }
    if let Some(got) = teams_on(ledger, day)? {
        return Ok(Some(got));
    }
    Ok(match nhl.week_schedule(day) {
        Ok(wk) => Some(
            wk.days
                .get(&day)
                .map(|gs| gs.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter(|g| g.game_type == 2)
                .flat_map(|g| [g.away.clone(), g.home.clone()])
                .collect(),
        ),
        Err(_) => None, // the flag is optional
    })
}

/// Store every skater's and goalie's per-game deployment for games in [day, end or day].
///
/// Idempotent (upserts keyed by player and game date; re-pulling a day in progress overwrites
/// it). `schedule` (team -> game dates, e.g. `ctx.schedule`) decides `back_to_back`; without it
/// the ledger / NHL week schedule is used. Returns counts per kind.
///
/// `record_empty=false` (the daily harness run): a date without rows is marked pulled in
/// `deployment_pulls` only when it is complete (see the Python docstring); incomplete dates are
/// listed under `"not_final"`.
pub fn pull_deployment(
    ledger: &Ledger,
    nhl: &dyn DeploymentSource,
    day: NaiveDate,
    end: Option<NaiveDate>,
    season: Option<i64>,
    schedule: Option<&Schedule>,
    record_empty: bool,
) -> Result<Value> {
    let end = end.unwrap_or(day);
    let season = season.filter(|s| *s != 0).unwrap_or_else(|| current_season(day));
    let schedule = schedule.filter(|s| !s.is_empty());
    let toi = nhl.skater_toi_games(season, day, end)?;
    let pp: HashMap<(i64, NaiveDate), NhlSkaterPpGame> = match nhl.skater_pp_games(season, day, end) {
        Ok(rows) => rows.into_iter().map(|r| ((r.player_id, r.date), r)).collect(),
        Err(_) => HashMap::new(), // the timeonice report already carries PP seconds
    };
    let mut team_pp: HashMap<(String, NaiveDate), f64> = HashMap::new();
    let mut source = "derived";
    match nhl.team_pp_toi_games(season, day, end) {
        Ok(rows) => {
            for t in rows {
                if let (Some(team), Some(p)) = (team_of(&t.team), t.pp_toi) {
                    team_pp.insert((team.to_string(), t.date), p);
                }
            }
            if !team_pp.is_empty() {
                source = "report";
            }
        }
        Err(_) => team_pp.clear(),
    }
    let mut derived: HashMap<(String, NaiveDate), f64> = HashMap::new();
    for r in &toi {
        if let Some(t) = team_of(&r.team) {
            *derived.entry((t.to_string(), r.date)).or_insert(0.0) += r.pp_toi.unwrap_or(0.0);
        }
    }
    let goalies = nhl.goalie_games(season, day, end)?;

    let now = now_iso();
    let mut rows: Vec<Row> = Vec::with_capacity(toi.len());
    for r in &toi {
        let key = r.team.clone().map(|t| (t, r.date));
        let mut tpp: Option<f64> = key.as_ref().and_then(|k| team_pp.get(k)).copied();
        if tpp.is_none() {
            if let Some(t) = team_of(&r.team) {
                tpp = Some(derived.get(&(t.to_string(), r.date)).copied().unwrap_or(0.0) / PP_UNITS_ON_ICE);
            }
        }
        let pp_row = pp.get(&(r.player_id, r.date));
        let own = match r.pp_toi {
            Some(v) => Some(v),
            None => pp_row.and_then(|p| p.pp_toi),
        };
        let mut share: Option<f64> = None;
        match (own, tpp) {
            (Some(o), Some(t)) if t != 0.0 => share = Some((o / t).max(0.0).min(1.0)),
            _ => {
                if let (Some(p), None) = (pp_row, tpp) {
                    share = p.pp_share;
                }
            }
        }
        let mut row = Row::new();
        row.insert("nhl_id".into(), r.player_id.into());
        row.insert("game_date".into(), iso(r.date).into());
        row.insert("team".into(), opt_s(&r.team));
        row.insert("game_id".into(), opt_i(r.game_id));
        row.insert("opponent".into(), opt_s(&r.opponent));
        row.insert("toi".into(), opt_f(minutes(r.toi)));
        row.insert("ev_toi".into(), opt_f(minutes(r.ev_toi)));
        row.insert("pp_toi".into(), opt_f(minutes(own)));
        row.insert("sh_toi".into(), opt_f(minutes(r.sh_toi)));
        row.insert("shifts".into(), opt_i(r.shifts));
        row.insert("team_pp_toi".into(), opt_f(minutes(tpp)));
        row.insert("pp_share".into(), opt_f(share.map(|s| round_to(s, 4))));
        row.insert("pulled_at".into(), now.clone().into());
        rows.push(row);
    }
    ledger.upsert("deployment_daily", &rows, &["nhl_id", "game_date"])?;

    let mut pulled: BTreeMap<NaiveDate, BTreeSet<String>> = BTreeMap::new();
    for g in &goalies {
        if let Some(t) = team_of(&g.team) {
            pulled.entry(g.date).or_default().insert(t.to_string());
        }
    }
    for r in &toi {
        if let Some(t) = team_of(&r.team) {
            pulled.entry(r.date).or_default().insert(t.to_string());
        }
    }
    let mut grows: Vec<Row> = Vec::with_capacity(goalies.len());
    let mut prev_cache: HashMap<NaiveDate, Option<BTreeSet<String>>> = HashMap::new();
    for g in &goalies {
        let prev = g.date - Duration::days(1);
        if !prev_cache.contains_key(&prev) {
            let v = if day <= prev && prev <= end && schedule.is_none() {
                Some(pulled.get(&prev).cloned().unwrap_or_default())
            } else {
                teams_playing(prev, ledger, nhl, schedule, &pulled)?
            };
            prev_cache.insert(prev, v);
        }
        let teams_prev = &prev_cache[&prev];
        let b2b: Option<i64> = match (teams_prev, team_of(&g.team)) {
            (Some(tp), Some(t)) => Some(tp.contains(t) as i64),
            _ => None,
        };
        let mut row = Row::new();
        row.insert("nhl_id".into(), g.player_id.into());
        row.insert("game_date".into(), iso(g.date).into());
        row.insert("team".into(), opt_s(&g.team));
        row.insert("game_id".into(), opt_i(g.game_id));
        row.insert("opponent".into(), opt_s(&g.opponent));
        row.insert("started".into(), (g.started as i64).into());
        row.insert("sa".into(), Value::from(g.sa));
        row.insert("sv".into(), Value::from(g.sv));
        row.insert("ga".into(), Value::from(g.ga));
        row.insert("toi".into(), opt_f(minutes(g.toi)));
        row.insert("back_to_back".into(), opt_i(b2b));
        row.insert("pulled_at".into(), now.clone().into());
        grows.push(row);
    }
    ledger.upsert("goalie_starts", &grows, &["nhl_id", "game_date"])?;

    let mut per_day_s: HashMap<String, i64> = HashMap::new();
    let mut per_day_g: HashMap<String, i64> = HashMap::new();
    for r in &rows {
        *per_day_s.entry(r["game_date"].as_str().unwrap_or("").to_string()).or_insert(0) += 1;
    }
    for r in &grows {
        *per_day_g.entry(r["game_date"].as_str().unwrap_or("").to_string()).or_insert(0) += 1;
    }
    let mut marked: Vec<Row> = Vec::new();
    let mut not_final: Vec<Value> = Vec::new();
    let empty = BTreeSet::new();
    for d in days(day, end) {
        let key = iso(d);
        if !record_empty {
            // final = every team the schedule has playing that day is in the reports (no
            // schedule: any rows at all); otherwise leave the date unmarked
            let seen = pulled.get(&d).unwrap_or(&empty);
            let fin = match schedule {
                Some(s) => s.iter().filter(|(_, ds)| ds.contains(&d)).all(|(t, _)| seen.contains(t)),
                None => !seen.is_empty(),
            };
            if !fin {
                not_final.push(Value::from(key));
                continue;
            }
        }
        let mut m = Row::new();
        m.insert("game_date".into(), key.clone().into());
        m.insert("n_skaters".into(), per_day_s.get(&key).copied().unwrap_or(0).into());
        m.insert("n_goalies".into(), per_day_g.get(&key).copied().unwrap_or(0).into());
        m.insert("pulled_at".into(), now.clone().into());
        marked.push(m);
    }
    ledger.upsert("deployment_pulls", &marked, &["game_date"])?;
    let mut out = Row::new();
    out.insert("start".into(), iso(day).into());
    out.insert("end".into(), iso(end).into());
    out.insert("skaters".into(), (rows.len() as i64).into());
    out.insert("goalies".into(), (grows.len() as i64).into());
    out.insert("team_pp_source".into(), if rows.is_empty() { Value::Null } else { source.into() });
    if !record_empty {
        out.insert("not_final".into(), Value::Array(not_final));
    }
    Ok(Value::Object(out))
}

// --------------------------------------------------------------------------- skater summary

fn num(r: &Row, k: &str) -> Option<f64> {
    r.get(k).and_then(|v| v.as_f64())
}

/// `_mean`: builtin `sum` (compensated) over the non-None values.
fn mean(xs: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let vals: Vec<f64> = xs.flatten().collect();
    if vals.is_empty() {
        None
    } else {
        Some(py_sum(vals.iter().copied()) / vals.len() as f64)
    }
}

/// Time-weighted PP share: sum of own PP minutes / sum of team PP minutes (games with a PP).
fn pp_ratio(rows: &[Row]) -> Option<f64> {
    let (mut own, mut team) = (0.0, 0.0);
    for r in rows {
        let tpp = num(r, "team_pp_toi");
        if let Some(t) = tpp.filter(|t| *t != 0.0) {
            own += num(r, "pp_toi").unwrap_or(0.0);
            team += t;
        }
    }
    if team > 0.0 {
        Some((own / team).min(1.0))
    } else {
        None
    }
}

fn r_(x: Option<f64>, nd: i32) -> Option<f64> {
    x.map(|v| round_to(v, nd))
}

/// Summary of one skater's season rows; see the module docstring of the Python file.
/// `prior` is the trend baseline when fewer than `MIN_BASE_GP` games precede the recent window.
pub fn summarize_rows(rows: &[Row], last_n: usize, prior: Option<&Prior>) -> Row {
    let mut sorted: Vec<Row> = rows.to_vec();
    sorted.sort_by(|a, b| {
        a.get("game_date").and_then(|v| v.as_str()).unwrap_or("").cmp(b.get("game_date").and_then(|v| v.as_str()).unwrap_or(""))
    });
    let rows = sorted;
    let mut out = Row::new();
    for k in SUMMARY_KEYS {
        out.insert(k.to_string(), Value::Null);
    }
    out.insert("gp".into(), (rows.len() as i64).into());
    out.insert("last_n".into(), (last_n as i64).into());
    if rows.is_empty() {
        return out;
    }
    // rows[-last_n:] / rows[:-last_n] (with Python's -0 quirk)
    let recent: &[Row] = if last_n == 0 { &rows } else { &rows[rows.len().saturating_sub(last_n)..] };
    let before: &[Row] = if rows.len() > last_n { &rows[..rows.len() - last_n] } else { &[] };
    let toi_avg = r_(mean(recent.iter().map(|r| num(r, "toi"))), 3);
    let pp_share_avg = r_(pp_ratio(recent), 4);
    let set = |out: &mut Row, k: &str, v: Option<f64>| {
        out.insert(k.to_string(), opt_f(v));
    };
    set(&mut out, "toi_avg", toi_avg);
    set(&mut out, "pp_toi_avg", r_(mean(recent.iter().map(|r| num(r, "pp_toi"))), 3));
    set(&mut out, "pp_share_avg", pp_share_avg);
    set(&mut out, "season_toi", r_(mean(rows.iter().map(|r| num(r, "toi"))), 3));
    set(&mut out, "season_pp_toi", r_(mean(rows.iter().map(|r| num(r, "pp_toi"))), 3));
    set(&mut out, "season_pp_share", r_(pp_ratio(&rows), 4));
    let (base_toi, base_pp, src): (Option<f64>, Option<f64>, Option<&str>) = if before.len() >= MIN_BASE_GP {
        (mean(before.iter().map(|r| num(r, "toi"))), pp_ratio(before), Some("season"))
    } else if let Some(p) = prior.filter(|p| !p.is_empty()) {
        (p.get("toi").copied().flatten(), p.get("pp_share").copied().flatten(), Some("prior"))
    } else {
        (None, None, None)
    };
    set(&mut out, "baseline_toi", r_(base_toi, 3));
    set(&mut out, "baseline_pp_share", r_(base_pp, 4));
    out.insert("baseline_source".into(), src.map(Value::from).unwrap_or(Value::Null));
    if rows.len() >= last_n {
        if let (Some(b), Some(t)) = (base_toi, toi_avg) {
            set(&mut out, "toi_trend", Some(round_to(t - b, 3)));
        }
        if let (Some(b), Some(p)) = (base_pp, pp_share_avg) {
            set(&mut out, "pp_share_trend", Some(round_to(p - b, 4)));
        }
    }
    out
}

const ROW_COLS: &str = "nhl_id, game_date, team, toi, ev_toi, pp_toi, sh_toi, shifts, team_pp_toi, pp_share";

/// nhl_id -> this season's deployment rows up to `as_of` (sorted by date).
pub fn season_rows(ledger: &Ledger, as_of: NaiveDate, nhl_ids: Option<&[i64]>) -> Result<Map<i64, Vec<Row>>> {
    let start = iso(season_start_date(as_of));
    let end = iso(as_of);
    let mut sql = format!("SELECT {ROW_COLS} FROM deployment_daily WHERE game_date >= ? AND game_date <= ?");
    let ids: Option<Vec<i64>> = nhl_ids.map(|v| {
        let s: BTreeSet<i64> = v.iter().copied().collect();
        s.into_iter().collect()
    });
    let mut id_params: Vec<i64> = Vec::new();
    if let Some(ids) = &ids {
        if ids.is_empty() {
            return Ok(Map::new());
        }
        if ids.len() <= 500 {
            sql.push_str(&format!(" AND nhl_id IN ({})", vec!["?"; ids.len()].join(",")));
            id_params = ids.clone();
        }
    }
    sql.push_str(" ORDER BY nhl_id, game_date");
    let mut params: Vec<&dyn ToSql> = vec![&start, &end];
    for i in &id_params {
        params.push(i);
    }
    let wanted: Option<std::collections::HashSet<i64>> = ids.as_ref().map(|v| v.iter().copied().collect());
    let mut out: Map<i64, Vec<Row>> = Map::new();
    for r in ledger.query(&sql, &params)? {
        let id = r.get("nhl_id").and_then(|v| v.as_i64()).unwrap_or(0);
        if wanted.as_ref().map(|w| w.contains(&id)).unwrap_or(true) {
            out.entry(id).or_default().push(r);
        }
    }
    Ok(out)
}

/// `{gp, toi_avg, pp_toi_avg, pp_share_avg (last `last_n` GP), season_toi, ...}` for one skater
/// this season up to `as_of` (minutes, shares 0..1; None when unknown).
pub fn deployment_summary(
    ledger: &Ledger,
    nhl_id: i64,
    as_of: NaiveDate,
    last_n: usize,
    prior: Option<&Prior>,
) -> Result<Row> {
    let rows = season_rows(ledger, as_of, Some(&[nhl_id]))?;
    Ok(summarize_rows(rows.get(&nhl_id).map(|v| v.as_slice()).unwrap_or(&[]), last_n, prior))
}

/// [`deployment_summary`] for many skaters with one query (only ids with rows).
pub fn deployment_summaries(
    ledger: &Ledger,
    nhl_ids: &[i64],
    as_of: NaiveDate,
    last_n: usize,
    priors: Option<&Map<i64, Prior>>,
) -> Result<Map<i64, Row>> {
    Ok(season_rows(ledger, as_of, Some(nhl_ids))?
        .into_iter()
        .map(|(pid, rows)| {
            let p = priors.and_then(|m| m.get(&pid));
            (pid, summarize_rows(&rows, last_n, p))
        })
        .collect())
}

// --------------------------------------------------------------------------- goalies

/// One team's goalie usage this season.
#[derive(Clone, Debug, PartialEq)]
pub struct TeamGoalieCounts {
    /// team games this season up to `as_of` (dates with a goalie row)
    pub games: i64,
    /// nhl_id -> starts for that team
    pub starts: Map<i64, i64>,
    /// nhl_id -> team games from that goalie's first appearance for the team onward
    pub since_first: Map<i64, i64>,
    /// the team's game dates (ISO)
    pub dates: BTreeSet<String>,
    /// nhl_id -> the dates that goalie appeared for the team
    pub appeared: Map<i64, BTreeSet<String>>,
    /// second nights
    pub b2b: i64,
    /// second nights the team's #1 (most starts) started
    pub b2b_starter: i64,
    /// the #1's nhl_id
    pub starter: Option<i64>,
}

/// Team games a goalie must have missed before his first appearance to count as a late arrival
/// (a trade, a call-up, a holdout or a long injury) rather than a backup waiting for a start.
pub const LATE_ARRIVAL_GAMES: i64 = 5;

impl TeamGoalieCounts {
    /// The team games a goalie's starts are measured against. The whole season, except for a
    /// late arrival: the games before he joined say nothing about his share of the starts, so
    /// only the games from his first appearance count.
    pub fn games_for(&self, nhl_id: i64) -> i64 {
        match self.since_first.get(&nhl_id) {
            Some(n) if self.games - n >= LATE_ARRIVAL_GAMES => *n,
            _ => self.games,
        }
    }

    /// [`Self::games_for`], less the games the goalie could not play in. A game missed through
    /// injury says nothing about his share of the starts when he is fit, and the time he is
    /// still expected to miss is discounted separately, so counting it here too would charge
    /// the injury twice.
    ///
    /// `statuses` = his recorded `(ISO date, unavailable)` statuses, oldest first, the current
    /// one included. They are recorded only on days the app ran, so they can be days stale, and
    /// an appearance is the better evidence that he was fit. A game he did not appear in is
    /// left out when
    /// * the latest record on or before it says unavailable and he has not played since that
    ///   record, or
    /// * the next record after it says unavailable and he did not play in between (he was hurt
    ///   some time after his last game; the record came later).
    pub fn games_available(&self, nhl_id: i64, statuses: &[(String, bool)]) -> i64 {
        let none = BTreeSet::new();
        let appeared = self.appeared.get(&nhl_id).unwrap_or(&none);
        let counted = self.games_for(nhl_id) as usize;
        let window = self.dates.iter().skip(self.dates.len().saturating_sub(counted));
        window
            .filter(|d| {
                let d: &String = d;
                if appeared.contains(d) {
                    return true;
                }
                let out_since = match statuses.iter().rev().find(|(day, _)| day <= d) {
                    Some((day, true)) => appeared.range::<String, _>(day..d).next().is_none(),
                    _ => false,
                };
                let out_soon = match statuses.iter().find(|(day, _)| day > d) {
                    Some((day, true)) => appeared.range::<String, _>(d..day).next().is_none(),
                    _ => false,
                };
                !(out_since || out_soon)
            })
            .count() as i64
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// team -> counts of goalie rows from the season start through `as_of`.
pub fn team_goalie_counts(ledger: &Ledger, as_of: NaiveDate) -> Result<Map<String, TeamGoalieCounts>> {
    let (s, e) = (iso(season_start_date(as_of)), iso(as_of));
    let rows = ledger.query(
        "SELECT nhl_id, game_date, team, started, back_to_back FROM goalie_starts \
         WHERE game_date >= ? AND game_date <= ? AND team IS NOT NULL",
        &[&s, &e],
    )?;
    let mut out: Map<String, TeamGoalieCounts> = Map::new();
    let mut dates: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut b2b_dates: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut starters_on: HashMap<(String, String), BTreeSet<i64>> = HashMap::new();
    let mut first_seen: HashMap<(String, i64), String> = HashMap::new();
    for r in &rows {
        let t = r["team"].as_str().unwrap_or("").to_string();
        let gd = r["game_date"].as_str().unwrap_or("").to_string();
        let id = r["nhl_id"].as_i64().unwrap_or(0);
        let first = first_seen.entry((t.clone(), id)).or_insert_with(|| gd.clone());
        if gd < *first {
            *first = gd.clone();
        }
        let d = out.entry(t.clone()).or_insert(TeamGoalieCounts {
            games: 0,
            starts: Map::new(),
            since_first: Map::new(),
            dates: BTreeSet::new(),
            appeared: Map::new(),
            b2b: 0,
            b2b_starter: 0,
            starter: None,
        });
        dates.entry(t.clone()).or_default().insert(gd.clone());
        d.appeared.entry(id).or_default().insert(gd.clone());
        if truthy(r.get("started")) {
            *d.starts.entry(id).or_insert(0) += 1;
            starters_on.entry((t.clone(), gd.clone())).or_default().insert(id);
        }
        if truthy(r.get("back_to_back")) {
            b2b_dates.entry(t).or_default().insert(gd);
        }
    }
    let empty = BTreeSet::new();
    for (t, d) in out.iter_mut() {
        let team_dates = dates.get(t).unwrap_or(&empty);
        d.games = team_dates.len() as i64;
        d.dates = team_dates.clone();
        for ((team, id), first) in &first_seen {
            if team == t {
                d.since_first.insert(*id, team_dates.range(first.clone()..).count() as i64);
            }
        }
        d.since_first.sort_keys();
        if !d.starts.is_empty() {
            // max(sorted(starts), key=starts.get): first maximum in id order
            let mut ids: Vec<i64> = d.starts.keys().copied().collect();
            ids.sort();
            let mut best = ids[0];
            for &k in &ids[1..] {
                if d.starts[&k] > d.starts[&best] {
                    best = k;
                }
            }
            d.starter = Some(best);
        }
        let bd = b2b_dates.get(t).unwrap_or(&empty);
        d.b2b = bd.len() as i64;
        d.b2b_starter = bd
            .iter()
            .filter(|day| match d.starter {
                Some(st) => starters_on.get(&(t.clone(), (*day).clone())).map(|s| s.contains(&st)).unwrap_or(false),
                None => false,
            })
            .count() as i64;
    }
    Ok(out)
}

/// (starts for `team`, the `team` games they are measured against: see
/// [`TeamGoalieCounts::games_for`]) this season up to `as_of`; `team` defaults to the goalie's
/// most recent team in the ledger. (0, 0) when unknown.
pub fn goalie_start_counts(ledger: &Ledger, nhl_id: i64, as_of: NaiveDate, team: Option<&str>) -> Result<(i64, i64)> {
    let team: Option<String> = match team {
        Some(t) => Some(t.to_string()),
        None => {
            let (s, e) = (iso(season_start_date(as_of)), iso(as_of));
            let last = ledger.query(
                "SELECT team FROM goalie_starts WHERE nhl_id=? AND game_date >= ? AND game_date <= ? \
                 ORDER BY game_date DESC LIMIT 1",
                &[&nhl_id, &s, &e],
            )?;
            last.first().and_then(|r| r["team"].as_str().map(String::from))
        }
    };
    let Some(team) = team.filter(|t| !t.is_empty()) else { return Ok((0, 0)) };
    let counts = team_goalie_counts(ledger, as_of)?;
    Ok(match counts.get(&team) {
        Some(d) => (d.starts.get(&nhl_id).copied().unwrap_or(0), d.games_for(nhl_id)),
        None => (0, 0),
    })
}

/// (probability the team's #1 starts the second night of a back-to-back, second nights seen).
///
/// `B2B_SECOND_NIGHT_P` (0.35) until the team has `B2B_MIN_SAMPLE` (5) second nights in the
/// ledger; then the observed rate shrunk toward 0.35 with k = `B2B_SHRINK_K`.
pub fn b2b_second_night_rate(
    ledger: &Ledger,
    team: &str,
    as_of: NaiveDate,
    counts: Option<&Map<String, TeamGoalieCounts>>,
) -> Result<(f64, i64)> {
    let owned;
    let counts = match counts {
        Some(c) => c,
        None => {
            owned = team_goalie_counts(ledger, as_of)?;
            &owned
        }
    };
    let d = counts.get(team);
    let n = d.map(|d| d.b2b).unwrap_or(0);
    if n < B2B_MIN_SAMPLE {
        return Ok((B2B_SECOND_NIGHT_P, n));
    }
    let hits = d.map(|d| d.b2b_starter).unwrap_or(0) as f64;
    Ok(((hits + B2B_SHRINK_K * B2B_SECOND_NIGHT_P) / (n as f64 + B2B_SHRINK_K), n))
}
