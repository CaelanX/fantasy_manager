//! Rust port of `fantasy_manager/harness/outcomes.py` (grade recommendation episodes and my own
//! moves against realized NHL results) plus the archive scoring half of
//! `fantasy_manager/backtest/archive.py` (`score_snapshot` / `score_archive`, which need
//! `fm_backtest` and therefore cannot live in `fm_engine::archive`).
//! Owner: work package `W5-archive-outcomes`.
//!
//! See the Python module docstring for the windows and gain definitions; the port is line by line.
//! Deviations:
//! * `grade_outcomes` takes `as_of` (Python defaulted to `date.today()`).
//! * `Realized`'s Python `from_rows` is [`Realized::from_rows`]; `Players` keeps the ledger rows
//!   as JSON [`Row`]s.
//! * `replacement_pts` takes `Option<&mut ReplCache>` for Python's `cache: dict | None`.
//! * Python `sum(floats)` is `py_sum` (3.12 compensated summation) wherever Python used `sum`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, Result};
use chrono::{Duration, NaiveDate};
use fm_backtest::data::PlayerSeason;
use fm_backtest::evaluate::metrics;
use fm_backtest::scoring::{fpg, scorer, PRESETS};
use fm_core::models::{normalize_name, ScoringConfig, ScoringKind};
use fm_core::Map;
use fm_engine::archive::{pick_snapshot, snapshot_files};
use fm_engine::scoring::PointsScoring;
use fm_pycompat::num::{py_sum, round_to};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::episode_match::{first_lockable_day, load_episodes};
use crate::ledger::{json_dumps, now_iso, Ledger, Row};

pub const ROS: &str = "ros";
/// an NHL team's regular season: 82 games in ~186 days
pub const GAMES_PER_DAY: f64 = 82.0 / 186.0;
pub const REPLACEMENT_N: usize = 5;
pub const FLAG_KINDS: [&str; 2] = ["sell_high", "buy_low"];
/// regular season over by April 30 (ros windows complete after)
pub const SEASON_END: (u32, u32) = (4, 30);

/// `WINDOWS[name]` (days).
pub fn window_days(name: &str) -> i64 {
    match name {
        "7d" => 7,
        "28d" => 28,
        _ => 0,
    }
}

fn origin_of_status(status: &str) -> String {
    if status == "expired" { "ignored".to_string() } else { status.to_string() }
}

// --------------------------------------------------------------------------- row helpers

