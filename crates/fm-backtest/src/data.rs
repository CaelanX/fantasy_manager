//! Rust port of `fantasy_manager/backtest/data.py`: the per-player-season history table and the
//! in-season checkpoint windows, built from the free NHL stats REST reports.
//! Owner: work package `W3-harness-deploy-data`.
//!
//! Storage is plain JSON under `<fm_data_dir>/backtest/` (`player_seasons.json`, `windows.json`),
//! byte-compatible with Python (compact separators, ensure_ascii, struct field order = `asdict`).
//!
//! Deviations (API shape only):
//! * `date.today()` defaults are explicit `today` parameters.
//! * `CountingFetcher` is `Arc`-shared and exposes [`CountingFetcher::into_fetch`], which adapts
//!   it to fm-http's `FetchJson` (the NHL client's TTL argument is ignored, like Python where the
//!   fetcher owns the TTL policy).
//! * `fetch_*` report errors as `anyhow::Error` where Python raised.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use chrono::{Duration, NaiveDate};
use fm_core::models::{normalize_name, Player, Split, StatLine};
use fm_core::Map;
use fm_http::{FetchJson, HttpCache, Params};
use fm_pycompat::num::round_to;
use fm_pycompat::pyjson::{dumps, DumpOpts};
use fm_sources::nhl::{current_season, goalie_from_row, skater_from_rows, NhlClient};
use serde_json::{json, Value};

pub const FIRST_SEASON: i64 = 20102011;
pub const LAST_SEASON: i64 = 20252026;
pub const DAY: f64 = 86400.0;
/// finished seasons never change
pub const TTL_HISTORICAL: f64 = 30.0 * DAY;
pub const TTL_CURRENT: f64 = 0.5 * DAY;

/// `TOI` and `PPTOI` are total minutes (all situations, power play), `A1` / `A2` primary and
/// secondary assists; these four are present only when the deployment reports have a row for the
/// player (see [`DEPLOYMENT_REPORTS`]).
pub const SKATER_KEYS: [&str; 22] = [
    "G", "A", "PTS", "PM", "PIM", "PPG", "PPA", "PPP", "SHG", "SHA", "SHP", "GWG", "SOG", "HIT", "BLK", "FOW", "FOL",
    "ENG", "TOI", "PPTOI", "A1", "A2",
];
/// NHL skater reports fetched (best effort) for time on ice and the assist split: `timeonice`
/// (`timeOnIce`, `ppTimeOnIce`, seconds) and `scoringpergame` (`totalPrimaryAssists`,
/// `totalSecondaryAssists`). Both accept the same `gameDate` window filter as `summary`.
pub const DEPLOYMENT_REPORTS: [&str; 2] = ["timeonice", "scoringpergame"];
pub const GOALIE_KEYS: [&str; 11] = ["GS", "W", "L", "OTL", "GA", "SA", "SV", "SO", "G", "A", "PIM"];

/// In-season checkpoints (month, day) and the windows stored for each one.
pub const CHECKPOINTS: [(u32, u32); 3] = [(11, 1), (12, 1), (1, 1)];
pub const WINDOWS: [&str; 5] = ["to_date", "last30", "last15", "last7", "rest"];
pub const WINDOW_DAYS: [(&str, i64); 3] = [("last30", 30), ("last15", 15), ("last7", 7)];

// --------------------------------------------------------------------------- seasons

pub fn season_start_year(season: i64) -> i64 {
    season.div_euclid(10000)
}

pub fn season_label(season: i64) -> String {
    let y = season_start_year(season);
    let next = (y + 1).to_string();
    format!("{y}-{}", &next[next.len().saturating_sub(2)..])
}

pub fn next_season(season: i64) -> i64 {
    season + 10001
}

pub fn prev_season(season: i64) -> i64 {
    season - 10001
}

pub fn season_range(first: i64, last: i64) -> Vec<i64> {
    let mut out = Vec::new();
    let mut s = first;
    while s <= last {
        out.push(s);
        s = next_season(s);
    }
    out
}

