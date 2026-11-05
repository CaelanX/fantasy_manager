//! Rust port of `fantasy_manager/backtest/fit.py`: fit the valuation constants from history.
//! Owner: work package `W4-backtest-models`.
//!
//! * `fit_shrinkage_k`  - preseason k per position group, grid search minimizing MAE.
//! * `fit_age_curve`    - delta method with a player-clustered bootstrap
//!   (`random.Random(7).choices` stream reproduced draw for draw by `PyRandom`).
//! * `fit_inseason`     - in-season k and recency weights, coordinate descent.
//!
//! Deviations from the Python:
//! * The bootstrap draws are generated sequentially (the RNG stream is order dependent) and the
//!   resampled level computations then run on rayon; the per-age samples are read back in draw
//!   order, so the result is identical.
//! * `dict[float, float]` MAE curves are `Vec<(k, mae)>` in grid order ([`Curve`]).
//! * `InseasonObs.splits` is an insertion-ordered map keyed `last30` / `last15` / `last7`.

use fm_core::Map;
use fm_engine::scoring::PointsScoring;
use fm_engine::valuation::blend::{
    k_baseline_snapshot, k_inseason_snapshot, recency_weights_snapshot, EXPECTED_GAMES,
};
use fm_pycompat::num::{median, py_sum, round_to};
use fm_pycompat::random::PyRandom;
use rayon::prelude::*;
use serde_json::{json, Value};

use crate::data::{prev_season, SeasonTable};
use crate::models::{group_means, weighted_rates, FittedParams};
use crate::scoring::fpg;

pub const GROUPS: [&str; 3] = ["F", "D", "G"];
pub const MIN_GP: i64 = 20;
/// ratios off a tiny base explode
pub const MIN_FPG_FOR_RATIO: f64 = 0.25;
pub const AGE_MIN: i64 = 18;
pub const AGE_MAX: i64 = 40;
/// pool ratios over age +- 1
pub const AGE_WINDOW: i64 = 1;
/// fewer pooled pairs than this -> flat (ratio 1.0)
pub const AGE_MIN_POOLED: usize = 25;
pub const YOY_CLAMP: (f64, f64) = (0.75, 1.30);
pub const REPORT_AGES: [i64; 7] = [19, 21, 23, 25, 28, 31, 34];
pub const INSEASON_K_GRID: [i64; 16] = [0, 3, 5, 8, 10, 12, 15, 20, 25, 30, 40, 50, 60, 80, 100, 150];

/// `{k: MAE}` in grid order.
pub type Curve = Vec<(f64, f64)>;

/// `K_GRID = range(0, 101, 2) + (110, 120, 140, 160, 200, 250, 300)`.
pub fn k_grid() -> Vec<f64> {
    (0..=100).step_by(2).chain([110, 120, 140, 160, 200, 250, 300]).map(|k| k as f64).collect()
}

/// `curve.get(k)`.
pub fn curve_get(curve: &Curve, k: f64) -> Option<f64> {
    curve.iter().find(|(kk, _)| *kk == k).map(|(_, v)| *v)
}

// --------------------------------------------------------------------------- season pairs

#[derive(Clone, Debug, PartialEq)]
pub struct Pair {
    pub player_id: i64,
    /// target season N (the pair is N-1 -> N)
    pub season: i64,
    pub group: String,
    pub age_prev: Option<f64>,
    pub gp_prev: i64,
    pub fpg_prev: f64,
    /// positional-mean FPG of season N-1 (the app's shrink target)
    pub fpg_mean: f64,
    pub gp_next: i64,
    pub fpg_next: f64,
    /// 5/4/3 GP-weighted FPG over seasons N-1..N-3
    pub fpg_multi: f64,
    /// its effective games (units of season N-1)
    pub n_multi: f64,
}

impl Pair {
    /// `Pair(pid, season, group, age_prev, gp, fpg_prev, mean, gp, fpg_next)` (multi fields 0).
    pub fn simple(
        player_id: i64,
        season: i64,
        group: &str,
        age_prev: Option<f64>,
        gp: i64,
        fpg_prev: f64,
        fpg_mean: f64,
        fpg_next: f64,
    ) -> Pair {
        Pair {
            player_id,
            season,
            group: group.to_string(),
            age_prev,
            gp_prev: gp,
            fpg_prev,
            fpg_mean,
            gp_next: gp,
            fpg_next,
            fpg_multi: 0.0,
            n_multi: 0.0,
        }
    }
}