fn s_of(r: &Row, k: &str) -> Option<String> {
    r.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn f_of(r: &Row, k: &str) -> Option<f64> {
    match r.get(k) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn i_of(r: &Row, k: &str) -> Option<i64> {
    match r.get(k) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn parse_day(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|e| anyhow!("invalid isoformat string: {s:?} ({e})"))
}

fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn fv(x: f64) -> Value {
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

fn ofv(x: Option<f64>) -> Value {
    x.map(fv).unwrap_or(Value::Null)
}

// --------------------------------------------------------------------------- scoring config

fn meta(ledger: &Ledger, key: &str) -> Result<Option<String>> {
    let k = key.to_string();
    let rows = ledger.query("SELECT value FROM meta WHERE key=?", &[&k])?;
    Ok(rows.first().and_then(|r| s_of(r, "value")))
}

fn set_meta(ledger: &Ledger, key: &str, value: &str) -> Result<()> {
    let (k, v) = (key.to_string(), value.to_string());
    ledger.execute(
        "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        &[&k, &v],
    )?;
    ledger.commit()
}

/// Remember the league's ScoringConfig (`meta['scoring:<league>']`, set at daily-run time).
pub fn store_scoring(ledger: &Ledger, league: &str, cfg: &ScoringConfig) -> Result<()> {
    let v = serde_json::to_value(cfg)?;
    set_meta(ledger, &format!("scoring:{league}"), &json_dumps(&v, true))
}

fn archive_scoring(data_dir: &Path, league: &str) -> Option<Value> {
    let d = data_dir.join("archive");
    if !d.is_dir() {
        return None;
    }
    for path in snapshot_files(&d, "projections", league).into_iter().rev() {
        let Some(snap) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
            continue;
        };
        if let Some(sc) = snap.get("scoring").filter(|s| s.is_object()) {
            if sc.get("kind").is_some_and(|k| !k.is_null() && k.as_str() != Some("")) {
                return Some(sc.clone());
            }
        }
    }
    None
}

/// `(config, source)`: `meta` first, else the newest archived projections header (then stored in
/// `meta`), else the backtest preset of the same name.
pub fn scoring_config(ledger: &Ledger, league: &str) -> Result<(Option<ScoringConfig>, &'static str)> {
    if let Some(raw) = meta(ledger, &format!("scoring:{league}"))?.filter(|r| !r.is_empty()) {
        if let Ok(cfg) = serde_json::from_str::<ScoringConfig>(&raw) {
            return Ok((Some(cfg), "meta"));
        }
    }
    if let Some(arch) = archive_scoring(&ledger.data_dir, league) {
        if let Ok(cfg) = serde_json::from_value::<ScoringConfig>(arch) {
            store_scoring(ledger, league, &cfg)?;
            return Ok((Some(cfg), "archive"));
        }
    }
    if let Some(cfg) = PRESETS.get(league) {
        return Ok((Some(cfg.clone()), "preset"));
    }
    Ok((None, "none"))
}

/// PointsScoring for a points league, None otherwise (categories leagues are not graded).
pub fn points_scorer(cfg: Option<&ScoringConfig>) -> Option<PointsScoring> {
    let cfg = cfg?;
    if cfg.kind != ScoringKind::Points {
        return None;
    }
    scorer(cfg).ok()
}

fn stats_of(v: &Value) -> Map<String, f64> {
    v.as_object()
        .map(|o| o.iter().filter_map(|(k, x)| x.as_f64().map(|f| (k.clone(), f))).collect())
        .unwrap_or_default()
}

// --------------------------------------------------------------------------- realized results

/// Realized fantasy points per player per game date under one scoring.
#[derive(Clone, Debug, Default)]
pub struct Realized {
    pub pulled: HashSet<NaiveDate>,
    pub by_player: HashMap<i64, HashMap<NaiveDate, (i64, f64)>>,
}

impl Realized {
    pub fn new(ledger: &Ledger, sc: &PointsScoring, start: Option<NaiveDate>, end: Option<NaiveDate>) -> Result<Realized> {
        let mut where_ = Vec::new();
        let mut params: Vec<String> = Vec::new();
        if let Some(s) = start {
            where_.push("game_date >= ?");
            params.push(iso(s));
        }
        if let Some(e) = end {
            where_.push("game_date <= ?");
            params.push(iso(e));
        }
        let clause = if where_.is_empty() { String::new() } else { format!(" WHERE {}", where_.join(" AND ")) };
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        let mut out = Realized::default();
        for r in ledger.query(&format!("SELECT game_date FROM realized_pulls{clause}"), &refs)? {
            if let Some(d) = s_of(&r, "game_date") {
                out.pulled.insert(parse_day(&d)?);
            }
        }
        for r in ledger.query(
            &format!("SELECT nhl_id, game_date, gp, stats_json FROM realized_daily{clause}"),
            &refs,
        )? {
            let raw = s_of(&r, "stats_json").filter(|s| !s.is_empty()).unwrap_or_else(|| "{}".into());
            let Ok(parsed) = serde_json::from_str::<Value>(&raw) else { continue };
            let stats = stats_of(&parsed);
            let non_empty = parsed.as_object().is_some_and(|o| !o.is_empty());
            let pts = if non_empty { sc.value(&stats) } else { 0.0 };
            let (Some(nid), Some(day)) = (i_of(&r, "nhl_id"), s_of(&r, "game_date")) else { continue };
            out.by_player.entry(nid).or_default().insert(parse_day(&day)?, (i_of(&r, "gp").unwrap_or(0), pts));
        }
        Ok(out)
    }

    pub fn from_rows(pulled: impl IntoIterator<Item = NaiveDate>, rows: HashMap<i64, HashMap<NaiveDate, (i64, f64)>>) -> Realized {
        Realized { pulled: pulled.into_iter().collect(), by_player: rows }
    }

    pub fn day(&self, nhl_id: i64, d: NaiveDate) -> (i64, f64) {
        self.by_player.get(&nhl_id).and_then(|m| m.get(&d)).copied().unwrap_or((0, 0.0))
    }

    pub fn window(&self, nhl_id: i64, days: &[NaiveDate]) -> (i64, f64) {
        let mut gp = 0;
        let mut pts = 0.0;
        if let Some(rows) = self.by_player.get(&nhl_id) {
            for d in days {
                let (g, p) = rows.get(d).copied().unwrap_or((0, 0.0));
                gp += g;
                pts += p;
            }
        }
        (gp, pts)
    }

    pub fn covered(&self, days: &[NaiveDate]) -> bool {
        days.iter().all(|d| self.pulled.contains(d))
    }
}

pub fn days_between(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    if end >= start {
        (0..=(end - start).num_days()).map(|i| start + Duration::days(i)).collect()
    } else {
        Vec::new()
    }
}

pub fn season_end(day: NaiveDate) -> NaiveDate {
    use chrono::Datelike;
    let y = if day.month() >= 9 { day.year() } else { day.year() - 1 };
    NaiveDate::from_ymd_opt(y + 1, SEASON_END.0, SEASON_END.1).expect("valid season end")
}

#[derive(Clone, Debug)]
pub struct Window {
    pub name: String,
    pub start: NaiveDate,
    /// planned last day
    pub end: NaiveDate,
    /// pulled days graded so far (start .. min(end, as_of - 1))
    pub graded: Vec<NaiveDate>,
    pub complete: bool,
}

impl Window {
    pub fn n_days(&self) -> i64 {
        (self.end - self.start).num_days() + 1
    }
}

/// The window if it has started before `as_of` and at least one of its days was pulled (None
/// otherwise); only pulled days are graded.
pub fn make_window(name: &str, start: NaiveDate, end: NaiveDate, as_of: NaiveDate, realized: &Realized) -> Option<Window> {
    let last = end.min(as_of - Duration::days(1));
    if last < start {
        return None;
    }
    let graded: Vec<NaiveDate> = days_between(start, last).into_iter().filter(|d| realized.pulled.contains(d)).collect();
    if graded.is_empty() {
        return None;
    }
    let complete = end < as_of && realized.covered(&days_between(start, end));
    Some(Window { name: name.to_string(), start, end, graded, complete })
}

pub fn standard_windows(basis: NaiveDate, as_of: NaiveDate, realized: &Realized, names: &[&str]) -> Vec<Window> {
    let start = basis + Duration::days(1);
    names
        .iter()
        .filter_map(|name| {
            let end = if *name == ROS { season_end(basis) } else { basis + Duration::days(window_days(name)) };
            make_window(name, start, end, as_of, realized)
        })
        .collect()
}

// --------------------------------------------------------------------------- per-kind gains

/// Everything grading needs to know about a league's players (identity, projections).
#[derive(Clone, Debug, Default)]
pub struct Players {
    /// cid -> nhl_id
    pub nhl: HashMap<String, i64>,
    /// as_of -> cid -> projections row
    pub proj: HashMap<String, HashMap<String, Row>>,
    /// projection days, sorted
    pub days: Vec<String>,
}

impl Players {
    pub fn nhl_id(&self, cid: &str) -> Option<i64> {
        self.nhl.get(cid).copied()
    }

    /// The projections of `day` or of the latest earlier projection day (empty if none).
    pub fn proj_on(&self, day: NaiveDate) -> Option<&HashMap<String, Row>> {
        let key = iso(day);
        let n = self.days.partition_point(|d| d.as_str() <= key.as_str());
        if n == 0 {
            return None;
        }
        self.proj.get(&self.days[n - 1])
    }
}

pub fn load_players(ledger: &Ledger, league: &str) -> Result<Players> {
    let mut pl = Players::default();
    let l = league.to_string();
    for sql in [
        "SELECT cid, nhl_id FROM transactions WHERE league=? AND nhl_id IS NOT NULL",
        "SELECT cid, nhl_id FROM rec_players WHERE league=? AND nhl_id IS NOT NULL",
        "SELECT cid, nhl_id FROM projections WHERE league=? AND nhl_id IS NOT NULL ORDER BY as_of",
    ] {
        for r in ledger.query(sql, &[&l])? {
            if let (Some(c), Some(n)) = (s_of(&r, "cid"), i_of(&r, "nhl_id")) {
                pl.nhl.insert(c, n);
            }
        }
    }
    for r in ledger.query(
        "SELECT as_of, cid, nhl_id, fpg, vorp, games_next7, positions, inputs_json FROM projections WHERE league=?",
        &[&l],
    )? {
        let (Some(a), Some(c)) = (s_of(&r, "as_of"), s_of(&r, "cid")) else { continue };
        pl.proj.entry(a).or_default().insert(c, r);
    }
    let mut days: Vec<String> = pl.proj.keys().cloned().collect();
    days.sort();
    pl.days = days;
    Ok(pl)
}

/// `(points, games, unmatched cids)` of `cids` over `days`.
fn pts_of(players: &Players, realized: &Realized, cids: &[String], days: &[NaiveDate]) -> (f64, i64, Vec<String>) {
    let (mut pts, mut gp, mut missing) = (0.0, 0, Vec::new());
    for c in cids {
        match players.nhl_id(c) {
            None => missing.push(c.clone()),
            Some(nid) => {
                let (g, p) = realized.window(nid, days);
                pts += p;
                gp += g;
            }
        }
    }
    (pts, gp, missing)
}

pub type ReplCache = HashMap<(NaiveDate, Vec<NaiveDate>, BTreeSet<String>), Option<f64>>;

/// Mean realized points over `days` of the `REPLACEMENT_N` players with |vorp| closest to 0 in
/// the basis day's projections, other than the traded ones (None without v2 vorp).
pub fn replacement_pts(
    players: &Players,
    realized: &Realized,
    basis: NaiveDate,
    days: &[NaiveDate],
    cache: Option<&mut ReplCache>,
    exclude: &[String],
) -> Option<f64> {
    let skip: BTreeSet<String> = exclude.iter().cloned().collect();
    let key = (basis, days.to_vec(), skip);
    if let Some(c) = cache.as_deref() {
        if let Some(v) = c.get(&key) {
            return *v;
        }
    }
    let mut rows: Vec<(f64, &str, i64)> = Vec::new();
    if let Some(proj) = players.proj_on(basis) {
        for r in proj.values() {
            let cid = s_of_ref(r, "cid");
            let (Some(vorp), Some(nid)) = (f_of(r, "vorp"), i_of(r, "nhl_id")) else { continue };
            if nid == 0 || key.2.contains(cid) {
                continue;
            }
            rows.push((vorp.abs(), cid, nid));
        }
    }
    rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(b.1)));
    rows.truncate(REPLACEMENT_N);
    let val = if rows.is_empty() {
        None
    } else {
        Some(py_sum(rows.iter().map(|(_, _, nid)| realized.window(*nid, days).1)) / rows.len() as f64)
    };
    if let Some(c) = cache {
        c.insert(key, val);
    }
    val
}

