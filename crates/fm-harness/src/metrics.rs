//! Rust port of `fantasy_manager/harness/metrics.py`: the bar - weekly accuracy /
//! recommendation-quality snapshots with trust labels (M2). See the Python module docstring for
//! the definitions; the port is line by line.
//! Owner: work package `W6-harness-metrics-refit`.
//!
//! Deviations:
//! * Rows / reports are JSON maps (`Row` / `Value`) with Python's key order.
//! * `grade_week`'s shadow-scoring error text is `"Error: <message>"` (no Python exception type).
//! * `league_report.matchup_calibration` comes from [`matchup_calibration`]
//!   (`harness::matchup_calib::calibration_report`, `None` on a failure).
//! * Python `sum(floats)` is `py_sum` wherever Python used the builtin `sum`; explicit `+=` loops
//!   (the bootstrap) stay plain loops.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use chrono::{Datelike, Duration, NaiveDate};
use fm_backtest::evaluate::{metrics as bt_metrics, spearman};
use fm_core::Map;
use fm_engine::scoring::PointsScoring;
use fm_engine::valuation::params as vparams;
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::{py_sum, round_to};
use fm_pycompat::random::PyRandom;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::ledger::{json_dumps, now_iso, Ledger, Row};
use crate::outcomes::{days_between, points_scorer, scoring_config, Realized, FLAG_KINDS};
use crate::params_store::ParamsStore;

pub const POOLS: [&str; 3] = ["F", "D", "G"];
pub const MIN_GP_28: i64 = 8;
pub const PROJ_PROVISIONAL: usize = 150;
pub const PROJ_RELIABLE: usize = 400;
pub const PROJ_RELIABLE_WEEKS: usize = 4;
pub const HIT_PROVISIONAL: usize = 20;
pub const HIT_RELIABLE: usize = 50;
pub const CALIBRATION_MIN: usize = 100;
pub const DECILE_MIN: usize = 100;
pub const BOOTSTRAP_N: usize = 200;
pub const BOOTSTRAP_SEED: i64 = 20260928;
pub const Z95: f64 = 1.959964;
pub const BASELINES: [&str; 4] = ["to_date", "preseason", "provider", "last_season"];
/// "alert": followed alerts only (harness.match); informational alerts are never graded
pub const HIT_KINDS: [&str; 6] = ["waiver", "lineup", "injury", "sell_high", "buy_low", "alert"];
pub const HIT_ORIGINS: [&str; 4] = ["followed", "partial", "ignored", "user_only"];
pub const CORE_HIT_ROWS: [&str; 5] =
    ["waiver:followed", "waiver:ignored", "lineup:followed", "lineup:ignored", "waiver:user_only"];
pub const CALIBRATION_KINDS: [&str; 3] = ["waiver", "injury", "lineup"];

pub fn pool_label(pool: &str) -> &str {
    match pool {
        "F" => "forwards",
        "D" => "defense",
        "G" => "goalies",
        other => other,
    }
}

pub fn trust_order(t: &str) -> i64 {
    match t {
        "provisional" => 1,
        "reliable" => 2,
        _ => 0,
    }
}

// --------------------------------------------------------------------------- small JSON helpers

