//! `analysis/outlook.py: future_values` (and its `_age`), needed by the trade engine's need
//! finder. Owner: work package `W7-trades`.

use fm_core::models::{LeagueContext, Player};
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::{Cid, Map};

use crate::valuation::dynasty::{healthy_fpg, position_group, ProductionCurves};

/// `FUTURE_YEARS`
pub const FUTURE_YEARS: usize = 3;

/// `_age(p, ctx, dyn)`: the dynasty value's age, else from the birth date.
pub fn age(p: &Player, ctx: &LeagueContext, dyn_: Option<&Map<Cid, DynastyValue>>) -> Option<f64> {
    if let Some(a) = dyn_.and_then(|d| d.get(&p.cid)).and_then(|dv| dv.age) {
        return Some(a);
    }
    p.birth_date.map(|b| (ctx.as_of - b).num_days() as f64 / 365.25)
}

/// His healthy per-game value 1..`years` (default FUTURE_YEARS) seasons ahead (age curve x
/// pedigree).
pub fn future_values(
    p: &Player,
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dyn_: Option<&Map<Cid, DynastyValue>>,
    years: Option<usize>,
) -> Vec<f64> {
    future_values_with(&ProductionCurves::current(), p, ctx, values, dyn_, years)
}

/// [`future_values`] with the production curves built once (hot loops).
pub fn future_values_with(
    curves: &ProductionCurves,
    p: &Player,
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dyn_: Option<&Map<Cid, DynastyValue>>,
    years: Option<usize>,
) -> Vec<f64> {
    let years = years.unwrap_or(FUTURE_YEARS);
    let Some(pv) = values.get(&p.cid) else {
        return vec![0.0; years];
    };
    let base = healthy_fpg(pv).max(0.0);
    let ped = match dyn_.and_then(|d| d.get(&p.cid)).map(|dv| dv.pedigree) {
        Some(x) if x != 0.0 => x,
        _ => 1.0,
    };
    let (group, a) = (position_group(p), age(p, ctx, dyn_));
    (1..=years).map(|y| base * curves.trajectory_factor(group, a, y as f64, ped)).collect()
}