/// '20162017-20252026', '20232024,20242025', '2016-2025' (start years) -> season ids.
pub fn parse_seasons(spec: Option<&str>, default: &[i64]) -> Result<Vec<i64>> {
    let spec = match spec {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(default.to_vec()),
    };
    fn one(tok: &str) -> Result<i64> {
        let tok = tok.trim();
        if tok.chars().count() == 4 {
            let y: i64 = tok.parse().with_context(|| format!("invalid literal for int(): {tok:?}"))?;
            return Ok(y * 10000 + y + 1);
        }
        tok.parse().with_context(|| format!("invalid literal for int(): {tok:?}"))
    }
    let mut out: Vec<i64> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            out.extend(season_range(one(a)?, one(b)?));
        } else {
            out.push(one(part)?);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Birth date text -> date (`str(x)[:10]` then `date.fromisoformat`).
fn parse_birth(s: &str) -> Option<NaiveDate> {
    let head: String = s.chars().take(10).collect();
    NaiveDate::parse_from_str(&head, "%Y-%m-%d").ok()
}

pub fn age_on_oct1(birth_date: Option<&str>, season: i64) -> Option<f64> {
    let bd = parse_birth(birth_date.filter(|s| !s.is_empty())?)?;
    let oct1 = NaiveDate::from_ymd_opt(season_start_year(season) as i32, 10, 1)?;
    Some(round_to((oct1 - bd).num_days() as f64 / 365.25, 3))
}

pub fn position_group(position: Option<&str>) -> &'static str {
    match position {
        Some("G") => "G",
        Some("D") => "D",
        _ => "F",
    }
}

// --------------------------------------------------------------------------- rows

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlayerSeason {
    pub player_id: i64,
    pub season: i64,
    pub name: String,
    /// F / D / G
    pub group: String,
    /// C / LW / RW / D / G
    pub position: Option<String>,
    pub gp: i64,
    /// season totals (no GP key)
    pub stats: Map<String, f64>,
    pub team: Option<String>,
    pub birth_date: Option<String>,
    /// on Oct 1 of the season's start year
    pub age: Option<f64>,
}

impl PlayerSeason {
    pub fn is_goalie(&self) -> bool {
        self.group == "G"
    }

    pub fn per_game(&self) -> Map<String, f64> {
        if self.gp <= 0 {
            return Map::new();
        }
        self.stats.iter().map(|(k, v)| (k.clone(), v / self.gp as f64)).collect()
    }

    pub fn statline(&self, split: Split) -> StatLine {
        let mut stats = self.stats.clone();
        stats.insert("GP".to_string(), self.gp as f64);
        StatLine { split, gp: self.gp, stats }
    }

    /// App-side `Player` carrying this row as its `split` line (for valuation functions).
    pub fn to_player(&self, split: Split) -> Result<Player> {
        let pos = self.position.clone().unwrap_or_else(|| if self.is_goalie() { "G" } else { "C" }.to_string());
        let mut lines = serde_json::Map::new();
        if self.gp > 0 {
            lines.insert(split.as_str().to_string(), serde_json::to_value(self.statline(split))?);
        }
        let v = json!({
            "cid": format!("nhl:{}", self.player_id),
            "name": self.name,
            "name_norm": normalize_name(&self.name),
            "ids": {"nhl": self.player_id.to_string()},
            "team": self.team,
            "positions": [pos],
            "lines": Value::Object(lines),
        });
        serde_json::from_value(v).map_err(|e| anyhow!("building Player: {e}"))
    }

    /// `asdict(self)`: field order of the dataclass.
    pub fn to_dict(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// `PlayerSeason.from_dict(d)`.
    pub fn from_dict(d: &Value) -> Result<PlayerSeason> {
        let get_i = |k: &str| -> Result<i64> {
            d.get(k).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f.trunc() as i64)))
                .ok_or_else(|| anyhow!("KeyError: {k:?}"))
        };
        let s = |k: &str| d.get(k).and_then(|v| v.as_str()).map(|x| x.to_string());
        let stats = d
            .get("stats")
            .and_then(|v| v.as_object())
            .map(|o| o.iter().filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f))).collect())
            .unwrap_or_default();
        Ok(PlayerSeason {
            player_id: get_i("player_id")?,
            season: get_i("season")?,
            name: s("name").unwrap_or_default(),
            group: s("group").filter(|g| !g.is_empty()).unwrap_or_else(|| "F".to_string()),
            position: s("position"),
            gp: d.get("gp").and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f.trunc() as i64))).unwrap_or(0),
            stats,
            team: s("team"),
            birth_date: s("birth_date"),
            age: d.get("age").and_then(|v| v.as_f64()),
        })
    }
}

/// Stats of one player over a date window (gp + totals).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowLine {
    pub gp: i64,
    pub stats: Map<String, f64>,
}