pub(crate) fn fv(x: f64) -> Value {
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

pub(crate) fn ofv(x: Option<f64>) -> Value {
    x.map(fv).unwrap_or(Value::Null)
}

/// `_nan`: None for NaN.
pub(crate) fn nan_none(x: f64) -> Option<f64> {
    (!x.is_nan()).then_some(x)
}

pub(crate) fn f_of(r: &Row, k: &str) -> Option<f64> {
    val_f64(r.get(k))
}

pub(crate) fn val_f64(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn i_of(r: &Row, k: &str) -> Option<i64> {
    match r.get(k) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Some(Value::Bool(b)) => Some(*b as i64),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn s_of<'a>(r: &'a Row, k: &str) -> Option<&'a str> {
    r.get(k).and_then(|v| v.as_str())
}

pub(crate) fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

pub(crate) fn parse_day(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Python truthiness of a JSON value.
pub(crate) fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn row_of(v: Value) -> Row {
    match v {
        Value::Object(m) => m,
        _ => Row::new(),
    }
}

// --------------------------------------------------------------------------- statistics

/// Wilson score interval for k successes out of n (None, None when n == 0).
pub fn wilson(k: i64, n: i64) -> (Option<f64>, Option<f64>) {
    wilson_z(k, n, Z95)
}

pub fn wilson_z(k: i64, n: i64, z: f64) -> (Option<f64>, Option<f64>) {
    if n <= 0 {
        return (None, None);
    }
    let (kf, nf) = (k as f64, n as f64);
    let p = kf / nf;
    let denom = 1.0 + z * z / nf;
    let centre = (p + z * z / (2.0 * nf)) / denom;
    let half = z * (p * (1.0 - p) / nf + z * z / (4.0 * nf * nf)).sqrt() / denom;
    (Some(0.0f64.max(centre - half)), Some(1.0f64.min(centre + half)))
}

/// Linear-interpolated percentile (q in 0..100) of `xs`.
pub fn percentile(xs: &[f64], q: f64) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut s = xs.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pos = (s.len() - 1) as f64 * q / 100.0;
    let lo = pos.floor() as usize;
    let hi = (lo + 1).min(s.len() - 1);
    Some(s[lo] + (s[hi] - s[lo]) * (pos - lo as f64))
}

/// 1 - MAE_fm / MAE_base on paired absolute errors.
pub fn skill_score(fm_abs: &[f64], base_abs: &[f64]) -> Option<f64> {
    let sb = py_sum(base_abs.iter().copied());
    if fm_abs.is_empty() || sb <= 0.0 {
        None
    } else {
        Some(1.0 - py_sum(fm_abs.iter().copied()) / sb)
    }
}

/// Paired bootstrap CI of the skill score, clustered by player: `pairs` are
/// (player, |err_fm|, |err_base|); whole players are resampled with replacement.
pub fn bootstrap_skill(pairs: &[(i64, f64, f64)], n_boot: usize, seed: i64, level: f64) -> (Option<f64>, Option<f64>) {
    let mut by: Map<i64, [f64; 2]> = Map::new();
    for &(pid, a, b) in pairs {
        let acc = by.entry(pid).or_insert([0.0, 0.0]);
        acc[0] += a;
        acc[1] += b;
    }
    if by.len() < 2 {
        return (None, None);
    }
    let vals: Vec<[f64; 2]> = by.values().copied().collect();
    let n = vals.len() as i64;
    let mut rng = PyRandom::new(seed);
    let mut stats = Vec::with_capacity(n_boot);
    for _ in 0..n_boot {
        let (mut sf, mut sb) = (0.0, 0.0);
        for _ in 0..n {
            let [a, b] = vals[rng.randrange_to(n) as usize];
            sf += a;
            sb += b;
        }
        if sb > 0.0 {
            stats.push(1.0 - sf / sb);
        }
    }
    let tail = (1.0 - level) / 2.0 * 100.0;
    (percentile(&stats, tail), percentile(&stats, 100.0 - tail))
}

/// `bootstrap_skill(pairs)` with the module defaults.
pub fn bootstrap_skill_default(pairs: &[(i64, f64, f64)]) -> (Option<f64>, Option<f64>) {
    bootstrap_skill(pairs, BOOTSTRAP_N, BOOTSTRAP_SEED, 0.95)
}

/// Predicted vs realized by decile of predicted (terciles when n < `DECILE_MIN`).
pub fn calibration_bins(pred: &[f64], real: &[f64]) -> Vec<Value> {
    let n = pred.len();
    if n < 3 {
        return Vec::new();
    }
    let k = if n >= DECILE_MIN { 10 } else { 3 };
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| pred[a].partial_cmp(&pred[b]).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    for b in 0..k {
        let idx = &order[b * n / k..(b + 1) * n / k];
        if idx.is_empty() {
            continue;
        }
        let ps: Vec<f64> = idx.iter().map(|&i| pred[i]).collect();
        let rs: Vec<f64> = idx.iter().map(|&i| real[i]).collect();
        let mn = ps.iter().copied().fold(f64::INFINITY, f64::min);
        let mx = ps.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let hits = rs.iter().filter(|r| **r > 0.0).count();
        out.push(json!({
            "bin": b + 1, "n": idx.len(), "pred_lo": round_to(mn, 3), "pred_hi": round_to(mx, 3),
            "mean_pred": round_to(py_sum(ps.iter().copied()) / ps.len() as f64, 3),
            "mean_real": round_to(py_sum(rs.iter().copied()) / rs.len() as f64, 3),
            "hit_rate": round_to(hits as f64 / rs.len() as f64, 3),
        }));
    }
    out
}

// --------------------------------------------------------------------------- trust labels

pub fn season_start_year(day: NaiveDate) -> i32 {
    if day.month() >= 9 {
        day.year()
    } else {
        day.year() - 1
    }
}

pub fn projection_trust(n: usize, weeks: usize, pool: &str, week: NaiveDate) -> &'static str {
    if n < PROJ_PROVISIONAL {
        return "hidden";
    }
    let mut label = if n >= PROJ_RELIABLE && weeks >= PROJ_RELIABLE_WEEKS { "reliable" } else { "provisional" };
    let jan1 = NaiveDate::from_ymd_opt(season_start_year(week) + 1, 1, 1).expect("valid date");
    if pool == "G" && week < jan1 {
        label = "provisional";
    }
    label
}

pub fn hit_trust(n: usize) -> &'static str {
    if n < HIT_PROVISIONAL {
        "hidden"
    } else if n >= HIT_RELIABLE {
        "reliable"
    } else {
        "provisional"
    }
}

pub fn calibration_trust(n: usize) -> &'static str {
    if n < CALIBRATION_MIN {
        "hidden"
    } else {
        "reliable"
    }
}

/// Observations still needed before `metric` leaves `hidden`.
pub fn need_for(metric: &str, n: usize) -> Option<i64> {
    let target = match metric {
        "proj_fpg_mae" | "proj_fpg_skill" | "proj_week_mae" => PROJ_PROVISIONAL,
        "hit_rate" | "counterfactual" => HIT_PROVISIONAL,
        "calibration" => CALIBRATION_MIN,
        _ => return None,
    };
    Some(0i64.max(target as i64 - n as i64))
}

fn need_json(metric: &str, n: usize) -> Value {
    need_for(metric, n).map(Value::from).unwrap_or(Value::Null)
}

// --------------------------------------------------------------------------- projection accuracy

pub fn pool_of(positions: Option<&str>) -> &'static str {
    let ps: BTreeSet<String> = positions
        .unwrap_or("")
        .split(',')
        .map(|p| p.trim().to_uppercase())
        .filter(|p| !p.is_empty())
        .collect();
    if ps.len() == 1 && ps.contains("G") {
        return "G";
    }
    if ps.contains("D") && !["C", "LW", "RW", "F", "W"].iter().any(|x| ps.contains(*x)) {
        return "D";
    }
    "F"
}

pub fn week_monday(day: NaiveDate) -> NaiveDate {
    day - Duration::days(day.weekday().num_days_from_monday() as i64)
}

/// (ISO-week Monday, snapshot day): the Monday itself, else the week's earliest day.
pub fn weekly_snapshots<'a>(days: impl IntoIterator<Item = &'a str>) -> Vec<(NaiveDate, NaiveDate)> {
    let mut by: HashMap<NaiveDate, NaiveDate> = HashMap::new();
    for s in days {
        let Some(d) = parse_day(s) else { continue };
        let m = week_monday(d);
        let e = by.entry(m).or_insert(d);
        if d < *e {
            *e = d;
        }
    }
    let mut out: Vec<(NaiveDate, NaiveDate)> = by.into_iter().collect();
    out.sort();
    out
}

