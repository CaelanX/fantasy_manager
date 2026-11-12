//! Rust port of `fantasy_manager/valuation/blend.py`: rate estimation (multi-season baseline shrunk
//! toward the positional mean, season-to-date shrunk toward that baseline, light recency blend,
//! preseason nudge for unproven skaters).
//! Owner: work package `W2-valuation-base` (wave 2).
//!
//! Notes:
//! * Live code reads every tunable through [`super::params`] at call time; `*_snapshot()` return the
//!   import-time snapshots Python keeps as module constants (`K_BASELINE`, ...), taken on first use.
//! * Python builds per-key results from `set(a) | set(b)` (hash order). Here the union is `a`'s keys
//!   in order followed by `b`'s new keys: each key's value is independent of the order, and every
//!   SUM over keys (`share` in `blend_recency`) runs in the fixed splits order.

use std::sync::OnceLock;

use fm_core::models::StatLine;
use fm_core::Map;
use fm_pycompat::num::py_sum;
use serde_json::Value;

use super::params;

pub type Rates = Map<String, f64>;

pub const PROJECTION_GP: i64 = 82;
pub const MIN_FULL_PROJECTION_GP: i64 = 20;
pub const EXPECTED_GAMES: [(&str, f64); 3] = [("last30", 13.0), ("last15", 6.5), ("last7", 3.3)];
/// Marcel's weights for seasons N-1, N-2, N-3.
pub const MULTI_SEASON_WEIGHTS: [f64; 3] = [5.0, 4.0, 3.0];
pub const PRIOR_SPLITS: [&str; 3] = ["prior", "prior2", "prior3"];
pub const PROJECTION_BLEND_FULL_GP: i64 = 40;

/// Counting stats that may be shrunk in season with their own k (`params::k_inseason_volume`)
/// instead of the scoring stats' k. File 04 section 1 of the knowledge base: hits and blocked
/// shots repeat year to year, and shot volume far more than shooting percentage. Penalty minutes
/// are included as a volume stat too, though their fitted k came out slower, not faster.
pub const VOLUME_STATS: [&str; 4] = ["SOG", "HIT", "BLK", "PIM"];

pub const PRESEASON_STATS: [&str; 7] = ["G", "A", "PTS", "SOG", "PPP", "PPG", "PPA"];
pub const PRESEASON_MAX_WEIGHT: f64 = 0.25;
pub const PRESEASON_FULL_GP: i64 = 12;
pub const PRESEASON_SHRINK_K: i64 = 9;
pub const PRESEASON_FADE_GP: i64 = 20;

// ---- import-time snapshots (backtest / report)
static K_BASELINE: OnceLock<Map<String, f64>> = OnceLock::new();
static K_INSEASON: OnceLock<Map<String, f64>> = OnceLock::new();
static K_PROJECTION: OnceLock<Map<String, f64>> = OnceLock::new();
static RECENCY: OnceLock<Map<String, f64>> = OnceLock::new();
static PROJ_WEIGHT: OnceLock<f64> = OnceLock::new();

/// `blend.K_BASELINE`.
pub fn k_baseline_snapshot() -> &'static Map<String, f64> {
    K_BASELINE.get_or_init(|| params::k_baseline(None))
}
/// `blend.K_INSEASON`.
pub fn k_inseason_snapshot() -> &'static Map<String, f64> {
    K_INSEASON.get_or_init(|| params::k_inseason(None))
}
/// `blend.K_PROJECTION`.
pub fn k_projection_snapshot() -> &'static Map<String, f64> {
    K_PROJECTION.get_or_init(|| params::k_projection(None))
}
/// `blend.RECENCY_WEIGHTS`.
pub fn recency_weights_snapshot() -> &'static Map<String, f64> {
    RECENCY.get_or_init(|| params::recency_weights(None))
}
/// `blend.PROJECTION_WEIGHT`.
pub fn projection_weight_snapshot() -> f64 {
    *PROJ_WEIGHT.get_or_init(|| params::projection_weight(None))
}

type Params<'a> = Option<&'a Value>;

