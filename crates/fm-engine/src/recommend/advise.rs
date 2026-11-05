//! Rust port of `fantasy_manager/recommend/advise.py`: run every available recommender and merge
//! into one ranked list (scores rescaled 0-10 by rank within each kind group, moves to make
//! first). Owner: work package `W8-plan-claims-advise`.
//!
//! Deviations: Python's importlib / inspect dispatch is a static engine table ([`run_engine`])
//! that calls each engine with the arguments Python's `_call` would pass (`dynasty_values` /
//! `history` only where the engine declares them, every other argument at its default). The
//! engines run in parallel (rayon) and their results are concatenated in `include` order;
//! a failing engine is logged and skipped, as in Python.

use std::collections::HashSet;

use fm_core::models::{LeagueContext, Reason, RecKind, Recommendation};
use fm_core::values::{DynastyValue, Horizon, PlayerValue};
use fm_core::{Cid, Map, PyDateTime};
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::round_to;
use fm_pycompat::order::{cmp_f64, sort_desc_by};
use rayon::prelude::*;

use crate::recommend::alerts::{recommend_line_alerts, LineAlertOpts};
use crate::recommend::flags::{recommend_flags, recommend_role_alerts};
use crate::recommend::injuries::{recommend_injuries, History};
use crate::recommend::lineup::recommend_lineup;
use crate::recommend::strength::strength_for;
use crate::recommend::trades::{recommend_trades, TradeOpts};
use crate::recommend::waivers::{recommend_matchup_waivers, recommend_trending_alerts, recommend_waivers, WaiverOpts};
use crate::valuation::schedule::ScheduleMemo;

pub const PRIORITY: [&str; 6] = ["injury", "lineup", "waiver", "alert", "trade", "flags"];
/// moves to make now, listed before alerts / trades / flags
pub const MOVE_GROUPS: [&str; 3] = ["injury", "lineup", "waiver"];
/// include names of the engine table, in Python's `ENGINES` order
pub const ENGINES: [&str; 9] = [
    "injuries",
    "lineup",
    "waivers",
    "matchup_waivers",
    "trades",
    "flags",
    "line_alerts",
    "role_alerts",
    "trending_alerts",
];
pub const DEFAULT_INCLUDE: [&str; 6] = ["lineup", "waivers", "trades", "flags", "injuries", "alerts"];

/// `KIND_GROUP.get(kind)`.
pub fn kind_group(kind: RecKind) -> &'static str {
    match kind {
        RecKind::Injury => "injury",
        RecKind::Alert => "alert",
        RecKind::Lineup => "lineup",
        RecKind::Waiver => "waiver",
        RecKind::Trade => "trade",
        RecKind::SellHigh | RecKind::BuyLow => "flags",
    }
}

/// include-name aliases expanding to several engines
pub fn aliases(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "alerts" => Some(&["line_alerts", "role_alerts", "trending_alerts"]),
        "waivers" => Some(&["waivers", "matchup_waivers"]),
        _ => None,
    }
}

/// Keyword arguments of `advise`.
pub struct AdviseOpts<'a> {
    pub dynasty_values: Option<&'a Map<Cid, DynastyValue>>,
    pub history: &'a History<'a>,
    pub include: &'a [&'a str],
    pub limit: Option<usize>,
}

impl Default for AdviseOpts<'_> {
    fn default() -> Self {
        AdviseOpts { dynasty_values: None, history: &History::None, include: &DEFAULT_INCLUDE, limit: None }
    }
}

/// One engine of the table with Python's `_call` arguments; None for an unknown name.
pub fn run_engine(
    name: &str,
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    history: &History<'_>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Option<anyhow::Result<Vec<Recommendation>>> {
    let hist = match history {
        History::Store(h) => History::Store(*h),
        History::Snapshots(m) => History::Snapshots(*m),
        History::None => History::None,
    };
    Some(match name {
        "injuries" => recommend_injuries(ctx, values, hist, Horizon::Week, dynasty_values, now, memo),
        "lineup" => recommend_lineup(ctx, values, Horizon::Week, now, memo).map_err(Into::into),
        "waivers" => {
            let opts = WaiverOpts { dynasty_values, ..WaiverOpts::default() };
            recommend_waivers(ctx, values, &opts, None, now, memo).map_err(Into::into)
        }
        "matchup_waivers" => recommend_matchup_waivers(ctx, values, 5, dynasty_values, &[], now, memo).map_err(Into::into),
        "trades" => {
            let opts = TradeOpts { dynasty_values, ..TradeOpts::default() };
            recommend_trades(ctx, values, &opts, None, None)
        }
        "flags" => recommend_flags(ctx, values, None, None).map_err(Into::into),
        "line_alerts" => Ok(recommend_line_alerts(ctx, Some(values), &LineAlertOpts::default())),
        "role_alerts" => Ok(recommend_role_alerts(ctx, None, None)),
        "trending_alerts" => Ok(recommend_trending_alerts(ctx, Some(values), 10, Horizon::Season)),
        _ => return None,
    })
}

