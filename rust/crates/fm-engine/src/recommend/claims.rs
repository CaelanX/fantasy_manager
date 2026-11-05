//! Rust port of `fantasy_manager/recommend/claims.py`: the claim planner (`fm claims`) - which
//! pickups to place now, in which order, dropping whom, when each counts, and a FAAB bid.
//! Owner: work package `W8-plan-claims-advise`.
//!
//! Deviations: the clock (`now`) and the schedule memo are parameters; the waiver engines'
//! errors propagate as `CoreError` (Python lets them raise too). Each processed roster (a new
//! context in Python) gets a fresh `ScheduleMemo`, as Python's id(ctx)-keyed memo would.

use std::collections::HashSet;

use chrono::{Duration, NaiveDate};
use fm_core::models::{FantasyTeam, LeagueContext, Player, Recommendation, RosterSlot};
use fm_core::normalize::normalize_team;
use fm_core::provider::LeagueProvider;
use fm_core::values::{DynastyValue, Horizon, PlayerValue};
use fm_core::{Cid, CoreError, Map, PyDateTime};
use fm_pycompat::fmt::{general, repr_f64};
use fm_pycompat::num::py_round;
use fm_pycompat::time::strftime_date;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::recommend::base::{moves_left, no_moves_text};
use crate::recommend::headline::market_suffix;
use crate::recommend::lineup::{daily_plan, unavailable};
use crate::recommend::plan::earliest;
use crate::recommend::waivers::{market_multiplier, recommend_matchup_waivers, recommend_waivers, WaiverOpts};
use crate::valuation::schedule::ScheduleMemo;

/// claims listed (also capped by the moves left)
pub const CLAIM_LIMIT: usize = 5;
/// pickups asked from the waiver engine per step
pub const CANDIDATES: usize = 15;
/// of the remaining budget, for the best pickup
pub const FAAB_MAX_SHARE: f64 = 0.35;
/// season FPG gain that earns the full share
pub const FAAB_GAIN_SCALE: f64 = 2.0;
pub const FAAB_MIN_BID: f64 = 1.0;

