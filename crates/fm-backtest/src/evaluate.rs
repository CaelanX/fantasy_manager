//! Rust port of `fantasy_manager/backtest/evaluate.py` (the scoring-statistics half).
//!
//! Owner: work package `W2-harness-ledger-stats`.
//!
//! Ported here: the metrics (`rankdata`, `pearson`, `spearman`, `top_n_precision`, `metrics`),
//! the result rows, `Prediction`, the pooling helper, and the summaries (`summarize`,
//! `head_to_head`) that `report.rs` consumes.
//!
//! W4-backtest-models finished the file: `preseason_predictions`, `evaluate_preseason`,
//! `inseason_observations`, `evaluate_inseason`. `evaluate_preseason` / `evaluate_inseason` run
//! their per-season fits on rayon and keep the Python result order.
//!
//! NaN: Python's `float("nan")` metrics stay `f64::NAN` here (they are `Option`-free fields);
//! `ResultRow::to_dict` turns them into JSON `null` (the port-wide NaN rule).

use anyhow::Result;
use fm_core::models::{Split, StatLine};
use fm_core::Map;
use fm_engine::analysis::team_factors::{add_goalie_line, shrunk_goalie_team_rates, GoalieTotals};
use fm_engine::scoring::PointsScoring;
use fm_engine::valuation::blend::{blend_recency, shrink_inseason, shrink_k, VOLUME_STATS};
use fm_engine::valuation::params::k_inseason_volume;
use fm_engine::valuation::regression::apply_team_goalie_rates;
use fm_pycompat::num::py_sum;
use indexmap::IndexMap;
use rayon::prelude::*;
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;

use crate::data::{prev_season, season_label, SeasonTable, WindowLine, Windows};
use crate::fit::{
    fit_inseason, fit_k_toi, fit_preseason, inseason_projection, inseason_projection_split, minutes_projection, InseasonFit,
    InseasonObs,
};
use crate::models::{app_baseline_rates, get_models, group_means, make_context, prior_lines, prior_rows, Model};
use crate::scoring::fpg;

pub const MIN_GP: i64 = 20;
pub const MIN_GP_REST: i64 = 10;
pub const TOP_N: [usize; 2] = [50, 100];
pub const INSEASON_MODELS: [&str; 7] =
    ["preseason", "to_date", "fm_shrink_only", "fm_current", "fm_fitted", "fm_fitted_stat", "fm_minutes"];

// --------------------------------------------------------------------------- metrics

/// Python's `sorted(..., key=...)` on floats (NaN compares equal, keeping input order).
fn cmp_keys(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// Ranks starting at 1, ties get their average rank.
pub fn rankdata(xs: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..xs.len()).collect();
    order.sort_by(|&a, &b| cmp_keys(xs[a], xs[b]));
    let mut ranks = vec![0.0; xs.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j + 1 < order.len() && xs[order[j + 1]] == xs[order[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for t in i..=j {
            ranks[order[t]] = avg;
        }
        i = j + 1;
    }
    ranks
}

pub fn pearson(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len();
    if n < 2 {
        return f64::NAN;
    }
    let nf = n as f64;
    let (mx, my) = (py_sum(x.iter().copied()) / nf, py_sum(y.iter().copied()) / nf);
    let sxy = py_sum(x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)));
    let sxx = py_sum(x.iter().map(|a| (a - mx).powf(2.0)));
    let syy = py_sum(y.iter().map(|b| (b - my).powf(2.0)));
    if sxx > 0.0 && syy > 0.0 {
        sxy / (sxx * syy).sqrt()
    } else {
        f64::NAN
    }
}

pub fn spearman(x: &[f64], y: &[f64]) -> f64 {
    pearson(&rankdata(x), &rankdata(y))
}

