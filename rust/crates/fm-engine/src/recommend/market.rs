//! Rust port of `fantasy_manager/recommend/market.py`: how the league at large rates players
//! (the market), independent of our model, and the consensus check that compares the two.
//! Owner: work package `W5-recommend-base`.
//!
//! `MarketPool` gives every player a 0-100 market percentile within the league's rostered pool
//! (ESPN ADP, % rostered, Fantrax's projected FP/G; in dynasty leagues also our dynasty value).
//!
//! Python memoizes `value_and_sources` per cid in a dict. The trade search calls the pool from
//! rayon, so the Rust pool is immutable and `Sync`: the cache is precomputed for every rostered
//! player at construction, any other player (free agents) is computed on demand (the result is a
//! pure function of the player and the pool, so this is identical to Python's memo).

use std::collections::HashMap;

use fm_core::models::{Player, Reason, Recommendation};
use fm_core::values::{PickAsset, PlayerValue};
use fm_core::{Cid, LeagueContext, Map};
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::{py_round_i64, py_sum, pymod_i64, round_to};
use fm_pycompat::order::{bisect_left, bisect_right, cmp_f64};
use smallvec::SmallVec;

/// Fantrax's own fantasy points in its StatLines (`providers.fantrax.FPTS_KEY`)
pub const FPTS_STAT: &str = "FPTS";
/// Component names in Python dict insertion order.
const ADP: usize = 0;
const OWNED: usize = 1;
const PROJ: usize = 2;
const DYN: usize = 3;
const FPG: usize = 4;
pub const COMPONENT_NAMES: [&str; 5] = ["adp", "owned", "proj", "dyn", "fpg"];
/// `MARKET_WEIGHTS` (% rostered saturates near 100); `fpg` has no weight
const WEIGHTS: [Option<f64>; 5] = [Some(2.0), Some(1.0), Some(2.0), Some(2.0), None];
pub const MARKET_MIN_REF: usize = 10;
pub const DISAGREE_GAP: f64 = 25.0;
pub const CLEAR_FAVOR_PTS: f64 = 10.0;
/// independent of our model
pub const CONSENSUS_COMPONENTS: [&str; 3] = ["adp", "owned", "proj"];

pub fn market_weight(component: &str) -> Option<f64> {
    COMPONENT_NAMES.iter().position(|c| *c == component).and_then(|i| WEIGHTS[i])
}

pub fn market_source_label(component: &str) -> &'static str {
    match component {
        "adp" => "ADP",
        "owned" => "rostered %",
        "proj" => "Fantrax projected FP/G",
        "dyn" => "dynasty value",
        "fpg" => "our FPG",
        _ => "",
    }
}

/// PlayerValue reason codes that move our value away from consensus, as reader-facing drivers.
pub fn driver_label(code: &str) -> Option<&'static str> {
    Some(match code {
        "ROLE_DFO" | "ROLE_BOOST" | "ROLE_DEPTH" => "line / PP role",
        "ROLE_NEWS" => "role news",
        "ROLE_SCRATCH" => "scratched",
        "START_NEWS" => "goalie news",
        "ROOKIE_BLEND" | "NHLE" | "PEDIGREE_PRIOR" => "rookie model",
        "GP_EXPECTATION" | "ROOKIE_GAMES" => "expected games",
        "RETURN_ESTIMATE" => "injury return estimate",
        "START_SHARE" | "START_ACTUAL" => "goalie start share",
        "GSAX" => "goalie save talent (GSAx)",
        "ASSIST_SHRINK" => "assist luck",
        "PRESEASON" => "preseason",
        "TEAMMATE_OUT" => "teammate out",
        _ => return None,
    })
}

/// Reference group: per-game scoring components (proj, dyn, fpg) rank goalies and skaters
/// separately; ADP and % rostered are already cross-position market prices. 0 = ungrouped.
fn group_idx(k: usize, p: &Player) -> usize {
    if k == PROJ || k == DYN || k == FPG {
        if p.is_goalie() {
            1
        } else {
            2
        }
    } else {
        0
    }
}

type Raw = [Option<f64>; 5];

/// `_market_raw`: raw market signals for `p`, oriented so that higher = more valuable.
fn market_raw(p: &Player, values: &Map<Cid, PlayerValue>, dyn_: Option<&Map<Cid, f64>>) -> Raw {
    let mut out: Raw = [None; 5];
    if let Some(adp) = p.adp.filter(|a| *a > 0.0) {
        out[ADP] = Some(-adp);
    }
    if let Some(o) = p.pct_owned {
        out[OWNED] = Some(o);
    }
    if let Some(proj) = p.lines.get("projected") {
        if proj.gp > 0 {
            if let Some(f) = proj.stats.get(FPTS_STAT) {
                out[PROJ] = Some(*f / proj.gp as f64);
            }
        }
    }
    if let Some(v) = dyn_.and_then(|d| d.get(&p.cid)) {
        out[DYN] = Some(*v);
    }
    if let Some(pv) = values.get(&p.cid) {
        out[FPG] = Some(pv.fpg_season);
    }
    out
}