/// In-season k: games of shrinkage of season-to-date toward the preseason baseline.
pub fn shrink_k(is_goalie: bool, params: Params) -> f64 {
    params::k_inseason_goalie(is_goalie, params)
}

/// k of the multi-season history toward the positional mean for group F / D / G.
pub fn baseline_k(group: &str) -> f64 {
    let kb = params::k_baseline(None);
    kb.get(group).copied().unwrap_or(kb["F"])
}

pub fn projection_k(is_goalie: bool, params: Params) -> f64 {
    params::k_projection_goalie(is_goalie, params)
}

/// (gp*cur + k*prior)/(gp+k) per stat.
///
/// A stat missing from `cur` borrows the prior. A stat missing from the prior shrinks toward 0.
pub fn shrink_toward(cur: &Rates, gp: f64, prior: &Rates, k: f64) -> Rates {
    shrink_toward_by_stat(cur, gp, prior, |_| k)
}

/// Season-to-date rates shrunk toward `prior` in season: a skater volume stat with its own fitted
/// k (`params::k_inseason_volume`) uses it, every other stat uses [`shrink_k`].
///
/// Departs from the Python, which used one k for every stat: shot, hit and block rates stabilise
/// much sooner than scoring rates, and the per-stat k won out of sample in the in-season backtest.
pub fn shrink_inseason(cur: &Rates, gp: f64, prior: &Rates, is_goalie: bool, params: Params) -> Rates {
    let volume = if is_goalie { Map::new() } else { params::k_inseason_volume(params) };
    let k = shrink_k(is_goalie, params);
    shrink_toward_by_stat(cur, gp, prior, |stat| volume.get(stat).copied().unwrap_or(k))
}

/// [`shrink_toward`] with the k of each stat given by `k_of(stat)`.
pub fn shrink_toward_by_stat(cur: &Rates, gp: f64, prior: &Rates, k_of: impl Fn(&str) -> f64) -> Rates {
    if gp <= 0.0 || cur.is_empty() {
        return prior.clone();
    }
    if prior.is_empty() {
        return cur.clone();
    }
    let mut out = Rates::with_capacity(cur.len().max(prior.len()));
    let mut put = |key: &String| {
        let c = cur.get(key).copied().unwrap_or_else(|| prior.get(key).copied().unwrap_or(0.0));
        let p = prior.get(key).copied().unwrap_or(0.0);
        let k = k_of(key);
        out.insert(key.clone(), (gp * c + k * p) / (gp + k));
    };
    for key in cur.keys() {
        put(key);
    }
    for key in prior.keys() {
        if !cur.contains_key(key) {
            put(key);
        }
    }
    out
}

/// rate = (GP*rate_cur + k*rate_prior)/(GP+k); GP==0 -> prior rates; no prior -> current.
pub fn shrunk_rates(current: Option<&StatLine>, prior_or_proj: Option<&StatLine>, is_goalie: bool, params: Params) -> Rates {
    let prior = prior_or_proj.map(|l| l.per_game()).unwrap_or_default();
    let gp = current.map(|c| c.gp).unwrap_or(0);
    if gp <= 0 {
        return prior;
    }
    shrink_inseason(&current.unwrap().per_game(), gp as f64, &prior, is_goalie, params)
}

/// Effective games behind a baseline line for shrinking it toward the positional mean.
///
/// ESPN projections are already regressed, so a projection counts as a full season (PROJECTION_GP)
/// unless it projects very few games (< MIN_FULL_PROJECTION_GP).
pub fn baseline_sample(line: Option<&StatLine>) -> f64 {
    match line {
        None => 0.0,
        Some(l) if l.gp <= 0 => 0.0,
        Some(l) if l.split.as_str() == "projected" && l.gp >= MIN_FULL_PROJECTION_GP => PROJECTION_GP as f64,
        Some(l) => l.gp as f64,
    }
}