impl WindowLine {
    pub fn per_game(&self) -> Map<String, f64> {
        if self.gp > 0 {
            self.stats.iter().map(|(k, v)| (k.clone(), v / self.gp as f64)).collect()
        } else {
            Map::new()
        }
    }

    pub fn statline(&self, split: Split) -> StatLine {
        let mut stats = self.stats.clone();
        stats.insert("GP".to_string(), self.gp as f64);
        StatLine { split, gp: self.gp, stats }
    }
}

type SkaterStats = (i64, Map<String, f64>, Option<String>, Option<String>);

/// player_id -> row of each [`DEPLOYMENT_REPORTS`] report (a failing report is left empty).
type DeploymentRows = [std::collections::HashMap<i64, Value>; 2];

fn deployment_rows(client: &NhlClient, season: i64, extra: Option<&str>) -> Result<DeploymentRows> {
    let mut out: DeploymentRows = Default::default();
    for (i, rep) in DEPLOYMENT_REPORTS.iter().enumerate() {
        if let Ok(rows) = report(client, "skater", rep, season, extra) {
            out[i] = by_player(rows)?;
        }
    }
    Ok(out)
}

/// `TOI` / `PPTOI` (minutes, from seconds) and `A1` / `A2` from the deployment report rows.
/// Departs from the Python, which stored none of these; a player without a row keeps no key, so a
/// missing report reads as unknown rather than zero.
fn add_deployment(st: &mut Map<String, f64>, pid: i64, dep: &DeploymentRows) {
    let val = |r: &Value, k: &str| r.get(k).and_then(Value::as_f64);
    if let Some(r) = dep[0].get(&pid) {
        for (src, canon) in [("timeOnIce", "TOI"), ("ppTimeOnIce", "PPTOI")] {
            if let Some(s) = val(r, src) {
                st.insert(canon.to_string(), round_to(s / 60.0, 2));
            }
        }
    }
    if let Some(r) = dep[1].get(&pid) {
        for (src, canon) in [("totalPrimaryAssists", "A1"), ("totalSecondaryAssists", "A2")] {
            if let Some(v) = val(r, src) {
                st.insert(canon.to_string(), v);
            }
        }
    }
}

fn skater_stats(
    summary: &Value,
    realtime: Option<&Value>,
    faceoffs: Option<&Value>,
    dep: &DeploymentRows,
) -> Result<SkaterStats> {
    let row = skater_from_rows(summary, realtime, faceoffs, 2)?;
    let mut st = row.stats.clone();
    add_deployment(&mut st, row.player_id, dep);
    for k in ["HIT", "BLK", "ENG"] {
        // no realtime row -> zero, not missing
        st.entry(k.to_string()).or_insert(0.0);
    }
    if let (Some(ppp), Some(ppg)) = (st.get("PPP").copied(), st.get("PPG").copied()) {
        st.insert("PPA".to_string(), ppp - ppg);
    }
    if let (Some(shp), Some(shg)) = (st.get("SHP").copied(), st.get("SHG").copied()) {
        st.insert("SHA".to_string(), shp - shg);
    }
    let gp = st.get("GP").copied().unwrap_or(0.0).trunc() as i64;
    let stats = SKATER_KEYS.iter().filter_map(|k| st.get(*k).map(|v| (k.to_string(), *v))).collect();
    Ok((gp, stats, row.position, row.team))
}

fn goalie_stats(row: &Value) -> Result<(i64, Map<String, f64>, Option<String>)> {
    let g = goalie_from_row(row, 2)?;
    let mut st = g.stats.clone();
    for (src, canon) in [("goals", "G"), ("assists", "A"), ("penaltyMinutes", "PIM")] {
        let v = match row.get(src) {
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            _ => 0.0,
        };
        st.insert(canon.to_string(), v);
    }
    let gp = st.get("GP").copied().unwrap_or(0.0).trunc() as i64;
    let stats = GOALIE_KEYS.iter().filter_map(|k| st.get(*k).map(|v| (k.to_string(), *v))).collect();
    Ok((gp, stats, g.team))
}

// --------------------------------------------------------------------------- fetching

