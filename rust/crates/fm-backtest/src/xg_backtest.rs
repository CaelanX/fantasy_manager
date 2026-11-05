//! Rust port of `fantasy_manager/backtest/xg_backtest.py`: does shrinking goals toward expected
//! goals (MoneyPuck ixG) improve next-season projections?
//! Owner: work package `W4-backtest-models`.
//!
//! 1. Projection test (`evaluate_projections`): naive / fm_current, each as is and with every
//!    history season's goal rate first shrunk toward that season's ixG rate for k in `K_GRID`.
//! 2. Shooting-% regression (`shooting_pairs` / `shooting_summary`).
//! 3. Luck terms (`luck_summary`).
//!
//! Report: `<fm_data_dir>/backtest/xg-report-YYYY-MM-DD.md`.
//!
//! Deviations from the Python:
//! * The `argparse` `main()` is the `fm` binary's job (fm-cli); `run_xg_backtest` is the entry.
//! * `XgShrunk` holds its base model and the xG table behind `Arc` (Python shares references).
//! * Result dicts (`shooting_summary`, `luck_summary`) are `Map<String, f64>` (counts as floats);
//!   `headline` returns `None` for Python's empty dict.
//! * Report text is written with `\n` line ends.
//! * The OLS in `ols_slope` is the plain normal equation form of the Python (no library solve).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use chrono::NaiveDate;
use fm_core::Map;
use fm_engine::scoring::PointsScoring;
use fm_engine::valuation::regression::{shrink_goals, ASSIST_LUCK_CLAMP, K_GOALS};
use fm_http::HttpCache;
use fm_pycompat::fmt::{fixed, general};
use fm_pycompat::num::py_sum;
use fm_sources::moneypuck::{onice_sh_norms, xg_calibration, MoneyPuckClient, MpSkater, CREDIT};
use indexmap::IndexMap;

use crate::data::{backtest_dir, load_table, season_label, PlayerSeason, SeasonTable};
use crate::evaluate::{metrics, pearson, preseason_predictions, spearman, MIN_GP};
use crate::models::{FmCurrent, Model, NaiveLastSeason, ProjectionContext, Rates};
use crate::pipeline::re_log;
use crate::scoring::{scorer, PRESETS};

pub const FIRST_MP_YEAR: i64 = 2018;
pub const LAST_MP_YEAR: i64 = 2025;
/// |goals - ixG| in N-1 for the "big gap" pool
pub const BIG_GAP: f64 = 5.0;
pub const MIN_SOG: f64 = 50.0;
pub const MIN_GP_ASSISTS: i64 = 40;
pub const SCORINGS: [&str; 2] = ["espn", "fantrax"];
const INF: f64 = f64::INFINITY;

/// `K_GRID = (5, 10, 15, 30, 60, 120, inf)`.
pub fn k_grid() -> Vec<f64> {
    vec![5.0, 10.0, 15.0, 30.0, 60.0, 120.0, INF]
}

/// 2025 -> 20252026.
pub fn sid(year: i64) -> i64 {
    year * 10000 + year + 1
}

pub fn k_label(k: f64) -> String {
    if k == INF {
        "ixG".to_string()
    } else {
        general(k, None, false)
    }
}

/// 'fm_current+xg15', 'naive+xgcal60', 'fm_current+xgixG'.
pub fn arm_name(base: &str, k: f64, calibrate: bool) -> String {
    format!("{base}+xg{}{}", if calibrate { "cal" } else { "" }, k_label(k))
}

// --------------------------------------------------------------------------- data

pub struct XgSeason {
    /// situation "all" by NHL id
    pub all: Map<i64, MpSkater>,
    /// situation "5on5"
    pub five: Map<i64, MpSkater>,
    /// group -> league goals / ixG
    pub cal: Map<String, f64>,
}

pub type XgTable = Map<i64, XgSeason>;