/// (baseline per-game rates shrunk toward the positional-group mean, effective GP).
pub fn shrink_baseline(line: Option<&StatLine>, mean: &Rates, is_goalie: bool, k: Option<f64>, params: Params) -> (Rates, f64) {
    let Some(line) = line.filter(|l| l.gp > 0) else {
        return (Rates::new(), 0.0);
    };
    let n = baseline_sample(Some(line));
    let rates = line.per_game();
    if mean.is_empty() {
        return (rates, n);
    }
    let k = k.unwrap_or_else(|| projection_k(is_goalie, params));
    let out = rates
        .iter()
        .map(|(key, v)| (key.clone(), (n * v + k * mean.get(key).copied().unwrap_or(*v)) / (n + k)))
        .collect();
    (out, n)
}

/// (GP-weighted per-game rates, effective games, total GP) over `lines` = seasons N-1, N-2, N-3
/// (most recent first; None / 0-GP entries are skipped), or None without games.
///
/// Effective games are `sum(w_i * GP_i) / w_1`.
pub fn multi_season_sample(lines: &[Option<&StatLine>], weights: &[f64]) -> Option<(Rates, f64, i64)> {
    let mut num: Map<String, f64> = Map::new();
    let mut den: Map<String, f64> = Map::new();
    let mut wgp = 0.0f64;
    let mut gp_total: i64 = 0;
    for (w, line) in weights.iter().zip(lines.iter()) {
        let Some(line) = line.filter(|l| l.gp > 0) else { continue };
        let wg = w * line.gp as f64;
        wgp += wg;
        gp_total += line.gp;
        for (key, v) in line.per_game() {
            *num.entry(key.clone()).or_insert(0.0) += wg * v;
            *den.entry(key).or_insert(0.0) += wg;
        }
    }
    if wgp <= 0.0 {
        return None;
    }
    let rates = num.iter().map(|(k, n)| (k.clone(), n / den[k])).collect();
    Some((rates, wgp / weights[0], gp_total))
}

/// The backtest's `fm_multi` rates (before aging): seasons N-1..N-3 weighted 5/4/3 by GP, shrunk
/// toward the position group's mean with `k` (default `K_BASELINE[group]`) games.
pub fn multi_season_baseline(
    lines: &[Option<&StatLine>],
    position_group: &str,
    means: &Map<String, Rates>,
    k: Option<f64>,
) -> Rates {
    let Some((rates, n, _)) = multi_season_sample(lines, &MULTI_SEASON_WEIGHTS) else {
        return Rates::new();
    };
    let mean = match means.get(position_group) {
        Some(m) if !m.is_empty() => m,
        _ => return rates,
    };
    shrink_toward(&rates, n, mean, k.unwrap_or_else(|| baseline_k(position_group)))
}

/// Weight of the league projection when blended with the multi-season history.
pub fn projection_blend_weight(history_gp: i64, params: Params) -> f64 {
    if history_gp <= 0 {
        return 1.0;
    }
    let frac = (history_gp as f64 / PROJECTION_BLEND_FULL_GP as f64).min(1.0);
    1.0 - (1.0 - params::projection_weight(params)) * frac
}

/// wa*a + (1-wa)*b per stat; a stat only one side carries keeps that side's rate.
pub fn blend_rates(a: &Rates, b: &Rates, wa: f64) -> Rates {
    let mut out = Rates::with_capacity(a.len().max(b.len()));
    let mut put = |key: &String| {
        let va = a.get(key).copied().unwrap_or_else(|| b.get(key).copied().unwrap_or(0.0));
        let vb = b.get(key).copied().unwrap_or(va);
        out.insert(key.clone(), wa * va + (1.0 - wa) * vb);
    };
    for key in a.keys() {
        put(key);
    }
    for key in b.keys() {
        if !a.contains_key(key) {
            put(key);
        }
    }
    out
}