/// |projected top n  intersect  actual top n| / n (None if fewer than n players).
pub fn top_n_precision(pred: &[f64], actual: &[f64], n: usize) -> Option<f64> {
    if pred.len() < n {
        return None;
    }
    let top = |v: &[f64]| -> std::collections::HashSet<usize> {
        let mut idx: Vec<usize> = (0..pred.len()).collect();
        idx.sort_by(|&a, &b| cmp_keys(-v[a], -v[b]));
        idx.into_iter().take(n).collect()
    };
    let (tp, ta) = (top(pred), top(actual));
    Some(tp.intersection(&ta).count() as f64 / n as f64)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Metrics {
    pub n: usize,
    pub mae: f64,
    pub rmse: f64,
    pub bias: f64,
    pub spearman: f64,
    pub top50: Option<f64>,
    pub top100: Option<f64>,
}

pub fn metrics(pred: &[f64], actual: &[f64], top: bool) -> Metrics {
    let n = pred.len();
    if n == 0 {
        let nan = f64::NAN;
        return Metrics { n: 0, mae: nan, rmse: nan, bias: nan, spearman: nan, top50: None, top100: None };
    }
    let nf = n as f64;
    let err: Vec<f64> = pred.iter().zip(actual).map(|(p, a)| p - a).collect();
    Metrics {
        n,
        mae: py_sum(err.iter().map(|e| e.abs())) / nf,
        rmse: (py_sum(err.iter().map(|e| e * e)) / nf).sqrt(),
        bias: py_sum(err.iter().copied()) / nf,
        spearman: spearman(pred, actual),
        top50: if top { top_n_precision(pred, actual, 50) } else { None },
        top100: if top { top_n_precision(pred, actual, 100) } else { None },
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResultRow {
    /// preseason / inseason
    pub kind: String,
    pub season: i64,
    pub checkpoint: Option<String>,
    pub model: String,
    /// skaters / goalies
    pub pool: String,
    pub n: usize,
    pub mae: f64,
    pub rmse: f64,
    pub bias: f64,
    pub spearman: f64,
    pub top50: Option<f64>,
    pub top100: Option<f64>,
}

impl ResultRow {
    /// `asdict(row)` (field order of the dataclass; NaN becomes null).
    pub fn to_dict(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m.into_iter().collect(),
            _ => Map::new(),
        }
    }

    /// `getattr(row, metric)` for the float metrics (`None` for an unset top-N).
    pub fn metric(&self, name: &str) -> Option<f64> {
        match name {
            "mae" => Some(self.mae),
            "rmse" => Some(self.rmse),
            "bias" => Some(self.bias),
            "spearman" => Some(self.spearman),
            "top50" => self.top50,
            "top100" => self.top100,
            _ => None,
        }
    }
}

/// `_row`.
pub fn row(kind: &str, season: i64, checkpoint: Option<&str>, model: &str, pool: &str, m: &Metrics) -> ResultRow {
    ResultRow {
        kind: kind.to_string(),
        season,
        checkpoint: checkpoint.map(|s| s.to_string()),
        model: model.to_string(),
        pool: pool.to_string(),
        n: m.n,
        mae: m.mae,
        rmse: m.rmse,
        bias: m.bias,
        spearman: m.spearman,
        top50: m.top50,
        top100: m.top100,
    }
}

// --------------------------------------------------------------------------- preseason

#[derive(Clone, Debug, PartialEq)]
pub struct Prediction {
    pub season: i64,
    pub player_id: i64,
    pub name: String,
    pub group: String,
    pub model: String,
    pub projected: f64,
    pub actual: f64,
}

/// `_pools`, already sorted by `(model, pool)` as `evaluate_preseason` iterates it; the
/// predictions inside keep their input order.
pub fn pools<'a>(preds: &'a [Prediction]) -> Vec<((String, String), Vec<&'a Prediction>)> {
    let mut out: IndexMap<(String, String), Vec<&Prediction>> = IndexMap::new();
    for p in preds {
        let pool = if p.group == "G" { "goalies" } else { "skaters" };
        out.entry((p.model.clone(), pool.to_string())).or_default().push(p);
    }
    let mut v: Vec<_> = out.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

pub fn preseason_predictions(
    table: &SeasonTable,
    scoring: &PointsScoring,
    season: i64,
    models: &[Box<dyn Model>],
    min_gp_prev: i64,
    min_gp: i64,
) -> Vec<Prediction> {
    let ctx = make_context(table, season, scoring, None);
    let prev = prev_season(season);
    // players are independent: evaluate them on rayon, concatenated in the table's order
    let per_player: Vec<Vec<Prediction>> = table
        .season_rows(season)
        .par_iter()
        .map(|row| {
            let mut out = Vec::new();
            let pid = row.player_id;
            let Some(last) = table.get(pid, prev) else { return out };
            if row.gp < min_gp || last.gp < min_gp_prev || last.group != row.group {
                return out;
            }
            let history = table.history(pid, season);
            let age = row.age.or_else(|| table.age(pid, season));
            let actual = fpg(Some(&row.per_game()), scoring);
            for m in models {
                let Some(r) = m.rates(&history, age, &ctx) else { continue };
                out.push(Prediction {
                    season,
                    player_id: pid,
                    name: row.name.clone(),
                    group: row.group.clone(),
                    model: m.name().to_string(),
                    projected: fpg(Some(&r), scoring),
                    actual,
                });
            }
            out
        })
        .collect();
    per_player.into_iter().flatten().collect()
}

/// Rows per (season, model, pool) plus every prediction. `fm_fitted` / `fm_multi` are re-fitted
/// for every N on pairs ending at N-1 (rolling origin, no look-ahead).
pub fn evaluate_preseason(
    table: &SeasonTable,
    scoring: &PointsScoring,
    seasons: &[i64],
    model_names: Option<&[String]>,
    min_gp_prev: i64,
    min_gp: i64,
    log: Option<&mut dyn FnMut(&str)>,
) -> Result<(Vec<ResultRow>, Vec<Prediction>)> {
    let table_seasons = table.seasons();
    let first = table_seasons.iter().copied().min().unwrap_or(0);
    let age_groups = ["F".to_string(), "D".to_string()];
    let per_season: Vec<Result<(i64, Vec<ResultRow>, Vec<Prediction>)>> = seasons
        .par_iter()
        .map(|&n| {
            let mut models = get_models(model_names)?;
            if models.iter().any(|m| m.uses_params()) {
                let train: Vec<i64> = table_seasons
                    .iter()
                    .copied()
                    .filter(|&s| prev_season(s) >= first && s <= prev_season(n))
                    .collect();
                let (params, _) = fit_preseason(table, scoring, &train, 0, &age_groups);
                for m in models.iter_mut() {
                    if m.uses_params() {
                        m.set_params(&params);
                    }
                }
            }
            let preds = preseason_predictions(table, scoring, n, &models, min_gp_prev, min_gp);
            let mut rows = Vec::new();
            for ((model, pool), ps) in pools(&preds) {
                let pred: Vec<f64> = ps.iter().map(|p| p.projected).collect();
                let act: Vec<f64> = ps.iter().map(|p| p.actual).collect();
                rows.push(row("preseason", n, None, &model, &pool, &metrics(&pred, &act, pool == "skaters")));
            }
            Ok((n, rows, preds))
        })
        .collect();
    let mut rows = Vec::new();
    let mut all_preds = Vec::new();
    let mut log = log;
    for r in per_season {
        let (n, rs, preds) = r?;
        if let Some(l) = log.as_mut() {
            let players: std::collections::HashSet<i64> = preds.iter().map(|p| p.player_id).collect();
            l(&format!("{}: {} players projected", season_label(n), players.len()));
        }
        rows.extend(rs);
        all_preds.extend(preds);
    }
    Ok((rows, all_preds))
}

// --------------------------------------------------------------------------- in-season

/// One observation per (checkpoint, player) with the app's own in-season FPG attached. Unlike the
/// Python, a goalie's app rates take wins and shots against per start from his team's goalies to date.
pub fn inseason_observations(
    table: &SeasonTable,
    windows: &Windows,
    scoring: &PointsScoring,
    seasons: &[i64],
    min_gp_rest: i64,
) -> Vec<InseasonObs> {
    let mut out = Vec::new();
    let empty: Map<String, Map<i64, WindowLine>> = Map::new();
    let no_rows: Map<i64, WindowLine> = Map::new();
    for &s in seasons {
        let prev = prev_season(s);
        let means = group_means(table, prev);
        let Some(days) = windows.get(&s) else { continue };
        let mut days: Vec<_> = days.iter().collect();
        days.sort_by(|a, b| a.0.cmp(b.0));
        for (day, wins) in days {
            let _ = &empty;
            let td = wins.get("to_date").unwrap_or(&no_rows);
            let rest = wins.get("rest").unwrap_or(&no_rows);
            // the app's team goalie rates: this season's goalie lines by team (the season row's team)
            let mut totals = GoalieTotals::new();
            for (pid, cur) in td {
                if let Some(team) = table.get(*pid, s).filter(|r| r.is_goalie()).and_then(|r| r.team.clone()) {
                    add_goalie_line(&mut totals, team, &cur.stats, cur.gp);
                }
            }
            let team_rates = shrunk_goalie_team_rates(&totals);
            for (pid, cur) in td {
                let Some(after) = rest.get(pid) else { continue };
                if after.gp < min_gp_rest || cur.gp < 1 {
                    continue;
                }
                let Some(info) = table.get(*pid, s).or_else(|| table.get(*pid, prev)) else { continue };
                let group = info.group.as_str();
                let history = table.history(*pid, s);
                let rows = prior_rows(&history, s, 3);
                let latest = rows.iter().find_map(|r| *r);
                let (base, has_base) = match latest {
                    Some(l) if l.group == group => {
                        // the app's preseason baseline: seasons N-1..N-3, fitted k, age factor
                        let lines = prior_lines(&rows);
                        let base = app_baseline_rates(l, &lines, &means, table.age(*pid, s));
                        let hb = !base.is_empty();
                        (base, hb)
                    }
                    // the app borrows the positional mean for a player with games but no baseline
                    _ => {
                        let m = means.get(group).cloned().unwrap_or_default();
                        let hb = !m.is_empty();
                        (m, hb)
                    }
                };
                let is_goalie = group == "G";
                let cur_rates = cur.per_game();
                let shrunk = if has_base {
                    shrink_inseason(&cur_rates, cur.gp as f64, &base, is_goalie, None)
                } else {
                    cur_rates.clone()
                };
                let lines: Vec<(&str, Option<&WindowLine>)> = ["last30", "last15", "last7"]
                    .iter()
                    .map(|w| (*w, wins.get(*w).and_then(|m| m.get(pid))))
                    .collect();
                let sl: Vec<Option<StatLine>> = lines
                    .iter()
                    .zip([Split::Last30, Split::Last15, Split::Last7])
                    .map(|((_, ln), sp)| ln.map(|l| l.statline(sp)))
                    .collect();
                let mut app_rates = blend_recency(&shrunk, sl[0].as_ref(), sl[1].as_ref(), sl[2].as_ref(), None);
                // the skater's minutes per game to date and in the baseline (see `minutes_projection`)
                let toi = if is_goalie || !has_base {
                    None
                } else {
                    cur_rates.get("TOI").zip(base.get("TOI")).map(|(c, b)| (*c, *b))
                };
                let team = if is_goalie { table.get(*pid, s).and_then(|r| r.team.as_ref()) } else { None };
                if let Some(t) = team.and_then(|t| team_rates.get(t)) {
                    app_rates = apply_team_goalie_rates(&app_rates, t.wins_per_start, t.shots_per_start);
                }
                let splits = lines
                    .iter()
                    .map(|(w, ln)| {
                        let v = match ln {
                            Some(l) => (l.gp, fpg(Some(&l.per_game()), scoring)),
                            None => (0, 0.0),
                        };
                        (w.to_string(), v)
                    })
                    .collect();
                out.push(InseasonObs {
                    season: s,
                    day: day.clone(),
                    player_id: *pid,
                    group: group.to_string(),
                    gp_td: cur.gp,
                    fpg_td: fpg(Some(&cur_rates), scoring),
                    fpg_base: fpg(Some(&base), scoring),
                    vol_points: volume_points(&cur_rates, &base, scoring),
                    has_base,
                    splits,
                    gp_rest: after.gp,
                    fpg_rest: fpg(Some(&after.per_game()), scoring),
                    fpg_app: fpg(Some(&app_rates), scoring),
                    toi,
                    ..Default::default()
                });
            }
        }
    }
    out
}

/// Volume stat -> (points per game it scores in `cur`, in `base`), for the stats the scoring pays.
fn volume_points(cur: &Map<String, f64>, base: &Map<String, f64>, scoring: &PointsScoring) -> Map<String, (f64, f64)> {
    let pts = |rates: &Map<String, f64>, stat: &str| -> f64 {
        let one: Map<String, f64> = rates.get(stat).map(|v| (stat.to_string(), *v)).into_iter().collect();
        fpg(Some(&one), scoring)
    };
    VOLUME_STATS
        .iter()
        .map(|s| (s.to_string(), (pts(cur, s), pts(base, s))))
        .filter(|(_, (c, b))| *c != 0.0 || *b != 0.0)
        .collect()
}

/// Rows per (season, checkpoint, model, pool); fm_fitted (one skater k), fm_fitted_stat (a
/// separate k for the volume stats) and fm_minutes (a separate k for minutes) are fitted
/// leave-one-season-out.
pub fn evaluate_inseason(obs: &[InseasonObs], cv: bool) -> (Vec<ResultRow>, Map<i64, InseasonFit>) {
    let mut seasons: Vec<i64> = obs.iter().map(|o| o.season).collect();
    seasons.sort();
    seasons.dedup();
    let fitted: Vec<(i64, InseasonFit)> = seasons
        .par_iter()
        .map(|&s| {
            let train: Vec<InseasonObs> = if cv && seasons.len() > 1 {
                obs.iter().filter(|o| o.season != s).cloned().collect()
            } else {
                obs.to_vec()
            };
            (s, fit_inseason(&train, 2))
        })
        .collect();
    let fits: Map<i64, InseasonFit> = fitted.into_iter().collect();
    // the minutes k of fm_minutes, fitted on the same training seasons
    let k_app = shrink_k(false, None);
    let k_toi: Map<i64, f64> = seasons
        .iter()
        .map(|&s| {
            let train: Vec<&InseasonObs> =
                obs.iter().filter(|o| o.group != "G" && (!cv || seasons.len() <= 1 || o.season != s)).collect();
            (s, fit_k_toi(&train, k_app))
        })
        .collect();
    let mut groups: std::collections::BTreeMap<(i64, String, String), Vec<&InseasonObs>> = Default::default();
    for o in obs {
        let pool = if o.group == "G" { "goalies" } else { "skaters" };
        groups.entry((o.season, o.day.clone(), pool.to_string())).or_default().push(o);
    }
    let so: Map<String, f64> = [("season", 1.0), ("last30", 0.0), ("last15", 0.0), ("last7", 0.0)]
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
    let no_split: Map<String, f64> = Map::new();
    let app_k_volume = k_inseason_volume(None);
    let mut rows = Vec::new();
    for ((s, day, pool), os) in groups {
        let f = &fits[&s];
        let goalies = pool == "goalies";
        let k_fit = if goalies { f.k_goalie } else { f.k_skater };
        let k_shrink = shrink_k(goalies, None);
        let app_volume = if goalies { &no_split } else { &app_k_volume };
        let k_volume = if goalies { &no_split } else { &f.k_skater_volume };
        let preds: [(&str, Vec<f64>); 7] = [
            ("preseason", os.iter().map(|o| if o.has_base { o.fpg_base } else { o.fpg_td }).collect()),
            ("to_date", os.iter().map(|o| o.fpg_td).collect()),
            ("fm_shrink_only", os.iter().map(|o| inseason_projection_split(o, k_shrink, app_volume, &so)).collect()),
            ("fm_current", os.iter().map(|o| o.fpg_app).collect()),
            ("fm_fitted", os.iter().map(|o| inseason_projection(o, k_fit, &f.recency_weights)).collect()),
            (
                "fm_fitted_stat",
                os.iter().map(|o| inseason_projection_split(o, k_fit, k_volume, &f.recency_weights)).collect(),
            ),
            ("fm_minutes", os.iter().map(|o| minutes_projection(o, k_toi[&s], k_app)).collect()),
        ];
        let actual: Vec<f64> = os.iter().map(|o| o.fpg_rest).collect();
        for model in INSEASON_MODELS {
            let p = &preds.iter().find(|(n, _)| *n == model).expect("model").1;
            rows.push(row("inseason", s, Some(&day), model, &pool, &metrics(p, &actual, !goalies)));
        }
    }
    (rows, fits)
}

// --------------------------------------------------------------------------- summaries

/// One model's entry of `summarize`: mean of each metric over seasons (and checkpoints).
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub mae: Option<f64>,
    pub rmse: Option<f64>,
    pub bias: Option<f64>,
    pub spearman: Option<f64>,
    pub top50: Option<f64>,
    pub top100: Option<f64>,
    pub n: i64,
    pub seasons: usize,
}

impl Summary {
    /// `s[name]` for the metric keys.
    pub fn get(&self, name: &str) -> Option<f64> {
        match name {
            "mae" => self.mae,
            "rmse" => self.rmse,
            "bias" => self.bias,
            "spearman" => self.spearman,
            "top50" => self.top50,
            "top100" => self.top100,
            _ => None,
        }
    }
}

/// model -> mean of each metric over seasons (and checkpoints).
pub fn summarize(rows: &[ResultRow], kind: &str, pool: &str) -> IndexMap<String, Summary> {
    let mut by: IndexMap<&str, Vec<&ResultRow>> = IndexMap::new();
    for r in rows {
        if r.kind == kind && r.pool == pool {
            by.entry(r.model.as_str()).or_default().push(r);
        }
    }
    let mut out = IndexMap::new();
    for (model, rs) in by {
        let avg = |attr: &str| -> Option<f64> {
            let vals: Vec<f64> =
                rs.iter().filter_map(|r| r.metric(attr)).filter(|v| !v.is_nan()).collect();
            if vals.is_empty() {
                None
            } else {
                Some(py_sum(vals.iter().copied()) / vals.len() as f64)
            }
        };
        out.insert(
            model.to_string(),
            Summary {
                mae: avg("mae"),
                rmse: avg("rmse"),
                bias: avg("bias"),
                spearman: avg("spearman"),
                top50: avg("top50"),
                top100: avg("top100"),
                n: rs.iter().map(|r| r.n as i64).sum(),
                seasons: rs.len(),
            },
        );
    }
    out
}

/// (seasons where `a` is better than `b`, seasons compared) on `metric` (lower MAE/RMSE, higher
/// Spearman/top-N is better).
pub fn head_to_head(rows: &[ResultRow], a: &str, b: &str, kind: &str, pool: &str, metric: &str) -> (i64, i64) {
    let lower = metric == "mae" || metric == "rmse";
    let mut idx: IndexMap<(i64, Option<String>), IndexMap<String, Option<f64>>> = IndexMap::new();
    for r in rows {
        if r.kind == kind && r.pool == pool && (r.model == a || r.model == b) {
            idx.entry((r.season, r.checkpoint.clone())).or_default().insert(r.model.clone(), r.metric(metric));
        }
    }
    let (mut wins, mut total) = (0, 0);
    for v in idx.values() {
        if let (Some(Some(va)), Some(Some(vb))) = (v.get(a), v.get(b)) {
            total += 1;
            let better = if lower { va < vb } else { va > vb };
            if better {
                wins += 1;
            }
        }
    }
    (wins, total)
}
