//! Rust port of `fantasy_manager/backtest/models.py`: candidate preseason projection models.
//! Owner: work package `W4-backtest-models`.
//!
//! Every model maps a player's history (rows for seasons < N, oldest first) and age on Oct 1 of
//! season N to projected per-game rates for season N; [`project`] turns those into fantasy
//! points per game with the context's scoring.
//!
//! Deviations from the Python:
//! * `Model` is a trait; `get_models` returns boxed models. Python's `isinstance(m, (FmFitted,
//!   FmMulti))` + `m.params = params` is [`Model::set_params`] (true for those two models).
//! * `FmCurrent` and `group_means` need `valuate.baseline_rates` / `valuate.positional_means`,
//!   whose Rust port (`fm_engine::valuation::valuate`, W4-valuate) is not available yet. The
//!   no-projection path they use (a player carrying only `prior*` lines) is implemented here
//!   directly on `PlayerSeason` rows ([`positional_means`], [`history_baseline_rates`]),
//!   line for line from `valuate.py`; re-point once valuate lands.

use anyhow::{bail, Result};
use fm_core::models::{Split, StatLine};
use fm_core::Map;
use fm_engine::scoring::PointsScoring;
use fm_engine::valuation::blend::{
    multi_season_baseline, multi_season_sample, shrink_toward, MULTI_SEASON_WEIGHTS,
};
use fm_engine::valuation::params::{self, AgeYoy};
use serde_json::{json, Value};

use crate::data::{prev_season, PlayerSeason, SeasonTable};
use crate::scoring::fpg;

pub type Rates = Map<String, f64>;

/// seasons N-1, N-2, N-3 (5/4/3)
pub const MARCEL_WEIGHTS: [f64; 3] = MULTI_SEASON_WEIGHTS;
/// regression, in games of the most recent season
pub const MARCEL_K: f64 = 30.0;
pub const MARCEL_PEAK_AGE: f64 = 27.0;
/// +0.6% per year below peak
pub const MARCEL_YOUNG: f64 = 0.006;
/// -0.3% per year above peak
pub const MARCEL_OLD: f64 = 0.003;

const PRIOR_SPLIT_LIST: [Split; 3] = [Split::Prior, Split::Prior2, Split::Prior3];

fn fmap(pairs: &[(&str, f64)]) -> Map<String, f64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

/// Constants for `fm_fitted`: shrinkage k per group and a year-over-year age factor (expected FPG
/// ratio from the season at age a to the next one) per group.
#[derive(Clone, Debug, PartialEq)]
pub struct FittedParams {
    pub k: Map<String, f64>,
    pub k_multi: Map<String, f64>,
    pub age_yoy: AgeYoy,
    pub age_groups: Vec<String>,
}

impl Default for FittedParams {
    fn default() -> Self {
        FittedParams {
            k: fmap(&[("F", 20.0), ("D", 20.0), ("G", 12.0)]),
            k_multi: fmap(&[("F", 10.0), ("D", 10.0), ("G", 30.0)]),
            age_yoy: AgeYoy::new(),
            age_groups: vec!["F".to_string(), "D".to_string()],
        }
    }
}

impl FittedParams {
    /// Multiplier from last season (age `age_prev` on Oct 1) to this one.
    pub fn age_factor(&self, group: &str, age_prev: Option<f64>) -> f64 {
        params::age_factor(group, age_prev, Some(&self.age_yoy), Some(&self.age_groups))
    }

    pub fn to_dict(&self) -> Value {
        let yoy: serde_json::Map<String, Value> = self
            .age_yoy
            .iter()
            .map(|(g, c)| {
                (g.clone(), Value::Object(c.iter().map(|(a, r)| (a.to_string(), json!(*r))).collect()))
            })
            .collect();
        json!({"k": self.k, "k_multi": self.k_multi, "age_yoy": Value::Object(yoy), "age_groups": self.age_groups})
    }

    /// `FittedParams.from_dict` (`k_multi` falls back to the default when absent / empty).
    pub fn from_dict(d: &Value) -> Result<FittedParams> {
        let fmap_of = |key: &str| -> Map<String, f64> {
            d.get(key)
                .and_then(|v| v.as_object())
                .map(|o| o.iter().filter_map(|(g, v)| v.as_f64().map(|x| (g.clone(), x))).collect())
                .unwrap_or_default()
        };
        let k_multi = fmap_of("k_multi");
        let mut age_yoy = AgeYoy::new();
        if let Some(o) = d.get("age_yoy").and_then(|v| v.as_object()) {
            for (g, c) in o {
                let mut curve = Map::new();
                for (a, r) in c.as_object().into_iter().flatten() {
                    let age: i64 = a.parse().map_err(|_| anyhow::anyhow!("bad age key {a:?}"))?;
                    curve.insert(age, r.as_f64().unwrap_or(f64::NAN));
                }
                age_yoy.insert(g.clone(), curve);
            }
        }
        let groups: Vec<String> = d
            .get("age_groups")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        Ok(FittedParams {
            k: fmap_of("k"),
            k_multi: if k_multi.is_empty() { FittedParams::default().k_multi } else { k_multi },
            age_yoy,
            age_groups: if groups.is_empty() { vec!["F".to_string(), "D".to_string()] } else { groups },
        })
    }
}