/// Merged, normalized and ranked recommendations from every available engine.
pub fn advise(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    opts: &AdviseOpts<'_>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Vec<Recommendation> {
    let names = expand_include(opts.include);
    let results: Vec<(String, Option<anyhow::Result<Vec<Recommendation>>>)> = names
        .par_iter()
        .map(|n| (n.clone(), run_engine(n, ctx, values, opts.dynasty_values, opts.history, now, memo)))
        .collect();
    let mut raw: Vec<Recommendation> = Vec::new();
    for (name, res) in results {
        match res {
            None => tracing::info!("advise: skipping {name} (unknown engine)"),
            Some(Ok(recs)) => raw.extend(recs),
            Some(Err(e)) => tracing::warn!("advise: {name} failed: {e}"),
        }
    }
    let mut merged = normalize_scores(dedupe_pickups(raw));
    sort_merged(&mut merged);
    if let Some(l) = opts.limit.filter(|l| *l > 0) {
        merged.truncate(l);
    }
    merged
}

/// `recs` plus `extra` (normalized on their own, e.g. trades called with trade-block wants) in
/// advise order: the CLI / dashboard `_run_advise` merge.
pub fn merge_normalized(recs: Vec<Recommendation>, extra: Vec<Recommendation>, limit: Option<usize>) -> Vec<Recommendation> {
    let mut merged = recs;
    merged.extend(normalize_scores(extra));
    sort_merged(&mut merged);
    if let Some(l) = limit.filter(|l| *l > 0) {
        merged.truncate(l);
    }
    merged
}

/// Rank-based 0-10 rescale within each kind group; raw score kept as a RAW_SCORE reason.
pub fn normalize_scores(recs: Vec<Recommendation>) -> Vec<Recommendation> {
    let groups = group_by_kind(recs, true);
    let mut out: Vec<Recommendation> = Vec::new();
    for (_, mut items) in groups {
        sort_desc_by(&mut items, |a, b| cmp_f64(a.score, b.score));
        let n = items.len();
        for (i, mut r) in items.into_iter().enumerate() {
            let norm = 10.0 * (n - i) as f64 / n as f64;
            let raw = r.score;
            if r.strength.is_none() {
                r.strength = Some(strength_for(&r));
            }
            r.rank_in_kind = Some(i as i64 + 1);
            r.kind_total = Some(n as i64);
            r.reasons.push(Reason {
                code: "RAW_SCORE".to_string(),
                text: format!("{} engine score {} (rank {}/{n})", r.kind.as_str(), fixed(raw, 3, false), i + 1),
                value: Some(raw),
                baseline: None,
            });
            r.score = round_to(norm, 4);
            out.push(r);
        }
    }
    out
}

fn priority(r: &Recommendation) -> usize {
    let g = kind_group(r.kind);
    PRIORITY.iter().position(|p| *p == g).unwrap_or(PRIORITY.len())
}

/// `order_key`: moves to make first, then score, then kind priority.
pub fn order_cmp(a: &Recommendation, b: &Recommendation) -> std::cmp::Ordering {
    let tier = |r: &Recommendation| if MOVE_GROUPS.contains(&kind_group(r.kind)) { 0 } else { 1 };
    tier(a).cmp(&tier(b)).then_with(|| cmp_f64(-a.score, -b.score)).then_with(|| priority(a).cmp(&priority(b)))
}

/// `merged.sort(key=order_key)` (stable).
pub fn sort_merged(recs: &mut [Recommendation]) {
    recs.sort_by(order_cmp);
}