/// Mid-rank percentile (0-100) of `x` within the sorted reference `ref_` (ties share the middle
/// of their range; 50 when `ref_` is empty).
pub fn percentile(x: f64, ref_: &[f64]) -> f64 {
    if ref_.is_empty() {
        return 50.0;
    }
    let (lo, hi) = (bisect_left(ref_, x), bisect_right(ref_, x));
    100.0 * (lo as f64 + 0.5 * (hi - lo) as f64) / ref_.len() as f64
}

/// Components behind a market value, in a fixed order.
pub type Sources = SmallVec<[&'static str; 4]>;
/// {component: percentile}, in Python dict (insertion) order.
pub type Components = SmallVec<[(&'static str, f64); 5]>;

static EMPTY_VALUES: std::sync::LazyLock<Map<Cid, PlayerValue>> = std::sync::LazyLock::new(Map::new);

/// How the league at large rates each player (0-100), independent of our model.
///
/// Reference pool: every rostered player in the league (the tradeable assets). Components, each
/// a percentile within the rostered players that report it (projected FP/G, dynasty value and
/// FPG: within the player's group, goalies or skaters) and used only when at least
/// `MARKET_MIN_REF` of them do: ESPN ADP (lower = better), % rostered, Fantrax's projected FP/G,
/// and in dynasty leagues our dynasty value. Market value = their weighted mean. A player with
/// none of them falls back to the percentile of our season FPG (needs two rostered players with
/// a value); with nothing at all, 0.
pub struct MarketPool<'a> {
    pub values: &'a Map<Cid, PlayerValue>,
    dyn_: Option<&'a Map<Cid, f64>>,
    /// `ref[component][group]`, sorted ascending
    refs: [[Option<Vec<f64>>; 3]; 5],
    cache: HashMap<String, (f64, Sources)>,
}

impl<'a> MarketPool<'a> {
    pub fn new(ctx: &LeagueContext, values: Option<&'a Map<Cid, PlayerValue>>, dynasty_values: Option<&'a Map<Cid, f64>>) -> Self {
        let values: &'a Map<Cid, PlayerValue> = values.unwrap_or(&EMPTY_VALUES);
        let dyn_ = dynasty_values.filter(|d| ctx.dynasty && !d.is_empty());
        let mut rostered: Map<&str, &Player> = Map::new();
        for t in &ctx.teams {
            for q in t.players_iter() {
                rostered.insert(q.cid.as_str(), q);
            }
        }
        let mut acc: [[Vec<f64>; 3]; 5] = Default::default();
        for p in rostered.values() {
            for (k, v) in market_raw(p, values, dyn_).iter().enumerate() {
                if let Some(v) = v {
                    acc[k][group_idx(k, p)].push(*v);
                }
            }
        }
        let mut refs: [[Option<Vec<f64>>; 3]; 5] = Default::default();
        for k in 0..5 {
            for g in 0..3 {
                let mut xs = std::mem::take(&mut acc[k][g]);
                let need = if WEIGHTS[k].is_some() { MARKET_MIN_REF } else { 2 };
                if !xs.is_empty() && xs.len() >= need {
                    xs.sort_by(|a, b| cmp_f64(*a, *b));
                    refs[k][g] = Some(xs);
                }
            }
        }
        let mut pool = MarketPool { values, dyn_, refs, cache: HashMap::new() };
        let cache: HashMap<String, (f64, Sources)> =
            rostered.values().map(|p| (p.cid.clone(), pool.compute_value_and_sources(p))).collect();
        pool.cache = cache;
        pool
    }

    /// The reference list for a component and a player's group (`pool.ref.get((k, group))`).
    pub fn reference(&self, component: &str, p: &Player) -> Option<&[f64]> {
        let k = COMPONENT_NAMES.iter().position(|c| *c == component)?;
        self.refs[k][group_idx(k, p)].as_deref()
    }

    /// The sorted skater dynasty-value reference (`pool.ref[("dyn", "S")]`).
    fn dyn_skater_ref(&self) -> Option<&[f64]> {
        self.refs[DYN][2].as_deref()
    }

    /// {component: percentile} for every component `p` reports and the pool supports.
    pub fn components(&self, p: &Player) -> Components {
        let mut out = Components::new();
        for (k, v) in market_raw(p, self.values, self.dyn_).iter().enumerate() {
            if let Some(v) = v {
                if let Some(r) = self.refs[k][group_idx(k, p)].as_deref() {
                    out.push((COMPONENT_NAMES[k], percentile(*v, r)));
                }
            }
        }
        out
    }

    fn compute_value_and_sources(&self, p: &Player) -> (f64, Sources) {
        let comps = self.components(p);
        let market: Components = comps.iter().copied().filter(|(k, _)| market_weight(k).is_some()).collect();
        if !market.is_empty() {
            let w = py_sum(market.iter().map(|(k, _)| market_weight(k).unwrap_or(0.0)));
            let num = py_sum(market.iter().map(|(k, v)| market_weight(k).unwrap_or(0.0) * v));
            let mut names: Sources = market.iter().map(|(k, _)| *k).collect();
            names.sort();
            (num / w, names)
        } else if let Some((_, v)) = comps.iter().find(|(k, _)| *k == "fpg") {
            (*v, SmallVec::from_slice(&["fpg"]))
        } else {
            (0.0, Sources::new())
        }
    }

    pub fn value_and_sources(&self, p: &Player) -> (f64, Sources) {
        match self.cache.get(&p.cid) {
            Some(hit) => hit.clone(),
            None => self.compute_value_and_sources(p),
        }
    }

    pub fn value(&self, p: &Player) -> f64 {
        match self.cache.get(&p.cid) {
            Some(hit) => hit.0,
            None => self.compute_value_and_sources(p).0,
        }
    }

    /// Components behind these players' values, in a fixed display order.
    pub fn sources<'p>(&self, players: impl IntoIterator<Item = &'p Player>) -> Vec<&'static str> {
        let mut used: Sources = Sources::new();
        for p in players {
            for s in self.value_and_sources(p).1 {
                if !used.contains(&s) {
                    used.push(s);
                }
            }
        }
        ["adp", "owned", "proj", "dyn", "fpg"].into_iter().filter(|k| used.contains(k)).collect()
    }

    /// (market percentile, sources) from the independent signals only, or None without any.
    pub fn consensus(&self, p: &Player) -> Option<(f64, Sources)> {
        let comps: Components = self.components(p).into_iter().filter(|(k, _)| CONSENSUS_COMPONENTS.contains(k)).collect();
        if comps.is_empty() {
            return None;
        }
        let w = py_sum(comps.iter().map(|(k, _)| market_weight(k).unwrap_or(0.0)));
        let num = py_sum(comps.iter().map(|(k, v)| market_weight(k).unwrap_or(0.0) * v));
        let src: Sources = CONSENSUS_COMPONENTS.iter().copied().filter(|c| comps.iter().any(|(k, _)| k == c)).collect();
        Some((num / w, src))
    }

    /// Percentile of our season FPG among the rostered players of his group.
    pub fn model_percentile(&self, p: &Player) -> Option<f64> {
        let fpg = self.values.get(&p.cid)?.fpg_season;
        let r = self.refs[FPG][group_idx(FPG, p)].as_deref()?;
        Some(percentile(fpg, r))
    }
}

