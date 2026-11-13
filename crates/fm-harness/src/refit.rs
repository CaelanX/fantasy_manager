//! Rust port of `fantasy_manager/harness/refit.py`: bounded auto-correction of a few valuation
//! parameters (M3, `fm harness refit`). See the Python module docstring for the replay table, the
//! objective, the gate, the calendar and champion / challenger; the port is line by line.
//! Owner: work package `W6-harness-metrics-refit`.
//!
//! Deviations:
//! * `run_refit` takes `as_of` explicitly and its keyword arguments as [`RefitOptions`]
//!   (`hist_pool_k` replaces the tests' monkeypatch of `HIST_POOL_K`).
//! * Python's `inseason_projection(obs)` dispatches on the type: [`inseason_projection`] for
//!   replay rows, [`hist_projection`] for historical checkpoints.
//! * The per-observation replays inside one loss evaluation run in parallel (rayon); errors are
//!   collected in order and summed with `py_sum`, so results are identical to the sequential
//!   Python. The coordinate descent itself is sequential (same order, same memo keys).
//! * Unknown groups raise an `anyhow` error with Python's `ValueError` message.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};
use chrono::{Datelike, Duration, NaiveDate};
use fm_backtest::data::{load_table, load_windows};
use fm_backtest::evaluate::{inseason_observations, MIN_GP_REST};
use fm_backtest::fit::{inseason_projection_split, InseasonObs};
use fm_core::models::{Player, StatLine};
use fm_core::values::Horizon;
use fm_core::Map;
use fm_engine::recommend::trades::{calibrate_acceptance, AcceptParams};
use fm_engine::scoring::PointsScoring;
use fm_engine::valuation::params as vparams;
use fm_engine::valuation::schedule::{proj_week, shrunk_share};
use fm_engine::valuation::valuate::{player_rates, position_group, History, Means};
use fm_pycompat::fmt::{fixed, general, percent};
use fm_pycompat::num::{py_sum, round_to};
use rayon::prelude::*;
use serde::Serialize;
use serde_json::{json, Value};

use crate::ingest::record_run;
use crate::ledger::Ledger;
use crate::metrics::{bootstrap_skill, f_of, fv, iso, ofv, s_of, truthy, val_f64, weekly_snapshots, MIN_GP_28};
use crate::outcomes::{days_between, points_scorer, scoring_config, Realized};
use crate::params_store::{ParamsStore, Rec, PACKAGED};

// ---- calendar
pub fn refit_start() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 2).expect("date")
}
pub fn auto_apply_start() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 16).expect("date")
}
pub fn tier_b_apply_start() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 12, 1).expect("date")
}
pub const REFIT_EVERY_DAYS: i64 = 14;

// ---- gate
pub const MIN_LIVE_OBS: usize = 400;
pub const MIN_WEEKS: usize = 4;
pub const MIN_GOALIE_OBS: usize = 150;
pub const MAX_GROUPS: usize = 2;
pub const MIN_HOLDOUT_GAIN: f64 = 0.01;
pub const MAX_HIST_WORSE: f64 = 0.005;
pub const CI_LEVEL: f64 = 0.90;
pub const BOOTSTRAP_N: usize = 500;
pub const BOOTSTRAP_SEED: i64 = 20261102;
pub const HIST_POOL_K: f64 = 3000.0;
pub const HOLDOUT_WEEKS: usize = 2;
pub const SEARCH_ROUNDS: usize = 2;
/// per knob, evenly spread over [-bound, +bound]
pub const GRID_POINTS: usize = 9;
pub const EPS: f64 = 1e-9;

pub const TIER_A: [&str; 4] = ["k_inseason", "recency_weights", "projection_weight", "k_projection"];
pub const TIER_B: [&str; 3] = ["availability", "start_share", "offnight_bonus"];
pub const GROUPS: [&str; 7] = [
    "k_inseason",
    "recency_weights",
    "projection_weight",
    "k_projection",
    "availability",
    "start_share",
    "offnight_bonus",
];
pub const NOT_ELIGIBLE: [&str; 5] = ["age curves", "age_yoy", "k_baseline", "dynasty weights", "recommender thresholds"];
pub const AVAIL_STATUSES: [&str; 5] = ["dtd", "out", "ir", "ltir", "suspended"];
pub const RECENCY_SPLITS: [&str; 3] = ["last30", "last15", "last7"];
const RECENCY_ALL: [&str; 4] = ["season", "last30", "last15", "last7"];

/// Knob values by name (Python dict order).
pub type Knobs = Map<String, f64>;

// --------------------------------------------------------------------------- knobs

#[derive(Clone, Copy, Debug)]
pub struct Knob {
    pub name: &'static str,
    pub group: &'static str,
    /// where it lives in the params JSON
    pub path: &'static [&'static str],
    /// per-cycle bound (fraction of the current value when relative)
    pub step: f64,
    pub relative: bool,
    pub lo: f64,
    pub hi: f64,
    pub goalie: bool,
    /// fpg (28-day FPG, pooled with history) | week (7-day points)
    pub objective: &'static str,
}

impl Knob {
    pub fn bound_label(&self) -> String {
        if self.relative {
            format!("+-{}", percent(self.step, 0, false))
        } else {
            format!("+-{}", general(self.step, None, false))
        }
    }

    pub fn bounds(&self, current: f64) -> (f64, f64) {
        let d = if self.relative { current.abs() * self.step } else { self.step };
        (self.lo.max(current - d), self.hi.min(current + d))
    }
}

const INF: f64 = f64::INFINITY;

const fn knob(
    name: &'static str,
    group: &'static str,
    path: &'static [&'static str],
    step: f64,
    relative: bool,
    lo: f64,
    hi: f64,
    goalie: bool,
    objective: &'static str,
) -> Knob {
    Knob { name, group, path, step, relative, lo, hi, goalie, objective }
}

pub static KNOBS: [Knob; 17] = [
    knob("k_inseason.skater", "k_inseason", &["inseason", "k_skater"], 0.20, true, 0.0, INF, false, "fpg"),
    knob("k_inseason.goalie", "k_inseason", &["inseason", "k_goalie"], 0.20, true, 0.0, INF, true, "fpg"),
    knob("recency.season", "recency_weights", &["inseason", "recency_weights", "season"], 0.05, false, 0.0, 1.0, false, "fpg"),
    knob("recency.last30", "recency_weights", &["inseason", "recency_weights", "last30"], 0.05, false, 0.0, 1.0, false, "fpg"),
    knob("recency.last15", "recency_weights", &["inseason", "recency_weights", "last15"], 0.05, false, 0.0, 1.0, false, "fpg"),
    knob("recency.last7", "recency_weights", &["inseason", "recency_weights", "last7"], 0.05, false, 0.0, 1.0, false, "fpg"),
    knob("projection_weight", "projection_weight", &["projection", "weight"], 0.10, false, 0.0, 1.0, false, "fpg"),
    knob("k_projection.skater", "k_projection", &["projection", "k_skater"], 0.20, true, 0.0, INF, false, "fpg"),
    knob("k_projection.goalie", "k_projection", &["projection", "k_goalie"], 0.20, true, 0.0, INF, true, "fpg"),
    knob("availability.dtd.week", "availability", &["availability", "dtd", "week"], 0.10, false, 0.0, 1.0, false, "week"),
    knob("availability.out.week", "availability", &["availability", "out", "week"], 0.10, false, 0.0, 1.0, false, "week"),
    knob("availability.ir.week", "availability", &["availability", "ir", "week"], 0.10, false, 0.0, 1.0, false, "week"),
    knob("availability.ltir.week", "availability", &["availability", "ltir", "week"], 0.10, false, 0.0, 1.0, false, "week"),
    knob(
        "availability.suspended.week",
        "availability",
        &["availability", "suspended", "week"],
        0.10,
        false,
        0.0,
        1.0,
        false,
        "week",
    ),
    knob("start_share.prior", "start_share", &["schedule", "start_share_prior"], 0.05, false, 0.0, 1.0, true, "week"),
    knob("start_share.k", "start_share", &["schedule", "start_share_k"], 0.20, true, 0.0, INF, true, "week"),
    knob("offnight_bonus", "offnight_bonus", &["schedule", "offnight_bonus"], 0.02, false, 0.0, 0.2, false, "week"),
];

/// `KNOB[name]`.
pub fn knob_by_name(name: &str) -> Option<&'static Knob> {
    KNOBS.iter().find(|k| k.name == name)
}

/// `GROUP_KNOBS[group]`.
pub fn group_knobs(group: &str) -> Vec<&'static Knob> {
    KNOBS.iter().filter(|k| k.group == group).collect()
}

/// `GROUP_OBJECTIVE[group]`.
pub fn group_objective(group: &str) -> &'static str {
    KNOBS.iter().find(|k| k.group == group).map(|k| k.objective).unwrap_or("fpg")
}

pub fn tier(group: &str) -> &'static str {
    if TIER_A.contains(&group) {
        "A"
    } else {
        "B"
    }
}