/// `CountingFetcher.ttl_for`: finished seasons never change, the current one is re-fetched.
pub fn ttl_for(today: NaiveDate, params: &Params) -> f64 {
    let cay = params
        .iter()
        .find(|(k, _)| k == "cayenneExp")
        .map(|(_, v)| match v {
            fm_http::ParamValue::Str(s) => s.clone(),
            _ => String::new(),
        })
        .unwrap_or_default();
    let cur = current_season(today);
    for tok in cay.replace('=', " ").split_whitespace() {
        if tok.len() == 8 && tok.chars().all(|c| c.is_ascii_digit()) {
            let n: i64 = tok.parse().unwrap_or(0);
            return if n >= cur { TTL_CURRENT } else { TTL_HISTORICAL };
        }
    }
    TTL_CURRENT
}

/// `fetch_json(url, params)` through `HttpCache` with a long TTL for finished seasons. Counts
/// network requests vs cache hits and the wall time spent on the network.
pub struct CountingFetcher {
    pub cache: Arc<HttpCache>,
    pub today: NaiveDate,
    pub pause: f64,
    network: AtomicU64,
    cached: AtomicU64,
    seconds: Mutex<f64>,
}

impl CountingFetcher {
    pub fn new(cache: Arc<HttpCache>, today: NaiveDate, pause: f64) -> Self {
        CountingFetcher { cache, today, pause, network: AtomicU64::new(0), cached: AtomicU64::new(0), seconds: Mutex::new(0.0) }
    }

    pub fn network(&self) -> u64 {
        self.network.load(Ordering::Relaxed)
    }
    pub fn cached(&self) -> u64 {
        self.cached.load(Ordering::Relaxed)
    }
    pub fn seconds(&self) -> f64 {
        *self.seconds.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn ttl_for(&self, params: &Params) -> f64 {
        ttl_for(self.today, params)
    }

    pub fn fetch(&self, url: &str, params: &Params) -> Result<Value> {
        let t0 = Instant::now();
        let resp = self.cache.fetch(url, params, &[], &[], self.ttl_for(params))?;
        if resp.from_cache {
            self.cached.fetch_add(1, Ordering::Relaxed);
        } else {
            self.network.fetch_add(1, Ordering::Relaxed);
            *self.seconds.lock().unwrap_or_else(|e| e.into_inner()) += t0.elapsed().as_secs_f64();
            if self.pause > 0.0 {
                std::thread::sleep(std::time::Duration::from_secs_f64(self.pause));
            }
        }
        if !(200..300).contains(&resp.status_code) {
            return Err(anyhow!("HTTP {} for {url}", resp.status_code));
        }
        resp.json()
    }

    pub fn summary(&self) -> String {
        format!("{} network requests ({:.1}s), {} cache hits", self.network(), self.seconds(), self.cached())
    }

    /// Adapter for `NhlClient::new` (the client's own TTL is ignored).
    pub fn into_fetch(self: Arc<Self>) -> Box<FetchJson<'static>> {
        Box::new(move |url, params, _ttl| self.fetch(url, params))
    }
}

fn report(client: &NhlClient, kind: &str, report: &str, season: i64, extra: Option<&str>) -> Result<Vec<Value>> {
    client.report(kind, report, Some(season), 2, extra)
}

fn player_id(r: &Value) -> Result<i64> {
    r.get("playerId").and_then(|v| v.as_i64()).ok_or_else(|| anyhow!("KeyError: 'playerId'"))
}

fn by_player(rows: Vec<Value>) -> Result<std::collections::HashMap<i64, Value>> {
    let mut m = std::collections::HashMap::new();
    for r in rows {
        m.insert(player_id(&r)?, r);
    }
    Ok(m)
}