pub struct ProjectionContext<'a> {
    /// target season N
    pub season: i64,
    pub scoring: &'a PointsScoring,
    /// positional means of season N-1 (F / D / G)
    pub means: Map<String, Rates>,
    pub fitted: Option<FittedParams>,
}

// --------------------------------------------------------------------------- valuate shims

/// `valuate.position_group` of the app `Player` that `PlayerSeason.to_player` builds.
fn position_group_of(r: &PlayerSeason) -> &'static str {
    let pos = r.position.as_deref().unwrap_or(if r.is_goalie() { "G" } else { "C" });
    match pos {
        "G" => "G",
        "D" => "D",
        _ => "F",
    }
}

fn min_gp_for_mean(group: &str) -> i64 {
    if group == "G" {
        10
    } else {
        20
    }
}

/// `valuate.positional_means` over rows treated as `prior` lines: mean prior-season per-game
/// rates by position group.
pub fn positional_means(rows: &[&PlayerSeason]) -> Map<String, Rates> {
    let mut sums: Map<String, Rates> = Map::new();
    let mut counts: Map<String, i64> = Map::new();
    for r in rows {
        let prior = r.statline(Split::Prior);
        let g = position_group_of(r);
        if prior.gp < min_gp_for_mean(g) {
            continue;
        }
        *counts.entry(g.to_string()).or_insert(0) += 1;
        let acc = sums.entry(g.to_string()).or_default();
        for (k, v) in prior.per_game() {
            let cur = acc.get(&k).copied().unwrap_or(0.0);
            acc.insert(k, cur + v);
        }
    }
    sums.into_iter()
        .map(|(g, acc)| {
            let n = counts[&g] as f64;
            (g, acc.into_iter().map(|(k, v)| (k, v / n)).collect())
        })
        .collect()
}

/// `valuate.baseline_rates` for a player that carries only `prior` / `prior2` / `prior3` lines
/// (no league projection, no current-season line): the multi-season history baseline
/// (`history_baseline`); empty without prior games.
pub fn history_baseline_rates(
    group: &str,
    lines: &[Option<StatLine>],
    means: &Map<String, Rates>,
    age: Option<f64>,
) -> Rates {
    let refs: Vec<Option<&StatLine>> = lines.iter().map(|l| l.as_ref()).collect();
    if multi_season_sample(&refs, &MULTI_SEASON_WEIGHTS).is_none() {
        return Rates::new();
    }
    let mut rates = multi_season_baseline(&refs, group, means, None);
    let age_prev = age.map(|a| a - 1.0);
    let f = params::age_factor(group, age_prev, None, None);
    if (f - 1.0).abs() > 1e-9 {
        rates = rates.into_iter().map(|(k, v)| (k, v * f)).collect();
    }
    rates
}

/// The app's `baseline_rates` of the player `latest.to_player("prior")` with lines `lines`.
pub fn app_baseline_rates(
    latest: &PlayerSeason,
    lines: &[Option<StatLine>],
    means: &Map<String, Rates>,
    age: Option<f64>,
) -> Rates {
    history_baseline_rates(position_group_of(latest), lines, means, age)
}

/// The app's `positional_means` over season `season`'s rows (as 'prior' lines).
pub fn group_means(table: &SeasonTable, season: i64) -> Map<String, Rates> {
    let rows: Vec<&PlayerSeason> = table.season_rows(season).into_iter().filter(|r| r.gp > 0).collect();
    positional_means(&rows)
}

pub fn make_context<'a>(
    table: &SeasonTable,
    season: i64,
    scoring: &'a PointsScoring,
    fitted: Option<FittedParams>,
) -> ProjectionContext<'a> {
    ProjectionContext { season, scoring, means: group_means(table, prev_season(season)), fitted }
}

// --------------------------------------------------------------------------- models