/// Every knob's value under `params` (default: the loaded params), via the accessors.
pub fn current_knobs(params: Option<&Value>) -> Knobs {
    let ki = vparams::k_inseason(params);
    let kp = vparams::k_projection(params);
    let rw = vparams::recency_weights(params);
    let av = vparams::availability_table(params);
    let mut out = Knobs::new();
    out.insert("k_inseason.skater".into(), ki["skater"]);
    out.insert("k_inseason.goalie".into(), ki["goalie"]);
    for s in RECENCY_ALL {
        out.insert(format!("recency.{s}"), rw[s]);
    }
    out.insert("projection_weight".into(), vparams::projection_weight(params));
    out.insert("k_projection.skater".into(), kp["skater"]);
    out.insert("k_projection.goalie".into(), kp["goalie"]);
    for st in AVAIL_STATUSES {
        out.insert(format!("availability.{st}.week"), av.get(st).map(|x| x.0).unwrap_or(1.0));
    }
    out.insert("start_share.prior".into(), vparams::start_share_prior(params));
    out.insert("start_share.k".into(), vparams::start_share_k(params));
    out.insert("offnight_bonus".into(), vparams::offnight_bonus(params));
    out
}

pub fn changed_knobs(candidate: &Knobs, current: &Knobs) -> Vec<String> {
    let mut out: Vec<String> = candidate
        .iter()
        .filter(|(k, v)| (**v - current.get(*k).copied().unwrap_or(**v)).abs() > EPS)
        .map(|(k, _)| k.clone())
        .collect();
    out.sort();
    out
}

pub fn changed_groups(candidate: &Knobs, current: &Knobs) -> Vec<String> {
    let set: BTreeSet<&str> =
        changed_knobs(candidate, current).iter().filter_map(|k| knob_by_name(k)).map(|k| k.group).collect();
    set.into_iter().map(String::from).collect()
}

/// The nested params override for the knobs `names` (default all given). Recency weights are
/// always written as a complete set of four (the accessor rejects partial sets).
pub fn knobs_override(knobs: &Knobs, names: Option<&[String]>) -> Value {
    let mut names: BTreeSet<String> = match names {
        Some(n) => n.iter().cloned().collect(),
        None => knobs.keys().cloned().collect(),
    };
    if names.iter().any(|n| knob_by_name(n).is_some_and(|k| k.group == "recency_weights")) {
        for s in RECENCY_ALL {
            names.insert(format!("recency.{s}"));
        }
    }
    let mut out = serde_json::Map::new();
    for n in &names {
        let (Some(k), Some(v)) = (knob_by_name(n), knobs.get(n)) else { continue };
        let mut d = &mut out;
        for key in &k.path[..k.path.len() - 1] {
            let e = d.entry(key.to_string()).or_insert_with(|| json!({}));
            if !e.is_object() {
                *e = json!({});
            }
            d = e.as_object_mut().expect("object");
        }
        d.insert(k.path[k.path.len() - 1].to_string(), fv(*v));
    }
    Value::Object(out)
}

/// A full params mapping: `base` (default the loaded params) with `knobs` applied.
pub fn params_for(knobs: &Knobs, base: Option<&Value>) -> Value {
    let over = knobs_override(knobs, None);
    match base {
        Some(b) => vparams::deep_merge(b, &over),
        None => vparams::deep_merge(&vparams::load_params(None), &over),
    }
}

// --------------------------------------------------------------------------- replay table

/// One matured (weekly snapshot, player) observation rebuilt from the archived inputs.
#[derive(Clone, Debug)]
pub struct ReplayObs {
    pub league: String,
    pub snap: NaiveDate,
    /// ISO-week Monday of the snapshot
    pub week: NaiveDate,
    /// NHL id (bootstrap cluster)
    pub player: i64,
    /// F / D / G
    pub pool: &'static str,
    pub p: Player,
    pub means: Means,
    pub history: History,
    /// the league's PointsScoring
    pub sc: Arc<PointsScoring>,
    pub status: String,
    pub games: Option<i64>,
    pub offnight: Option<i64>,
    pub share: Option<f64>,
    pub share_src: Option<String>,
    pub share_starts: Option<f64>,
    pub share_tg: Option<f64>,
    pub fpg_archived: Option<f64>,
    pub proj_week_archived: Option<f64>,
    /// realized FPG over the next 28 days (>= MIN_GP_28 GP)
    pub real_fpg28: Option<f64>,
    pub gp28: i64,
    /// realized points over the next 7 days
    pub real_pts7: Option<f64>,
    pub gp7: i64,
    /// False: inputs predate the replay fields (means / zeros)
    pub exact: bool,
}

impl ReplayObs {
    pub fn is_goalie(&self) -> bool {
        self.pool == "G"
    }

    /// The FPG-space view of this row (the backtest's `InseasonObs` shape).
    pub fn to_inseason(&self) -> InseasonObs {
        let f = |line: Option<&StatLine>| -> (i64, f64) {
            match line {
                Some(l) if l.gp > 0 => (l.gp, self.sc.value(&l.per_game())),
                _ => (0, 0.0),
            }
        };
        let season = self.p.lines.get("season");
        let proj = self.p.lines.get("projected");
        let (hist_rates, hist_gp, _) = &self.history;
        let mut splits = Map::new();
        for s in RECENCY_SPLITS {
            splits.insert(s.to_string(), f(self.p.lines.get(s)));
        }
        InseasonObs {
            season: 0,
            day: iso(self.snap),
            player_id: self.player,
            group: self.pool.to_string(),
            gp_td: season.map(|s| s.gp).unwrap_or(0),
            fpg_td: f(season).1,
            fpg_base: 0.0,
            // Archived rows carry no per-stat baseline, so they use the single k_skater for every stat.
            vol_points: Map::new(),
            has_base: false,
            splits,
            gp_rest: self.gp28,
            fpg_rest: self.real_fpg28.unwrap_or(0.0),
            fpg_app: self.fpg_archived.unwrap_or(0.0),
            toi: None,
            fpg_proj: proj.filter(|p| p.gp > 0).map(|p| f(Some(p)).1),
            fpg_hist: (!hist_rates.is_empty()).then(|| self.sc.value(hist_rates)),
            status: Some(self.status.clone()),
            gp_hist3: (*hist_gp != 0).then_some(*hist_gp),
            start_share: self.share,
        }
    }
}

/// Healthy FPG for one replay row under `params` (a full params mapping; default the loaded
/// ones): `valuation.valuate.player_rates` itself.
pub fn inseason_projection(obs: &ReplayObs, params: Option<&Value>) -> f64 {
    let rates = player_rates(&obs.p, &obs.means, None, params, Some(&obs.history));
    if rates.is_empty() {
        0.0
    } else {
        obs.sc.value(&rates)
    }
}

/// A historical checkpoint under `params`: the backtest's FPG-space formula with the candidate k
/// and recency weights. Unlike the Python, a skater's volume stats use their own k
/// (`k_inseason_volume`), as the app does.
pub fn hist_projection(obs: &InseasonObs, params: Option<&Value>) -> f64 {
    let k = vparams::k_inseason_for(if obs.group == "G" { "goalie" } else { "skater" }, params);
    let volume = if obs.group == "G" { Map::new() } else { vparams::k_inseason_volume(params) };
    inseason_projection_split(obs, k, &volume, &vparams::recency_weights(params))
}

/// Projected points over the 7-day window under `params` (None without a schedule).
pub fn week_projection(obs: &ReplayObs, params: Option<&Value>, fpg: Option<f64>) -> Option<f64> {
    let games = obs.games?;
    let fpg = fpg.unwrap_or_else(|| inseason_projection(obs, params));
    let a_week = vparams::availability(&obs.status, Horizon::Week, params);
    let mut share = 1.0;
    if obs.is_goalie() {
        if obs.share_src.as_deref() == Some("history") && obs.share_starts.is_some() && obs.share_tg.is_some() {
            share = shrunk_share(obs.share_starts.unwrap(), obs.share_tg.unwrap(), None, None, params);
        } else if let Some(s) = obs.share {
            share = s;
        }
    }
    Some(proj_week(fpg, a_week, games, obs.offnight.unwrap_or(0), params) * share)
}

/// `_line`: an archived `{gp, stats}` dict as a StatLine (None when unusable).
fn line_of(split: &str, d: Option<&Value>) -> Option<StatLine> {
    let d = d?.as_object()?;
    let gp_v = d.get("gp").filter(|g| !g.is_null())?;
    let gp = match gp_v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64))?,
        Value::Bool(b) => *b as i64,
        Value::String(s) => s.trim().parse::<i64>().ok()?,
        _ => return None,
    };
    let mut stats = Map::new();
    if let Some(st) = d.get("stats").filter(|s| truthy(Some(s))) {
        for (k, v) in st.as_object()? {
            let f = match v {
                Value::Number(n) => n.as_f64()?,
                Value::Bool(b) => *b as i64 as f64,
                Value::String(s) => s.trim().parse::<f64>().ok()?,
                _ => return None,
            };
            stats.insert(k.clone(), f);
        }
    }
    let split = serde_json::from_value(json!(split)).ok()?;
    Some(StatLine { split, gp, stats })
}