/// All skater and goalie rows of one regular season.
pub fn fetch_season(client: &NhlClient, season: i64) -> Result<Vec<PlayerSeason>> {
    let mut births: std::collections::HashMap<i64, String> = Default::default();
    for kind in ["skater", "goalie"] {
        for r in report(client, kind, "bios", season, None)? {
            if let Some(bd) = r.get("birthDate").filter(|v| !v.is_null() && v.as_str() != Some("")) {
                let s = bd.as_str().map(|s| s.to_string()).unwrap_or_else(|| bd.to_string());
                births.insert(player_id(&r)?, s.chars().take(10).collect());
            }
        }
    }
    let realtime = by_player(report(client, "skater", "realtime", season, None)?)?;
    let faceoffs = match report(client, "skater", "faceoffwins", season, None) {
        Ok(rows) => by_player(rows)?,
        Err(_) => Default::default(),
    };
    let dep = deployment_rows(client, season, None)?;
    let empty = json!({});
    let mut out = Vec::new();
    for r in report(client, "skater", "summary", season, None)? {
        let pid = player_id(&r)?;
        let (gp, stats, pos, team) =
            skater_stats(&r, Some(realtime.get(&pid).unwrap_or(&empty)), faceoffs.get(&pid), &dep)?;
        let bd = births.get(&pid).cloned();
        out.push(PlayerSeason {
            player_id: pid,
            season,
            name: r.get("skaterFullName").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            group: position_group(pos.as_deref()).to_string(),
            position: pos,
            gp,
            stats,
            team,
            age: age_on_oct1(bd.as_deref(), season),
            birth_date: bd,
        });
    }
    for r in report(client, "goalie", "summary", season, None)? {
        let pid = player_id(&r)?;
        let (gp, stats, team) = goalie_stats(&r)?;
        let bd = births.get(&pid).cloned();
        out.push(PlayerSeason {
            player_id: pid,
            season,
            name: r.get("goalieFullName").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            group: "G".to_string(),
            position: Some("G".to_string()),
            gp,
            stats,
            team,
            age: age_on_oct1(bd.as_deref(), season),
            birth_date: bd,
        });
    }
    Ok(out)
}

/// player_id -> WindowLine for games in [start, end] (inclusive) of a regular season.
/// `faceoffs` also fetches the faceoffwins report (FOW / FOL), best effort; the
/// [`DEPLOYMENT_REPORTS`] are always fetched, best effort.
pub fn fetch_window(
    client: &NhlClient,
    season: i64,
    start: Option<NaiveDate>,
    end: Option<NaiveDate>,
    faceoffs: bool,
) -> Result<Map<i64, WindowLine>> {
    let mut parts = Vec::new();
    if let Some(s) = start {
        parts.push(format!("gameDate>=\"{}\"", s.format("%Y-%m-%d")));
    }
    if let Some(e) = end {
        parts.push(format!("gameDate<=\"{}\"", e.format("%Y-%m-%d")));
    }
    let extra = if parts.is_empty() { None } else { Some(parts.join(" and ")) };
    let extra = extra.as_deref();
    let realtime = by_player(report(client, "skater", "realtime", season, extra)?)?;
    let fo = if faceoffs {
        match report(client, "skater", "faceoffwins", season, extra) {
            Ok(rows) => by_player(rows)?,
            Err(_) => Default::default(),
        }
    } else {
        Default::default()
    };
    let dep = deployment_rows(client, season, extra)?;
    let empty = json!({});
    let mut out: Map<i64, WindowLine> = Map::new();
    for r in report(client, "skater", "summary", season, extra)? {
        let pid = player_id(&r)?;
        let (gp, stats, _, _) = skater_stats(&r, Some(realtime.get(&pid).unwrap_or(&empty)), fo.get(&pid), &dep)?;
        if gp > 0 {
            out.insert(pid, WindowLine { gp, stats });
        }
    }
    for r in report(client, "goalie", "summary", season, extra)? {
        let (gp, stats, _) = goalie_stats(&r)?;
        if gp > 0 {
            out.insert(player_id(&r)?, WindowLine { gp, stats });
        }
    }
    Ok(out)
}

pub fn checkpoint_dates(season: i64) -> Vec<NaiveDate> {
    let y = season_start_year(season) as i32;
    CHECKPOINTS
        .iter()
        .filter_map(|(m, d)| NaiveDate::from_ymd_opt(if *m >= 9 { y } else { y + 1 }, *m, *d))
        .collect()
}

/// Windows around checkpoint `day`: to_date / last30 / last15 / last7 end the day before, rest
/// starts on `day`.
pub fn fetch_checkpoint(client: &NhlClient, season: i64, day: NaiveDate) -> Result<Map<String, Map<i64, WindowLine>>> {
    let end = day - Duration::days(1);
    let mut out = Map::new();
    out.insert("to_date".to_string(), fetch_window(client, season, None, Some(end), false)?);
    for (name, days) in WINDOW_DAYS {
        out.insert(name.to_string(), fetch_window(client, season, Some(day - Duration::days(days)), Some(end), false)?);
    }
    out.insert("rest".to_string(), fetch_window(client, season, Some(day), None, false)?);
    Ok(out)
}

// --------------------------------------------------------------------------- storage

pub fn backtest_dir(data_dir: &Path) -> Result<PathBuf> {
    let p = data_dir.join("backtest");
    std::fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))?;
    Ok(p)
}