/// NHL season id -> MoneyPuck skater rows (all situations / 5-on-5).
pub fn load_xg(client: &MoneyPuckClient, years: &[i64], mut log: Option<&mut dyn FnMut(&str)>) -> Result<XgTable> {
    let mut out = XgTable::new();
    for &y in years {
        let t0 = Instant::now();
        let rows = client.skaters(y, None, false)?;
        let alls: Vec<&MpSkater> = rows.iter().filter(|r| r.situation == "all").collect();
        let season = XgSeason {
            all: alls.iter().map(|r| (r.nhl_id, (*r).clone())).collect(),
            five: rows.iter().filter(|r| r.situation == "5on5").map(|r| (r.nhl_id, r.clone())).collect(),
            cal: xg_calibration(alls.iter().copied()),
        };
        if let Some(l) = log.as_mut() {
            l(&format!(
                "MoneyPuck {}: {} skaters ({}s)",
                season_label(sid(y)),
                season.all.len(),
                fixed(t0.elapsed().as_secs_f64(), 1, false)
            ));
        }
        out.insert(sid(y), season);
    }
    Ok(out)
}

/// `row` with its goal total shrunk toward ixG (unchanged for goalies / no ixG). `calibrate`
/// scales ixG by the season's league goals / ixG ratio of the player's group.
pub fn adjust_row(row: &PlayerSeason, xg: &XgTable, k: f64, calibrate: bool) -> PlayerSeason {
    if row.is_goalie() || row.gp <= 0 || !row.stats.contains_key("G") {
        return row.clone();
    }
    let Some(season) = xg.get(&row.season) else { return row.clone() };
    let Some(mp) = season.all.get(&row.player_id) else { return row.clone() };
    if mp.gp <= 0 {
        return row.clone();
    }
    let scale = if calibrate { season.cal.get(mp.group()).copied().unwrap_or(1.0) } else { 1.0 };
    let rates = shrink_goals(&row.per_game(), Some(mp.ixg / mp.gp as f64 * scale), row.gp as f64, k);
    let mut out = row.clone();
    out.stats = rates.into_iter().map(|(key, v)| (key, v * row.gp as f64)).collect();
    out
}

/// Wraps a backtest model: the history's goal rates are shrunk toward ixG first.
pub struct XgShrunk {
    pub base: Arc<dyn Model>,
    pub xg: Arc<XgTable>,
    pub k: f64,
    pub calibrate: bool,
    pub name: String,
}

impl XgShrunk {
    pub fn new(base: Arc<dyn Model>, xg: Arc<XgTable>, k: f64, calibrate: bool) -> Self {
        let name = arm_name(base.name(), k, calibrate);
        XgShrunk { base, xg, k, calibrate, name }
    }
}

impl Model for XgShrunk {
    fn name(&self) -> &str {
        &self.name
    }
    fn rates(&self, history: &[&PlayerSeason], age: Option<f64>, ctx: &ProjectionContext) -> Option<Rates> {
        let adjusted: Vec<PlayerSeason> = history.iter().map(|h| adjust_row(h, &self.xg, self.k, self.calibrate)).collect();
        let refs: Vec<&PlayerSeason> = adjusted.iter().collect();
        self.base.rates(&refs, age, ctx)
    }
}

// --------------------------------------------------------------------------- 1. projections

#[derive(Clone, Debug, PartialEq)]
pub struct ProjRow {
    pub scoring: String,
    pub season: i64,
    /// skaters / big_gap
    pub pool: String,
    pub model: String,
    pub n: usize,
    pub mae: f64,
    pub spearman: f64,
    pub bias: f64,
}