fn s_of_ref<'a>(r: &'a Row, k: &str) -> &'a str {
    r.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

/// Expected games per day of the added player(s): games_next7 / 7 that day, else the NHL mean.
pub fn games_rate(players: &Players, basis: NaiveDate, adds: &[String]) -> f64 {
    let rates: Vec<f64> = match players.proj_on(basis) {
        None => Vec::new(),
        Some(proj) => adds
            .iter()
            .filter_map(|c| proj.get(c).and_then(|r| f_of(r, "games_next7")).map(|g| g / 7.0))
            .collect(),
    };
    if rates.is_empty() {
        GAMES_PER_DAY
    } else {
        py_sum(rates.iter().copied()) / rates.len() as f64
    }
}

/// `predicted_gain` as points over an `n_days` window (`gain_pts_h`).
pub fn predicted_pts(gain: Option<f64>, units: Option<&str>, n_days: i64, rate: f64) -> Option<f64> {
    let (gain, units) = (gain?, units?);
    match units {
        "week_pts" => Some(gain * n_days as f64 / 7.0),
        // points over a horizon inside the window: as is
        "day_pts" | "matchup_pts" => Some(gain),
        "season_fpg" | "lineup_fpg" | "dynasty" => Some(gain * rate * n_days as f64),
        _ => None,
    }
}

fn fpg_of(line: Option<&Value>, sc: &PointsScoring) -> Option<f64> {
    let line = line?;
    let gp = line.get("gp").and_then(|g| g.as_f64()).filter(|g| *g != 0.0)?;
    Some(sc.value(&stats_of(line.get("stats").unwrap_or(&Value::Null))) / gp)
}

pub fn l15_fpg(players: &Players, cid: &str, basis: NaiveDate, sc: &PointsScoring) -> Option<f64> {
    let row = players.proj_on(basis)?.get(cid)?;
    let raw = s_of(row, "inputs_json").filter(|s| !s.is_empty())?;
    let inputs: Value = serde_json::from_str(&raw).ok()?;
    fpg_of(inputs.get("last15"), sc)
}