const STATUSES: [&str; 7] = ["healthy", "dtd", "out", "ir", "ltir", "suspended", "unknown"];

fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_i64().map(|i| i.to_string()).unwrap_or_else(|| {
            n.as_f64().map(fm_pycompat::fmt::repr_f64).unwrap_or_default()
        }),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

fn rates_of(v: Option<&Value>) -> Map<String, f64> {
    v.and_then(|x| x.as_object())
        .map(|o| o.iter().filter_map(|(k, x)| val_f64(Some(x)).map(|f| (k.clone(), f))).collect())
        .unwrap_or_default()
}

fn opt_i64(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        _ => None,
    }
}

/// Position means by group (`{g: dict(m)}`).
pub type PosMeans = Map<String, Map<String, f64>>;

/// One replay row from an archived projection row + its v2 inputs (None if unusable).
pub fn obs_from_inputs(
    league: &str,
    snap: NaiveDate,
    row: &crate::ledger::Row,
    inputs: &Value,
    means: Option<&PosMeans>,
    sc: Arc<PointsScoring>,
) -> Option<ReplayObs> {
    let nhl = row.get("nhl_id").filter(|v| !v.is_null())?;
    let nhl_id = opt_i64(Some(nhl)).or_else(|| nhl.as_str().and_then(|s| s.trim().parse().ok()))?;
    let positions: Vec<String> = match inputs.get("positions").filter(|p| truthy(Some(p))) {
        Some(Value::Array(a)) => a.iter().map(py_str).filter(|x| !x.is_empty()).collect(),
        Some(Value::String(s)) => s.chars().map(|c| c.to_string()).collect(),
        _ => {
            let pos = row.get("positions").filter(|p| truthy(Some(p))).map(py_str).unwrap_or_default();
            pos.split(',').filter(|x| !x.is_empty()).map(String::from).collect()
        }
    };
    let status = [inputs.get("status"), row.get("status")]
        .into_iter()
        .flatten()
        .find(|v| truthy(Some(v)))
        .map(py_str)
        .unwrap_or_else(|| "unknown".into());
    let status = if STATUSES.contains(&status.as_str()) { status } else { "unknown".to_string() };
    let mut lines: Map<String, StatLine> = Map::new();
    for split in ["season", "last30", "last15", "last7"] {
        if let Some(ln) = line_of(split, inputs.get(split)) {
            lines.insert(split.to_string(), ln);
        }
    }
    if let Some(proj) = line_of("projected", inputs.get("projection")) {
        lines.insert("projected".to_string(), proj);
    }
    let cid = row.get("cid").filter(|c| truthy(Some(c))).map(py_str).unwrap_or_else(|| py_str(nhl));
    let name = row.get("name").filter(|c| truthy(Some(c))).map(py_str).unwrap_or_default();
    let mut p: Player = serde_json::from_value(json!({
        "cid": cid, "name": name, "name_norm": "", "ids": {}, "team": null,
        "positions": if positions.is_empty() { vec!["C".to_string()] } else { positions },
        "status": status,
    }))
    .ok()?;
    p.lines = lines;
    // v3: the in-season xG goal shrink
    if let Some(v) = inputs.get("ixg_per_game").filter(|v| !v.is_null()) {
        p.ixg_per_game = val_f64(Some(v));
    }
    if let Some(v) = inputs.get("goals_minus_ixg").filter(|v| !v.is_null()) {
        p.goals_minus_ixg = val_f64(Some(v));
    }
    if let Some(v) = inputs.get("xg_split").filter(|v| !v.is_null()) {
        p.xg_split = Some(py_str(v));
    }
    let empty = json!({});
    let hist = inputs.get("history").filter(|h| truthy(Some(h))).unwrap_or(&empty);
    let history: History = (rates_of(hist.get("rates")), opt_i64(hist.get("gp")).unwrap_or(0), String::new());
    let share = inputs.get("share").filter(|h| truthy(Some(h))).unwrap_or(&empty);
    let zeros_ok = ["season", "last30", "last15", "last7"].iter().all(|s| {
        let d = inputs.get(*s).filter(|x| truthy(Some(x))).unwrap_or(&empty);
        truthy(d.get("zeros")) || !truthy(d.get("gp"))
    });
    let pool = position_group(&p);
    let means_map: Means = means.map(|m| m.iter().map(|(g, r)| (g.clone(), r.clone())).collect()).unwrap_or_default();
    Some(ReplayObs {
        league: league.to_string(),
        snap,
        week: snap - Duration::days(snap.weekday().num_days_from_monday() as i64),
        player: nhl_id,
        pool,
        p,
        means: means_map,
        history,
        sc,
        status,
        games: opt_i64(inputs.get("games_next7")),
        offnight: opt_i64(inputs.get("offnight_next7")),
        share: val_f64(inputs.get("start_share")),
        share_src: share.get("source").filter(|v| !v.is_null()).map(py_str),
        share_starts: val_f64(share.get("starts")),
        share_tg: val_f64(share.get("team_games")),
        fpg_archived: f_of(row, "fpg"),
        proj_week_archived: f_of(row, "proj_week"),
        real_fpg28: None,
        gp28: 0,
        real_pts7: None,
        gp7: 0,
        exact: means.is_some_and(|m| !m.is_empty()) && zeros_ok,
    })
}

fn pos_means_of(v: &Value) -> Option<PosMeans> {
    let o = v.as_object()?;
    Some(o.iter().map(|(g, m)| (g.clone(), rates_of(Some(m)))).collect())
}

/// `_means`: the archive header's positional means of that day (ledger `meta`, else the file).
pub fn means_for(ledger: &Ledger, league: &str, day: &str) -> Result<Option<PosMeans>> {
    let key = format!("means:{league}:{day}");
    let rows = ledger.query("SELECT value FROM meta WHERE key=?", &[&key])?;
    if let Some(r) = rows.first() {
        let raw = s_of(r, "value").unwrap_or("");
        return Ok(serde_json::from_str::<Value>(raw).ok().and_then(|m| pos_means_of(&m)));
    }
    // older ingests: read the archive header itself
    let path = ledger.data_dir.join("archive").join(format!("projections-{league}-{day}.json"));
    let m = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("position_means").cloned());
    Ok(m.and_then(|m| pos_means_of(&m)).filter(|m| !m.is_empty()))
}

pub fn known_leagues(ledger: &Ledger) -> Result<Vec<String>> {
    Ok(ledger
        .query("SELECT DISTINCT league FROM projections ORDER BY league", &[])?
        .iter()
        .filter_map(|r| s_of(r, "league").map(String::from))
        .collect())
}

/// Every matured (weekly snapshot, player) observation as of `as_of` (targets only from windows
/// that ended before `as_of` and were fully pulled). Info: `{"leagues": {..}, "skipped": {..}}`.
pub fn replay_table(ledger: &Ledger, as_of: NaiveDate, leagues: Option<&[String]>) -> Result<(Vec<ReplayObs>, Value)> {
    let mut info_leagues = serde_json::Map::new();
    let mut skipped = serde_json::Map::new();
    let mut out: Vec<ReplayObs> = Vec::new();
    let last_day = as_of - Duration::days(1);
    let lgs: Vec<String> = match leagues {
        Some(l) if !l.is_empty() => l.to_vec(),
        _ => known_leagues(ledger)?,
    };
    for league in lgs {
        let (cfg, _) = scoring_config(ledger, &league)?;
        let Some(sc) = points_scorer(cfg.as_ref()) else {
            skipped.insert(league.clone(), json!("not a points league (or no scoring known)"));
            continue;
        };
        let sc = Arc::new(sc);
        let days: Vec<String> = ledger
            .query("SELECT DISTINCT as_of FROM projections WHERE league=? ORDER BY as_of", &[&league])?
            .iter()
            .filter_map(|r| s_of(r, "as_of").map(String::from))
            .collect();
        if days.is_empty() {
            continue;
        }
        let realized = Realized::new(ledger, &sc, None, Some(as_of))?;
        let n0 = out.len();
        for (_, snap) in weekly_snapshots(days.iter().map(|s| s.as_str())) {
            let d28 = days_between(snap + Duration::days(1), snap + Duration::days(28));
            let d7 = &d28[..7];
            let ok28 = d28[27] <= last_day && realized.covered(&d28);
            let ok7 = d7[6] <= last_day && realized.covered(d7);
            if !(ok28 || ok7) {
                continue;
            }
            let snap_s = iso(snap);
            let means = means_for(ledger, &league, &snap_s)?;
            for r in ledger.query(
                "SELECT cid, name, nhl_id, positions, status, fpg, proj_week, inputs_json FROM projections \
                 WHERE league=? AND as_of=?",
                &[&league, &snap_s],
            )? {
                let raw = s_of(&r, "inputs_json").filter(|s| !s.is_empty()).unwrap_or("null");
                let Ok(inputs @ Value::Object(_)) = serde_json::from_str::<Value>(raw) else { continue };
                let Some(mut o) = obs_from_inputs(&league, snap, &r, &inputs, means.as_ref(), sc.clone()) else {
                    continue;
                };
                if ok28 {
                    let (gp, pts) = realized.window(o.player, &d28);
                    if gp >= MIN_GP_28 {
                        o.real_fpg28 = Some(pts / gp as f64);
                        o.gp28 = gp;
                    }
                }
                if ok7 && o.games.is_some() {
                    let (gp, pts) = realized.window(o.player, d7);
                    o.real_pts7 = Some(pts);
                    o.gp7 = gp;
                }
                if o.real_fpg28.is_some() || o.real_pts7.is_some() {
                    out.push(o);
                }
            }
        }
        info_leagues.insert(league.clone(), json!(out.len() - n0));
    }
    Ok((out, json!({"leagues": info_leagues, "skipped": skipped})))
}