pub fn evaluate_projections(
    table: &SeasonTable,
    xg: &Arc<XgTable>,
    seasons: &[i64],
    scoring_name: &str,
    scoring: &PointsScoring,
    k_grid: &[f64],
) -> Vec<ProjRow> {
    let bases: [Arc<dyn Model>; 2] = [Arc::new(NaiveLastSeason), Arc::new(FmCurrent)];
    let mut models: Vec<Box<dyn Model>> = vec![Box::new(NaiveLastSeason), Box::new(FmCurrent)];
    for b in &bases {
        for cal in [false, true] {
            for &k in k_grid {
                models.push(Box::new(XgShrunk::new(Arc::clone(b), Arc::clone(xg), k, cal)));
            }
        }
    }
    let mut out = Vec::new();
    for &season in seasons {
        let preds: Vec<_> =
            preseason_predictions(table, scoring, season, &models, MIN_GP, MIN_GP).into_iter().filter(|p| p.group != "G").collect();
        let prev = xg.get(&(season - 10001));
        let big: std::collections::HashSet<i64> = preds
            .iter()
            .filter(|p| prev.is_some_and(|pv| pv.all.get(&p.player_id).is_some_and(|m| m.goals_minus_ixg().abs() >= BIG_GAP)))
            .map(|p| p.player_id)
            .collect();
        for (pool, keep) in [("skaters", None), ("big_gap", Some(&big))] {
            let mut by_model: IndexMap<&str, Vec<&crate::evaluate::Prediction>> = IndexMap::new();
            for p in &preds {
                if keep.is_none_or(|k| k.contains(&p.player_id)) {
                    by_model.entry(p.model.as_str()).or_default().push(p);
                }
            }
            for m in &models {
                let Some(ps) = by_model.get(m.name()) else { continue };
                if ps.is_empty() {
                    continue;
                }
                let pred: Vec<f64> = ps.iter().map(|p| p.projected).collect();
                let act: Vec<f64> = ps.iter().map(|p| p.actual).collect();
                let mt = metrics(&pred, &act, false);
                out.push(ProjRow {
                    scoring: scoring_name.to_string(),
                    season,
                    pool: pool.to_string(),
                    model: m.name().to_string(),
                    n: mt.n,
                    mae: mt.mae,
                    spearman: mt.spearman,
                    bias: mt.bias,
                });
            }
        }
    }
    out
}

// --------------------------------------------------------------------------- 2./3. regression

#[derive(Clone, Debug, PartialEq)]
pub struct ShPair {
    /// N
    pub season: i64,
    /// (G - ixG) / SOG in N-1
    pub gmx_per_sog: f64,
    pub gmx_per_gp_prev: f64,
    pub gmx_per_gp_next: f64,
    pub sh_prev: f64,
    pub sh_next: f64,
    /// ixG / SOG in N-1
    pub xsh_prev: f64,
}

pub fn shooting_pairs(xg: &XgTable, min_sog: f64) -> Vec<ShPair> {
    let mut seasons: Vec<i64> = xg.keys().copied().collect();
    seasons.sort();
    let mut out = Vec::new();
    for s in seasons {
        let Some(prev) = xg.get(&(s - 10001)) else { continue };
        for (pid, b) in &xg[&s].all {
            let Some(a) = prev.all.get(pid) else { continue };
            if a.sog < min_sog || b.sog < min_sog || a.gp <= 0 || b.gp <= 0 {
                continue;
            }
            out.push(ShPair {
                season: s,
                gmx_per_sog: (a.goals - a.ixg) / a.sog,
                gmx_per_gp_prev: (a.goals - a.ixg) / a.gp as f64,
                gmx_per_gp_next: (b.goals - b.ixg) / b.gp as f64,
                sh_prev: a.goals / a.sog,
                sh_next: b.goals / b.sog,
                xsh_prev: a.ixg / a.sog,
            });
        }
    }
    out
}

pub fn ols_slope(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len();
    if n < 2 {
        return f64::NAN;
    }
    let nf = n as f64;
    let (mx, my) = (py_sum(x.iter().copied()) / nf, py_sum(y.iter().copied()) / nf);
    let sxx = py_sum(x.iter().map(|a| (a - mx).powf(2.0)));
    if sxx > 0.0 {
        py_sum(x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my))) / sxx
    } else {
        f64::NAN
    }
}

fn mae_of(pred: &[f64], actual: &[f64]) -> f64 {
    if pred.is_empty() {
        f64::NAN
    } else {
        py_sum(pred.iter().zip(actual).map(|(p, a)| (p - a).abs())) / pred.len() as f64
    }
}