/// (N-1 -> N) pairs for every target season N in `seasons`.
pub fn season_pairs(
    table: &SeasonTable,
    scoring: &PointsScoring,
    seasons: &[i64],
    min_gp_prev: i64,
    min_gp_next: i64,
) -> Vec<Pair> {
    let mut out = Vec::new();
    for &n in seasons {
        let prev = prev_season(n);
        let means: Map<String, f64> =
            group_means(table, prev).iter().map(|(g, m)| (g.clone(), fpg(Some(m), scoring))).collect();
        for a in table.season_rows(prev) {
            let Some(b) = table.get(a.player_id, n) else { continue };
            if a.gp < min_gp_prev || b.gp < min_gp_next || a.group != b.group {
                continue;
            }
            let w = weighted_rates(&table.history(a.player_id, n), n, &crate::models::MARCEL_WEIGHTS);
            out.push(Pair {
                player_id: a.player_id,
                season: n,
                group: a.group.clone(),
                age_prev: a.age.or_else(|| table.age(a.player_id, prev)),
                gp_prev: a.gp,
                fpg_prev: fpg(Some(&a.per_game()), scoring),
                fpg_mean: means.get(&a.group).copied().unwrap_or(0.0),
                gp_next: b.gp,
                fpg_next: fpg(Some(&b.per_game()), scoring),
                fpg_multi: w.as_ref().map(|w| fpg(Some(&w.0), scoring)).unwrap_or(0.0),
                n_multi: w.as_ref().map(|w| w.1).unwrap_or(0.0),
            });
        }
    }
    out
}

// --------------------------------------------------------------------------- shrinkage k

pub fn shrink_projection(p: &Pair, k: f64, age_factor: f64) -> f64 {
    (p.gp_prev as f64 * p.fpg_prev + k * p.fpg_mean) / (p.gp_prev as f64 + k) * age_factor
}

pub fn multi_projection(p: &Pair, k: f64, age_factor: f64) -> f64 {
    (p.n_multi * p.fpg_multi + k * p.fpg_mean) / (p.n_multi + k) * age_factor
}

/// Mean absolute error (NaN for no errors).
pub fn mae(errors: &[f64]) -> f64 {
    if errors.is_empty() {
        f64::NAN
    } else {
        py_sum(errors.iter().map(|e| e.abs())) / errors.len() as f64
    }
}

pub type AgeFactorFn<'a> = &'a (dyn Fn(&Pair) -> f64 + Sync);
pub type ProjectionFn = fn(&Pair, f64, f64) -> f64;

/// (best k, {k: MAE}) for projecting fpg_next from the shrunk prior season (or, with
/// `projection = multi_projection`, from the 5/4/3-weighted seasons). NaN / empty without pairs.
pub fn fit_shrinkage_k(
    pairs: &[Pair],
    grid: &[f64],
    age_factor: Option<AgeFactorFn>,
    projection: ProjectionFn,
) -> (f64, Curve) {
    if pairs.is_empty() {
        return (f64::NAN, Vec::new());
    }
    let factors: Vec<f64> = pairs.iter().map(|p| age_factor.map(|f| f(p)).unwrap_or(1.0)).collect();
    let curve: Curve = grid
        .par_iter()
        .map(|&k| {
            let errs: Vec<f64> =
                pairs.iter().zip(&factors).map(|(p, f)| projection(p, k, *f) - p.fpg_next).collect();
            (k, mae(&errs))
        })
        .collect();
    // min(curve, key=lambda k: (round(curve[k], 9), k))
    let mut best: Option<(f64, f64)> = None;
    for &(k, v) in &curve {
        let key = (round_to(v, 9), k);
        let better = match best {
            None => true,
            Some(b) => key.0 < b.0 || (key.0 == b.0 && key.1 < b.1),
        };
        if better {
            best = Some(key);
        }
    }
    (best.map(|b| b.1).unwrap_or(f64::NAN), curve)
}