// --------------------------------------------------------------------------- historical checkpoints

type HistKey = (PathBuf, String);

fn hist_cache() -> &'static Mutex<HashMap<HistKey, Arc<Vec<InseasonObs>>>> {
    static C: OnceLock<Mutex<HashMap<HistKey, Arc<Vec<InseasonObs>>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The backtest's in-season checkpoint observations (`data/backtest`) under scoring `sc`; loaded
/// lazily, cached per (data dir, scoring). [] when the backtest data is missing.
pub fn historical_observations(data_dir: &Path, sc: &PointsScoring) -> Arc<Vec<InseasonObs>> {
    let dir = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let sorted = |m: &Map<String, f64>| {
        let mut v: Vec<(String, String)> = m.iter().map(|(k, x)| (k.clone(), fm_pycompat::fmt::repr_f64(*x))).collect();
        v.sort();
        v
    };
    let key = (dir, format!("{:?}|{:?}", sorted(&sc.weights), sorted(&sc.goalie_weights)));
    let mut cache = hist_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = cache.get(&key) {
        return v.clone();
    }
    let obs = (|| -> Result<Vec<InseasonObs>> {
        let table = load_table(data_dir)?;
        let windows = load_windows(data_dir)?;
        if table.seasons().is_empty() || windows.is_empty() {
            return Ok(Vec::new());
        }
        let mut seasons: Vec<i64> = windows.keys().copied().collect();
        seasons.sort();
        Ok(inseason_observations(&table, &windows, sc, &seasons, MIN_GP_REST))
    })()
    .unwrap_or_default();
    let v = Arc::new(obs);
    cache.insert(key, v.clone());
    v
}

// --------------------------------------------------------------------------- losses

pub fn mae(errs: &[f64]) -> Option<f64> {
    if errs.is_empty() {
        None
    } else {
        Some(py_sum(errs.iter().map(|e| e.abs())) / errs.len() as f64)
    }
}

pub fn fpg_errors(obs: &[&ReplayObs], params: &Value) -> Vec<f64> {
    obs.par_iter()
        .filter_map(|o| o.real_fpg28.map(|r| inseason_projection(o, Some(params)) - r))
        .collect()
}

pub fn week_errors(obs: &[&ReplayObs], params: &Value) -> Vec<f64> {
    obs.par_iter()
        .filter_map(|o| {
            let real = o.real_pts7?;
            week_projection(o, Some(params), None).map(|pw| pw - real)
        })
        .collect()
}

pub fn hist_mae(hist: &[InseasonObs], params: &Value) -> Option<f64> {
    let ks = vparams::k_inseason_for("skater", Some(params));
    let kg = vparams::k_inseason_for("goalie", Some(params));
    let rw = vparams::recency_weights(Some(params));
    let (volume, no_split) = (vparams::k_inseason_volume(Some(params)), Map::new());
    let errs: Vec<f64> = hist
        .par_iter()
        .map(|o| {
            let (k, kv) = if o.group == "G" { (kg, &no_split) } else { (ks, &volume) };
            inseason_projection_split(o, k, kv, &rw) - o.fpg_rest
        })
        .collect();
    mae(&errs)
}

pub fn pool_weight(n_live: usize, k: f64) -> f64 {
    if n_live > 0 {
        n_live as f64 / (n_live as f64 + k)
    } else {
        0.0
    }
}

/// Memoised objective evaluation for one data split.
pub struct Losses<'a> {
    pub fpg_obs: Vec<&'a ReplayObs>,
    pub week_obs: Vec<&'a ReplayObs>,
    pub hist_obs: &'a [InseasonObs],
    pub base: &'a Value,
    pub w: f64,
    hist_memo: HashMap<[u64; 6], Option<f64>>,
}