/// The model's own view of a move that day: fpg(adds) - fpg(drops) from that day's projections.
pub fn model_fpg_delta(players: &Players, day: NaiveDate, adds: &[String], drops: &[String]) -> Option<f64> {
    if adds.is_empty() && drops.is_empty() {
        return None;
    }
    let empty = HashMap::new();
    let proj = players.proj_on(day).unwrap_or(&empty);
    let mut vals = Vec::new();
    for (c, sign) in adds.iter().map(|c| (c, 1.0)).chain(drops.iter().map(|c| (c, -1.0))) {
        let f = proj.get(c).and_then(|r| f_of(r, "fpg"))?;
        vals.push(sign * f);
    }
    Some(py_sum(vals))
}

// --------------------------------------------------------------------------- grading one subject

/// One thing to grade: an episode on one basis, or one of my user-only moves.
#[derive(Clone, Debug)]
pub struct Subject {
    pub league: String,
    pub kind: String,
    pub origin: String,
    /// first_seen | acted_on | decision
    pub basis: String,
    pub day: NaiveDate,
    pub adds: Vec<String>,
    pub drops: Vec<String>,
    pub episode_id: Option<String>,
    pub decision_id: Option<String>,
    pub predicted_gain: Option<f64>,
    pub gain_units: Option<String>,
    pub horizon_days: Option<i64>,
    pub title: Option<String>,
    pub strength: Option<f64>,
    pub last_seen: Option<NaiveDate>,
}

impl Subject {
    pub fn new(league: &str, kind: &str, origin: &str, basis: &str, day: NaiveDate, adds: Vec<String>, drops: Vec<String>) -> Subject {
        Subject {
            league: league.into(),
            kind: kind.into(),
            origin: origin.into(),
            basis: basis.into(),
            day,
            adds,
            drops,
            episode_id: None,
            decision_id: None,
            predicted_gain: None,
            gain_units: None,
            horizon_days: None,
            title: None,
            strength: None,
            last_seen: None,
        }
    }
}

pub fn outcome_id(s: &Subject, window: &str) -> String {
    let r = match (&s.episode_id, s.basis.as_str()) {
        (Some(e), b) if !e.is_empty() && b != "decision" => format!("ep:{e}"),
        _ => format!("dec:{}", s.decision_id.as_deref().map(|d| d.to_string()).unwrap_or_else(|| "None".into())),
    };
    let mut h = Sha1::new();
    h.update(format!("{}|{}|{}|{}", s.league, r, s.basis, window).as_bytes());
    hex::encode(h.finalize())[..20].to_string()
}

fn row_of(
    s: &Subject,
    w: &Window,
    realized_gain: Option<f64>,
    hit: Option<i64>,
    pred_pts: Option<f64>,
    mut detail: serde_json::Map<String, Value>,
    predicted_gain: Option<f64>,
    units: Option<&str>,
) -> Row {
    detail.insert("graded_through".into(), json!(iso(*w.graded.last().expect("non-empty window"))));
    detail.insert("days".into(), json!(w.graded.len()));
    detail.insert("title".into(), json!(s.title));
    detail.insert("day".into(), json!(iso(s.day)));
    detail.insert("strength".into(), ofv(s.strength));
    let mut r = Row::new();
    r.insert("outcome_id".into(), json!(outcome_id(s, &w.name)));
    r.insert("league".into(), json!(s.league));
    r.insert("episode_id".into(), json!(s.episode_id));
    r.insert("decision_id".into(), json!(s.decision_id));
    r.insert("window".into(), json!(w.name));
    r.insert("basis".into(), json!(s.basis));
    r.insert("predicted_gain".into(), ofv(predicted_gain.or(s.predicted_gain)));
    r.insert("realized_gain".into(), ofv(realized_gain.map(|g| round_to(g, 4))));
    r.insert("gain_units".into(), json!(units.map(|u| u.to_string()).or_else(|| s.gain_units.clone())));
    r.insert("hit".into(), json!(hit));
    r.insert("partial".into(), json!(if w.complete { 0 } else { 1 }));
    r.insert("graded_at".into(), json!(now_iso()));
    r.insert("kind".into(), json!(s.kind));
    r.insert("origin".into(), json!(s.origin));
    r.insert("complete".into(), json!(w.complete as i64));
    r.insert("window_start".into(), json!(iso(w.start)));
    r.insert("window_end".into(), json!(iso(w.end)));
    r.insert("predicted_pts".into(), ofv(pred_pts.map(|p| round_to(p, 4))));
    r.insert("detail_json".into(), json!(json_dumps(&Value::Object(detail), true)));
    r
}

fn hit_of(v: Option<f64>) -> Option<i64> {
    v.map(|x| (x > 0.0) as i64)
}

fn obj(pairs: Vec<(&str, Value)>) -> serde_json::Map<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