/// `fit_shrinkage_k(pairs)` with the defaults (K_GRID, no aging, single-season projection).
pub fn fit_shrinkage_k_default(pairs: &[Pair]) -> (f64, Curve) {
    fit_shrinkage_k(pairs, &k_grid(), None, shrink_projection)
}

// --------------------------------------------------------------------------- age curve

#[derive(Clone, Debug, PartialEq)]
pub struct AgeCurve {
    pub group: String,
    /// expected FPG ratio age a -> a+1
    pub yoy: Map<i64, f64>,
    /// trajectory, peak = 1.0
    pub level: Map<i64, f64>,
    /// pairs whose prev-season age is exactly a
    pub n: Map<i64, i64>,
    /// bootstrap 5th pct of level
    pub lo: Map<i64, f64>,
    /// bootstrap 95th pct of level
    pub hi: Map<i64, f64>,
}

impl AgeCurve {
    /// `max(level, key=level.get)`: the first age with the highest level.
    pub fn peak_age(&self) -> i64 {
        let mut best: Option<(i64, f64)> = None;
        for (&a, &v) in &self.level {
            if best.is_none_or(|(_, bv)| v > bv) {
                best = Some((a, v));
            }
        }
        best.map(|b| b.0).unwrap_or(0)
    }

    pub fn to_dict(&self) -> Value {
        let mut ages: Vec<i64> = self.level.keys().copied().collect();
        ages.sort();
        let mut m = serde_json::Map::new();
        for a in ages {
            m.insert(
                a.to_string(),
                json!({
                    "level": round_to(self.level[&a], 4),
                    "yoy": round_to(self.yoy.get(&a).copied().unwrap_or(1.0), 4),
                    "n": self.n.get(&a).copied().unwrap_or(0),
                    "lo": self.lo.get(&a).map(|v| round_to(*v, 4)),
                    "hi": self.hi.get(&a).map(|v| round_to(*v, 4)),
                }),
            );
        }
        json!({"peak_age": self.peak_age(), "ages": Value::Object(m)})
    }
}

fn base_of(p: &Pair, k: Option<f64>) -> f64 {
    match k {
        Some(k) if k != 0.0 => shrink_projection(p, k, 1.0),
        _ => p.fpg_prev,
    }
}

fn yoy_levels(
    pairs: &[&Pair],
    ages: &[i64],
    window: i64,
    min_pooled: usize,
    k: Option<f64>,
) -> (Map<i64, f64>, Map<i64, f64>) {
    let mut by_age: std::collections::HashMap<i64, Vec<f64>> = std::collections::HashMap::new();
    for p in pairs {
        let base = base_of(p, k);
        let Some(age) = p.age_prev else { continue };
        if base < MIN_FPG_FOR_RATIO {
            continue;
        }
        by_age.entry(age.floor() as i64).or_default().push(p.fpg_next / base);
    }
    let mut yoy: Map<i64, f64> = Map::new();
    for &a in ages {
        let mut pooled: Vec<f64> = Vec::new();
        for b in (a - window)..=(a + window) {
            if let Some(v) = by_age.get(&b) {
                pooled.extend_from_slice(v);
            }
        }
        let r = if pooled.len() >= min_pooled { median(&pooled) } else { 1.0 };
        yoy.insert(a, r.max(YOY_CLAMP.0).min(YOY_CLAMP.1));
    }
    let mut level: Map<i64, f64> = Map::new();
    level.insert(ages[0], 1.0);
    for &a in &ages[..ages.len() - 1] {
        let next = level[&a] * yoy[&a];
        level.insert(a + 1, next);
    }
    let peak = level.values().copied().fold(f64::NEG_INFINITY, f64::max);
    let level = level.into_iter().map(|(a, v)| (a, v / peak)).collect();
    (yoy, level)
}

/// Options of [`fit_age_curve`] (Python keyword arguments).
#[derive(Clone, Debug)]
pub struct AgeCurveOpts {
    pub boot: usize,
    pub seed: i64,
    pub ages: Vec<i64>,
    pub window: i64,
    pub min_pooled: usize,
    /// shrink the base season toward the mean (None: raw FPG)
    pub k: Option<f64>,
}