pub fn shooting_summary(pairs: &[ShPair]) -> Map<String, f64> {
    let mut out = Map::new();
    if pairs.is_empty() {
        out.insert("n".to_string(), 0.0);
        return out;
    }
    let n = pairs.len() as f64;
    let x: Vec<f64> = pairs.iter().map(|p| p.gmx_per_sog).collect();
    let dy: Vec<f64> = pairs.iter().map(|p| p.sh_next - p.sh_prev).collect();
    let league = py_sum(pairs.iter().map(|p| p.sh_prev)) / n;
    let sh_dev: Vec<f64> = pairs.iter().map(|p| p.sh_prev - league).collect();
    let nxt: Vec<f64> = pairs.iter().map(|p| p.sh_next).collect();
    // best 50/50-style blend of last season's sh% and xsh% for predicting next season's sh%
    let blend: Vec<(f64, f64)> = [0.0, 0.25, 0.5, 0.75, 1.0]
        .iter()
        .map(|&w| {
            let pred: Vec<f64> = pairs.iter().map(|p| w * p.sh_prev + (1.0 - w) * p.xsh_prev).collect();
            (w, mae_of(&pred, &nxt))
        })
        .collect();
    let mut best = blend[0];
    for b in &blend {
        if b.1 < best.1 {
            best = *b;
        }
    }
    let sh_prev: Vec<f64> = pairs.iter().map(|p| p.sh_prev).collect();
    let xsh_prev: Vec<f64> = pairs.iter().map(|p| p.xsh_prev).collect();
    let gp_prev: Vec<f64> = pairs.iter().map(|p| p.gmx_per_gp_prev).collect();
    let gp_next: Vec<f64> = pairs.iter().map(|p| p.gmx_per_gp_next).collect();
    for (k, v) in [
        ("n", n),
        ("corr_gmx_vs_dsh", pearson(&x, &dy)),
        ("spearman_gmx_vs_dsh", spearman(&x, &dy)),
        ("slope_gmx_vs_dsh", ols_slope(&x, &dy)),
        ("corr_shdev_vs_dsh", pearson(&sh_dev, &dy)),
        ("corr_sh_prev_next", pearson(&sh_prev, &nxt)),
        ("corr_xsh_prev_next", pearson(&xsh_prev, &nxt)),
        ("mae_sh_prev", blend[4].1),
        ("mae_xsh_prev", blend[0].1),
        ("best_blend_w_sh", best.0),
        ("mae_best_blend", best.1),
        ("corr_gmx_gp_persistence", pearson(&gp_prev, &gp_next)),
    ] {
        out.insert(k.to_string(), v);
    }
    out
}

/// How much of `valuation.regression`'s luck terms goes away the next season (skaters with
/// >= `min_gp` GP in N-1 and N).
pub fn luck_summary(table: &SeasonTable, xg: &XgTable, min_gp: i64) -> Map<String, f64> {
    let (mut gx, mut gy, mut ax, mut ay, mut dx) = (vec![], vec![], vec![], vec![], vec![]);
    let (lo, hi) = ASSIST_LUCK_CLAMP;
    let mut seasons: Vec<i64> = xg.keys().copied().collect();
    seasons.sort();
    for s in seasons {
        let Some(prev) = xg.get(&(s - 10001)) else { continue };
        let norms = onice_sh_norms(prev.five.values(), 200.0);
        for (pid, mp) in &prev.all {
            let a = table.get(*pid, s - 10001);
            let b = table.get(*pid, s);
            let (Some(a), Some(b)) = (a, b) else { continue };
            if a.is_goalie() || a.gp < min_gp || b.gp < min_gp || mp.gp <= 0 {
                continue;
            }
            gx.push((mp.goals - mp.ixg) / mp.gp as f64);
            gy.push(
                b.stats.get("G").copied().unwrap_or(0.0) / b.gp as f64
                    - a.stats.get("G").copied().unwrap_or(0.0) / a.gp as f64,
            );
            let Some(five) = prev.five.get(pid) else { continue };
            let Some(oi) = five.onice_sh_pct.filter(|v| *v != 0.0) else { continue };
            let Some(norm) = norms.get(five.group()) else { continue };
            let a_pg = a.stats.get("A").copied().unwrap_or(0.0) / a.gp as f64;
            ax.push(a_pg * hi.min(lo.max(1.0 - norm / oi)));
            ay.push(b.stats.get("A").copied().unwrap_or(0.0) / b.gp as f64 - a_pg);
            dx.push(oi - norm);
        }
    }
    let nan = f64::NAN;
    let mut out = Map::new();
    for (k, v) in [
        ("n_goals", gx.len() as f64),
        ("corr_goal_luck", if gx.is_empty() { nan } else { pearson(&gx, &gy) }),
        ("slope_goal_luck", if gx.is_empty() { nan } else { ols_slope(&gx, &gy) }),
        ("n", ax.len() as f64),
        ("corr_assist_luck", if ax.is_empty() { nan } else { pearson(&ax, &ay) }),
        ("slope_assist_luck", if ax.is_empty() { nan } else { ols_slope(&ax, &ay) }),
        ("corr_onice_delta_vs_dA", if dx.is_empty() { nan } else { pearson(&dx, &ay) }),
    ] {
        out.insert(k.to_string(), v);
    }
    out
}