/// All stored player-season rows, indexed by season and by player.
#[derive(Clone, Debug, Default)]
pub struct SeasonTable {
    pub rows: Vec<PlayerSeason>,
    /// season -> player_id -> index into `rows`
    by_season: Map<i64, Map<i64, usize>>,
    by_player: Map<i64, Map<i64, usize>>,
}

impl SeasonTable {
    pub fn new(rows: impl IntoIterator<Item = PlayerSeason>) -> Self {
        let mut t = SeasonTable { rows: rows.into_iter().collect(), ..Default::default() };
        t.reindex();
        t
    }

    fn reindex(&mut self) {
        self.by_season.clear();
        self.by_player.clear();
        for (i, r) in self.rows.iter().enumerate() {
            self.by_season.entry(r.season).or_default().insert(r.player_id, i);
            self.by_player.entry(r.player_id).or_default().insert(r.season, i);
        }
    }

    /// Sorted season ids present.
    pub fn seasons(&self) -> Vec<i64> {
        let mut s: Vec<i64> = self.by_season.keys().copied().collect();
        s.sort();
        s
    }

    pub fn add(&mut self, rows: Vec<PlayerSeason>) {
        let drop: std::collections::HashSet<(i64, i64)> = rows.iter().map(|r| (r.season, r.player_id)).collect();
        self.rows.retain(|r| !drop.contains(&(r.season, r.player_id)));
        self.rows.extend(rows);
        self.reindex();
    }

    pub fn get(&self, player_id: i64, season: i64) -> Option<&PlayerSeason> {
        self.by_season.get(&season)?.get(&player_id).map(|&i| &self.rows[i])
    }

    /// Rows of one season (insertion order of the table), as `by_season[season].values()`.
    pub fn season_rows(&self, season: i64) -> Vec<&PlayerSeason> {
        self.by_season.get(&season).map(|m| m.values().map(|&i| &self.rows[i]).collect()).unwrap_or_default()
    }

    /// The player's rows for seasons < `before`, oldest first.
    pub fn history(&self, player_id: i64, before: i64) -> Vec<&PlayerSeason> {
        let Some(m) = self.by_player.get(&player_id) else { return Vec::new() };
        let mut seasons: Vec<i64> = m.keys().copied().filter(|s| *s < before).collect();
        seasons.sort();
        seasons.iter().map(|s| &self.rows[m[s]]).collect()
    }

    /// Age on Oct 1 of `season` from any stored birth date of the player.
    pub fn age(&self, player_id: i64, season: i64) -> Option<f64> {
        for &i in self.by_player.get(&player_id)?.values() {
            if let Some(bd) = self.rows[i].birth_date.as_deref().filter(|b| !b.is_empty()) {
                return age_on_oct1(Some(bd), season);
            }
        }
        None
    }
}

pub fn table_path(data_dir: &Path) -> Result<PathBuf> {
    Ok(backtest_dir(data_dir)?.join("player_seasons.json"))
}

pub fn load_table(data_dir: &Path) -> Result<SeasonTable> {
    let p = table_path(data_dir)?;
    if !p.exists() {
        return Ok(SeasonTable::default());
    }
    let d: Value = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
    let rows = d.get("rows").and_then(|r| r.as_array()).map(|a| a.as_slice()).unwrap_or(&[]);
    Ok(SeasonTable::new(rows.iter().map(PlayerSeason::from_dict).collect::<Result<Vec<_>>>()?))
}