pub trait Model: Send + Sync {
    fn name(&self) -> &str;
    fn rates(&self, history: &[&PlayerSeason], age: Option<f64>, ctx: &ProjectionContext) -> Option<Rates>;
    /// Install fitted params (`m.params = params`); true when the model uses them.
    fn set_params(&mut self, _params: &FittedParams) -> bool {
        false
    }
    /// `isinstance(m, (FmFitted, FmMulti))`.
    fn uses_params(&self) -> bool {
        false
    }
}

/// Projected fantasy points per game for season `ctx.season` (None: model abstains).
pub fn project(model: &dyn Model, history: &[&PlayerSeason], age: Option<f64>, ctx: &ProjectionContext) -> Option<f64> {
    model.rates(history, age, ctx).map(|r| fpg(Some(&r), ctx.scoring))
}

/// `_last`: the latest row with games (of `season`, if given).
fn last<'a>(history: &[&'a PlayerSeason], season: Option<i64>) -> Option<&'a PlayerSeason> {
    history.iter().rev().find(|h| h.gp > 0 && season.is_none_or(|s| h.season == s)).copied()
}

pub struct NaiveLastSeason;

impl Model for NaiveLastSeason {
    fn name(&self) -> &str {
        "naive"
    }
    fn rates(&self, history: &[&PlayerSeason], _age: Option<f64>, _ctx: &ProjectionContext) -> Option<Rates> {
        last(history, None).map(|l| l.per_game())
    }
}

/// Rows of seasons N-1, N-2, ... (None where the player has no games that season).
pub fn prior_rows<'a>(history: &[&'a PlayerSeason], target: i64, n: usize) -> Vec<Option<&'a PlayerSeason>> {
    (0..n).map(|i| last(history, Some(target - 10001 * (i as i64 + 1)))).collect()
}

/// The rows as the app's `prior` / `prior2` / `prior3` StatLines.
pub fn prior_lines(rows: &[Option<&PlayerSeason>]) -> Vec<Option<StatLine>> {
    rows.iter().zip(PRIOR_SPLIT_LIST.iter()).map(|(r, split)| r.map(|r| r.statline(*split))).collect()
}

/// (GP-weighted per-game rates over seasons N-1, N-2, ..., effective games in units of the most
/// recent season, position group) or None without any of those seasons.
pub fn weighted_rates(history: &[&PlayerSeason], target: i64, weights: &[f64]) -> Option<(Rates, f64, String)> {
    let rows = prior_rows(history, target, weights.len());
    let group = rows.iter().find_map(|r| r.map(|r| r.group.clone()));
    let lines = prior_lines(&rows);
    let refs: Vec<Option<&StatLine>> = lines.iter().map(|l| l.as_ref()).collect();
    let sample = multi_season_sample(&refs, weights);
    match (sample, group) {
        (Some((rates, n, _)), Some(g)) => Some((rates, n, g)),
        _ => None,
    }
}

pub fn marcel_age_factor(age: Option<f64>) -> f64 {
    let Some(age) = age else { return 1.0 };
    if age < MARCEL_PEAK_AGE {
        1.0 + MARCEL_YOUNG * (MARCEL_PEAK_AGE - age)
    } else {
        1.0 - MARCEL_OLD * (age - MARCEL_PEAK_AGE)
    }
}

pub struct Marcel {
    pub weights: Vec<f64>,
    pub k: f64,
    pub age: bool,
}

impl Default for Marcel {
    fn default() -> Self {
        Marcel { weights: MARCEL_WEIGHTS.to_vec(), k: MARCEL_K, age: true }
    }
}

impl Model for Marcel {
    fn name(&self) -> &str {
        "marcel"
    }
    fn rates(&self, history: &[&PlayerSeason], age: Option<f64>, ctx: &ProjectionContext) -> Option<Rates> {
        let (rate, n, group) = weighted_rates(history, ctx.season, &self.weights)?;
        let empty = Rates::new();
        let mean = ctx.means.get(&group).unwrap_or(&empty);
        let mut out: Rates = rate
            .iter()
            .map(|(k, v)| (k.clone(), (n * v + self.k * mean.get(k).copied().unwrap_or(*v)) / (n + self.k)))
            .collect();
        if self.age && group != "G" {
            let f = marcel_age_factor(age);
            out = out.into_iter().map(|(k, v)| (k, v * f)).collect();
        }
        Some(out)
    }
}

/// The app's preseason baseline without a league projection: `valuate.baseline_rates` on seasons
/// N-1..N-3 with the packaged fitted params.
pub struct FmCurrent;

impl Model for FmCurrent {
    fn name(&self) -> &str {
        "fm_current"
    }
    fn rates(&self, history: &[&PlayerSeason], age: Option<f64>, ctx: &ProjectionContext) -> Option<Rates> {
        let rows = prior_rows(history, ctx.season, MARCEL_WEIGHTS.len());
        let latest = rows.iter().find_map(|r| *r)?;
        let lines = prior_lines(&rows);
        let rates = app_baseline_rates(latest, &lines, &ctx.means, age);
        if rates.is_empty() {
            None
        } else {
            Some(rates)
        }
    }
}