/// Base weights (`params.recency_weights()`) scaled by min(1, GP/expected); the shortfall goes back
/// to 'season'. Keys: season, last30, last15, last7 (in that order).
pub fn recency_weights(gp_l30: i64, gp_l15: i64, gp_l7: i64, params: Params) -> Map<String, f64> {
    let base = params::recency_weights(params);
    let gps = [("last30", gp_l30), ("last15", gp_l15), ("last7", gp_l7)];
    let mut season = base["season"];
    let mut w: Map<String, f64> = Map::new();
    w.insert("season".to_string(), season);
    for ((split, exp), (_, gp)) in EXPECTED_GAMES.iter().zip(gps.iter()) {
        let frac = ((*gp).max(0) as f64 / exp).min(1.0);
        w.insert(split.to_string(), base[*split] * frac);
        season += base[*split] * (1.0 - frac);
    }
    w.insert("season".to_string(), season);
    w
}

/// Blend season rates with recent splits using [`recency_weights`].
pub fn blend_recency(
    season_rates: &Rates,
    l30: Option<&StatLine>,
    l15: Option<&StatLine>,
    l7: Option<&StatLine>,
    params: Params,
) -> Rates {
    let splits: [(&str, Option<&StatLine>); 3] = [("last30", l30), ("last15", l15), ("last7", l7)];
    let w = recency_weights(
        l30.map(|s| s.gp).unwrap_or(0),
        l15.map(|s| s.gp).unwrap_or(0),
        l7.map(|s| s.gp).unwrap_or(0),
        params,
    );
    let mut rates: Rates = season_rates.iter().map(|(k, v)| (k.clone(), w["season"] * v)).collect();
    let pgs: Vec<Option<Rates>> = splits.iter().map(|(_, l)| l.map(|l| l.per_game())).collect();
    for ((name, _), pg) in splits.iter().zip(pgs.iter()) {
        let Some(pg) = pg else { continue };
        let wn = w[*name];
        if wn <= 0.0 {
            continue;
        }
        let keys: Vec<String> =
            rates.keys().cloned().chain(pg.keys().filter(|k| !rates.contains_key(*k)).cloned()).collect();
        for k in keys {
            // stats absent from a split fall back to the season rate
            let v = pg.get(&k).copied().unwrap_or_else(|| season_rates.get(&k).copied().unwrap_or(0.0));
            let cur = rates.get(&k).copied().unwrap_or(0.0);
            rates.insert(k, cur + wn * v);
        }
    }
    // keys present only in splits need the season share filled from the split itself
    let new_keys: Vec<String> = rates.keys().filter(|k| !season_rates.contains_key(*k)).cloned().collect();
    for k in new_keys {
        let share = py_sum(splits.iter().zip(pgs.iter()).filter_map(|((n, _), pg)| {
            let pg = pg.as_ref()?;
            (w[*n] > 0.0 && pg.contains_key(&k)).then(|| w[*n])
        }));
        if share > 0.0 {
            let v = rates[&k] / share;
            rates.insert(k, v);
        }
    }
    rates
}

// --------------------------------------------------------------------------- preseason (weak signal)

/// Weight of preseason rates in an unproven player's baseline:
/// `PRESEASON_MAX_WEIGHT * gp_pre / (gp_pre + PRESEASON_SHRINK_K)`, faded by
/// `max(0, 1 - season_gp / PRESEASON_FADE_GP)`. 0 without preseason games.
pub fn preseason_weight(gp_pre: i64, season_gp: i64) -> f64 {
    if gp_pre <= 0 {
        return 0.0;
    }
    let w = PRESEASON_MAX_WEIGHT * gp_pre as f64 / (gp_pre + PRESEASON_SHRINK_K) as f64;
    w * (1.0 - season_gp.max(0) as f64 / PRESEASON_FADE_GP as f64).max(0.0)
}

/// `(1 - w) * base + w * pre` for `stats` both sides carry; every other stat keeps the baseline rate.
pub fn blend_preseason(base: &Rates, pre: &Rates, w: f64, stats: &[&str]) -> Rates {
    let mut out = base.clone();
    if w <= 0.0 {
        return out;
    }
    for key in stats {
        if let (Some(b), Some(p)) = (base.get(*key), pre.get(*key)) {
            out.insert((*key).to_string(), (1.0 - w) * b + w * p);
        }
    }
    out
}