impl Default for AgeCurveOpts {
    fn default() -> Self {
        AgeCurveOpts {
            boot: 200,
            seed: 7,
            ages: (AGE_MIN..=AGE_MAX).collect(),
            window: AGE_WINDOW,
            min_pooled: AGE_MIN_POOLED,
            k: None,
        }
    }
}

/// Age curve of `group`.
pub fn fit_age_curve(pairs: &[Pair], group: &str, opts: &AgeCurveOpts) -> AgeCurve {
    let ages = &opts.ages;
    let gp: Vec<&Pair> = pairs.iter().filter(|p| p.group == group).collect();
    let (yoy, level) = yoy_levels(&gp, ages, opts.window, opts.min_pooled, opts.k);
    let mut n: Map<i64, i64> = Map::new();
    for p in &gp {
        if let Some(age) = p.age_prev {
            if base_of(p, opts.k) >= MIN_FPG_FOR_RATIO {
                *n.entry(age.floor() as i64).or_insert(0) += 1;
            }
        }
    }
    let mut curve = AgeCurve {
        group: group.to_string(),
        yoy,
        level,
        n: ages.iter().map(|a| (*a, n.get(a).copied().unwrap_or(0))).collect(),
        lo: Map::new(),
        hi: Map::new(),
    };
    if opts.boot > 0 && !gp.is_empty() {
        let mut rng = PyRandom::new(opts.seed);
        let mut by_player: Map<i64, Vec<&Pair>> = Map::new();
        for p in &gp {
            by_player.entry(p.player_id).or_default().push(p);
        }
        let groups: Vec<&Vec<&Pair>> = by_player.values().collect();
        // the RNG stream is sequential: draw every resample first
        let draws: Vec<Vec<usize>> = (0..opts.boot).map(|_| rng.choices_idx(groups.len(), groups.len())).collect();
        let levels: Vec<Vec<f64>> = draws
            .par_iter()
            .map(|draw| {
                let sample: Vec<&Pair> = draw.iter().flat_map(|&i| groups[i].iter().copied()).collect();
                let (_, lv) = yoy_levels(&sample, ages, opts.window, opts.min_pooled, opts.k);
                ages.iter().map(|a| lv[a]).collect()
            })
            .collect();
        for (ai, &a) in ages.iter().enumerate() {
            let mut s: Vec<f64> = levels.iter().map(|l| l[ai]).collect();
            s.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
            let m = (s.len() - 1) as f64;
            curve.lo.insert(a, s[(0.05 * m) as usize]);
            curve.hi.insert(a, s[(0.95 * m).ceil() as usize]);
        }
    }
    curve
}

/// `[[age, level], ...]` in the shape of `valuation.dynasty.AGE_CURVES` (peak = 1.0).
pub fn dynasty_anchors(curve: &AgeCurve, ages: &[i64]) -> Vec<(i64, f64)> {
    ages.iter().filter(|a| curve.level.contains_key(*a)).map(|a| (*a, round_to(curve.level[a], 3))).collect()
}

pub const DYNASTY_ANCHOR_AGES: [i64; 10] = [19, 21, 23, 25, 27, 29, 31, 33, 35, 37];

// --------------------------------------------------------------------------- preseason bundle

pub struct PreseasonInfo {
    pub pairs: usize,
    pub curves: Map<String, AgeCurve>,
    pub k_curves: Map<String, Curve>,
    pub k_multi_curves: Map<String, Curve>,
    pub k_skater_pooled: f64,
    pub k_curve_skater_pooled: Curve,
}