#[derive(Clone, Debug)]
pub struct ProjObs {
    pub week: NaiveDate,
    pub player: i64,
    pub pool: &'static str,
    pub pred: f64,
    pub real: f64,
    pub gp: i64,
    /// Indexed like [`BASELINES`].
    pub baselines: [Option<f64>; 4],
}

impl ProjObs {
    pub fn base(&self, b: &str) -> Option<f64> {
        BASELINES.iter().position(|x| *x == b).and_then(|i| self.baselines[i])
    }
}

#[derive(Clone, Debug)]
pub struct WeekObs {
    pub week: NaiveDate,
    pub player: i64,
    pub pool: &'static str,
    pub pred: f64,
    pub real: f64,
    pub rate_err: f64,
    pub avail_err: f64,
}

fn inputs_of(row: &Row) -> Value {
    let raw = s_of(row, "inputs_json").filter(|s| !s.is_empty()).unwrap_or("null");
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => v,
        _ => json!({}),
    }
}

fn stats_map(v: Option<&Value>) -> Map<String, f64> {
    v.and_then(|x| x.as_object())
        .map(|o| o.iter().filter_map(|(k, x)| val_f64(Some(x)).map(|f| (k.clone(), f))).collect())
        .unwrap_or_default()
}

/// `outcomes._fpg_of`: points per game of an archived line (None without GP).
pub(crate) fn fpg_of(line: Option<&Value>, sc: &PointsScoring) -> Option<f64> {
    let line = line.filter(|l| truthy(Some(l)))?;
    let gp = val_f64(line.get("gp")).filter(|g| *g != 0.0)?;
    Some(sc.value(&stats_map(line.get("stats"))) / gp)
}

/// The baselines available for one archived projection row.
pub fn baseline_values(row: &Row, sc: &PointsScoring, preseason: &HashMap<String, Row>) -> [Option<f64>; 4] {
    let inp = inputs_of(row);
    let mut out = [None; 4];
    out[0] = fpg_of(inp.get("season"), sc);
    if let Some(pre) = s_of(row, "cid").and_then(|c| preseason.get(c)) {
        if let Some(f) = f_of(pre, "fpg") {
            out[1] = Some(f);
        }
    }
    let mut prov = inp.get("projection").cloned().filter(|p| truthy(Some(p)));
    if prov.is_none() {
        if let Some(pj) = s_of(row, "projected_json").filter(|s| !s.is_empty()) {
            prov = serde_json::from_str::<Value>(pj).ok();
        }
    }
    out[2] = fpg_of(prov.as_ref(), sc);
    let hist = inp.get("history").filter(|h| truthy(Some(h)));
    if let Some(rates) = hist.and_then(|h| h.get("rates")).filter(|r| truthy(Some(r))) {
        out[3] = Some(sc.value(&stats_map(Some(rates))));
    }
    out
}

/// Every matured (snapshot, player) window as of `week` (a Monday).
pub fn projection_observations(
    ledger: &Ledger,
    league: &str,
    sc: &PointsScoring,
    realized: &Realized,
    week: NaiveDate,
) -> Result<(Vec<ProjObs>, Vec<WeekObs>, serde_json::Map<String, Value>)> {
    let lg = league.to_string();
    let days: Vec<String> = ledger
        .query("SELECT DISTINCT as_of FROM projections WHERE league=? ORDER BY as_of", &[&lg])?
        .iter()
        .filter_map(|r| s_of(r, "as_of").map(String::from))
        .collect();
    let snaps = weekly_snapshots(days.iter().map(|s| s.as_str()));
    let mut info = serde_json::Map::new();
    let (mut matured28, mut matured7, mut unpulled, mut unmatched) = (0i64, 0i64, 0i64, 0i64);
    info.insert("snapshots".to_string(), json!(snaps.len()));
    let mut fpg_obs = Vec::new();
    let mut wk_obs = Vec::new();
    if days.is_empty() {
        for k in ["matured_28d", "matured_7d", "unpulled", "unmatched"] {
            info.insert(k.into(), json!(0));
        }
        return Ok((fpg_obs, wk_obs, info));
    }
    let cols = "cid, nhl_id, positions, fpg, fpg_week, proj_week, inputs_json, projected_json";
    let sql = format!("SELECT {cols} FROM projections WHERE league=? AND as_of=?");
    let mut preseason: HashMap<String, Row> = HashMap::new();
    for r in ledger.query(&sql, &[&lg, &days[0]])? {
        if let Some(c) = s_of(&r, "cid") {
            preseason.insert(c.to_string(), r.clone());
        }
    }
    let last_day = week - Duration::days(1);
    for (monday, snap) in snaps {
        let d28 = days_between(snap + Duration::days(1), snap + Duration::days(28));
        let d7 = &d28[..7];
        let ok28 = d28[d28.len() - 1] <= last_day && realized.covered(&d28);
        let ok7 = d7[6] <= last_day && realized.covered(d7);
        if (d28[d28.len() - 1] <= last_day && !ok28) || (d7[6] <= last_day && !ok7) {
            unpulled += 1;
        }
        if !(ok28 || ok7) {
            continue;
        }
        matured28 += ok28 as i64;
        matured7 += ok7 as i64;
        let snap_s = iso(snap);
        for r in ledger.query(&sql, &[&lg, &snap_s])? {
            let Some(pid) = i_of(&r, "nhl_id") else {
                unmatched += 1;
                continue;
            };
            let pool = pool_of(s_of(&r, "positions"));
            if ok28 {
                if let Some(fpg) = f_of(&r, "fpg") {
                    let (gp, pts) = realized.window(pid, &d28);
                    if gp >= MIN_GP_28 {
                        fpg_obs.push(ProjObs {
                            week: monday,
                            player: pid,
                            pool,
                            pred: fpg,
                            real: pts / gp as f64,
                            gp,
                            baselines: baseline_values(&r, sc, &preseason),
                        });
                    }
                }
            }
            if ok7 {
                if let Some(pred) = f_of(&r, "proj_week").filter(|p| *p > 0.0) {
                    let pf = f_of(&r, "fpg_week").or_else(|| f_of(&r, "fpg")).unwrap_or(0.0);
                    let (gp, pts) = realized.window(pid, d7);
                    let real_fpg = if gp != 0 { pts / gp as f64 } else { 0.0 };
                    let rate = (pf - real_fpg) * gp as f64;
                    wk_obs.push(WeekObs {
                        week: monday,
                        player: pid,
                        pool,
                        pred,
                        real: pts,
                        rate_err: rate,
                        avail_err: pred - pts - rate,
                    });
                }
            }
        }
    }
    info.insert("matured_28d".into(), json!(matured28));
    info.insert("matured_7d".into(), json!(matured7));
    info.insert("unpulled".into(), json!(unpulled));
    info.insert("unmatched".into(), json!(unmatched));
    Ok((fpg_obs, wk_obs, info))
}