pub fn grade_subject(
    s: &Subject,
    players: &Players,
    realized: &Realized,
    as_of: NaiveDate,
    sc: &PointsScoring,
    provider: &str,
    mut repl_cache: Option<&mut ReplCache>,
) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    let units = s.gain_units.as_deref();
    if s.kind == "lineup" {
        let start = if provider == "fantrax" { first_lockable_day("fantrax", s.day) } else { s.day };
        // a daily start/sit (day_pts) is about that day only
        let days: i64 = if units == Some("day_pts") { 1 } else { 7 };
        let Some(w) = make_window("7d", start, start + Duration::days(days - 1), as_of, realized) else {
            return out;
        };
        let unmatched: Vec<String> =
            s.adds.iter().chain(&s.drops).filter(|c| players.nhl_id(c).is_none()).cloned().collect();
        let nid = |c: &String| players.nhl_id(c).expect("matched");
        let (gain, shared): (Option<f64>, i64);
        if !unmatched.is_empty() || s.adds.is_empty() {
            // "Bench X (out)": nothing realized to compare
            gain = None;
            shared = 0;
        } else if s.drops.is_empty() {
            // an empty starting slot filled: his points vs nothing
            let n = w.graded.iter().filter(|d| s.adds.iter().any(|c| realized.day(nid(c), **d).0 != 0)).count() as i64;
            shared = n;
            gain = if n != 0 { Some(pts_of(players, realized, &s.adds, &w.graded).0) } else { None };
        } else if provider == "fantrax" {
            // weekly lock: the whole locked week counts
            let (p_in, g_in, _) = pts_of(players, realized, &s.adds, &w.graded);
            let (p_out, g_out, _) = pts_of(players, realized, &s.drops, &w.graded);
            gain = if g_in + g_out != 0 { Some(p_in - p_out) } else { None };
            shared = w.graded.len() as i64;
        } else {
            // ESPN daily: only days both sides played
            let (mut g, mut sh) = (0.0, 0);
            for d in &w.graded {
                let ins: Vec<(i64, f64)> = s.adds.iter().map(|c| realized.day(nid(c), *d)).collect();
                let outs: Vec<(i64, f64)> = s.drops.iter().map(|c| realized.day(nid(c), *d)).collect();
                let played_in = ins.iter().any(|(g, _)| *g != 0);
                let played_out = outs.iter().any(|(g, _)| *g != 0);
                let ok = if days == 1 { played_in || played_out } else { played_in && played_out };
                if ok {
                    g += py_sum(ins.iter().map(|x| x.1)) - py_sum(outs.iter().map(|x| x.1));
                    sh += 1;
                }
            }
            shared = sh;
            gain = if sh == 0 { None } else { Some(g) };
        }
        let pp = predicted_pts(s.predicted_gain, units, w.n_days(), GAMES_PER_DAY);
        let detail = obj(vec![
            ("shared_days", json!(shared)),
            ("unmatched", json!(unmatched)),
            ("lock", json!(provider == "fantrax")),
        ]);
        out.push(row_of(s, &w, gain, hit_of(gain), pp, detail, None, None));
        return out;
    }

    if FLAG_KINDS.contains(&s.kind.as_str()) {
        let target = if s.kind == "sell_high" { &s.drops } else { &s.adds };
        for w in standard_windows(s.day, as_of, realized, &["28d"]) {
            let mut detail = obj(vec![("players", json!(target))]);
            let mut gain = None;
            if target.len() == 1 && players.nhl_id(&target[0]).is_some() {
                let base = l15_fpg(players, &target[0], s.day, sc);
                let (gp, pts) = realized.window(players.nhl[&target[0]], &w.graded);
                detail.insert("l15_fpg".into(), ofv(base));
                detail.insert("gp".into(), json!(gp));
                detail.insert("fpg_window".into(), if gp != 0 { fv(pts / gp as f64) } else { Value::Null });
                if let Some(b) = base {
                    if gp > 0 {
                        gain = Some(pts / gp as f64 - b);
                    }
                }
            } else {
                let un: Vec<&String> = target.iter().filter(|c| players.nhl_id(c).is_none()).collect();
                detail.insert("unmatched".into(), json!(un));
            }
            let hit = gain.map(|g| if s.kind == "sell_high" { (g < 0.0) as i64 } else { (g > 0.0) as i64 });
            out.push(row_of(s, &w, gain, hit, None, detail, None, Some("season_fpg")));
        }
        return out;
    }

    if s.adds.is_empty() && s.drops.is_empty() {
        return out; // an IR-only move: nothing to grade
    }
    let rate = games_rate(players, s.day, &s.adds);
    for w in standard_windows(s.day, as_of, realized, &["7d", "28d", ROS]) {
        let (p_in, g_in, m_in) = pts_of(players, realized, &s.adds, &w.graded);
        let (p_out, g_out, m_out) = pts_of(players, realized, &s.drops, &w.graded);
        let unmatched: Vec<String> = m_in.into_iter().chain(m_out).collect();
        let mut detail = obj(vec![
            ("pts_in", fv(round_to(p_in, 3))),
            ("pts_out", fv(round_to(p_out, 3))),
            ("gp_in", json!(g_in)),
            ("gp_out", json!(g_out)),
            ("unmatched", json!(unmatched)),
        ]);
        // nobody on either side played (preseason, a break): nothing to judge, not a miss
        let mut gain: Option<f64> = if !unmatched.is_empty() || g_in + g_out == 0 { None } else { Some(p_in - p_out) };
        if s.kind == "trade" {
            if let Some(g) = gain {
                let n_diff = s.adds.len() as i64 - s.drops.len() as i64;
                if n_diff != 0 {
                    let excl: Vec<String> = s.adds.iter().chain(&s.drops).cloned().collect();
                    let repl = replacement_pts(players, realized, s.day, &w.graded, repl_cache.as_deref_mut(), &excl);
                    detail.insert("replacement_pts".into(), ofv(repl.map(|r| round_to(r, 3))));
                    if let Some(r) = repl {
                        gain = Some(g - n_diff as f64 * r);
                    }
                }
            }
        }
        if s.kind == "trade" && w.name == ROS {
            detail.insert("label".into(), json!(if !w.complete { "partial" } else { "season" }));
        }
        let n_days = if w.name != ROS { w.n_days() } else { w.graded.len() as i64 };
        let pp = predicted_pts(
            s.predicted_gain,
            units,
            n_days,
            if units == Some("season_fpg") { rate } else { GAMES_PER_DAY },
        );
        if s.origin == "user_only" {
            let m = model_fpg_delta(players, s.day, &s.adds, &s.drops);
            detail.insert("model_fpg_delta".into(), ofv(m.map(|x| round_to(x, 4))));
            let pp = predicted_pts(m, Some("season_fpg"), n_days, rate);
            out.push(row_of(s, &w, gain, hit_of(gain), pp, detail, m, Some("season_fpg")));
        } else {
            out.push(row_of(s, &w, gain, hit_of(gain), pp, detail, None, None));
        }
    }
    out
}