/// `_dedupe_pickups`: one waiver rec per added player; the injuries engine's "Hold X: swap IR
/// slots" verdict is dropped when the lineup engine already proposes that swap.
pub fn dedupe_pickups(recs: Vec<Recommendation>) -> Vec<Recommendation> {
    let mut seen: HashSet<String> = HashSet::new();
    let swapped: Vec<String> = recs
        .iter()
        .filter(|r| r.kind == RecKind::Lineup && r.reasons.iter().any(|x| x.code == "IR_SWAP"))
        .flat_map(|r| r.subjects.iter().map(|p| p.name.clone()))
        .collect();
    let mut out = Vec::new();
    for r in recs {
        if r.kind == RecKind::Injury
            && swapped.iter().any(|n| r.title.starts_with(&format!("Hold {n} ")))
            && r.reasons.iter().any(|x| x.code == "HOLD_VERDICT" && x.text.starts_with("Swapping IR slots"))
        {
            continue;
        }
        if r.kind == RecKind::Waiver {
            if let Some(a) = r.add.first() {
                if !seen.insert(a.cid.clone()) {
                    continue;
                }
            }
        }
        out.push(r);
    }
    out
}

/// Engine names for `include` with aliases expanded, duplicates dropped.
pub fn expand_include(include: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in include {
        let names: Vec<&str> = aliases(name).map(|a| a.to_vec()).unwrap_or_else(|| vec![*name]);
        for n in names {
            if !out.iter().any(|x| x == n) {
                out.push(n.to_string());
            }
        }
    }
    out
}

/// Recommendations bucketed by kind (or by group), buckets in kind-priority order, order within
/// a bucket preserved.
pub fn group_by_kind(recs: Vec<Recommendation>, by_group: bool) -> Map<String, Vec<Recommendation>> {
    let mut recs = recs;
    recs.sort_by_key(priority);
    let mut out: Map<String, Vec<Recommendation>> = Map::new();
    for r in recs {
        let key = if by_group { kind_group(r.kind).to_string() } else { r.kind.as_str().to_string() };
        out.entry(key).or_default().push(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: RecKind, score: f64, title: &str) -> Recommendation {
        Recommendation::new(kind, score, title)
    }

    #[test]
    fn expand_include_expands_aliases_once() {
        assert_eq!(
            expand_include(&["lineup", "waivers", "alerts", "waivers"]),
            vec!["lineup", "waivers", "matchup_waivers", "line_alerts", "role_alerts", "trending_alerts"]
        );
        assert_eq!(expand_include(&DEFAULT_INCLUDE).len(), 9);
        for n in expand_include(&DEFAULT_INCLUDE) {
            assert!(ENGINES.contains(&n.as_str()));
        }
    }

    #[test]
    fn normalize_ranks_within_group_and_orders_moves_first() {
        let recs = vec![
            rec(RecKind::Trade, 5.0, "t1"),
            rec(RecKind::Waiver, 0.2, "w1"),
            rec(RecKind::Waiver, 0.9, "w2"),
            rec(RecKind::SellHigh, 1.0, "s"),
            rec(RecKind::BuyLow, 2.0, "b"),
            rec(RecKind::Alert, 3.0, "a"),
        ];
        let mut out = normalize_scores(recs);
        sort_merged(&mut out);
        let titles: Vec<&str> = out.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["w2", "w1", "a", "t1", "b", "s"]);
        let w2 = &out[0];
        assert_eq!((w2.score, w2.rank_in_kind, w2.kind_total), (10.0, Some(1), Some(2)));
        assert_eq!(out[1].score, 5.0);
        assert_eq!(w2.reasons.last().unwrap().text, "waiver engine score 0.900 (rank 1/2)");
        let b = out.iter().find(|r| r.title == "b").unwrap();
        assert_eq!((b.rank_in_kind, b.kind_total), (Some(1), Some(2)));
    }

    #[test]
    fn dedupe_keeps_first_waiver_per_player() {
        let p: fm_core::models::Player = serde_json::from_value(serde_json::json!({"cid": "x", "name": "x", "name_norm": "x",
            "ids": {}, "team": null, "positions": ["C"]}))
        .unwrap();
        let mut a = rec(RecKind::Waiver, 1.0, "season");
        a.add = vec![p.clone()];
        let mut b = rec(RecKind::Waiver, 2.0, "matchup");
        b.add = vec![p];
        let out = dedupe_pickups(vec![a, b]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "season");
    }
}