/// k and age curve per group from (N-1 -> N) pairs: k without aging, the age curve against the
/// k-shrunk base, k again on age-adjusted projections, and the final curve (+ bootstrap).
pub fn fit_preseason(
    table: &SeasonTable,
    scoring: &PointsScoring,
    target_seasons: &[i64],
    boot: usize,
    age_groups: &[String],
) -> (FittedParams, PreseasonInfo) {
    let pairs = season_pairs(table, scoring, target_seasons, MIN_GP, MIN_GP);
    let grid = k_grid();
    let by_group: Map<&str, Vec<Pair>> =
        GROUPS.iter().map(|g| (*g, pairs.iter().filter(|p| p.group == *g).cloned().collect())).collect();
    let kb = k_baseline_snapshot();
    let k0: Map<&str, f64> = GROUPS
        .iter()
        .map(|g| {
            let v = fit_shrinkage_k(&by_group[g], &grid, None, shrink_projection).0;
            (*g, if v.is_nan() { kb[*g] } else { v })
        })
        .collect();
    let curve_opts = |boot: usize, k: f64| AgeCurveOpts { boot, k: Some(k), ..Default::default() };
    let curves0: Map<&str, AgeCurve> =
        GROUPS.iter().map(|g| (*g, fit_age_curve(&pairs, g, &curve_opts(0, k0[g])))).collect();
    let mut params = FittedParams {
        k: Map::new(),
        age_yoy: curves0.iter().map(|(g, c)| (g.to_string(), c.yoy.clone())).collect(),
        age_groups: age_groups.to_vec(),
        ..FittedParams::default()
    };
    let mut k_curves: Map<String, Curve> = Map::new();
    for g in GROUPS {
        let (best, curve) = {
            let p = &params;
            let af = move |pair: &Pair| p.age_factor(g, pair.age_prev);
            fit_shrinkage_k(&by_group[g], &grid, Some(&af), shrink_projection)
        };
        params.k.insert(g.to_string(), if best.is_nan() { k0[g] } else { best });
        k_curves.insert(g.to_string(), curve);
    }
    let curves: Map<String, AgeCurve> = GROUPS
        .iter()
        .map(|g| (g.to_string(), fit_age_curve(&pairs, g, &curve_opts(boot, params.k[*g]))))
        .collect();
    params.age_yoy = curves.iter().map(|(g, c)| (g.clone(), c.yoy.clone())).collect();
    let mut k_multi_curves: Map<String, Curve> = Map::new();
    for g in GROUPS {
        let sub: Vec<Pair> = by_group[g].iter().filter(|p| p.n_multi > 0.0).cloned().collect();
        let (best, curve) = {
            let p = &params;
            let af = move |pair: &Pair| p.age_factor(g, pair.age_prev);
            fit_shrinkage_k(&sub, &grid, Some(&af), multi_projection)
        };
        let fallback = params.k_multi.get(g).copied().unwrap_or(10.0);
        params.k_multi.insert(g.to_string(), if best.is_nan() { fallback } else { best });
        k_multi_curves.insert(g.to_string(), curve);
    }
    // skaters pooled, for the app's single skater constant
    let sk: Vec<Pair> = pairs.iter().filter(|p| p.group == "F" || p.group == "D").cloned().collect();
    let (best_sk, curve_sk) = {
        let p = &params;
        let af = move |pair: &Pair| p.age_factor(&pair.group, pair.age_prev);
        fit_shrinkage_k(&sk, &grid, Some(&af), shrink_projection)
    };
    let info = PreseasonInfo {
        pairs: pairs.len(),
        curves,
        k_curves,
        k_multi_curves,
        k_skater_pooled: best_sk,
        k_curve_skater_pooled: curve_sk,
    };
    (params, info)
}

// --------------------------------------------------------------------------- in-season