// --------------------------------------------------------------------------- subjects of a league

fn json_list(v: Option<String>) -> Vec<String> {
    let Some(raw) = v.filter(|s| !s.is_empty()) else { return Vec::new() };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Array(a)) => {
            let mut out: Vec<String> = a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
            out.sort();
            out
        }
        _ => Vec::new(),
    }
}

fn sorted_strs(v: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = v
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    out.sort();
    out
}

fn truthy_list(v: Option<&Value>) -> bool {
    v.and_then(|a| a.as_array()).is_some_and(|a| !a.is_empty())
}

/// (adds, drops) graded for an alert episode: the matched move of a followed alert, nothing (not
/// graded) otherwise or when nothing was added (a bare drop).
fn alert_sides(status: &str, m: &Value) -> (Vec<String>, Vec<String>) {
    if !(status == "followed" || status == "partial") || !truthy_list(m.get("added")) {
        return (Vec::new(), Vec::new());
    }
    (sorted_strs(m.get("added")), sorted_strs(m.get("dropped")))
}

pub fn load_subjects(ledger: &Ledger, league: &str) -> Result<Vec<Subject>> {
    let eps: HashMap<String, _> = load_episodes(ledger, league)?.into_iter().map(|e| (e.episode_id.clone(), e)).collect();
    let l = league.to_string();
    let rows = ledger.query("SELECT * FROM rec_episodes WHERE league=? ORDER BY first_seen", &[&l])?;
    let mut decisions: HashMap<String, Row> = HashMap::new();
    for d in ledger.query("SELECT * FROM decisions WHERE league=? AND episode_id IS NOT NULL", &[&l])? {
        if let Some(e) = s_of(&d, "episode_id") {
            decisions.insert(e, d);
        }
    }
    let mut out: Vec<Subject> = Vec::new();
    for r in &rows {
        let Some(eid) = s_of(r, "episode_id") else { continue };
        let Some(e) = eps.get(&eid) else { continue };
        let status = s_of(r, "status").unwrap_or_default();
        let kind = s_of(r, "kind").unwrap_or_default();
        let origin = origin_of_status(&status);
        let m: Value = s_of(r, "match_json")
            .filter(|s| !s.is_empty())
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(|v: &Value| v.is_object())
            .unwrap_or_else(|| json!({}));
        let make = |basis: &str, day: NaiveDate, adds: Vec<String>, drops: Vec<String>, decision_id: Option<String>| {
            let mut s = Subject::new(league, &kind, &origin, basis, day, adds, drops);
            s.episode_id = Some(eid.clone());
            s.decision_id = decision_id;
            s.predicted_gain = f_of(r, "predicted_gain");
            s.gain_units = s_of(r, "gain_units");
            s.horizon_days = i_of(r, "horizon_days");
            s.title = s_of(r, "title");
            s.strength = f_of(r, "strength");
            s.last_seen = Some(e.last_seen);
            s
        };
        let mut adds0: Vec<String> = e.adds.iter().cloned().collect();
        let mut drops0: Vec<String> = e.drops.iter().cloned().collect();
        if kind == "alert" {
            (adds0, drops0) = alert_sides(&status, &m);
        }
        out.push(make("first_seen", e.first_seen, adds0, drops0, None));
        let dec = decisions.get(&eid);
        if let Some(acted) = s_of(r, "acted_on").filter(|a| !a.is_empty()) {
            if (status == "followed" || status == "partial") && kind != "lineup" {
                let mut adds: Vec<String> = e.adds.iter().cloned().collect();
                let mut drops: Vec<String> = e.drops.iter().cloned().collect();
                if kind == "alert" {
                    (adds, drops) = alert_sides(&status, &m);
                } else if kind == "waiver" && truthy_list(m.get("added")) {
                    adds = sorted_strs(m.get("added"));
                    drops = sorted_strs(m.get("dropped"));
                } else if kind == "trade" && truthy_list(m.get("got")) {
                    adds = sorted_strs(m.get("got"));
                    drops = sorted_strs(m.get("gave"));
                }
                let did = dec.and_then(|d| s_of(d, "decision_id"));
                out.push(make("acted_on", parse_day(&acted)?, adds, drops, did));
            }
        }
    }
    for d in ledger.query("SELECT * FROM decisions WHERE league=? AND origin='user_only'", &[&l])? {
        let kind = s_of(&d, "kind").filter(|k| !k.is_empty()).unwrap_or_else(|| "waiver".into());
        let day = parse_day(&s_of(&d, "day").unwrap_or_default())?;
        let mut s = Subject::new(
            league,
            &kind,
            "user_only",
            "decision",
            day,
            json_list(s_of(&d, "adds_json")),
            json_list(s_of(&d, "drops_json")),
        );
        s.decision_id = s_of(&d, "decision_id");
        out.push(s);
    }
    Ok(out)
}

/// The model's own pick of the same kind that day: the rec episode (first_seen basis) live on my
/// move's day with the highest predicted gain (then strength).
pub fn model_alternative<'a>(s: &Subject, subjects: &'a [Subject]) -> Option<&'a Subject> {
    let mut best: Option<((f64, f64), &Subject)> = None;
    for e in subjects {
        if e.basis != "first_seen" || e.kind != s.kind || e.episode_id.is_none() || e.adds.is_empty() {
            continue;
        }
        let last = e.last_seen.unwrap_or(e.day);
        if !(e.day <= s.day && s.day <= last) {
            continue;
        }
        let key = (e.predicted_gain.unwrap_or(f64::NEG_INFINITY), e.strength.unwrap_or(f64::NEG_INFINITY));
        let better = match &best {
            None => true,
            Some((bk, _)) => key.0 > bk.0 || (key.0 == bk.0 && key.1 > bk.1),
        };
        if better {
            best = Some((key, e));
        }
    }
    best.map(|(_, e)| e)
}