/// `_snap`: one `metric_snapshots` row.
#[allow(clippy::too_many_arguments)]
pub fn snap_row(
    week: NaiveDate,
    league: &str,
    metric: &str,
    pool: &str,
    value: Option<f64>,
    n: usize,
    lo: Option<f64>,
    hi: Option<f64>,
    trust: &str,
    detail: Value,
) -> Row {
    let mut h = Sha1::new();
    h.update(format!("{}|{league}|{metric}|{pool}", iso(week)).as_bytes());
    let sid: String = hex::encode(h.finalize()).chars().take(20).collect();
    let rnd = |v: Option<f64>| -> Value { v.filter(|x| !x.is_nan()).map(|x| fv(round_to(x, 4))).unwrap_or(Value::Null) };
    let detail = if detail.is_object() { detail } else { json!({}) };
    row_of(json!({
        "snapshot_id": sid, "week": iso(week), "league": league, "metric": metric, "pool": pool,
        "value": rnd(value), "n": n, "ci_lo": rnd(lo), "ci_hi": rnd(hi), "trust": trust,
        "detail_json": json_dumps(&detail, true), "created_at": now_iso(),
    }))
}

fn rnd4(v: Option<f64>) -> Value {
    v.map(|x| fv(round_to(x, 4))).unwrap_or(Value::Null)
}

fn mean_abs<I: IntoIterator<Item = f64>>(xs: I) -> (f64, usize) {
    let v: Vec<f64> = xs.into_iter().map(f64::abs).collect();
    (py_sum(v.iter().copied()), v.len())
}

/// One snapshot week's baseline MAEs and skills (for the /health sparklines).
pub fn week_baselines(wk: NaiveDate, ws: &[&ProjObs]) -> Value {
    let mut base_mae = serde_json::Map::new();
    for (bi, b) in BASELINES.iter().enumerate() {
        let sub: Vec<&&ProjObs> = ws.iter().filter(|o| o.baselines[bi].is_some()).collect();
        let v = if sub.is_empty() {
            Value::Null
        } else {
            let (s, n) = mean_abs(sub.iter().map(|o| o.baselines[bi].unwrap() - o.real));
            rnd4(Some(s / n as f64))
        };
        base_mae.insert(b.to_string(), v);
    }
    let td: Vec<&&ProjObs> = ws.iter().filter(|o| o.baselines[0].is_some()).collect();
    let mut skill = serde_json::Map::new();
    let fm_abs: Vec<f64> = td.iter().map(|o| (o.pred - o.real).abs()).collect();
    let td_abs: Vec<f64> = td.iter().map(|o| (o.baselines[0].unwrap() - o.real).abs()).collect();
    skill.insert("fm".into(), rnd4(skill_score(&fm_abs, &td_abs)));
    for (bi, b) in BASELINES.iter().enumerate().skip(1) {
        let sub: Vec<&&&ProjObs> = td.iter().filter(|o| o.baselines[bi].is_some()).collect();
        let v = if sub.is_empty() {
            Value::Null
        } else {
            let a: Vec<f64> = sub.iter().map(|o| (o.baselines[bi].unwrap() - o.real).abs()).collect();
            let c: Vec<f64> = sub.iter().map(|o| (o.baselines[0].unwrap() - o.real).abs()).collect();
            rnd4(skill_score(&a, &c))
        };
        skill.insert(b.to_string(), v);
    }
    json!({"week": iso(wk), "n_to_date": td.len(), "base_mae": base_mae, "skill": skill})
}