pub const SPLITS: [&str; 3] = ["last30", "last15", "last7"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InseasonObs {
    pub season: i64,
    pub day: String,
    pub player_id: i64,
    pub group: String,
    pub gp_td: i64,
    pub fpg_td: f64,
    pub fpg_base: f64,
    pub has_base: bool,
    /// last30 / last15 / last7 -> (gp, fpg)
    pub splits: Map<String, (i64, f64)>,
    pub gp_rest: i64,
    pub fpg_rest: f64,
    /// the app's own pipeline (blend.py functions)
    pub fpg_app: f64,
    // live (harness replay) rows only; historical checkpoints have no league projection
    pub fpg_proj: Option<f64>,
    pub fpg_hist: Option<f64>,
    pub status: Option<String>,
    pub gp_hist3: Option<i64>,
    pub start_share: Option<f64>,
}

/// `blend.recency_weights` with arbitrary base weights (shortfall goes to 'season').
pub fn recency_weights_for(base: &Map<String, f64>, gps: &Map<String, i64>) -> Map<String, f64> {
    let mut w: Map<String, f64> = Map::new();
    let mut season = base["season"];
    w.insert("season".to_string(), season);
    for (split, exp) in EXPECTED_GAMES {
        let frac = (gps.get(split).copied().unwrap_or(0).max(0) as f64 / exp).min(1.0);
        let b = base.get(split).copied().unwrap_or(0.0);
        w.insert(split.to_string(), b * frac);
        season += b * (1.0 - frac);
        w.insert("season".to_string(), season);
    }
    w
}

pub fn inseason_projection(o: &InseasonObs, k: f64, weights: &Map<String, f64>) -> f64 {
    let mut shrunk = if o.has_base {
        (o.gp_td as f64 * o.fpg_td + k * o.fpg_base) / (o.gp_td as f64 + k)
    } else {
        o.fpg_td
    };
    if o.gp_td <= 0 {
        shrunk = o.fpg_base;
    }
    let gps: Map<String, i64> = o.splits.iter().map(|(s, (gp, _))| (s.clone(), *gp)).collect();
    let w = recency_weights_for(weights, &gps);
    let mut out = w["season"] * shrunk;
    for (s, (gp, v)) in &o.splits {
        let ws = w.get(s).copied().unwrap_or(0.0);
        if *gp > 0 && ws > 0.0 {
            out += ws * v;
        }
    }
    out
}

pub fn inseason_mae(obs: &[InseasonObs], k: f64, weights: &Map<String, f64>) -> f64 {
    let errs: Vec<f64> = obs.iter().map(|o| inseason_projection(o, k, weights) - o.fpg_rest).collect();
    mae(&errs)
}

fn rw(season: f64, l30: f64, l15: f64, l7: f64) -> Map<String, f64> {
    [("season", season), ("last30", l30), ("last15", l15), ("last7", l7)].iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

pub fn recency_grid(step: f64) -> Vec<Map<String, f64>> {
    let mut out = Vec::new();
    let r = |hi: f64| -> Vec<f64> {
        (0..=(hi / step).round_ties_even() as i64).map(|i| round_to(i as f64 * step, 4)).collect()
    };
    for w30 in r(0.5) {
        for w15 in r(0.4) {
            for w7 in r(0.2) {
                if w30 + w15 + w7 <= 0.8 + 1e-9 {
                    out.push(rw(round_to(1.0 - w30 - w15 - w7, 4), w30, w15, w7));
                }
            }
        }
    }
    out
}

/// (A_season, A_30, A_15, A_7, actual) per observation.
type Comp = (f64, f64, f64, f64, f64);

/// Per observation (A_season, A_30, A_15, A_7, actual) so that for base weights w the projection
/// is sum(w_i * A_i): each split's weight shortfall goes back to the season rate.
pub fn components(obs: &[InseasonObs], k: f64) -> Vec<Comp> {
    let season_only = rw(1.0, 0.0, 0.0, 0.0);
    obs.iter()
        .map(|o| {
            let shrunk = inseason_projection(o, k, &season_only);
            let mut c = [shrunk, 0.0, 0.0, 0.0];
            for (i, s) in SPLITS.iter().enumerate() {
                let (gp, v) = o.splits.get(*s).copied().unwrap_or((0, 0.0));
                let exp = EXPECTED_GAMES.iter().find(|(n, _)| n == s).map(|(_, e)| *e).unwrap_or(1.0);
                let frac = if gp > 0 { (gp.max(0) as f64 / exp).min(1.0) } else { 0.0 };
                c[i + 1] = (1.0 - frac) * shrunk + frac * v;
            }
            (c[0], c[1], c[2], c[3], o.fpg_rest)
        })
        .collect()
}

pub fn mae_components(comps: &[Comp], w: &Map<String, f64>) -> f64 {
    let (ws, w30, w15, w7) = (w["season"], w["last30"], w["last15"], w["last7"]);
    py_sum(comps.iter().map(|(a, b, c, d, y)| (ws * a + w30 * b + w15 * c + w7 * d - y).abs())) / comps.len() as f64
}

/// Result of [`fit_inseason`].
#[derive(Clone, Debug, PartialEq)]
pub struct InseasonFit {
    pub k_skater: f64,
    pub k_goalie: f64,
    pub recency_weights: Map<String, f64>,
    pub mae_app: Option<f64>,
    pub mae_fitted: Option<f64>,
    pub mae_shrink_only_fitted_k: Option<f64>,
    /// {k: MAE} over `INSEASON_K_GRID` (empty without skater observations)
    pub k_curve_skater: Vec<(i64, f64)>,
    pub n_skater_obs: usize,
    pub n_goalie_obs: usize,
}

impl InseasonFit {
    /// The Python dict (NaN becomes null).
    pub fn to_value(&self) -> Value {
        let curve: serde_json::Map<String, Value> =
            self.k_curve_skater.iter().map(|(k, v)| (k.to_string(), json!(*v))).collect();
        json!({
            "k_skater": self.k_skater, "k_goalie": self.k_goalie, "recency_weights": self.recency_weights,
            "mae_app": self.mae_app, "mae_fitted": self.mae_fitted,
            "mae_shrink_only_fitted_k": self.mae_shrink_only_fitted_k,
            "k_curve_skater": Value::Object(curve),
            "n_skater_obs": self.n_skater_obs, "n_goalie_obs": self.n_goalie_obs,
        })
    }
}

/// Coordinate descent: k_skater with the app's weights, then weights, then k again (starting from
/// the app's current constants, `blend.K_INSEASON` / `RECENCY_WEIGHTS`); k_goalie with the fitted
/// weights (goalie samples are too small for their own weights).
pub fn fit_inseason(obs: &[InseasonObs], rounds: usize) -> InseasonFit {
    let sk: Vec<InseasonObs> = obs.iter().filter(|o| o.group != "G").cloned().collect();
    let gl: Vec<InseasonObs> = obs.iter().filter(|o| o.group == "G").cloned().collect();
    let app_w = recency_weights_snapshot().clone();
    let k_app = k_inseason_snapshot();
    let mut weights = app_w.clone();
    let mut k = k_app["skater"];
    let grid = recency_grid(0.05);

    // min(INSEASON_K_GRID, key=lambda kk: (mae, kk))
    let best_k = |sample: &[InseasonObs], w: &Map<String, f64>| -> f64 {
        let mut best: Option<(f64, i64)> = None;
        for &kk in &INSEASON_K_GRID {
            let key = (mae_components(&components(sample, kk as f64), w), kk);
            if best.is_none_or(|b| key.0 < b.0 || (key.0 == b.0 && key.1 < b.1)) {
                best = Some(key);
            }
        }
        best.map(|b| b.1 as f64).unwrap_or(0.0)
    };

    if !sk.is_empty() {
        for _ in 0..rounds {
            k = best_k(&sk, &weights);
            let comps = components(&sk, k);
            let scores: Vec<f64> = grid.par_iter().map(|w| mae_components(&comps, w)).collect();
            let mut bi = 0;
            for (i, s) in scores.iter().enumerate() {
                if *s < scores[bi] {
                    bi = i;
                }
            }
            weights = grid[bi].clone();
        }
        k = best_k(&sk, &weights);
    }
    let kg = if !gl.is_empty() { best_k(&gl, &weights) } else { k_app["goalie"] };
    let shrink_only = rw(1.0, 0.0, 0.0, 0.0);
    let have = !sk.is_empty();
    InseasonFit {
        k_skater: k,
        k_goalie: kg,
        mae_app: have.then(|| inseason_mae(&sk, k_app["skater"], &app_w)),
        mae_fitted: have.then(|| inseason_mae(&sk, k, &weights)),
        mae_shrink_only_fitted_k: have.then(|| inseason_mae(&sk, k, &shrink_only)),
        k_curve_skater: if have {
            INSEASON_K_GRID.iter().map(|&kk| (kk, inseason_mae(&sk, kk as f64, &weights))).collect()
        } else {
            Vec::new()
        },
        recency_weights: weights,
        n_skater_obs: sk.len(),
        n_goalie_obs: gl.len(),
    }
}