#[derive(Default)]
pub struct FmFitted {
    pub params: Option<FittedParams>,
}

impl FmFitted {
    pub fn new(params: Option<FittedParams>) -> Self {
        FmFitted { params }
    }
}

impl Model for FmFitted {
    fn name(&self) -> &str {
        "fm_fitted"
    }
    fn uses_params(&self) -> bool {
        true
    }
    fn set_params(&mut self, params: &FittedParams) -> bool {
        self.params = Some(params.clone());
        true
    }
    fn rates(&self, history: &[&PlayerSeason], age: Option<f64>, ctx: &ProjectionContext) -> Option<Rates> {
        let default;
        let params = match (&self.params, &ctx.fitted) {
            (Some(p), _) => p,
            (None, Some(p)) => p,
            (None, None) => {
                default = FittedParams::default();
                &default
            }
        };
        let last = last(history, Some(prev_season(ctx.season)))?;
        let empty = Rates::new();
        let mean = ctx.means.get(&last.group).unwrap_or(&empty);
        let k = params.k.get(&last.group).copied().unwrap_or(20.0);
        let rates = if !mean.is_empty() {
            shrink_toward(&last.per_game(), last.gp as f64, mean, k)
        } else {
            last.per_game()
        };
        let f = params.age_factor(&last.group, age.map(|a| a - 1.0));
        Some(rates.into_iter().map(|(key, v)| (key, v * f)).collect())
    }
}

/// Candidate: 5/4/3-weighted seasons, fitted k per group, fitted age factor.
#[derive(Default)]
pub struct FmMulti {
    pub params: Option<FittedParams>,
}

impl FmMulti {
    pub fn new(params: Option<FittedParams>) -> Self {
        FmMulti { params }
    }
}

impl Model for FmMulti {
    fn name(&self) -> &str {
        "fm_multi"
    }
    fn uses_params(&self) -> bool {
        true
    }
    fn set_params(&mut self, params: &FittedParams) -> bool {
        self.params = Some(params.clone());
        true
    }
    fn rates(&self, history: &[&PlayerSeason], age: Option<f64>, ctx: &ProjectionContext) -> Option<Rates> {
        let default;
        let params = match (&self.params, &ctx.fitted) {
            (Some(p), _) => p,
            (None, Some(p)) => p,
            (None, None) => {
                default = FittedParams::default();
                &default
            }
        };
        let rows = prior_rows(history, ctx.season, MARCEL_WEIGHTS.len());
        let group = rows.iter().find_map(|r| r.map(|r| r.group.clone()))?;
        let lines = prior_lines(&rows);
        let refs: Vec<Option<&StatLine>> = lines.iter().map(|l| l.as_ref()).collect();
        let out = multi_season_baseline(&refs, &group, &ctx.means, Some(params.k_multi.get(&group).copied().unwrap_or(10.0)));
        if out.is_empty() {
            return None;
        }
        let f = params.age_factor(&group, age.map(|a| a - 1.0));
        Some(out.into_iter().map(|(key, v)| (key, v * f)).collect())
    }
}

pub const MODEL_NAMES: [&str; 5] = ["naive", "marcel", "fm_current", "fm_fitted", "fm_multi"];
pub const DEFAULT_MODELS: [&str; 5] = MODEL_NAMES;

/// `MODELS[name]()`.
fn make_model(name: &str) -> Option<Box<dyn Model>> {
    Some(match name {
        "naive" => Box::new(NaiveLastSeason),
        "marcel" => Box::new(Marcel::default()),
        "fm_current" => Box::new(FmCurrent),
        "fm_fitted" => Box::new(FmFitted::default()),
        "fm_multi" => Box::new(FmMulti::default()),
        _ => return None,
    })
}

pub fn get_models(names: Option<&[String]>) -> Result<Vec<Box<dyn Model>>> {
    let names: Vec<String> = match names {
        Some(n) if !n.is_empty() => n.to_vec(),
        _ => DEFAULT_MODELS.iter().map(|s| s.to_string()).collect(),
    };
    let bad: Vec<&str> = names.iter().filter(|n| !MODEL_NAMES.contains(&n.as_str())).map(|s| s.as_str()).collect();
    if !bad.is_empty() {
        bail!("unknown model(s) {}; choose from {}", bad.join(", "), MODEL_NAMES.join(", "));
    }
    Ok(names.iter().filter_map(|n| make_model(n)).collect())
}