pub fn projection_rows(fpg_obs: &[ProjObs], wk_obs: &[WeekObs], week: NaiveDate, league: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for pool in POOLS {
        let obs: Vec<&ProjObs> = fpg_obs.iter().filter(|o| o.pool == pool).collect();
        let week_set: BTreeSet<NaiveDate> = obs.iter().map(|o| o.week).collect();
        let weeks = week_set.len();
        let trust = projection_trust(obs.len(), weeks, pool, week);
        let pred: Vec<f64> = obs.iter().map(|o| o.pred).collect();
        let real: Vec<f64> = obs.iter().map(|o| o.real).collect();
        let m = bt_metrics(&pred, &real, false);
        let mut base = serde_json::Map::new();
        for (bi, b) in BASELINES.iter().enumerate() {
            let sub: Vec<&&ProjObs> = obs.iter().filter(|o| o.baselines[bi].is_some()).collect();
            if sub.is_empty() {
                base.insert(b.to_string(), json!({"n": 0}));
                continue;
            }
            let bp: Vec<f64> = sub.iter().map(|o| o.baselines[bi].unwrap()).collect();
            let sr: Vec<f64> = sub.iter().map(|o| o.real).collect();
            let sp: Vec<f64> = sub.iter().map(|o| o.pred).collect();
            let mb = bt_metrics(&bp, &sr, false);
            let mf = bt_metrics(&sp, &sr, false);
            let fa: Vec<f64> = sub.iter().map(|o| (o.pred - o.real).abs()).collect();
            let ba: Vec<f64> = sub.iter().map(|o| (o.baselines[bi].unwrap() - o.real).abs()).collect();
            base.insert(
                b.to_string(),
                json!({"n": sub.len(), "mae": ofv(nan_none(mb.mae)), "spearman": ofv(nan_none(mb.spearman)),
                       "fm_mae": ofv(nan_none(mf.mae)), "skill": ofv(skill_score(&fa, &ba))}),
            );
        }
        let mut per_week = Vec::new();
        let mut per_week_base = Vec::new();
        for wk in &week_set {
            let ws: Vec<&ProjObs> = obs.iter().copied().filter(|o| o.week == *wk).collect();
            let (s, n) = mean_abs(ws.iter().map(|o| o.pred - o.real));
            per_week.push(json!({"week": iso(*wk), "n": ws.len(), "mae": round_to(s / n as f64, 4)}));
            per_week_base.push(week_baselines(*wk, &ws));
        }
        rows.push(snap_row(
            week,
            league,
            "proj_fpg_mae",
            pool,
            nan_none(m.mae),
            obs.len(),
            None,
            None,
            trust,
            json!({"bias": ofv(nan_none(m.bias)), "rmse": ofv(nan_none(m.rmse)), "spearman": ofv(nan_none(m.spearman)),
                   "weeks": weeks, "per_week": per_week, "per_week_base": per_week_base, "baselines": base,
                   "min_gp": MIN_GP_28, "need": need_json("proj_fpg_mae", obs.len())}),
        ));
        let paired: Vec<(i64, f64, f64)> = obs
            .iter()
            .filter_map(|o| o.baselines[0].map(|td| (o.player, (o.pred - o.real).abs(), (td - o.real).abs())))
            .collect();
        let pa: Vec<f64> = paired.iter().map(|p| p.1).collect();
        let pb: Vec<f64> = paired.iter().map(|p| p.2).collect();
        let sk = skill_score(&pa, &pb);
        let (lo, hi) = if paired.is_empty() { (None, None) } else { bootstrap_skill_default(&paired) };
        let td_weeks: BTreeSet<NaiveDate> = obs.iter().filter(|o| o.baselines[0].is_some()).map(|o| o.week).collect();
        let players: BTreeSet<i64> = paired.iter().map(|p| p.0).collect();
        rows.push(snap_row(
            week,
            league,
            "proj_fpg_skill",
            pool,
            sk,
            paired.len(),
            lo,
            hi,
            projection_trust(paired.len(), td_weeks.len(), pool, week),
            json!({"vs": "to_date", "players": players.len(), "bootstrap": BOOTSTRAP_N,
                   "need": need_json("proj_fpg_skill", paired.len())}),
        ));
        let wo: Vec<&WeekObs> = wk_obs.iter().filter(|o| o.pool == pool).collect();
        let wp: Vec<f64> = wo.iter().map(|o| o.pred).collect();
        let wr: Vec<f64> = wo.iter().map(|o| o.real).collect();
        let mw = bt_metrics(&wp, &wr, false);
        let wweeks = wo.iter().map(|o| o.week).collect::<BTreeSet<_>>().len();
        let (rate_mae, avail_mae) = if wo.is_empty() {
            (None, None)
        } else {
            let (rs, n) = mean_abs(wo.iter().map(|o| o.rate_err));
            let (as_, _) = mean_abs(wo.iter().map(|o| o.avail_err));
            (Some(rs / n as f64), Some(as_ / n as f64))
        };
        rows.push(snap_row(
            week,
            league,
            "proj_week_mae",
            pool,
            nan_none(mw.mae),
            wo.len(),
            None,
            None,
            projection_trust(wo.len(), wweeks, pool, week),
            json!({"bias": ofv(nan_none(mw.bias)), "spearman": ofv(nan_none(mw.spearman)), "weeks": wweeks,
                   "rate_mae": ofv(rate_mae), "avail_mae": ofv(avail_mae), "need": need_json("proj_week_mae", wo.len())}),
        ));
    }
    rows
}

// --------------------------------------------------------------------------- recommendation quality

pub fn primary_window(kind: &str, units: Option<&str>) -> &'static str {
    if kind == "lineup" || matches!(units, Some("week_pts") | Some("matchup_pts")) {
        "7d"
    } else {
        "28d"
    }
}

/// Complete outcomes whose window ended before `week` on each subject's primary window (rows
/// with their decoded `detail` appended).
pub fn matured_outcomes(ledger: &Ledger, league: &str, week: NaiveDate) -> Result<Vec<Row>> {
    let (lg, wk) = (league.to_string(), iso(week));
    let rows = ledger.query("SELECT * FROM outcomes WHERE league=? AND complete=1 AND window_end < ?", &[&lg, &wk])?;
    let mut out = Vec::new();
    for mut r in rows {
        let units = if s_of(&r, "origin") != Some("user_only") { s_of(&r, "gain_units") } else { None };
        if s_of(&r, "window") != Some(primary_window(s_of(&r, "kind").unwrap_or(""), units)) {
            continue;
        }
        if s_of(&r, "basis") == Some("acted_on") {
            continue;
        }
        let raw = s_of(&r, "detail_json").filter(|s| !s.is_empty()).unwrap_or("{}").to_string();
        let detail: Value = serde_json::from_str(&raw)?;
        r.insert("detail".into(), detail);
        out.push(r);
    }
    Ok(out)
}