/// Season id -> 5v5 on-ice shooting % norms (F / D).
pub fn season_norms(xg: &XgTable) -> Map<i64, Map<String, f64>> {
    let mut seasons: Vec<i64> = xg.keys().copied().collect();
    seasons.sort();
    seasons.into_iter().map(|s| (s, onice_sh_norms(xg[&s].five.values(), 200.0))).collect()
}

// --------------------------------------------------------------------------- report

fn avg(rows: &[&ProjRow], f: impl Fn(&ProjRow) -> f64) -> f64 {
    let vals: Vec<f64> = rows.iter().map(|r| f(r)).filter(|v| !v.is_nan()).collect();
    if vals.is_empty() {
        f64::NAN
    } else {
        py_sum(vals.iter().copied()) / vals.len() as f64
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct XgSummary {
    pub mae: f64,
    pub spearman: f64,
    pub bias: f64,
    pub n: usize,
    pub seasons: usize,
}

pub fn summarize(rows: &[ProjRow], scoring: &str, pool: &str) -> IndexMap<String, XgSummary> {
    let mut models: Vec<&str> = Vec::new();
    for r in rows {
        if r.scoring == scoring && r.pool == pool && !models.contains(&r.model.as_str()) {
            models.push(&r.model);
        }
    }
    let mut out = IndexMap::new();
    for m in models {
        let rs: Vec<&ProjRow> = rows.iter().filter(|r| r.scoring == scoring && r.pool == pool && r.model == m).collect();
        out.insert(
            m.to_string(),
            XgSummary {
                mae: avg(&rs, |r| r.mae),
                spearman: avg(&rs, |r| r.spearman),
                bias: avg(&rs, |r| r.bias),
                n: rs.iter().map(|r| r.n).sum(),
                seasons: rs.len(),
            },
        );
    }
    out
}

pub const HEADLINE_ARMS: [(&str, f64, bool); 5] = [
    ("fm_current", K_GOALS, false),
    ("fm_current", K_GOALS, true),
    ("fm_current", 5.0, true),
    ("naive", K_GOALS, false),
    ("naive", 60.0, true),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Headline {
    pub seasons: usize,
    pub mae_base: f64,
    pub mae_xg: f64,
    pub d_mae: f64,
    pub d_mae_pct: f64,
    pub d_spearman: f64,
    pub mae_wins: usize,
    pub spearman_wins: usize,
}

/// Average MAE / Spearman change of `base`+xg(k) vs `base` and seasons improved (None: no pairs).
pub fn headline(rows: &[ProjRow], scoring: &str, base: &str, k: f64, pool: &str, calibrate: bool) -> Option<Headline> {
    let name = arm_name(base, k, calibrate);
    let mut pairs: Vec<(&ProjRow, &ProjRow)> = Vec::new();
    for r in rows {
        if r.scoring == scoring && r.pool == pool && r.model == base {
            let o = rows.iter().find(|x| x.scoring == scoring && x.pool == pool && x.model == name && x.season == r.season);
            if let Some(o) = o {
                pairs.push((r, o));
            }
        }
    }
    if pairs.is_empty() {
        return None;
    }
    let n = pairs.len() as f64;
    let d_mae: Vec<f64> = pairs.iter().map(|(r, o)| o.mae - r.mae).collect();
    let d_sp: Vec<f64> = pairs.iter().map(|(r, o)| o.spearman - r.spearman).collect();
    let sum_d_mae = py_sum(d_mae.iter().copied());
    Some(Headline {
        seasons: pairs.len(),
        mae_base: py_sum(pairs.iter().map(|(r, _)| r.mae)) / n,
        mae_xg: py_sum(pairs.iter().map(|(_, o)| o.mae)) / n,
        d_mae: sum_d_mae / n,
        d_mae_pct: 100.0 * sum_d_mae / py_sum(pairs.iter().map(|(r, _)| r.mae)),
        d_spearman: py_sum(d_sp.iter().copied()) / n,
        mae_wins: d_mae.iter().filter(|d| **d < 0.0).count(),
        spearman_wins: d_sp.iter().filter(|d| **d > 0.0).count(),
    })
}

/// `_f(v, fmt)`: "-" for None / NaN.
fn f_opt(v: Option<f64>, prec: usize) -> String {
    match v {
        Some(x) if !x.is_nan() => fixed(x, prec, false),
        _ => "-".to_string(),
    }
}

fn get(m: &Map<String, f64>, k: &str) -> f64 {
    m.get(k).copied().unwrap_or(f64::NAN)
}

#[allow(clippy::too_many_arguments)]
pub fn render_report(
    rows: &[ProjRow],
    sh: &Map<String, f64>,
    ast: &Map<String, f64>,
    norms: &Map<i64, Map<String, f64>>,
    seasons: &[i64],
    years: &[i64],
    counts: &Map<i64, usize>,
    runtime: f64,
    today: NaiveDate,
) -> String {
    let mut l: Vec<String> = Vec::new();
    let s = |x: &str| x.to_string();
    l.push(format!("# Expected-goals backtest ({})", today.format("%Y-%m-%d")));
    l.push(s(""));
    let tail = |y: i64| -> String {
        let t = (y + 1).to_string();
        t[t.len().saturating_sub(2)..].to_string()
    };
    l.push(format!(
        "{CREDIT} (season summaries {}-{}..{}-{}, {} skater rows). Target seasons {}..{}; skaters with >= {MIN_GP} GP in N-1 and N. Runtime {}s.",
        years[0],
        tail(years[0]),
        years[years.len() - 1],
        tail(years[years.len() - 1]),
        counts.values().sum::<usize>(),
        season_label(seasons[0]),
        season_label(seasons[seasons.len() - 1]),
        fixed(runtime, 0, false),
    ));
    l.push(s(""));
    l.push(s("Goal rate of every history season shrunk toward that season's ixG rate: G' = (GP*G + k*ixG/GP)/(GP + k); `xgixG` replaces goals with ixG. Seasons before 2018-19 carry no ixG and stay unadjusted."));
    l.push(s(""));
    l.push(s("## Headline (each arm vs its base model; `cal` = ixG scaled by the season's league goals / ixG of the player's group)"));
    l.push(s(""));
    l.push(s("| scoring | pool | arm | seasons | MAE base | MAE +xG | dMAE | dMAE % | dSpearman | MAE wins | Spearman wins |"));
    l.push(s("|---|---|---|---|---|---|---|---|---|---|---|"));
    for sc in SCORINGS {
        for pool in ["skaters", "big_gap"] {
            for (base, k, cal) in HEADLINE_ARMS {
                let Some(h) = headline(rows, sc, base, k, pool, cal) else { continue };
                l.push(format!(
                    "| {sc} | {pool} | {} | {} | {} | {} | {} | {}% | {} | {}/{} | {}/{} |",
                    arm_name(base, k, cal),
                    h.seasons,
                    fixed(h.mae_base, 4, false),
                    fixed(h.mae_xg, 4, false),
                    fixed(h.d_mae, 4, true),
                    fixed(h.d_mae_pct, 2, true),
                    fixed(h.d_spearman, 4, true),
                    h.mae_wins,
                    h.seasons,
                    h.spearman_wins,
                    h.seasons
                ));
            }
        }
    }
    l.push(s(""));
    for sc in SCORINGS {
        for pool in ["skaters", "big_gap"] {
            let summ = summarize(rows, sc, pool);
            if summ.is_empty() {
                continue;
            }
            l.push(format!("## Average over seasons: {sc}, {pool}"));
            l.push(s(""));
            l.push(s("| model | MAE | Spearman | bias | n |"));
            l.push(s("|---|---|---|---|---|"));
            for (m, v) in &summ {
                l.push(format!(
                    "| {m} | {} | {} | {} | {} |",
                    fixed(v.mae, 4, false),
                    fixed(v.spearman, 4, false),
                    fixed(v.bias, 4, true),
                    v.n
                ));
            }
            l.push(s(""));
        }
    }
    for sc in SCORINGS {
        l.push(format!("## Per season: {sc}, skaters (fm_current vs +xG k=15 / ixG)"));
        l.push(s(""));
        l.push(s("| season | n | MAE base | MAE k=15 | MAE ixG | Spearman base | Spearman k=15 | Spearman ixG |"));
        l.push(s("|---|---|---|---|---|---|---|---|"));
        for &se in seasons {
            let mut get_m: IndexMap<&str, &ProjRow> = IndexMap::new();
            for r in rows {
                if r.scoring == sc && r.pool == "skaters" && r.season == se {
                    get_m.insert(r.model.as_str(), r);
                }
            }
            let Some(b) = get_m.get("fm_current") else { continue };
            let x = get_m.get("fm_current+xg15");
            let f = get_m.get("fm_current+xgixG");
            l.push(format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                season_label(se),
                b.n,
                fixed(b.mae, 4, false),
                f_opt(x.map(|r| r.mae), 4),
                f_opt(f.map(|r| r.mae), 4),
                fixed(b.spearman, 4, false),
                f_opt(x.map(|r| r.spearman), 4),
                f_opt(f.map(|r| r.spearman), 4),
            ));
        }
        l.push(s(""));
    }
    let n_sh = sh.get("n").copied().unwrap_or(0.0);
    l.push(format!("## Shooting-% regression (pairs with >= {MIN_SOG:.0} SOG both seasons, n = {})", n_sh as i64));
    l.push(s(""));
    if n_sh != 0.0 {
        l.push(format!(
            "- corr((G - ixG)/SOG in N-1, sh% change N-1 -> N): **{}** (Spearman {}); slope {} (-1 = the whole excess disappears)",
            fixed(sh["corr_gmx_vs_dsh"], 3, false),
            fixed(sh["spearman_gmx_vs_dsh"], 3, false),
            fixed(sh["slope_gmx_vs_dsh"], 2, false)
        ));
        l.push(format!(
            "- for comparison corr(sh% minus league mean in N-1, sh% change): {}",
            fixed(sh["corr_shdev_vs_dsh"], 3, false)
        ));
        l.push(format!(
            "- predicting next season's sh%: corr with last sh% {} vs with ixG/SOG {}; MAE last sh% {}, ixG/SOG {}, best blend (weight {} on last sh%) {}",
            fixed(sh["corr_sh_prev_next"], 3, false),
            fixed(sh["corr_xsh_prev_next"], 3, false),
            fixed(sh["mae_sh_prev"], 4, false),
            fixed(sh["mae_xsh_prev"], 4, false),
            fixed(sh["best_blend_w_sh"], 2, false),
            fixed(sh["mae_best_blend"], 4, false)
        ));
        l.push(format!(
            "- persistence of finishing: corr((G - ixG)/GP N-1, same in N) = {}",
            fixed(sh["corr_gmx_gp_persistence"], 3, false)
        ));
        l.push(s(""));
    }
    let fo = |k: &str, p: usize| f_opt(ast.get(k).copied(), p);
    l.push(format!("## Luck terms of `valuation.regression` (>= {MIN_GP_ASSISTS} GP both seasons)"));
    l.push(s(""));
    l.push(format!(
        "- goal luck (G - ixG)/GP in N-1 vs change in G/GP (n = {}): corr **{}**, slope {} (-1 = all of the goal luck disappears the next season)",
        ast.get("n_goals").copied().unwrap_or(0.0) as i64,
        fo("corr_goal_luck", 3),
        fo("slope_goal_luck", 2)
    ));
    l.push(format!(
        "- assist luck A/GP * clamp(1 - norm / on-ice sh%) vs change in A/GP (n = {}): corr **{}**, slope {}",
        ast.get("n").copied().unwrap_or(0.0) as i64,
        fo("corr_assist_luck", 3),
        fo("slope_assist_luck", 2)
    ));
    l.push(format!(
        "- corr(5v5 on-ice sh% minus positional norm in N-1, A/GP change): {}",
        fo("corr_onice_delta_vs_dA", 3)
    ));
    let norm_txt: Vec<String> = norms
        .iter()
        .map(|(se, n)| format!("{} {} / {}", season_label(*se), fixed(get(n, "F"), 4, false), fixed(get(n, "D"), 4, false)))
        .collect();
    l.push(format!("- 5v5 on-ice sh% norms (F / D) by season: {}", norm_txt.join(", ")));
    l.push(s(""));
    l.join("\n") + "\n"
}

/// `run_xg_backtest` results (`results` dict).
pub struct XgResults {
    pub rows: Vec<ProjRow>,
    pub shooting: Map<String, f64>,
    pub assists: Map<String, f64>,
    pub norms: Map<i64, Map<String, f64>>,
    pub seasons: Vec<i64>,
    pub headline: Map<String, Option<Headline>>,
    pub runtime: f64,
}

/// Run all three tests; write the markdown report (`write`) and return (path, results).
#[allow(clippy::too_many_arguments)]
pub fn run_xg_backtest(
    cache: &HttpCache,
    data_dir: &std::path::Path,
    years: Option<&[i64]>,
    seasons: Option<&[i64]>,
    client: Option<&MoneyPuckClient>,
    table: Option<SeasonTable>,
    today: NaiveDate,
    mut log: Option<&mut dyn FnMut(&str)>,
    write: bool,
) -> Result<(Option<PathBuf>, XgResults)> {
    let t0 = Instant::now();
    let years: Vec<i64> = years.map(|y| y.to_vec()).unwrap_or_else(|| (FIRST_MP_YEAR..=LAST_MP_YEAR).collect());
    let own_client;
    let client = match client {
        Some(c) => c,
        None => {
            own_client = MoneyPuckClient::from_cache(cache, Some(today));
            &own_client
        }
    };
    let table = match table {
        Some(t) => t,
        None => load_table(data_dir)?,
    };
    if table.seasons().is_empty() {
        return Err(anyhow!("NHL history table is empty; run `fm backtest data` first"));
    }
    let xg = Arc::new(load_xg(client, &years, re_log(&mut log))?);
    let seasons: Vec<i64> = match seasons {
        Some(s) if !s.is_empty() => s.to_vec(),
        _ => years.iter().map(|y| sid(*y) + 10001).filter(|s| !table.season_rows(*s).is_empty()).collect(),
    };
    let mut rows: Vec<ProjRow> = Vec::new();
    for sc in SCORINGS {
        let scoring = scorer(&PRESETS[sc])?;
        rows.extend(evaluate_projections(&table, &xg, &seasons, sc, &scoring, &k_grid()));
        if let (Some(l), Some(h)) = (log.as_mut(), headline(&rows, sc, "fm_current", K_GOALS, "skaters", false)) {
            l(&format!(
                "{sc}: fm_current MAE {} -> {} with xG k=15 ({}%), Spearman {}",
                fixed(h.mae_base, 4, false),
                fixed(h.mae_xg, 4, false),
                fixed(h.d_mae_pct, 2, true),
                fixed(h.d_spearman, 4, true)
            ));
        }
    }
    let sh = shooting_summary(&shooting_pairs(&xg, MIN_SOG));
    let ast = luck_summary(&table, &xg, MIN_GP_ASSISTS);
    let norms = season_norms(&xg);
    let runtime = t0.elapsed().as_secs_f64();
    let headlines: Map<String, Option<Headline>> = SCORINGS
        .iter()
        .map(|sc| (sc.to_string(), headline(&rows, sc, "fm_current", K_GOALS, "skaters", false)))
        .collect();
    let mut path = None;
    if write {
        let p = backtest_dir(data_dir)?.join(format!("xg-report-{}.md", today.format("%Y-%m-%d")));
        let counts: Map<i64, usize> = years.iter().map(|y| (*y, xg[&sid(*y)].all.len())).collect();
        std::fs::write(&p, render_report(&rows, &sh, &ast, &norms, &seasons, &years, &counts, runtime, today))?;
        path = Some(p);
    }
    Ok((path, XgResults { rows, shooting: sh, assists: ast, norms, seasons, headline: headlines, runtime }))
}