/// `_ordinal`: 43 -> "43rd" (`int(round(x))`, banker's).
pub fn ordinal(x: f64) -> String {
    let n = py_round_i64(x);
    let m100 = pymod_i64(n, 100);
    let suffix = if (10..=20).contains(&m100) {
        "th"
    } else {
        match pymod_i64(n, 10) {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{n}{suffix}")
}

/// 0-100 market percentile of `player` within the league's rostered pool.
pub fn market_value(player: &Player, pool: &MarketPool<'_>) -> f64 {
    pool.value(player)
}

/// Model signals behind `p`'s value that consensus may not price in (DRIVER_LABELS order of
/// appearance, de-duplicated), plus an unhealthy status; at most 3.
pub fn drivers(p: &Player, pv: Option<&PlayerValue>) -> Vec<String> {
    let mut out: Vec<String> = pv
        .map(|pv| pv.reasons.iter().filter_map(|r| driver_label(&r.code)).map(str::to_string).collect())
        .unwrap_or_default();
    let st = p.status.as_str();
    if st != "healthy" && st != "unknown" {
        out.push(format!("status {st}"));
    }
    dedup_keep_order(out, 3)
}

fn dedup_keep_order(xs: Vec<String>, limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for x in xs {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out.truncate(limit);
    out
}

/// MARKET_CHECK for one player (`role`: "add", "drop", "receive", "send"): market vs model
/// percentile; value = gap (model - market), baseline = market percentile. None when the league
/// publishes no independent signal for him or he has no model value.
pub fn market_check_reason(p: &Player, pool: &MarketPool<'_>, role: &str) -> Option<Reason> {
    let (market, src) = pool.consensus(p)?;
    let model = pool.model_percentile(p)?;
    let gap = model - market;
    let labels = src.iter().map(|s| market_source_label(s)).collect::<Vec<_>>().join("/");
    let mut text = format!(
        "Market check ({role}) {}: market {} pct ({labels}) vs model {} ({})",
        p.name,
        ordinal(market),
        ordinal(model),
        fixed(gap, 0, true)
    );
    if gap.abs() >= DISAGREE_GAP {
        let why = drivers(p, pool.values.get(&p.cid));
        text += ": model disagrees with market, rates him ";
        text += if gap > 0.0 { "higher" } else { "lower" };
        if why.is_empty() {
            text += " (projection only, no specific signal)";
        } else {
            text += &format!(" ({})", why.join(", "));
        }
    } else {
        text += ": market agrees";
    }
    Some(Reason { code: "MARKET_CHECK".to_string(), text, value: Some(round_to(gap, 1)), baseline: Some(round_to(market, 1)) })
}

/// MARKET_CHECK reasons for (player, role) pairs, skipping players without signals.
pub fn market_check_reasons<'p>(pool: &MarketPool<'_>, players: impl IntoIterator<Item = (&'p Player, &'p str)>) -> Vec<Reason> {
    players.into_iter().filter_map(|(p, role)| market_check_reason(p, pool, role)).collect()
}

/// Percentile of a draft pick's dynasty value among the rostered skaters' dynasty values: a
/// model-only market stand-in; 50 without a dynasty reference.
pub fn pick_percentile(pool: &MarketPool<'_>, value: f64) -> f64 {
    match pool.dyn_skater_ref() {
        Some(r) if !r.is_empty() => percentile(value, r),
        _ => 50.0,
    }
}

/// MARKET_CHECK for a draft pick a trade moves: always model-only (value None, so it never counts
/// as an agreement or a disagreement in [`market_disagrees`]).
pub fn pick_check_reason(pick: &PickAsset, role: &str, holder_id: Option<&str>) -> Reason {
    Reason {
        code: "MARKET_CHECK".to_string(),
        text: format!(
            "Market check ({role}) {} pick: model-only value {} ({} net of its roster spot; expected slot {}); no consensus source for draft picks",
            pick.short(holder_id),
            fixed(pick.value, 1, false),
            fixed(pick.net, 1, false),
            fixed(pick.expected_slot, 0, false)
        ),
        value: None,
        baseline: None,
    }
}

/// True when any MARKET_CHECK on `rec` is a disagreement (|gap| >= DISAGREE_GAP) or the deal-level
/// overpay check (`trade_market_check`: value None, baseline = worth points to them) is present;
/// False when checks exist and all agree; None without any market check.
pub fn market_disagrees(rec: &Recommendation) -> Option<bool> {
    let mut any = false;
    let mut dis = false;
    for r in rec.reasons.iter().filter(|r| r.code == "MARKET_CHECK") {
        any = true;
        dis |= (r.value.is_none() && r.baseline.is_some_and(|b| b > CLEAR_FAVOR_PTS))
            || r.value.is_some_and(|v| v.abs() >= DISAGREE_GAP);
    }
    any.then_some(dis)
}

/// Why the model likes a trade the market does not: the drivers of players I receive whom the
/// model rates above the market and of players I send whom it rates below.
pub fn deal_drivers<'p>(
    pool: &MarketPool<'_>,
    get: impl IntoIterator<Item = &'p Player>,
    give: impl IntoIterator<Item = &'p Player>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (ps, sign) in [(get.into_iter().collect::<Vec<_>>(), 1.0), (give.into_iter().collect::<Vec<_>>(), -1.0)] {
        for p in ps {
            let (cons, model) = (pool.consensus(p), pool.model_percentile(p));
            if let (Some(cons), Some(model)) = (cons, model) {
                if sign * (model - cons.0) >= DISAGREE_GAP {
                    let d = drivers(p, pool.values.get(&p.cid));
                    if d.is_empty() {
                        out.push(format!("{}: projection only", p.name));
                    } else {
                        out.extend(d.into_iter().map(|d| format!("{}: {d}", p.name)));
                    }
                }
            }
        }
    }
    dedup_keep_order(out, 3)
}

/// Deal-level MARKET_CHECK when the market calls a trade clearly in the counterparty's favour
/// (overpay > CLEAR_FAVOR_PTS market worth points handed over) while our model has me gaining.
pub fn trade_market_check(overpay: f64, my_edge: f64, why: Option<&[String]>) -> Option<Reason> {
    if overpay <= CLEAR_FAVOR_PTS || my_edge <= 0.0 {
        return None;
    }
    let drivers = match why {
        Some(w) if !w.is_empty() => w.join("; "),
        _ => "projection only, no specific signal".to_string(),
    };
    Some(Reason {
        code: "MARKET_CHECK".to_string(),
        text: format!(
            "Model disagrees with market: you overpay by market value ({} worth pts to them); worth it only if you trust the model's edge of {} (drivers: {drivers})",
            fixed(overpay, 0, true),
            fixed(my_edge, 2, true)
        ),
        value: None,
        baseline: Some(overpay),
    })
}