pub fn hit_rows(outs: &[Row], week: NaiveDate, league: &str) -> Vec<Row> {
    let mut groups: Map<String, Vec<&Row>> = CORE_HIT_ROWS.iter().map(|k| (k.to_string(), Vec::new())).collect();
    for o in outs {
        let kind = s_of(o, "kind").unwrap_or("");
        let origin = s_of(o, "origin").unwrap_or("");
        if kind == "trade" || !HIT_ORIGINS.contains(&origin) || o.get("hit").map_or(true, |h| h.is_null()) {
            continue;
        }
        if !HIT_KINDS.contains(&kind) {
            continue;
        }
        groups.entry(format!("{kind}:{origin}")).or_default().push(o);
    }
    let mut keys: Vec<&String> = groups.keys().collect();
    keys.sort();
    let mut rows = Vec::new();
    for key in keys {
        let os = &groups[key];
        let n = os.len();
        let k: i64 = os.iter().map(|o| i_of(o, "hit").unwrap_or(0)).sum();
        let (lo, hi) = wilson(k, n as i64);
        let kind = key.split(':').next().unwrap_or("");
        let gains: Vec<f64> = os.iter().filter_map(|o| f_of(o, "realized_gain")).collect();
        let windows: BTreeSet<&str> = os.iter().filter_map(|o| s_of(o, "window")).collect();
        let mean_gain = if gains.is_empty() { None } else { Some(py_sum(gains.iter().copied()) / gains.len() as f64) };
        rows.push(snap_row(
            week,
            league,
            "hit_rate",
            key,
            (n > 0).then(|| k as f64 / n as f64),
            n,
            lo,
            hi,
            hit_trust(n),
            json!({"hits": k, "mean_gain": ofv(mean_gain),
                   "units": if FLAG_KINDS.contains(&kind) { "fpg change" } else { "pts" },
                   "windows": windows.into_iter().collect::<Vec<_>>(), "need": need_json("hit_rate", n)}),
        ));
    }
    rows
}

pub fn calibration_row(outs: &[Row], week: NaiveDate, league: &str) -> Row {
    let sel: Vec<&Row> = outs
        .iter()
        .filter(|o| {
            CALIBRATION_KINDS.contains(&s_of(o, "kind").unwrap_or(""))
                && s_of(o, "origin") != Some("user_only")
                && f_of(o, "predicted_pts").is_some()
                && f_of(o, "realized_gain").is_some()
        })
        .collect();
    let pred: Vec<f64> = sel.iter().map(|o| f_of(o, "predicted_pts").unwrap()).collect();
    let real: Vec<f64> = sel.iter().map(|o| f_of(o, "realized_gain").unwrap()).collect();
    let n = sel.len();
    let sp = py_sum(pred.iter().copied());
    let ratio = (!pred.is_empty() && sp > 0.0).then(|| py_sum(real.iter().copied()) / sp);
    let mut by_kind = serde_json::Map::new();
    for k in CALIBRATION_KINDS {
        by_kind.insert(k.into(), json!(sel.iter().filter(|o| s_of(o, "kind") == Some(k)).count()));
    }
    let spear = if n >= 3 { ofv(nan_none(spearman(&pred, &real))) } else { Value::Null };
    snap_row(
        week,
        league,
        "calibration",
        "all",
        ratio,
        n,
        None,
        None,
        calibration_trust(n),
        json!({"bins": calibration_bins(&pred, &real), "binning": if n >= DECILE_MIN { "decile" } else { "tercile" },
               "by_kind": by_kind, "spearman": spear, "value": "realized / predicted",
               "need": need_json("calibration", n)}),
    )
}

pub fn counterfactual_row(outs: &[Row], week: NaiveDate, league: &str) -> Row {
    let mine: Vec<&Row> = outs
        .iter()
        .filter(|o| {
            s_of(o, "origin") == Some("user_only") && s_of(o, "kind") == Some("waiver") && f_of(o, "realized_gain").is_some()
        })
        .collect();
    let detail = |o: &Row| o.get("detail").cloned().unwrap_or(Value::Null);
    let paired: Vec<(f64, f64)> = mine
        .iter()
        .filter_map(|o| {
            let d = detail(o);
            let alt = d.get("alt").filter(|a| truthy(Some(a))).cloned().unwrap_or(json!({}));
            val_f64(alt.get("gain")).map(|g| (f_of(o, "realized_gain").unwrap(), g))
        })
        .collect();
    let agreed: Vec<f64> = mine.iter().filter_map(|o| val_f64(detail(o).get("model_fpg_delta"))).collect();
    let n = paired.len();
    let my_mean = (n > 0).then(|| py_sum(paired.iter().map(|p| p.0)) / n as f64);
    let model_mean = (n > 0).then(|| py_sum(paired.iter().map(|p| p.1)) / n as f64);
    let value = match (my_mean, model_mean) {
        (Some(a), Some(b)) => Some(a - b),
        _ => None,
    };
    let my_mean_all = (!mine.is_empty())
        .then(|| py_sum(mine.iter().map(|o| f_of(o, "realized_gain").unwrap())) / mine.len() as f64);
    let model_better = (n > 0).then(|| paired.iter().filter(|(a, b)| b > a).count() as f64 / n as f64);
    let model_agreed =
        (!agreed.is_empty()).then(|| agreed.iter().filter(|d| **d > 0.0).count() as f64 / agreed.len() as f64);
    snap_row(
        week,
        league,
        "counterfactual",
        "user_only",
        value,
        n,
        None,
        None,
        hit_trust(n),
        json!({"moves": mine.len(), "my_mean_all": ofv(my_mean_all), "my_mean": ofv(my_mean),
               "model_mean": ofv(model_mean), "model_better": ofv(model_better), "model_agreed": ofv(model_agreed),
               "value": "my mean gain - the model's pick's mean gain (pts, same windows)",
               "need": need_json("counterfactual", n)}),
    )
}

pub fn trade_cases_row(ledger: &Ledger, league: &str, week: NaiveDate) -> Result<Row> {
    let (lg, wk) = (league.to_string(), iso(week));
    let rows = ledger.query(
        "SELECT * FROM outcomes WHERE league=? AND kind='trade' AND window IN ('28d','ros') AND origin IN \
         ('followed','partial','proposed','user_only') AND window_start < ? ORDER BY window_start",
        &[&lg, &wk],
    )?;
    let mut cases = Vec::new();
    for r in &rows {
        let raw = s_of(r, "detail_json").filter(|s| !s.is_empty()).unwrap_or("{}");
        let d: Value = serde_json::from_str(raw)?;
        let complete = truthy(r.get("complete"));
        let label = d
            .get("label")
            .filter(|l| truthy(Some(l)))
            .cloned()
            .unwrap_or_else(|| json!(if complete { "complete" } else { "partial" }));
        let g = |k: &str| r.get(k).cloned().unwrap_or(Value::Null);
        cases.push(json!({
            "title": d.get("title").cloned().unwrap_or(Value::Null), "origin": g("origin"), "basis": g("basis"),
            "day": d.get("day").cloned().unwrap_or(Value::Null), "window": g("window"),
            "realized_gain": g("realized_gain"), "complete": complete, "label": label,
            "predicted_gain": g("predicted_gain"), "gain_units": g("gain_units"),
        }));
    }
    let n = cases.len();
    Ok(snap_row(week, league, "trade_cases", "trade", None, n, None, None, "cases", json!({"cases": cases})))
}