impl<'a> Losses<'a> {
    pub fn new(live: &[&'a ReplayObs], hist: &'a [InseasonObs], base: &'a Value, hist_pool_k: f64) -> Self {
        let fpg_obs: Vec<&ReplayObs> = live.iter().copied().filter(|o| o.real_fpg28.is_some()).collect();
        let week_obs = live.iter().copied().filter(|o| o.real_pts7.is_some() && o.games.is_some()).collect();
        let w = if hist.is_empty() { 1.0 } else { pool_weight(fpg_obs.len(), hist_pool_k) };
        Losses { fpg_obs, week_obs, hist_obs: hist, base, w, hist_memo: HashMap::new() }
    }

    pub fn params(&self, knobs: &Knobs) -> Value {
        params_for(knobs, Some(self.base))
    }

    pub fn hist(&mut self, knobs: &Knobs) -> Option<f64> {
        let mut key = [0u64; 6];
        for (i, k) in ["k_inseason.skater", "k_inseason.goalie", "recency.season", "recency.last30", "recency.last15", "recency.last7"]
            .iter()
            .enumerate()
        {
            key[i] = round_to(knobs.get(*k).copied().unwrap_or(f64::NAN), 10).to_bits();
        }
        if let Some(v) = self.hist_memo.get(&key) {
            return *v;
        }
        let v = if self.hist_obs.is_empty() { None } else { hist_mae(self.hist_obs, &self.params(knobs)) };
        self.hist_memo.insert(key, v);
        v
    }

    pub fn live_fpg(&self, knobs: &Knobs) -> Option<f64> {
        mae(&fpg_errors(&self.fpg_obs, &self.params(knobs)))
    }

    pub fn live_week(&self, knobs: &Knobs) -> Option<f64> {
        mae(&week_errors(&self.week_obs, &self.params(knobs)))
    }

    pub fn loss(&mut self, knobs: &Knobs, objective: &str) -> f64 {
        if objective == "week" {
            return self.live_week(knobs).unwrap_or(f64::INFINITY);
        }
        let live = self.live_fpg(knobs);
        let h = self.hist(knobs);
        match (live, h) {
            (None, None) => f64::INFINITY,
            (None, Some(h)) => h,
            (Some(l), None) => l,
            (Some(l), Some(h)) => (1.0 - self.w) * h + self.w * l,
        }
    }
}

// --------------------------------------------------------------------------- search

pub fn knob_grid(k: &Knob, current: f64, points: usize) -> Vec<f64> {
    let (lo, hi) = k.bounds(current);
    if hi - lo <= EPS {
        return vec![current];
    }
    let mut vals = vec![current];
    for i in 0..points {
        let v = lo + (hi - lo) * i as f64 / (points - 1) as f64;
        let v = round_to(v, 6).max(lo).min(hi);
        if !vals.contains(&v) {
            vals.push(v);
        }
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    vals
}

/// Recency candidates from `at` moving one split at a time (the season weight absorbs the
/// difference), keeping every weight within +-0.05 of `cur`, >= 0 and the sum at 1.
pub fn recency_moves(cur: &Knobs, at: &Knobs, points: usize) -> Vec<Knobs> {
    let mut out = Vec::new();
    let season_knob = knob_by_name("recency.season").expect("knob");
    for split in RECENCY_SPLITS {
        let name = format!("recency.{split}");
        let k = knob_by_name(&name).expect("knob");
        for v in knob_grid(k, cur[&name], points) {
            let mut cand = Knobs::new();
            for s in RECENCY_SPLITS {
                let n = format!("recency.{s}");
                cand.insert(n.clone(), at[&n]);
            }
            cand.insert(name.clone(), v);
            let season = 1.0 - py_sum(cand.values().copied());
            let (lo, hi) = season_knob.bounds(cur["recency.season"]);
            if season < lo - EPS || season > hi + EPS || season < 0.0 {
                continue;
            }
            cand.insert("recency.season".into(), season);
            out.push(cand);
        }
    }
    out
}

fn merged(best: &Knobs, over: &Knobs) -> Knobs {
    let mut t = best.clone();
    for (k, v) in over {
        t.insert(k.clone(), *v);
    }
    t
}

/// Coordinate descent over `group`'s knobs from `current` inside the step bounds:
/// (best knobs, loss before, loss after) on `losses`' data.
pub fn search_group(group: &str, current: &Knobs, losses: &mut Losses<'_>, goalies_ok: bool, rounds: usize) -> (Knobs, f64, f64) {
    let objective = group_objective(group);
    let mut best = current.clone();
    let before = losses.loss(&best, objective);
    let mut best_loss = before;
    for _ in 0..rounds {
        let mut improved = false;
        if group == "recency_weights" {
            for cand in recency_moves(current, &best, GRID_POINTS) {
                let trial = merged(&best, &cand);
                let lv = losses.loss(&trial, objective);
                if lv < best_loss - 1e-12 {
                    best = trial;
                    best_loss = lv;
                    improved = true;
                }
            }
        } else {
            for k in group_knobs(group) {
                if k.goalie && !goalies_ok {
                    continue;
                }
                for v in knob_grid(k, current[k.name], GRID_POINTS) {
                    let mut trial = best.clone();
                    trial.insert(k.name.to_string(), v);
                    let lv = losses.loss(&trial, objective);
                    if lv < best_loss - 1e-12 {
                        best = trial;
                        best_loss = lv;
                        improved = true;
                    }
                }
            }
        }
        if !improved {
            break;
        }
    }
    (best, before, best_loss)
}

// --------------------------------------------------------------------------- gate

#[derive(Clone, Debug, Default, Serialize)]
pub struct ObjectiveStats {
    pub objective: String,
    pub n_holdout: usize,
    pub holdout_before: Option<f64>,
    pub holdout_after: Option<f64>,
    /// 90% CI of the relative holdout improvement
    pub ci_lo: Option<f64>,
    pub ci_hi: Option<f64>,
    pub hist_before: Option<f64>,
    pub hist_after: Option<f64>,
    pub n_hist: usize,
}

impl ObjectiveStats {
    pub fn new(objective: &str) -> Self {
        ObjectiveStats { objective: objective.to_string(), ..Default::default() }
    }

    pub fn gain(&self) -> Option<f64> {
        match (self.holdout_before, self.holdout_after) {
            (Some(b), Some(a)) if b > 0.0 => Some((b - a) / b),
            _ => None,
        }
    }

    pub fn hist_change(&self) -> Option<f64> {
        match (self.hist_before, self.hist_after) {
            (Some(b), Some(a)) if b > 0.0 => Some((a - b) / b),
            _ => None,
        }
    }

    /// `{**asdict(v), "gain": v.gain, "hist_change": v.hist_change}`.
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        if let Value::Object(m) = &mut v {
            m.insert("gain".into(), ofv(self.gain()));
            m.insert("hist_change".into(), ofv(self.hist_change()));
        }
        v
    }
}

#[derive(Clone, Debug, Default)]
pub struct GateStats {
    /// matured 28-day live observations
    pub n_live: usize,
    pub n_goalie: usize,
    /// distinct weekly snapshots among them
    pub weeks: usize,
    /// matured 7-day observations
    pub n_week: usize,
    pub objectives: Map<String, ObjectiveStats>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GateResult {
    pub passed: bool,
    pub reasons: Vec<String>,
    pub checks: Map<String, bool>,
}

impl GateResult {
    pub fn to_dict(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

pub fn insufficient_reasons(stats: &GateStats, goalie_change: bool) -> Vec<String> {
    let mut out = Vec::new();
    if stats.n_live < MIN_LIVE_OBS {
        out.push(format!(
            "insufficient live obs: {} matured 28-day player-windows, need {MIN_LIVE_OBS} ({} more)",
            stats.n_live,
            MIN_LIVE_OBS - stats.n_live
        ));
    }
    if stats.weeks < MIN_WEEKS {
        out.push(format!(
            "insufficient weekly snapshots: {} matured, need {MIN_WEEKS} ({} more)",
            stats.weeks,
            MIN_WEEKS - stats.weeks
        ));
    }
    if goalie_change && stats.n_goalie < MIN_GOALIE_OBS {
        out.push(format!(
            "insufficient goalie obs for goalie parameters: {}, need {MIN_GOALIE_OBS} ({} more)",
            stats.n_goalie,
            MIN_GOALIE_OBS - stats.n_goalie
        ));
    }
    out
}

fn g(x: f64) -> String {
    general(x, None, false)
}

/// Every guardrail a candidate must pass before it may be promoted (see module docstring).
pub fn gate(candidate: &Knobs, current: &Knobs, stats: &GateStats) -> GateResult {
    let mut reasons: Vec<String> = Vec::new();
    let mut checks: Map<String, bool> = Map::new();
    let mut unknown: Vec<&String> = candidate.keys().filter(|k| knob_by_name(k).is_none()).collect();
    unknown.sort();
    unknown.dedup();
    checks.insert("eligible".into(), unknown.is_empty());
    if !unknown.is_empty() {
        reasons.push(format!(
            "not eligible for refitting: {}",
            unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    let changed: Vec<String> =
        changed_knobs(candidate, current).into_iter().filter(|k| knob_by_name(k).is_some()).collect();
    let goalie_change = changed.iter().any(|k| knob_by_name(k).is_some_and(|x| x.goalie));
    let mut data = insufficient_reasons(stats, goalie_change);
    if !changed.is_empty()
        && changed.iter().any(|k| group_objective(knob_by_name(k).expect("knob").group) == "week")
        && stats.n_week < MIN_LIVE_OBS
    {
        data.push(format!(
            "insufficient 7-day live obs: {}, need {MIN_LIVE_OBS} ({} more)",
            stats.n_week,
            MIN_LIVE_OBS - stats.n_week
        ));
    }
    checks.insert("data".into(), data.is_empty());
    reasons.extend(data);
    checks.insert("changed".into(), !changed.is_empty());
    if changed.is_empty() {
        reasons.push("no parameter change".into());
    }
    let mut out_of_bounds = Vec::new();
    for k in &changed {
        let kn = knob_by_name(k).expect("knob");
        let cur = current[k.as_str()];
        let (lo, hi) = kn.bounds(cur);
        let v = candidate[k.as_str()];
        if !(lo - EPS <= v && v <= hi + EPS) || !v.is_finite() {
            out_of_bounds.push(format!(
                "{k} {} -> {} outside [{}, {}] ({})",
                g(cur),
                g(v),
                g(lo),
                g(hi),
                kn.bound_label()
            ));
        }
    }
    let rec: Vec<f64> = RECENCY_ALL
        .iter()
        .map(|s| {
            let n = format!("recency.{s}");
            candidate.get(&n).or_else(|| current.get(&n)).copied().unwrap_or(0.0)
        })
        .collect();
    let rs = py_sum(rec.iter().copied());
    if (rs - 1.0).abs() > EPS || rec.iter().copied().fold(f64::INFINITY, f64::min) < 0.0 {
        out_of_bounds.push(format!("recency weights must be >= 0 and sum to 1 (sum {})", fixed(rs, 12, false)));
    }
    checks.insert("bounds".into(), out_of_bounds.is_empty());
    reasons.extend(out_of_bounds);
    let groups: BTreeSet<&str> = changed.iter().map(|k| knob_by_name(k).expect("knob").group).collect();
    checks.insert("groups".into(), groups.len() <= MAX_GROUPS);
    if groups.len() > MAX_GROUPS {
        reasons.push(format!(
            "{} parameter groups changed ({}), at most {MAX_GROUPS} per cycle",
            groups.len(),
            groups.iter().copied().collect::<Vec<_>>().join(", ")
        ));
    }
    let objectives: BTreeSet<&str> = groups.iter().map(|gr| group_objective(gr)).collect();
    let mut stat_ok = true;
    for obj in &objectives {
        let s = stats.objectives.get(*obj);
        let Some((s, gain)) = s.and_then(|s| s.gain().map(|gn| (s, gn))) else {
            stat_ok = false;
            reasons.push(format!("{obj}: no holdout evaluation"));
            continue;
        };
        if gain < MIN_HOLDOUT_GAIN {
            stat_ok = false;
            reasons.push(format!(
                "{obj}: holdout MAE {} -> {} ({}), needs >= {} better",
                fixed(s.holdout_before.unwrap_or(f64::NAN), 4, false),
                fixed(s.holdout_after.unwrap_or(f64::NAN), 4, false),
                percent(gain, 2, true),
                percent(MIN_HOLDOUT_GAIN, 0, false)
            ));
        }
        if s.ci_lo.map_or(true, |c| c <= 0.0) {
            stat_ok = false;
            let ci = match s.ci_lo {
                None => "n/a".to_string(),
                Some(lo) => format!("[{}, {}]", percent(lo, 2, true), percent(s.ci_hi.unwrap_or(f64::NAN), 2, true)),
            };
            reasons.push(format!("{obj}: 90% bootstrap CI of the improvement {ci} does not exclude 0"));
        }
        if *obj == "fpg" {
            if s.hist_before.is_none() {
                stat_ok = false;
                reasons.push(
                    "fpg: historical checkpoints unavailable (run `fm backtest data`): cannot check that history \
                     does not get worse"
                        .into(),
                );
            } else if let Some(hc) = s.hist_change().filter(|hc| *hc > MAX_HIST_WORSE) {
                stat_ok = false;
                reasons.push(format!(
                    "fpg: historical MAE {} -> {} ({}), may not worsen by more than {}",
                    fixed(s.hist_before.unwrap_or(f64::NAN), 4, false),
                    fixed(s.hist_after.unwrap_or(f64::NAN), 4, false),
                    percent(hc, 2, true),
                    percent(MAX_HIST_WORSE, 1, false)
                ));
            }
        }
    }
    checks.insert("statistics".into(), stat_ok && !objectives.is_empty());
    let passed = checks.values().all(|v| *v);
    GateResult { passed, reasons, checks }
}

// --------------------------------------------------------------------------- calendar / policy

pub fn is_refit_day(day: NaiveDate) -> bool {
    day >= refit_start() && (day - refit_start()).num_days() % REFIT_EVERY_DAYS == 0
}

pub fn next_refit_day(day: NaiveDate) -> NaiveDate {
    if day <= refit_start() {
        return refit_start();
    }
    let days = (day - refit_start()).num_days();
    let k = (days + REFIT_EVERY_DAYS - 1) / REFIT_EVERY_DAYS;
    refit_start() + Duration::days(k * REFIT_EVERY_DAYS)
}

/// Groups the search may change: proposals / dry runs everything; a manual `--apply` Tier A
/// (plus Tier B from 2026-12-01); the daily auto-apply Tier A only.
pub fn allowed_groups(mode: &str, today: NaiveDate) -> Vec<&'static str> {
    match mode {
        "auto" => TIER_A.to_vec(),
        "apply" if today < tier_b_apply_start() => TIER_A.to_vec(),
        _ => GROUPS.to_vec(),
    }
}

pub fn auto_apply_allowed(today: NaiveDate, pref: bool) -> bool {
    pref && today >= auto_apply_start()
}

/// What `fm harness daily` runs today: None (not a refit day), 'auto' or 'propose'.
pub fn daily_refit_mode(today: NaiveDate, pref_auto_apply: bool) -> Option<&'static str> {
    if !is_refit_day(today) {
        return None;
    }
    Some(if auto_apply_allowed(today, pref_auto_apply) { "auto" } else { "propose" })
}

// --------------------------------------------------------------------------- orchestration

fn target_of(o: &ReplayObs, target: &str) -> bool {
    match target {
        "real_fpg28" => o.real_fpg28.is_some(),
        "real_pts7" => o.real_pts7.is_some(),
        _ => false,
    }
}

/// (train, holdout) indices into `obs`: the `weeks` most recent snapshot weeks with a `target`
/// are held out.
pub fn split_holdout(obs: &[ReplayObs], target: &str, weeks: usize) -> (Vec<usize>, Vec<usize>) {
    let have: Vec<usize> = (0..obs.len()).filter(|&i| target_of(&obs[i], target)).collect();
    let wk: Vec<NaiveDate> = have.iter().map(|&i| obs[i].week).collect::<BTreeSet<_>>().into_iter().collect();
    let hold: BTreeSet<NaiveDate> =
        if wk.len() > weeks { wk[wk.len() - weeks..].iter().copied().collect() } else { BTreeSet::new() };
    let train = have.iter().copied().filter(|&i| !hold.contains(&obs[i].week)).collect();
    let holdout = have.iter().copied().filter(|&i| hold.contains(&obs[i].week)).collect();
    (train, holdout)
}

/// Player-clustered paired bootstrap CI of the relative improvement 1 - MAE_after/MAE_before.
pub fn paired_ci(obs: &[&ReplayObs], before: &[f64], after: &[f64], level: f64) -> (Option<f64>, Option<f64>) {
    let pairs: Vec<(i64, f64, f64)> =
        obs.iter().zip(before).zip(after).map(|((o, b), a)| (o.player, a.abs(), b.abs())).collect();
    bootstrap_skill(&pairs, BOOTSTRAP_N, BOOTSTRAP_SEED, level)
}

#[derive(Clone, Debug, Serialize)]
pub struct RefitResult {
    pub as_of: String,
    pub mode: String,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub active_version: String,
    pub n_live: usize,
    pub n_goalie: usize,
    pub n_week: usize,
    pub weeks: usize,
    pub leagues: Map<String, i64>,
    pub groups_searched: Vec<String>,
    pub group_gains: Map<String, f64>,
    pub groups_selected: Vec<String>,
    pub current: Knobs,
    pub candidate: Knobs,
    pub rows: Vec<Value>,
    pub objectives: Map<String, Value>,
    pub gate: Option<GateResult>,
    /// none | dry-run | proposed | applied
    pub action: String,
    pub version: Option<String>,
    pub notes: Vec<String>,
    pub replay: Value,
}

impl RefitResult {
    fn new(as_of: NaiveDate, mode: &str, active_version: String) -> Self {
        RefitResult {
            as_of: iso(as_of),
            mode: mode.to_string(),
            locked: false,
            lock_reason: None,
            active_version,
            n_live: 0,
            n_goalie: 0,
            n_week: 0,
            weeks: 0,
            leagues: Map::new(),
            groups_searched: Vec::new(),
            group_gains: Map::new(),
            groups_selected: Vec::new(),
            current: Knobs::new(),
            candidate: Knobs::new(),
            rows: Vec::new(),
            objectives: Map::new(),
            gate: None,
            action: "none".into(),
            version: None,
            notes: Vec::new(),
            replay: json!({}),
        }
    }

    pub fn to_dict(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// How well the replay with the loaded params reproduces the archived fpg (exact rows only).
pub fn replay_check(obs: &[ReplayObs]) -> Value {
    let diffs: Vec<f64> = obs
        .par_iter()
        .filter(|o| o.exact && o.fpg_archived.is_some())
        .map(|o| (inseason_projection(o, None) - o.fpg_archived.unwrap()).abs())
        .collect();
    let max = diffs.iter().copied().fold(None, |m: Option<f64>, d| Some(m.map_or(d, |x| if d > x { d } else { x })));
    json!({"checked": diffs.len(), "max_abs_diff": ofv(max), "over_0.01": diffs.iter().filter(|d| **d > 0.01).count(),
           "approximate_rows": obs.iter().filter(|o| !o.exact).count()})
}

fn objective_stats(obj: &str, candidate: &Knobs, current: &Knobs, holdout: &[&ReplayObs], full: &mut Losses<'_>) -> ObjectiveStats {
    let (pc, pn) = (full.params(current), full.params(candidate));
    let mut s = ObjectiveStats::new(obj);
    let rows: Vec<&ReplayObs>;
    let (before, after);
    if obj == "fpg" {
        rows = holdout.iter().copied().filter(|o| o.real_fpg28.is_some()).collect();
        before = fpg_errors(&rows, &pc);
        after = fpg_errors(&rows, &pn);
        s.hist_before = full.hist(current);
        s.hist_after = full.hist(candidate);
        s.n_hist = full.hist_obs.len();
    } else {
        rows = holdout.iter().copied().filter(|o| o.real_pts7.is_some() && o.games.is_some()).collect();
        before = week_errors(&rows, &pc);
        after = week_errors(&rows, &pn);
    }
    s.n_holdout = before.len();
    s.holdout_before = mae(&before);
    s.holdout_after = mae(&after);
    if !before.is_empty() {
        let (lo, hi) = paired_ci(&rows, &before, &after, CI_LEVEL);
        s.ci_lo = lo;
        s.ci_hi = hi;
    }
    s
}

/// `run_refit`'s keyword arguments.
pub struct RefitOptions<'a> {
    /// 'dry-run' | 'propose' | 'apply' | 'auto'
    pub mode: String,
    pub only: Option<Vec<String>>,
    pub leagues: Option<Vec<String>>,
    pub force: bool,
    pub store: Option<&'a ParamsStore<'a>>,
    /// historical observations per league (tests); None: loaded from `<data_dir>/backtest`
    pub hist: Option<&'a Map<String, Vec<InseasonObs>>>,
    pub by: Option<String>,
    pub log: Option<&'a (dyn Fn(&str) + Sync)>,
    /// `HIST_POOL_K` (the tests monkeypatch it in Python)
    pub hist_pool_k: f64,
}

impl Default for RefitOptions<'_> {
    fn default() -> Self {
        RefitOptions {
            mode: "propose".into(),
            only: None,
            leagues: None,
            force: false,
            store: None,
            hist: None,
            by: None,
            log: None,
            hist_pool_k: HIST_POOL_K,
        }
    }
}

/// One refit cycle. `mode`: 'dry-run' (nothing written), 'propose' (write a proposed version when
/// the gate passes), 'apply' (propose + activate when the gate passes) or 'auto' (the daily run:
/// Tier A only, applied when the gate passes).
pub fn run_refit(ledger: &Ledger, as_of: NaiveDate, opts: &RefitOptions<'_>) -> Result<RefitResult> {
    let today = as_of;
    let mode = opts.mode.as_str();
    let own = ParamsStore::new(None, Some(ledger), None);
    let store: &ParamsStore<'_> = opts.store.unwrap_or(&own);
    let mut res = RefitResult::new(today, mode, store.active_name());
    let base = vparams::load_params(None);
    res.current = current_knobs(Some(&base));
    res.candidate = res.current.clone();
    if today < refit_start() && !opts.force {
        res.locked = true;
        res.lock_reason = Some(format!(
            "locked until {} (preseason; --force bypasses the calendar lock)",
            iso(refit_start())
        ));
        return Ok(res);
    }
    if opts.force && today < refit_start() {
        res.notes.push("calendar lock bypassed (--force); the statistical gate still applies".into());
    }
    let wanted: Vec<String> = match &opts.only {
        Some(o) if !o.is_empty() => o.clone(),
        _ => GROUPS.iter().map(|s| s.to_string()).collect(),
    };
    let bad: Vec<&str> = wanted.iter().map(|s| s.as_str()).filter(|g| !GROUPS.contains(g)).collect();
    if !bad.is_empty() {
        return Err(anyhow!(
            "unknown parameter group(s) {}; eligible: {} (not eligible: {})",
            bad.join(", "),
            GROUPS.join(", "),
            NOT_ELIGIBLE.join(", ")
        ));
    }
    let allowed = allowed_groups(mode, today);
    let dropped: Vec<&str> = wanted.iter().map(|s| s.as_str()).filter(|g| !allowed.contains(g)).collect();
    if !dropped.is_empty() {
        res.notes.push(if mode == "apply" {
            format!("{}: Tier B, proposal only until {}", dropped.join(", "), iso(tier_b_apply_start()))
        } else {
            format!("{}: not auto-applied (Tier B)", dropped.join(", "))
        });
    }
    res.groups_searched = wanted.iter().filter(|g| allowed.contains(&g.as_str())).cloned().collect();

    let (live, info) = replay_table(ledger, today, opts.leagues.as_deref())?;
    res.leagues =
        info["leagues"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0))).collect()).unwrap_or_default();
    let fpg_obs: Vec<&ReplayObs> = live.iter().filter(|o| o.real_fpg28.is_some()).collect();
    let mut stats = GateStats {
        n_live: fpg_obs.len(),
        n_goalie: fpg_obs.iter().filter(|o| o.is_goalie()).count(),
        weeks: fpg_obs.iter().map(|o| o.week).collect::<BTreeSet<_>>().len(),
        n_week: live.iter().filter(|o| o.real_pts7.is_some() && o.games.is_some()).count(),
        objectives: Map::new(),
    };
    res.n_live = stats.n_live;
    res.n_goalie = stats.n_goalie;
    res.weeks = stats.weeks;
    res.n_week = stats.n_week;
    let short = insufficient_reasons(&stats, false);
    if !short.is_empty() {
        let mut checks = Map::new();
        checks.insert("data".to_string(), false);
        res.gate = Some(GateResult { passed: false, reasons: short, checks });
        return Ok(res);
    }
    res.replay = replay_check(&live);