/// Counterfactual: the model's pick that day, realized over the same windows as my move.
fn attach_alternative(
    s: &Subject,
    rows: &mut [Row],
    subjects: &[Subject],
    players: &Players,
    realized: &Realized,
    repl_cache: &mut ReplCache,
) -> Result<()> {
    let alt = model_alternative(s, subjects);
    for r in rows.iter_mut() {
        let mut detail: Value = serde_json::from_str(&s_of(r, "detail_json").unwrap_or_else(|| "{}".into()))?;
        match alt {
            None => detail["alt"] = Value::Null,
            Some(alt) => {
                let start = parse_day(&s_of(r, "window_start").unwrap_or_default())?;
                let through = parse_day(detail["graded_through"].as_str().unwrap_or_default())?;
                let days = days_between(start, through);
                let (p_in, g_in, m_in) = pts_of(players, realized, &alt.adds, &days);
                let (p_out, g_out, m_out) = pts_of(players, realized, &alt.drops, &days);
                let mut gain = if !m_in.is_empty() || !m_out.is_empty() || g_in + g_out == 0 { None } else { Some(p_in - p_out) };
                if let Some(g) = gain {
                    if s.kind == "trade" && alt.adds.len() != alt.drops.len() {
                        let excl: Vec<String> = alt.adds.iter().chain(&alt.drops).cloned().collect();
                        let repl = replacement_pts(players, realized, s.day, &days, Some(repl_cache), &excl);
                        if let Some(rp) = repl {
                            gain = Some(g - (alt.adds.len() as f64 - alt.drops.len() as f64) * rp);
                        }
                    }
                }
                detail["alt"] = json!({"episode_id": alt.episode_id, "title": alt.title, "gain": ofv(gain.map(|g| round_to(g, 4)))});
            }
        }
        r.insert("detail_json".into(), json!(json_dumps(&detail, true)));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct GradeSummary {
    pub league: String,
    pub scoring: String,
    pub graded: i64,
    pub complete: i64,
    pub partial: i64,
    pub ungradable: i64,
    pub unmatched: i64,
    pub by_kind: Map<String, i64>,
    pub note: Option<String>,
}

impl GradeSummary {
    pub fn new(league: &str) -> GradeSummary {
        GradeSummary {
            league: league.into(),
            scoring: "none".into(),
            graded: 0,
            complete: 0,
            partial: 0,
            ungradable: 0,
            unmatched: 0,
            by_kind: Map::new(),
            note: None,
        }
    }

    /// `dict(self.__dict__)` (dataclass field order).
    pub fn to_dict(&self) -> Value {
        json!({
            "league": self.league, "scoring": self.scoring, "graded": self.graded, "complete": self.complete,
            "partial": self.partial, "ungradable": self.ungradable, "unmatched": self.unmatched,
            "by_kind": self.by_kind, "note": self.note,
        })
    }

    pub fn line(&self) -> String {
        if let Some(n) = &self.note {
            return format!("{}: {}", self.league, n);
        }
        let mut kinds: Vec<(&String, &i64)> = self.by_kind.iter().collect();
        kinds.sort();
        let kinds = if kinds.is_empty() {
            "none".to_string()
        } else {
            kinds.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")
        };
        format!(
            "{}: {} outcome rows ({} complete, {} partial window; {} without a realized gain, {} with unmatched players); {}",
            self.league, self.graded, self.complete, self.partial, self.ungradable, self.unmatched, kinds
        )
    }
}

/// Grade every episode and user-only move of `league` whose window has started by `as_of`;
/// idempotent.
pub fn grade_outcomes(ledger: &Ledger, league: &str, as_of: NaiveDate) -> Result<GradeSummary> {
    let mut summary = GradeSummary::new(league);
    let (cfg, src) = scoring_config(ledger, league)?;
    summary.scoring = src.to_string();
    let Some(sc) = points_scorer(cfg.as_ref()) else {
        summary.note = Some("not a points league (or no scoring known): outcomes not graded".into());
        return Ok(summary);
    };
    let subjects = load_subjects(ledger, league)?;
    let players = load_players(ledger, league)?;
    let first = subjects.iter().map(|s| s.day).min().unwrap_or(as_of);
    let realized = Realized::new(ledger, &sc, Some(first), Some(as_of))?;
    let mut repl_cache = ReplCache::new();
    let mut rows: Vec<Row> = Vec::new();
    for s in &subjects {
        let mut graded = grade_subject(s, &players, &realized, as_of, &sc, league, Some(&mut repl_cache));
        if s.origin == "user_only" && !graded.is_empty() {
            attach_alternative(s, &mut graded, &subjects, &players, &realized, &mut repl_cache)?;
        }
        rows.extend(graded);
    }
    let keep: HashSet<String> = rows.iter().filter_map(|r| s_of(r, "outcome_id")).collect();
    let l = league.to_string();
    for r in ledger.query("SELECT outcome_id FROM outcomes WHERE league=?", &[&l])? {
        if let Some(id) = s_of(&r, "outcome_id") {
            if !keep.contains(&id) {
                ledger.execute("DELETE FROM outcomes WHERE outcome_id=?", &[&id])?;
            }
        }
    }
    ledger.commit()?;
    ledger.upsert("outcomes", &rows, &["outcome_id"])?;
    summary.graded = rows.len() as i64;
    summary.complete = rows.iter().map(|r| i_of(r, "complete").unwrap_or(0)).sum();
    summary.partial = summary.graded - summary.complete;
    summary.ungradable = rows.iter().filter(|r| r.get("realized_gain").is_none_or(|v| v.is_null())).count() as i64;
    summary.unmatched = rows
        .iter()
        .filter(|r| {
            s_of(r, "detail_json")
                .and_then(|d| serde_json::from_str::<Value>(&d).ok())
                .is_some_and(|d| truthy_list(d.get("unmatched")))
        })
        .count() as i64;
    for r in &rows {
        *summary.by_kind.entry(s_of(r, "kind").unwrap_or_default()).or_insert(0) += 1;
    }
    Ok(summary)
}

// --------------------------------------------------------------------------- scoring the archive
// (backtest/archive.py: score_snapshot / score_archive)

struct ActualsIndex<'a> {
    by_id: HashMap<i64, &'a PlayerSeason>,
    by_name: HashMap<String, Vec<&'a PlayerSeason>>,
}

fn actuals_index(rows: &[PlayerSeason]) -> ActualsIndex<'_> {
    let mut by_id = HashMap::new();
    let mut by_name: HashMap<String, Vec<&PlayerSeason>> = HashMap::new();
    for r in rows {
        by_id.insert(r.player_id, r);
        by_name.entry(normalize_name(&r.name)).or_default().push(r);
    }
    ActualsIndex { by_id, by_name }
}