// --------------------------------------------------------------------------- weekly grade

/// The bar for `league` as of Monday `week` (rows not written) and some bookkeeping.
pub fn compute_week(ledger: &Ledger, league: &str, week: NaiveDate) -> Result<(Vec<Row>, serde_json::Map<String, Value>)> {
    let week = week_monday(week);
    let (cfg, src) = scoring_config(ledger, league)?;
    let sc = points_scorer(cfg.as_ref());
    let mut info: serde_json::Map<String, Value> = serde_json::Map::new();
    info.insert("league".into(), json!(league));
    info.insert("week".into(), json!(iso(week)));
    info.insert("scoring".into(), json!(src));
    let Some(sc) = sc else {
        info.insert("note".into(), json!("not a points league (or no scoring known): not graded"));
        return Ok((Vec::new(), info));
    };
    let realized = Realized::new(ledger, &sc, None, Some(week))?;
    let (fpg_obs, wk_obs, pinfo) = projection_observations(ledger, league, &sc, &realized, week)?;
    info.extend(pinfo);
    let mut rows = projection_rows(&fpg_obs, &wk_obs, week, league);
    let outs = matured_outcomes(ledger, league, week)?;
    rows.extend(hit_rows(&outs, week, league));
    rows.push(calibration_row(&outs, week, league));
    rows.push(counterfactual_row(&outs, week, league));
    rows.push(trade_cases_row(ledger, league, week)?);
    info.insert("outcomes_used".into(), json!(outs.len()));
    Ok((rows, info))
}

/// Compute and store (replace) the `metric_snapshots` rows of `league` for that week.
pub fn grade_week(ledger: &Ledger, league: &str, week_monday_: NaiveDate) -> Result<serde_json::Map<String, Value>> {
    let (rows, mut info) = compute_week(ledger, league, week_monday_)?;
    let (lg, wk) = (league.to_string(), info["week"].as_str().unwrap_or("").to_string());
    ledger.execute("DELETE FROM metric_snapshots WHERE league=? AND week=?", &[&lg, &wk])?;
    ledger.commit()?;
    ledger.upsert("metric_snapshots", &rows, &["snapshot_id"])?;
    info.insert("rows".into(), json!(rows.len()));
    // champion / challenger: shadow-score the version a promoted params version replaced
    match crate::refit::shadow_check(ledger, week_monday(week_monday_), None) {
        Ok(Some(sh)) => {
            info.insert("shadow".into(), sh);
        }
        Ok(None) => {}
        Err(e) => {
            info.insert("shadow_error".into(), json!(format!("Error: {e}")));
        }
    }
    let mut trust = serde_json::Map::new();
    for t in ["hidden", "provisional", "reliable", "cases"] {
        trust.insert(t.into(), json!(rows.iter().filter(|r| s_of(r, "trust") == Some(t)).count()));
    }
    info.insert("trust".into(), Value::Object(trust));
    Ok(info)
}

// --------------------------------------------------------------------------- reading the bar

/// A stored row with `detail_json` decoded into `detail` (moved to the end).
pub(crate) fn decode(mut r: Row) -> Row {
    let raw = r.shift_remove("detail_json");
    let d = raw
        .as_ref()
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| json!({}));
    r.insert("detail".into(), d);
    r
}

/// The latest graded week of `league` and its rows (ordered by metric, pool).
pub fn latest_rows(ledger: &Ledger, league: &str) -> Result<(Option<String>, Vec<Row>)> {
    let lg = league.to_string();
    let wk = ledger
        .query("SELECT MAX(week) w FROM metric_snapshots WHERE league=?", &[&lg])?
        .first()
        .and_then(|r| s_of(r, "w").map(String::from))
        .filter(|w| !w.is_empty());
    let Some(wk) = wk else { return Ok((None, Vec::new())) };
    let rows = ledger
        .query("SELECT * FROM metric_snapshots WHERE league=? AND week=? ORDER BY metric, pool", &[&lg, &wk])?
        .into_iter()
        .map(decode)
        .collect();
    Ok((Some(wk), rows))
}

fn pct(v: Option<f64>) -> String {
    match v {
        None => "-".into(),
        Some(x) => format!("{}%", fixed(x * 100.0, 0, false)),
    }
}

/// One line for the digest, or None while nothing is trustworthy (every metric hidden).
pub fn headline(ledger: &Ledger, league: &str) -> Result<Option<String>> {
    let (_, rows) = latest_rows(ledger, league)?;
    let mut bits = Vec::new();
    for r in &rows {
        let trust = s_of(r, "trust").unwrap_or("");
        let Some(value) = f_of(r, "value") else { continue };
        if !matches!(trust, "provisional" | "reliable") {
            continue;
        }
        let metric = s_of(r, "metric").unwrap_or("");
        let pool = s_of(r, "pool").unwrap_or("");
        let n = r.get("n").map(|v| v.to_string()).unwrap_or_default();
        if metric == "proj_fpg_skill" {
            let word = if value >= 0.0 { "beat" } else { "trail" };
            let ci = if f_of(r, "ci_lo").is_some() {
                format!(", 95% CI {} to {}", pct(f_of(r, "ci_lo")), pct(f_of(r, "ci_hi")))
            } else {
                String::new()
            };
            bits.push(format!(
                "{} projections {word} season-to-date by {}%{ci} ({trust}, n={n})",
                pool_label(pool),
                fixed(value.abs() * 100.0, 0, false)
            ));
        } else if metric == "hit_rate" && pool.ends_with(":followed") {
            let kind = pool.split(':').next().unwrap_or("");
            bits.push(format!(
                "{kind} recs followed hit {} (95% CI {}-{}, {trust}, n={n})",
                pct(Some(value)),
                pct(f_of(r, "ci_lo")),
                pct(f_of(r, "ci_hi"))
            ));
        }
    }
    Ok((!bits.is_empty()).then(|| format!("Model: {}", bits.join("; "))))
}