    // historical checkpoints, per league scoring (pooled)
    let mut hist_obs: Vec<InseasonObs> = Vec::new();
    let lgs: BTreeSet<&str> = live.iter().map(|o| o.league.as_str()).collect();
    for lg in lgs {
        match opts.hist {
            Some(h) => hist_obs.extend(h.get(lg).cloned().unwrap_or_default()),
            None => {
                let sc = live.iter().find(|o| o.league == lg).map(|o| o.sc.clone()).expect("league obs");
                hist_obs.extend(historical_observations(&ledger.data_dir, &sc).iter().cloned());
            }
        }
    }
    if opts.hist.is_none() && hist_obs.is_empty() {
        res.notes.push("historical checkpoints unavailable: Tier A candidates cannot pass the history check".into());
    }

    let (train_f, hold_f) = split_holdout(&live, "real_fpg28", HOLDOUT_WEEKS);
    let (train_w, hold_w) = split_holdout(&live, "real_pts7", HOLDOUT_WEEKS);
    let dedupe = |a: &[usize], b: &[usize]| -> Vec<&ReplayObs> {
        let mut seen = indexmap::IndexSet::new();
        for i in a.iter().chain(b) {
            seen.insert(*i);
        }
        seen.into_iter().map(|i| &live[i]).collect()
    };
    let train = dedupe(&train_f, &train_w);
    let holdout = dedupe(&hold_f, &hold_w);
    let mut tl = Losses::new(&train, &hist_obs, &base, opts.hist_pool_k);
    let goalies_ok = train_f.iter().filter(|&&i| live[i].is_goalie()).count() >= MIN_GOALIE_OBS;
    let mut best: Map<String, (Knobs, f64)> = Map::new();
    for gname in res.groups_searched.clone() {
        let (knobs, before, after) = search_group(&gname, &res.current, &mut tl, goalies_ok, SEARCH_ROUNDS);
        let gain = if before != 0.0 && before.is_finite() && before > 0.0 { (before - after) / before } else { 0.0 };
        res.group_gains.insert(gname.clone(), round_to(gain, 6));
        if gain > 0.0 && !changed_knobs(&knobs, &res.current).is_empty() {
            best.insert(gname.clone(), (knobs, gain));
        }
        if let Some(log) = opts.log {
            log(&format!("searched {gname}: training loss {} -> {}", fixed(before, 4, false), fixed(after, 4, false)));
        }
    }
    let mut chosen: Vec<String> = best.keys().cloned().collect();
    chosen.sort_by(|a, b| {
        (-best[a].1).partial_cmp(&-best[b].1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.cmp(b))
    });
    chosen.truncate(MAX_GROUPS);
    res.groups_selected = chosen.clone();
    let mut cand = res.current.clone();
    for gname in &chosen {
        for k in group_knobs(gname) {
            cand.insert(k.name.to_string(), best[gname].0[k.name]);
        }
    }
    res.candidate = cand.clone();