fn metrics_json(m: &fm_backtest::evaluate::Metrics) -> Value {
    json!({"n": m.n, "mae": fv(m.mae), "rmse": fv(m.rmse), "bias": fv(m.bias), "spearman": fv(m.spearman),
           "top50": ofv(m.top50), "top100": ofv(m.top100)})
}

/// Metrics of the provider projection and of `fm` vs actual FPG, on the players both cover.
/// `scoring` None = the snapshot's own.
pub fn score_snapshot(snapshot: &Value, season_actuals: &[PlayerSeason], scoring: Option<&ScoringConfig>, min_gp: i64) -> Result<Value> {
    let cfg: ScoringConfig = match scoring {
        Some(c) => c.clone(),
        None => match snapshot.get("scoring").filter(|s| s.as_object().is_some_and(|o| !o.is_empty())) {
            Some(s) => serde_json::from_value(s.clone())?,
            None => serde_json::from_value(json!({"kind": "points"}))?,
        },
    };
    let sc = scorer(&cfg)?;
    let idx = actuals_index(season_actuals);
    struct Pool {
        provider: Vec<f64>,
        fm: Vec<f64>,
        actual: Vec<f64>,
    }
    let mut pools: Map<String, Pool> = Map::new();
    let mut unmatched = 0;
    for rec in snapshot.get("players").and_then(|p| p.as_array()).map(|a| a.as_slice()).unwrap_or(&[]) {
        let nhl = match rec.get("nhl_id") {
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::String(s)) if !s.is_empty() => s.trim().parse().ok(),
            _ => None,
        };
        let mut act = nhl.and_then(|i| idx.by_id.get(&i).copied());
        if act.is_none() {
            let name = rec.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if let Some(c) = idx.by_name.get(&normalize_name(name)) {
                if c.len() == 1 {
                    act = Some(c[0]);
                }
            }
        }
        let proj = rec.get("projected").filter(|p| !p.is_null());
        let mine = rec.get("fm").filter(|p| !p.is_null());
        let Some(act) = act else {
            unmatched += 1;
            continue;
        };
        let proj_gp = proj.and_then(|p| p.get("gp")).and_then(|g| g.as_f64()).unwrap_or(0.0);
        let mine_rates = mine.and_then(|m| m.get("rates")).filter(|r| r.as_object().is_some_and(|o| !o.is_empty()));
        if act.gp < min_gp || proj.is_none() || proj_gp == 0.0 || mine_rates.is_none() {
            continue;
        }
        let proj = proj.unwrap();
        let line = fm_core::models::StatLine {
            split: fm_core::models::Split::Projected,
            gp: proj_gp as i64,
            stats: stats_of(proj.get("stats").unwrap_or(&Value::Null)),
        };
        let pool = pools.entry(if act.group == "G" { "goalies" } else { "skaters" }.to_string()).or_insert_with(|| Pool {
            provider: vec![],
            fm: vec![],
            actual: vec![],
        });
        pool.provider.push(fpg(Some(&line.per_game()), &sc));
        pool.fm.push(fpg(Some(&stats_of(mine_rates.unwrap())), &sc));
        pool.actual.push(fpg(Some(&act.per_game()), &sc));
    }
    let mut out_pools = serde_json::Map::new();
    for (pool, v) in &pools {
        let top = pool == "skaters";
        out_pools.insert(
            pool.clone(),
            json!({"provider": metrics_json(&metrics(&v.provider, &v.actual, top)),
                   "fm": metrics_json(&metrics(&v.fm, &v.actual, top))}),
        );
    }
    Ok(json!({"provider": snapshot.get("provider"), "as_of": snapshot.get("as_of"), "unmatched": unmatched,
              "pools": Value::Object(out_pools)}))
}

/// End-of-season grading: per provider, its archived projection vs ours vs actual FPG. `which`
/// picks the snapshot ("first" = preseason, "last", or an ISO date).
pub fn score_archive(
    season_actuals: &[PlayerSeason],
    data_dir: &Path,
    providers: &[&str],
    which: &str,
    scoring: Option<&ScoringConfig>,
    min_gp: i64,
    start: Option<NaiveDate>,
    end: Option<NaiveDate>,
) -> Result<Value> {
    let mut out = serde_json::Map::new();
    for prov in providers {
        let Some(path) = pick_snapshot(data_dir, prov, which, start, end)? else { continue };
        let snap: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let mut entry = serde_json::Map::new();
        entry.insert("snapshot".into(), json!(path.file_name().map(|n| n.to_string_lossy().into_owned())));
        if let Value::Object(m) = score_snapshot(&snap, season_actuals, scoring, min_gp)? {
            entry.extend(m);
        }
        out.insert(prov.to_string(), Value::Object(entry));
    }
    Ok(Value::Object(out))
}