fn write_tmp_replace(p: &Path, text: &str) -> Result<()> {
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

pub fn save_table(table: &SeasonTable, data_dir: &Path) -> Result<PathBuf> {
    let p = table_path(data_dir)?;
    let mut rows: Vec<&PlayerSeason> = table.rows.iter().collect();
    rows.sort_by_key(|r| (r.season, r.player_id));
    let payload = json!({
        "version": 1,
        "seasons": table.seasons(),
        "rows": rows.iter().map(|r| r.to_dict()).collect::<Vec<_>>(),
    });
    write_tmp_replace(&p, &dumps(&payload, &DumpOpts::compact()))?;
    Ok(p)
}

/// Fetch missing seasons into the stored table (sequential; each season saved as it lands).
pub fn build_table(
    client: &NhlClient,
    data_dir: &Path,
    seasons: &[i64],
    force: bool,
    mut log: Option<&mut dyn FnMut(&str)>,
) -> Result<SeasonTable> {
    let mut table = load_table(data_dir)?;
    let have: std::collections::HashSet<i64> = table.seasons().into_iter().collect();
    for &s in seasons {
        if have.contains(&s) && !force {
            continue;
        }
        let rows = fetch_season(client, s)?;
        let (n_g, n_s) = (rows.iter().filter(|r| r.is_goalie()).count(), rows.iter().filter(|r| !r.is_goalie()).count());
        table.add(rows);
        save_table(&table, data_dir)?;
        if let Some(l) = log.as_mut() {
            l(&format!("{}: {n_s} skaters, {n_g} goalies", season_label(s)));
        }
    }
    Ok(table)
}

// ---- in-season windows ---------------------------------------------------------

/// season -> day (ISO) -> window -> pid -> line
pub type Windows = Map<i64, Map<String, Map<String, Map<i64, WindowLine>>>>;

pub fn windows_path(data_dir: &Path) -> Result<PathBuf> {
    Ok(backtest_dir(data_dir)?.join("windows.json"))
}

pub fn load_windows(data_dir: &Path) -> Result<Windows> {
    let p = windows_path(data_dir)?;
    if !p.exists() {
        return Ok(Windows::new());
    }
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
    let mut out = Windows::new();
    for (s, days) in raw.as_object().ok_or_else(|| anyhow!("windows.json: not an object"))? {
        let season: i64 = s.parse()?;
        for (day, wins) in days.as_object().into_iter().flatten() {
            for (w, rows) in wins.as_object().into_iter().flatten() {
                let mut m = Map::new();
                for (pid, v) in rows.as_object().into_iter().flatten() {
                    let gp = v.get("gp").and_then(|g| g.as_i64().or_else(|| g.as_f64().map(|f| f.trunc() as i64)))
                        .ok_or_else(|| anyhow!("KeyError: 'gp'"))?;
                    let stats = v
                        .get("stats")
                        .and_then(|s| s.as_object())
                        .ok_or_else(|| anyhow!("KeyError: 'stats'"))?
                        .iter()
                        .filter_map(|(k, x)| x.as_f64().map(|f| (k.clone(), f)))
                        .collect();
                    m.insert(pid.parse::<i64>()?, WindowLine { gp, stats });
                }
                out.entry(season).or_default().entry(day.clone()).or_default().insert(w.clone(), m);
            }
        }
    }
    Ok(out)
}

pub fn save_windows(windows: &Windows, data_dir: &Path) -> Result<PathBuf> {
    let p = windows_path(data_dir)?;
    let mut raw = serde_json::Map::new();
    for (s, days) in windows {
        let mut dm = serde_json::Map::new();
        for (day, wins) in days {
            let mut wm = serde_json::Map::new();
            for (w, rows) in wins {
                let mut rm = serde_json::Map::new();
                for (pid, ln) in rows {
                    rm.insert(pid.to_string(), json!({"gp": ln.gp, "stats": ln.stats}));
                }
                wm.insert(w.clone(), Value::Object(rm));
            }
            dm.insert(day.clone(), Value::Object(wm));
        }
        raw.insert(s.to_string(), Value::Object(dm));
    }
    write_tmp_replace(&p, &dumps(&Value::Object(raw), &DumpOpts::compact()))?;
    Ok(p)
}

/// Fetch checkpoint windows (Nov 1 / Dec 1 / Jan 1) for each season; skips stored ones and
/// checkpoints that are still in the future.
pub fn build_windows(
    client: &NhlClient,
    data_dir: &Path,
    seasons: &[i64],
    force: bool,
    today: NaiveDate,
    mut log: Option<&mut dyn FnMut(&str)>,
) -> Result<Windows> {
    let mut windows = load_windows(data_dir)?;
    for &s in seasons {
        for day in checkpoint_dates(s) {
            if day >= today {
                continue;
            }
            let key = day.format("%Y-%m-%d").to_string();
            if windows.get(&s).map(|d| d.contains_key(&key)).unwrap_or(false) && !force {
                continue;
            }
            let cp = fetch_checkpoint(client, s, day)?;
            windows.entry(s).or_default().insert(key.clone(), cp);
            save_windows(&windows, data_dir)?;
            if let Some(l) = log.as_mut() {
                let n = windows[&s][&key]["to_date"].len();
                l(&format!("{} checkpoint {key}: {n} players with games to date", season_label(s)));
            }
        }
    }
    Ok(windows)
}