    let live_refs: Vec<&ReplayObs> = live.iter().collect();
    let mut full = Losses::new(&live_refs, &hist_obs, &base, opts.hist_pool_k);
    let objs: BTreeSet<&str> = chosen.iter().map(|gname| group_objective(gname)).collect();
    for obj in objs {
        let s = objective_stats(obj, &cand, &res.current, &holdout, &mut full);
        stats.objectives.insert(obj.to_string(), s);
    }
    res.objectives = stats.objectives.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
    let mut gres = gate(&cand, &res.current, &stats);
    if chosen.is_empty() {
        gres.reasons.insert(0, "no eligible group improves the training loss inside its step bounds".into());
    }
    let passed = gres.passed;
    res.gate = Some(gres);
    for gname in &chosen {
        let s = stats.objectives.get(group_objective(gname));
        for k in group_knobs(gname) {
            let (cur, cv) = (res.current[k.name], cand[k.name]);
            res.rows.push(json!({
                "param": k.name, "group": gname, "tier": tier(gname), "current": cur, "candidate": cv,
                "bound": k.bound_label(), "changed": (cv - cur).abs() > EPS,
                "holdout_before": ofv(s.and_then(|s| s.holdout_before)),
                "holdout_after": ofv(s.and_then(|s| s.holdout_after)),
                "hist_before": ofv(s.and_then(|s| s.hist_before)), "hist_after": ofv(s.and_then(|s| s.hist_after)),
            }));
        }
    }
    if mode == "dry-run" || !passed {
        res.action = if mode == "dry-run" { "dry-run" } else { "none" }.into();
        return Ok(res);
    }