pub const FANTRAX_RULE_LABELS: [&str; 9] = [
    "Free Agent Claim System",
    "Waiver Wire claim system",
    "# of days players remain on waivers for",
    "Waiver Wire players are processed",
    "Rotate Waiver Wire Claim Order After Each Claim",
    "Ensure 23+ hour waiver period",
    "Prevent Waiver \"Churning\"",
    "Max # of claims per week",
    "Lineup changes are executed",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaimItem {
    pub order: i64,
    /// "claim" | "free agent"
    pub kind: String,
    /// "season" | "matchup"
    pub horizon: String,
    pub add: Player,
    #[serde(default)]
    pub drop: Option<Player>,
    #[serde(default)]
    pub to_ir: Option<Player>,
    /// waiver clear date (claims)
    #[serde(default)]
    pub clears: Option<NaiveDate>,
    /// first day he counts
    pub effective: NaiveDate,
    /// days in my best lineup, rest of the matchup
    #[serde(default)]
    pub games: Vec<NaiveDate>,
    #[serde(default)]
    pub games_7d: i64,
    #[serde(default)]
    pub gain: Option<f64>,
    #[serde(default)]
    pub gain_units: Option<String>,
    #[serde(default)]
    pub headline: String,
    #[serde(default)]
    pub market: Option<String>,
    #[serde(default)]
    pub bid: Option<f64>,
    #[serde(default)]
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaimPlan {
    pub league: String,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub moves_left: Option<i64>,
    #[serde(default)]
    pub claims: Vec<ClaimItem>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Suggested FAAB bid for a pickup worth `gain` season FPG (module docstring, step 4).
pub fn faab_bid(gain: f64, remaining: f64, change: Option<f64>, min_bid: f64) -> f64 {
    if remaining <= 0.0 {
        return 0.0;
    }
    let share = FAAB_MAX_SHARE.min(gain.max(0.0) / FAAB_GAIN_SCALE * FAAB_MAX_SHARE);
    let bid = py_round(remaining * share * market_multiplier(change));
    remaining.min(min_bid.min(remaining).max(bid))
}

/// Python `str(value)` of a JSON value.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => match n.as_i64() {
            Some(i) if !n.is_f64() => i.to_string(),
            _ => repr_f64(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python `str.title()`.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_alpha = false;
    for c in s.chars() {
        if c.is_alphabetic() {
            if prev_alpha {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_alpha = true;
        } else {
            out.push(c);
            prev_alpha = false;
        }
    }
    out
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// (the league's claim rules in its own words, my waiver priority) from the provider.
pub fn rule_lines(ctx: &LeagueContext, provider: Option<&dyn LeagueProvider>) -> (Vec<String>, Option<String>) {
    let mut lines: Vec<String> = Vec::new();
    let mut priority: Option<String> = None;
    if ctx.provider == "fantrax" {
        if let Some(rules) = provider.and_then(|p| p.rules()) {
            for lab in FANTRAX_RULE_LABELS {
                if let Some(v) = rules.get(lab).filter(|v| !v.is_empty()) {
                    lines.push(format!("{lab}: {v}"));
                }
            }
            let mine = ctx.teams.iter().find(|t| t.owner_is_me).map(|t| t.team_id.clone());
            let order = provider
                .and_then(|p| mine.as_ref().and_then(|m| p.extras().standings.get(m)))
                .and_then(|row| row.get("waiver_order"))
                .filter(|v| !v.is_null());
            if let Some(order) = order {
                priority = Some(format!("{} of {}", py_str(order), ctx.teams.len()));
            }
        }
    }
    let acq: Option<&Map<String, Value>> = provider.map(|p| &p.extras().acquisition_settings).filter(|a| !a.is_empty());
    if ctx.provider == "espn" {
        if let Some(acq) = acq {
            let kind = if truthy(acq.get("isUsingAcquisitionBudget")) { "FAAB" } else { "waiver priority" };
            let get = |k: &str| acq.get(k).map_or("?".to_string(), py_str);
            lines.push(format!(
                "Waivers: {} ({kind}), players stay on waivers {} h after a drop",
                get("acquisitionType"),
                get("waiverHours")
            ));
            if truthy(acq.get("waiverProcessDays")) {
                let days: Vec<String> = match acq.get("waiverProcessDays") {
                    Some(Value::Array(a)) => a.iter().map(|d| title_case(&py_str(d))).collect(),
                    Some(other) => vec![title_case(&py_str(other))],
                    None => vec![],
                };
                lines.push(format!("Waiver processing: {} at {}:00", days.join(", "), get("waiverProcessHour")));
            }
            if let Some(rank) = provider.and_then(|p| p.extras().waiver_rank) {
                priority = Some(format!("{rank} of {}", ctx.teams.len()));
            }
        }
    }
    if let Some(f) = ctx.faab_remaining {
        lines.push(format!("FAAB left: ${}", general(f, None, false)));
    }
    (lines, priority)
}

/// `waivers._plan_team`: `team` after adding `add` to the bench, dropping `drop` and moving
/// `to_ir` to IR.
fn plan_team(team: &FantasyTeam, add: &Player, drop: Option<&Player>, to_ir: Option<&Player>) -> FantasyTeam {
    let mut slots: Vec<RosterSlot> = team
        .slots
        .iter()
        .filter(|s| !(s.player.is_some() && drop.is_some_and(|d| s.player.as_ref().is_some_and(|p| p.cid == d.cid))))
        .map(|s| match (&s.player, to_ir) {
            (Some(p), Some(ir)) if p.cid == ir.cid => RosterSlot { slot: "IR".into(), player: Some(p.clone()), starting: false },
            _ => s.clone(),
        })
        .collect();
    slots.push(RosterSlot { slot: "BN".into(), player: Some(add.clone()), starting: false });
    FantasyTeam { slots, ..team.clone() }
}

/// `_apply`: `ctx` after the claim processes.
fn apply(ctx: &LeagueContext, add: &Player, drop: Option<&Player>, to_ir: Option<&Player>) -> LeagueContext {
    let mut out = ctx.clone();
    out.teams = ctx.teams.iter().map(|t| if t.owner_is_me { plan_team(t, add, drop, to_ir) } else { t.clone() }).collect();
    out.free_agents = ctx.free_agents.iter().filter(|p| p.cid != add.cid).cloned().collect();
    out.moves_used_this_period = Some(ctx.moves_used_this_period.unwrap_or(0) + 1);
    if let Some(s) = ctx.moves_used_season {
        out.moves_used_season = Some(s + 1);
    }
    out
}

fn to_ir_of(rec: &Recommendation) -> Option<&Player> {
    if rec.drop.is_empty() && rec.title.contains("to IR") {
        rec.subjects.first()
    } else {
        None
    }
}

fn lineup_days(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    add: &Player,
    effective: NaiveDate,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Vec<NaiveDate> {
    let Ok(team) = ctx.my_team() else { return vec![] };
    let Some(plan) = daily_plan(ctx, team, values, None, now, memo) else { return vec![] };
    plan.days.iter().filter(|d| d.day >= effective && d.assign.values().any(|c| *c == add.cid)).map(|d| d.day).collect()
}

fn games_7d(ctx: &LeagueContext, p: &Player, start: NaiveDate) -> i64 {
    let team = normalize_team(p.team.as_deref()).unwrap_or_default();
    let end = start + Duration::days(7);
    ctx.schedule.get(&team).map_or(0, |ds| ds.iter().filter(|d| start <= **d && **d < end).count() as i64)
}

/// `_pick`: the next claim, the best season upgrade, else the best this-matchup stream.
fn pick(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    seen: &HashSet<String>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Result<Option<(Recommendation, &'static str)>, CoreError> {
    // an injured player is never the drop here: his hold-or-cut verdict decides (as in the plan)
    let hurt: Vec<Cid> = ctx.my_team()?.players_iter().filter(|p| unavailable(p.status)).map(|p| p.cid.clone()).collect();
    let opts = WaiverOpts { limit: CANDIDATES, horizon: Horizon::Season, dynasty_values, stream: false, exclude_drops: &hurt };
    for r in recommend_waivers(ctx, values, &opts, None, now, memo)? {
        if r.add.first().is_some_and(|a| !seen.contains(&a.cid)) {
            return Ok(Some((r, "season")));
        }
    }
    for r in recommend_matchup_waivers(ctx, values, CANDIDATES, dynasty_values, &hurt, now, memo)? {
        if r.add.first().is_some_and(|a| !seen.contains(&a.cid)) {
            return Ok(Some((r, "matchup")));
        }
    }
    Ok(None)
}

/// The ordered claim list (module docstring). `memo` is `ctx`'s schedule memo (its odds data dir
/// seeds the fresh memos of the processed rosters).
#[allow(clippy::too_many_arguments)]
pub fn plan_claims(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    provider: Option<&dyn LeagueProvider>,
    now: &PyDateTime,
    limit: usize,
    memo: &ScheduleMemo,
) -> Result<ClaimPlan, CoreError> {
    let (rules, priority) = rule_lines(ctx, provider);
    let mut plan = ClaimPlan { league: ctx.provider.clone(), rules, priority, moves_left: None, claims: vec![], notes: vec![] };
    let left = moves_left(ctx);
    plan.moves_left = left;
    if left == Some(0) {
        plan.notes.push(format!("{}: no claims until the limit resets.", no_moves_text(ctx)));
        return Ok(plan);
    }
    let cap = match left {
        None => limit,
        Some(l) => limit.min(l.max(0) as usize),
    };
    // each processed roster is a new context: Python's per-context memo (`_CTX_MEMO`, keyed by
    // id(ctx)) starts empty for it (team GA factors change when a dropped goalie leaves the pool)
    let mut work: Option<(LeagueContext, ScheduleMemo)> = None;
    let mut seen: HashSet<String> = HashSet::new();
    for k in 0..cap {
        let (w, wmemo): (&LeagueContext, &ScheduleMemo) = match &work {
            Some((c, m)) => (c, m),
            None => (ctx, memo),
        };
        let Some((rec, horizon)) = pick(w, values, dynasty_values, &seen, now, wmemo)? else { break };
        let Some(add) = rec.add.first() else { break };
        let drop = rec.drop.first();
        let to_ir = to_ir_of(&rec);
        seen.insert(add.cid.clone());
        let claim = add.waiver_until.is_some_and(|wu| wu >= ctx.as_of);
        let effective = earliest(w, add, now);
        let after = apply(w, add, drop, to_ir);
        let after_memo = memo.fresh();
        let mut item = ClaimItem {
            order: k as i64 + 1,
            kind: if claim { "claim" } else { "free agent" }.to_string(),
            horizon: horizon.to_string(),
            add: add.clone(),
            drop: drop.cloned(),
            to_ir: to_ir.cloned(),
            clears: if claim { add.waiver_until } else { None },
            effective,
            games: lineup_days(&after, values, add, effective, now, &after_memo),
            games_7d: games_7d(ctx, add, effective),
            gain: rec.predicted_gain,
            gain_units: rec.gain_units.map(|g| g.as_str().to_string()),
            headline: rec.headline.clone().unwrap_or_default(),
            market: {
                let s = market_suffix(&rec).trim_matches(|c| c == ' ' || c == '\u{b7}');
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            },
            bid: None,
            title: rec.title.clone(),
        };
        if let Some(faab) = ctx.faab_remaining {
            let fpg = rec.reasons.iter().find(|x| x.code == "VORP_DELTA" && x.value.is_some()).and_then(|x| x.value);
            item.bid = Some(faab_bid(fpg.unwrap_or(0.0), faab, add.pct_owned_change, FAAB_MIN_BID));
        }
        plan.claims.push(item);
        work = Some((after, after_memo));
    }
    if plan.claims.is_empty() {
        plan.notes.push("No pickup clears the waiver thresholds right now.".to_string());
    }
    if plan.claims.iter().any(|c| c.kind == "claim") {
        plan.notes.push(
            "Place claims in this order: each drop is chosen with the earlier claims already processed, so the roster stays \
             legal at every step."
                .to_string(),
        );
    }
    Ok(plan)
}

/// Watch lines for the weekly plan: season-upgrade claims of players on waivers the plan does
/// not already schedule.
pub fn claim_watch_lines(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    exclude: &HashSet<String>,
    limit: usize,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Result<Vec<String>, CoreError> {
    let opts = WaiverOpts { limit: CANDIDATES, horizon: Horizon::Season, dynasty_values, stream: false, exclude_drops: &[] };
    let mut out = Vec::new();
    for r in recommend_waivers(ctx, values, &opts, None, now, memo)? {
        let Some(a) = r.add.first() else { continue };
        let Some(wu) = a.waiver_until.filter(|w| *w >= ctx.as_of) else { continue };
        if exclude.contains(&a.cid) {
            continue;
        }
        let drop = r.drop.first().map_or(String::new(), |d| format!(", drop {}", d.name));
        let what = r.headline.clone().filter(|h| !h.is_empty()).unwrap_or_else(|| r.title.clone());
        out.push(format!("Claim {} (on waivers until {}){drop}: {what}", a.name, strftime_date(wu, "%a")));
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    //! Port of tests/test_claims.py (intent).
    use super::*;
    use crate::recommend::plan::tests::{days7, mon, now0};
    use crate::scoring::PointsScoring;
    use crate::valuation::valuate::valuate_league;
    use serde_json::json;

    fn mk(cid: &str, g: f64, waiver_until: Option<NaiveDate>) -> Player {
        serde_json::from_value(json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": "ALL",
            "positions": ["C"], "status": "healthy", "waiver_until": waiver_until,
            "lines": {"projected": {"split": "projected", "gp": 80, "stats": {"G": g * 80.0, "GP": 80.0}},
                      "season": {"split": "season", "gp": 20, "stats": {"G": g * 20.0, "GP": 20.0}}}}))
        .unwrap()
    }

    fn ctx_for(mine: &[(&str, Player)], fas: Vec<Player>, faab: Option<f64>, moves: Option<i64>) -> LeagueContext {
        let slots: Vec<RosterSlot> = mine
            .iter()
            .map(|(s, p)| RosterSlot { slot: s.to_string(), player: Some(p.clone()), starting: !["BN", "IR"].contains(s) })
            .collect();
        let mut schedule: Map<String, Vec<NaiveDate>> = Map::new();
        for i in 0..4 {
            schedule.insert(format!("Z{i}"), days7());
        }
        schedule.insert("ALL".into(), days7());
        let gpd: Map<String, i64> = days7().iter().map(|d| (d.format("%Y-%m-%d").to_string(), 8)).collect();
        let t = FantasyTeam { team_id: "1".into(), name: "me".into(), owner_is_me: true, slots, record: None };
        serde_json::from_value(json!({"provider": "test", "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {"G": 1.0}}, "roster_shape": {"C": 1, "BN": 2, "IR": 1},
            "teams": [t], "free_agents": fas, "matchup_period": 1, "as_of": "2026-10-05", "period_end": "2026-10-11",
            "schedule": schedule, "games_per_day": gpd, "moves_limit_per_period": moves,
            "moves_used_this_period": 0, "lineup_lock": "daily", "faab_remaining": faab}))
        .unwrap()
    }

    fn claims_for(ctx: &LeagueContext) -> ClaimPlan {
        let mut c = ctx.clone();
        let w: Map<String, f64> = [("G".to_string(), 1.0)].into_iter().collect();
        let values = valuate_league(&mut c, &PointsScoring::new(w, None));
        plan_claims(ctx, &values, None, None, &now0(), CLAIM_LIMIT, &ScheduleMemo::new(None)).unwrap()
    }

    fn fas() -> Vec<Player> {
        vec![mk("x", 3.0, None), mk("y", 2.5, None), mk("z", 0.5, None)]
    }

    #[test]
    fn order_with_dependent_drops() {
        let ctx = ctx_for(&[("C", mk("my_c", 3.5, None)), ("BN", mk("weak1", 0.1, None)), ("BN", mk("weak2", 0.2, None))], fas(), None, None);
        let cp = claims_for(&ctx);
        let pairs: Vec<(String, Option<String>)> =
            cp.claims.iter().take(2).map(|c| (c.add.cid.clone(), c.drop.as_ref().map(|d| d.cid.clone()))).collect();
        assert_eq!(pairs, vec![("x".into(), Some("weak1".into())), ("y".into(), Some("weak2".into()))]);
        assert_eq!(cp.claims.iter().take(2).map(|c| c.order).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn roster_legal_at_processing_after_an_open_spot_fills() {
        let ctx = ctx_for(&[("C", mk("my_c", 1.0, None)), ("BN", mk("weak1", 0.1, None))], fas(), None, None);
        let cp = claims_for(&ctx);
        assert!(cp.claims[0].add.cid == "x" && cp.claims[0].drop.is_none());
        assert!(cp.claims[1].add.cid == "y" && cp.claims[1].drop.as_ref().is_some_and(|d| d.cid == "weak1"));
    }

    #[test]
    fn claims_capped_by_moves_left() {
        let mut ctx = ctx_for(&[("C", mk("my_c", 3.5, None)), ("BN", mk("weak1", 0.1, None)), ("BN", mk("weak2", 0.2, None))], fas(), None, Some(1));
        assert_eq!(claims_for(&ctx).claims.len(), 1);
        ctx.moves_used_this_period = Some(1);
        let cp0 = claims_for(&ctx);
        assert!(cp0.claims.is_empty() && cp0.notes.iter().any(|n| n.contains("No moves left")));
    }

    #[test]
    fn waiver_claim_counts_from_the_day_after_it_clears() {
        let tue = days7()[1];
        let ctx = ctx_for(
            &[("C", mk("my_c", 0.2, None)), ("BN", mk("weak1", 0.1, None)), ("BN", mk("weak2", 0.15, None))],
            vec![mk("w", 3.0, Some(tue)), mk("z", 0.5, None), mk("q", 0.4, None)],
            None,
            None,
        );
        let c = &claims_for(&ctx).claims[0];
        assert!(c.add.cid == "w" && c.kind == "claim" && c.clears == Some(tue));
        assert_eq!(c.effective, days7()[2]);
        assert!(!c.games.is_empty() && c.games.iter().min().copied() >= Some(days7()[2]));
        assert_eq!(c.games_7d, 5);
    }

    #[test]
    fn free_agent_counts_today() {
        let ctx = ctx_for(&[("C", mk("my_c", 0.2, None)), ("BN", mk("weak1", 0.1, None)), ("BN", mk("weak2", 0.15, None))], fas(), None, None);
        let cp = claims_for(&ctx);
        let c = &cp.claims[0];
        assert!(c.kind == "free agent" && c.effective == mon() && c.clears.is_none());
        assert!(c.games.contains(&mon()));
    }

    #[test]
    fn faab_bid_bounds() {
        assert!((faab_bid(5.0, 100.0, None, FAAB_MIN_BID) - 100.0 * FAAB_MAX_SHARE).abs() < 1e-9);
        let b = faab_bid(0.01, 100.0, None, FAAB_MIN_BID);
        assert!((1.0..=100.0).contains(&b));
        assert_eq!(faab_bid(1.0, 0.0, None, FAAB_MIN_BID), 0.0);
        assert!(faab_bid(5.0, 3.0, None, FAAB_MIN_BID) <= 3.0);
        assert!(faab_bid(1.0, 100.0, Some(10.0), FAAB_MIN_BID) > faab_bid(1.0, 100.0, None, FAAB_MIN_BID));
    }

    #[test]
    fn faab_bid_on_claims_only_with_a_budget() {
        let mine = [("C", mk("my_c", 0.2, None)), ("BN", mk("weak1", 0.1, None)), ("BN", mk("weak2", 0.15, None))];
        assert!(claims_for(&ctx_for(&mine, fas(), None, None)).claims.iter().all(|c| c.bid.is_none()));
        let bids: Vec<Option<f64>> = claims_for(&ctx_for(&mine, fas(), Some(50.0), None)).claims.iter().map(|c| c.bid).collect();
        assert!(!bids.is_empty() && bids.iter().all(|b| b.is_some_and(|b| (1.0..=50.0).contains(&b))));
    }

    #[test]
    fn injured_player_is_never_the_drop() {
        let mut hurt = mk("hurt", 0.05, None);
        hurt.status = fm_core::models::Status::Ir;
        let ctx = ctx_for(&[("C", mk("my_c", 3.5, None)), ("BN", hurt), ("BN", mk("weak2", 0.2, None))], fas(), None, None);
        assert!(claims_for(&ctx).claims.iter().all(|c| c.drop.as_ref().map_or(true, |d| d.cid != "hurt")));
        let mut hurt2 = ctx_for(&[("C", mk("my_c", 3.5, None)), ("BN", mk("ok", 2.0, None)), ("BN", mk("weak2", 0.2, None))], fas(), None, None);
        hurt2.teams[0].slots[1].player.as_mut().unwrap().status = fm_core::models::Status::Ir;
        let cp = claims_for(&hurt2);
        let drops: Vec<String> = cp.claims.iter().filter_map(|c| c.drop.as_ref().map(|d| d.cid.clone())).collect();
        let want: Vec<String> = vec!["weak2".to_string()].into_iter().take(cp.claims.len()).collect();
        assert_eq!(drops, want);
    }

    #[test]
    fn title_case_and_py_str() {
        assert_eq!(title_case("SUNDAY"), "Sunday");
        assert_eq!(py_str(&json!(8)), "8");
        assert_eq!(py_str(&json!("WAIVERS_TRADITIONAL")), "WAIVERS_TRADITIONAL");
    }
}