pub fn not_judgeable(rows: &[Row]) -> Vec<Value> {
    rows.iter()
        .filter(|r| s_of(r, "trust") == Some("hidden"))
        .map(|r| {
            let need = r.get("detail").and_then(|d| d.get("need")).cloned().unwrap_or(Value::Null);
            json!({"metric": r.get("metric"), "pool": r.get("pool"), "n": r.get("n"), "need": need})
        })
        .collect()
}

/// What is not judgeable before any grading ran.
pub fn default_hidden() -> Vec<Value> {
    let mut rows = Vec::new();
    for m in ["proj_fpg_mae", "proj_week_mae"] {
        for p in POOLS {
            rows.push(json!({"metric": m, "pool": p, "n": 0, "need": need_json(m, 0)}));
        }
    }
    for k in CORE_HIT_ROWS {
        rows.push(json!({"metric": "hit_rate", "pool": k, "n": 0, "need": need_json("hit_rate", 0)}));
    }
    rows.push(json!({"metric": "calibration", "pool": "all", "n": 0, "need": need_json("calibration", 0)}));
    rows.push(json!({"metric": "counterfactual", "pool": "user_only", "n": 0, "need": need_json("counterfactual", 0)}));
    rows
}

pub fn params_info(ledger: &Ledger) -> Value {
    let (h, src) = (vparams::params_hash(None), vparams::source(None));
    let st = ParamsStore::new(None, Some(ledger), None);
    let (version, n) = (st.active_name(), st.versions().len());
    json!({"version": version, "hash": h, "source": src, "versions": n})
}

pub fn league_report(ledger: &Ledger, league: &str) -> Result<Value> {
    let (week, rows) = latest_rows(ledger, league)?;
    let by = |m: &str| -> Vec<Value> {
        rows.iter().filter(|r| s_of(r, "metric") == Some(m)).map(|r| Value::Object(r.clone())).collect()
    };
    let proj: Vec<Value> = rows
        .iter()
        .filter(|r| s_of(r, "metric").is_some_and(|m| m.starts_with("proj_")))
        .map(|r| Value::Object(r.clone()))
        .collect();
    let cal = by("calibration");
    let cf = by("counterfactual");
    let trades = by("trade_cases");
    let lg = league.to_string();
    let oc = ledger.query(
        "SELECT COUNT(*) n, SUM(complete) complete, SUM(realized_gain IS NULL) ungradable, \
         SUM(detail_json LIKE '%\"unmatched\": [\"%') unmatched FROM outcomes WHERE league=?",
        &[&lg],
    )?;
    let oc = oc.into_iter().next().unwrap_or_default();
    let mut outcomes = serde_json::Map::new();
    for k in ["n", "complete", "ungradable", "unmatched"] {
        outcomes.insert(k.into(), json!(i_of(&oc, k).unwrap_or(0)));
    }
    let trade_list = trades
        .first()
        .map(|t| t.get("detail").and_then(|d| d.get("cases")).cloned().unwrap_or(Value::Null))
        .unwrap_or_else(|| json!([]));
    Ok(json!({
        "league": league, "week": week, "graded": week.is_some(),
        "projection": proj, "hit_rates": by("hit_rate"),
        "calibration": cal.first().cloned().unwrap_or(Value::Null),
        "counterfactual": cf.first().cloned().unwrap_or(Value::Null),
        "trades": trade_list,
        "not_judgeable": if rows.is_empty() { default_hidden() } else { not_judgeable(&rows) },
        "outcomes": outcomes,
        "headline": headline(ledger, league)?,
        "matchup_calibration": matchup_calibration(ledger, league),
    }))
}

/// `harness.matchup_calib.calibration_report` (None on a failure: never breaks the bar).
pub fn matchup_calibration(ledger: &Ledger, league: &str) -> Option<Value> {
    crate::matchup_calib::calibration_report(ledger, league, crate::matchup_calib::MIN_MATCHUPS).ok()
}

pub fn known_leagues(ledger: &Ledger) -> Result<Vec<String>> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    for t in ["rec_episodes", "projections", "metric_snapshots", "decisions"] {
        for r in ledger.query(&format!("SELECT DISTINCT league FROM {t} WHERE league IS NOT NULL"), &[])? {
            if let Some(l) = s_of(&r, "league") {
                found.insert(l.to_string());
            }
        }
    }
    Ok(found.into_iter().collect())
}

/// The bar for every league (latest stored snapshot), for `fm harness status` and /health.
pub fn status_report(ledger: &Ledger, leagues: Option<&[String]>) -> Result<Value> {
    let lgs: Vec<String> = match leagues {
        Some(l) if !l.is_empty() => l.to_vec(),
        _ => known_leagues(ledger)?,
    };
    let mut by = serde_json::Map::new();
    for lg in &lgs {
        by.insert(lg.clone(), league_report(ledger, lg)?);
    }
    Ok(json!({
        "params": params_info(ledger), "leagues": by,
        "thresholds": {
            "projection": {"provisional": PROJ_PROVISIONAL, "reliable": PROJ_RELIABLE,
                           "reliable_weeks": PROJ_RELIABLE_WEEKS, "goalies": "provisional at most before January 1"},
            "hit_rate": {"provisional": HIT_PROVISIONAL, "reliable": HIT_RELIABLE},
            "calibration": {"min": CALIBRATION_MIN}, "trades": "case list only",
        },
    }))
}

pub fn empty_report() -> Value {
    json!({"params": null, "leagues": {}, "note": "no harness ledger yet: run `fm harness daily`"})
}