    let changed = changed_knobs(&cand, &res.current);
    let parent = store.active_name();
    let params = vparams::deep_merge(&store.active_params(), &knobs_override(&cand, Some(&changed)));
    let fo = stats.objectives.get("fpg").or_else(|| stats.objectives.values().next()).cloned().unwrap_or_default();
    let metrics = json!({
        "holdout_before": ofv(fo.holdout_before), "holdout_after": ofv(fo.holdout_after),
        "hist_before": ofv(fo.hist_before), "hist_after": ofv(fo.hist_after), "n_live": stats.n_live,
        "objectives": res.objectives, "groups": chosen, "as_of": iso(today),
    });
    let note = changed
        .iter()
        .map(|k| format!("{k} {} -> {}", g(res.current[k.as_str()]), g(cand[k.as_str()])))
        .collect::<Vec<_>>()
        .join("; ");
    let who = opts.by.clone().unwrap_or_else(|| if mode == "auto" { "auto" } else { "manual" }.to_string());
    let rec = store.propose(&params, &changed, Some(&metrics), Some(&note), Some(&parent), &who, None)?;
    let version = rec.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
    res.version = Some(version.clone());
    res.action = "proposed".into();
    if mode == "apply" || mode == "auto" {
        let tier_b = chosen.iter().any(|gname| tier(gname) == "B");
        if mode == "auto" && (tier_b || today < auto_apply_start()) {
            res.notes.push(format!("auto-apply only for Tier A from {}", iso(auto_apply_start())));
        } else {
            store.apply(&version, &who, Some(&note), Some(today))?;
            res.action = "applied".into();
        }
    }
    Ok(res)
}

// --------------------------------------------------------------------------- trade acceptance

/// The trade acceptance curve for `league` fitted on its recorded proposals
/// (`fm_engine::recommend::trades::calibrate_acceptance`: the prior until CALIBRATION_MIN_N
/// proposals have an outcome). A declined offer is only visible when the provider reports pending
/// proposals (`PROPOSED` transactions); without any, the recorded trades are only the accepted
/// ones and a fit would read every offer as accepted, so the prior is kept. Unlike the valuation
/// parameters above this is not a projection parameter, so it is not gated on projection error or
/// kept in the params store: the loader recomputes it from the ledger on every load.
pub fn trade_accept_params(ledger: &Ledger, league: &str) -> AcceptParams {
    let proposals = ledger.count("transactions", "WHERE league=? AND action='PROPOSED'", &[&league]).unwrap_or(0);
    if proposals == 0 {
        return AcceptParams { source: "prior (no recorded trade proposals)".to_string(), ..AcceptParams::default() };
    }
    calibrate_acceptance(ledger, Some(league), None, None)
}

// --------------------------------------------------------------------------- champion / challenger

/// Shadow-score the version the active one replaced on the snapshot whose 28-day window matured
/// in the week before `week` (a Monday), recorded in the active version's `shadow.weeks`. Two
/// straight weeks won by the shadow roll the active version back to it. None when no promoted
/// version with a shadow is active.
pub fn shadow_check(ledger: &Ledger, week: NaiveDate, store: Option<&ParamsStore<'_>>) -> Result<Option<Value>> {
    let own = ParamsStore::new(None, Some(ledger), None);
    let store: &ParamsStore<'_> = store.unwrap_or(&own);
    let Some(mut active) = store.active() else { return Ok(None) };
    let Some(Value::Object(mut sh)) = active.get("shadow").cloned() else { return Ok(None) };
    let shadow_name = sh
        .get("version")
        .filter(|v| truthy(Some(v)))
        .and_then(|v| v.as_str())
        .unwrap_or(PACKAGED)
        .to_string();
    let mut weeks: serde_json::Map<String, Value> =
        sh.get("weeks").and_then(|w| w.as_object()).cloned().unwrap_or_default();
    let wk = week - Duration::days(week.weekday().num_days_from_monday() as i64);
    let since_raw = [sh.get("since"), active.get("applied_at")]
        .into_iter()
        .flatten()
        .find(|v| truthy(Some(v)))
        .map(py_str)
        .unwrap_or_else(|| "1900-01-01".into());
    let since_s: String = since_raw.chars().take(10).collect();
    let since = fm_pycompat::time::date_fromisoformat(&since_s)
        .ok_or_else(|| anyhow!("Invalid isoformat string: {since_s:?}"))?;
    let (live, _) = replay_table(ledger, wk, None)?;
    let (lo, hi) = (wk - Duration::days(35), wk - Duration::days(29));
    let rows: Vec<&ReplayObs> =
        live.iter().filter(|o| o.real_fpg28.is_some() && lo <= o.snap && o.snap <= hi && o.snap >= since).collect();
    let active_version = active.get("version").cloned().unwrap_or(Value::Null);
    let mut result = serde_json::Map::new();
    result.insert("week".into(), json!(iso(wk)));
    result.insert("active".into(), active_version.clone());
    result.insert("shadow".into(), json!(shadow_name));
    result.insert("n".into(), json!(rows.len()));
    if !rows.is_empty() {
        let packaged = vparams::load_packaged(None);
        let params_of = |rec: Option<&Rec>| -> Value {
            rec.and_then(|r| r.get("params")).filter(|p| truthy(Some(p))).cloned().unwrap_or_else(|| json!({}))
        };
        let p_active = vparams::deep_merge(&packaged, &params_of(Some(&active)));
        let shadow_rec = store.get(&shadow_name);
        let p_shadow = vparams::deep_merge(&packaged, &params_of(shadow_rec.as_ref()));
        let ma = mae(&fpg_errors(&rows, &p_active));
        let ms = mae(&fpg_errors(&rows, &p_shadow));
        let winner = match (ms, ma) {
            (Some(s), Some(a)) if s < a => "shadow",
            _ => "active",
        };
        result.insert("active_mae".into(), ofv(ma));
        result.insert("shadow_mae".into(), ofv(ms));
        result.insert("winner".into(), json!(winner));
        weeks.insert(
            iso(wk),
            json!({"n": rows.len(), "active_mae": ofv(ma), "shadow_mae": ofv(ms), "winner": winner}),
        );
        sh.insert("weeks".into(), Value::Object(weeks.clone()));
        active.insert("shadow".into(), Value::Object(sh));
        store.update(&active)?;
    }
    let mut keys: Vec<&String> = weeks.keys().collect();
    keys.sort();
    let recent: Vec<&str> = keys
        .iter()
        .map(|k| weeks[k.as_str()].get("winner").and_then(|w| w.as_str()).unwrap_or(""))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(2)
        .collect();
    if recent.len() == 2 && recent.iter().all(|w| *w == "shadow") {
        let last2: Vec<&str> = keys[keys.len() - 2..].iter().map(|s| s.as_str()).collect();
        let note = format!(
            "auto-rollback: shadow {shadow_name} beat {} two weeks running ({})",
            py_str(&active_version),
            last2.join(", ")
        );
        let rb = store.rollback(Some(&shadow_name), "auto-rollback", Some(&note))?;
        result.insert("rolled_back".into(), rb.clone());
        let mut summary = serde_json::Map::new();
        summary.insert("note".into(), json!(note));
        if let Value::Object(m) = &rb {
            for (k, v) in m {
                summary.insert(k.clone(), v.clone());
            }
        }
        let _ = record_run(ledger, "auto-rollback", None, Some(&iso(wk)), Some(&Value::Object(summary)), "ok");
    }
    Ok(Some(Value::Object(result)))
}

#[cfg(test)]
mod accept_tests {
    use super::*;
    use fm_engine::recommend::trades::{ACCEPT_K, ACCEPT_X0, CALIBRATION_MIN_N};
    use rusqlite::params;

    /// One trade episode with its first rec's MARKET_VIEW value; `status` and `match_json` as the
    /// matcher writes them.
    fn episode(l: &Ledger, i: usize, perceived: f64, status: &str, match_json: &str) {
        let reasons = format!(r#"[{{"code": "MARKET_VIEW", "text": "", "value": {perceived}}}]"#);
        l.with_conn(|c| {
            c.execute(
                "INSERT INTO recs (league, as_of, rec_key, kind, reasons_json) VALUES ('fantrax', '2026-11-01', ?, 'trade', ?)",
                params![format!("k{i}"), reasons],
            )?;
            c.execute(
                "INSERT INTO rec_episodes (episode_id, league, rec_key, kind, first_seen, last_seen, n_days, status, match_json) \
                 VALUES (?, 'fantrax', ?, 'trade', '2026-11-01', '2026-11-01', 1, ?, ?)",
                params![format!("e{i}"), format!("k{i}"), status, match_json],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn acceptance_is_fitted_from_the_ledger_only_where_proposals_are_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let l = Ledger::open(dir.path()).unwrap();
        // accepted when they gain 10+ worth points, declined (window passed) otherwise
        for i in 0..CALIBRATION_MIN_N + 10 {
            let x = -20.0 + 1.5 * i as f64;
            if x >= 10.0 {
                episode(&l, i, x, "followed", "{}");
            } else {
                episode(&l, i, x, "proposed", r#"{"window_passed": true}"#);
            }
        }
        l.commit().unwrap();
        // no PROPOSED transactions: declines are invisible, the prior stays
        let p = trade_accept_params(&l, "fantrax");
        assert_eq!((p.x0, p.k, p.n), (ACCEPT_X0, ACCEPT_K, 0));
        assert!(p.source.starts_with("prior (no recorded"));
        l.with_conn(|c| {
            c.execute(
                "INSERT INTO transactions (league, tx_id, action, cid, team_id, ts, day) \
                 VALUES ('fantrax', 't1', 'PROPOSED', 'x', '1', '2026-11-01T10:00:00', '2026-11-01')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        l.commit().unwrap();
        // 30 outcomes: fitted; the midpoint moves from 5 toward the observed 10-point threshold
        let p = trade_accept_params(&l, "fantrax");
        assert_eq!(p.n, 30);
        assert!(p.source.starts_with("fit on 30"), "{}", p.source);
        assert!(p.x0 > ACCEPT_X0 && p.x0 < 12.0, "{p:?}");
        // another league has no proposals at all
        assert_eq!(trade_accept_params(&l, "espn").n, 0);
    }
}
