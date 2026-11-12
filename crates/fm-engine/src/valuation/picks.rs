//! Rust port of `fantasy_manager/valuation/picks.py`: draft picks as dynasty assets (expected
//! slot, slot uncertainty, a slot -> value curve anchored on the league's unproven players, future
//! discount, net over the roster spot; keeper leagues value the redraft of everyone not kept).
//! `PickAsset` lives in `fm_core::values`.
//! Owner: work package `W4-valuate` (wave 4).
//!
//! Deviation: Python's `dynasty_values: Mapping[str, Any]` (DynastyValues or floats) is always
//! `Map<Cid, DynastyValue>` here; the float view is built inside ([`dyn_floats`]).

use std::collections::HashSet;

use fm_core::models::{FantasyTeam, LeagueContext};
use fm_core::values::{DynastyValue, Horizon, PickAsset, PlayerValue};
use fm_core::Map;
use fm_pycompat::num::{py_round, py_sum};
use fm_pycompat::order::{cmp_f64, sort_desc_by};

use super::adjust::SeasonLength;
use super::dynasty::{
    age_on, apply_dynasty, healthy_fpg, keeper_count, min_replacement_fpg, mode_weights, own_replacement, position_group,
    positional_off, ProductionCurves, DISCOUNT,
};
use super::rookie::is_unproven;
use super::valuate::replacement_for;

/// a new draftee vs today's comparable-rank unproven player
pub const DRAFT_CLASS_DISCOUNT: f64 = 0.75;
/// expected slot pulled halfway to the middle per year ahead
pub const FUTURE_SLOT_SHRINK: f64 = 0.5;
/// win % weight in next draft's order at season end
pub const RECORD_WEIGHT_MAX: f64 = 0.8;
pub const SPREAD_NOW: f64 = 1.0;
pub const SPREAD_PER_YEAR: f64 = 2.0;
pub const SPREAD_EARLY: f64 = 3.0;

/// cid -> dynasty value (`_dyn` over the mapping).
pub fn dyn_floats(dynasty_values: &Map<String, DynastyValue>) -> Map<String, f64> {
    dynasty_values.iter().map(|(c, dv)| (c.clone(), dv.value)).collect()
}

fn starting_count(ctx: &LeagueContext) -> i64 {
    let n: i64 = ctx
        .roster_shape
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "BN" | "IR" | "IR+" | "NA" | "MIN"))
        .map(|(_, v)| *v)
        .sum();
    n.max(1)
}

/// Share of the regular season played (0..1), over the loaded schedule's season length in days
/// ([`SeasonLength::DEFAULT`] without one; Python always used 186 days).
pub fn elapsed(ctx: &LeagueContext) -> f64 {
    let days = SeasonLength::of(&ctx.schedule).days;
    match ctx.season_start {
        None => 0.0,
        Some(s) => ((ctx.as_of - s).num_days() as f64 / days).clamp(0.0, 1.0),
    }
}

fn zscores(xs: &Map<String, f64>) -> Map<String, f64> {
    if xs.is_empty() {
        return Map::new();
    }
    let n = xs.len() as f64;
    let m = py_sum(xs.values().copied()) / n;
    let mut sd = (py_sum(xs.values().map(|v| (v - m).powf(2.0))) / n).powf(0.5);
    if sd == 0.0 {
        sd = 1.0;
    }
    xs.iter().map(|(k, v)| (k.clone(), (v - m) / sd)).collect()
}

fn top_sum(team: &FantasyTeam, score: &Map<String, f64>, k: i64) -> f64 {
    let mut xs: Vec<f64> = team.players_iter().map(|p| score.get(&p.cid).copied().unwrap_or(0.0)).collect();
    sort_desc_by(&mut xs, |a, b| cmp_f64(*a, *b));
    py_sum(xs.into_iter().take(k.max(0) as usize))
}

/// team id -> slot (1 = weakest team picks first).
fn slots_by_strength(strength: &Map<String, f64>) -> Map<String, f64> {
    let mut order: Vec<&String> = strength.keys().collect();
    order.sort_by(|a, b| cmp_f64(strength[*a], strength[*b]));
    order.into_iter().enumerate().map(|(i, t)| (t.clone(), (i + 1) as f64)).collect()
}

/// team id -> expected slot in a draft `years_ahead` years after the next one.
pub fn expected_slots(
    ctx: &LeagueContext,
    values: &Map<String, PlayerValue>,
    dyn_values: &Map<String, f64>,
    years_ahead: i64,
) -> Map<String, f64> {
    let k = starting_count(ctx);
    let n = ctx.teams.len();
    let mid = (n as f64 + 1.0) / 2.0;
    if years_ahead == 0 {
        let fpg: Map<String, f64> = values.iter().map(|(c, pv)| (c.clone(), pv.fpg_season)).collect();
        let lineup = zscores(&ctx.teams.iter().map(|t| (t.team_id.clone(), top_sum(t, &fpg, k))).collect());
        let mut rec: Map<String, f64> = Map::new();
        for t in &ctx.teams {
            if let Some((w, l, ti)) = t.record {
                if w + l + ti > 0 {
                    rec.insert(t.team_id.clone(), (w as f64 + 0.5 * ti as f64) / (w + l + ti) as f64);
                }
            }
        }
        let w_rec = if rec.len() == n { RECORD_WEIGHT_MAX * elapsed(ctx) } else { 0.0 };
        let zr = zscores(&rec);
        let strength: Map<String, f64> = lineup
            .iter()
            .map(|(tid, z)| (tid.clone(), (1.0 - w_rec) * z + w_rec * zr.get(tid).copied().unwrap_or(0.0)))
            .collect();
        return slots_by_strength(&strength);
    }
    let strength: Map<String, f64> = ctx.teams.iter().map(|t| (t.team_id.clone(), top_sum(t, dyn_values, k))).collect();
    let slots = slots_by_strength(&strength);
    let shrink = FUTURE_SLOT_SHRINK.powf(years_ahead as f64);
    slots.into_iter().map(|(tid, s)| (tid, mid + (s - mid) * shrink)).collect()
}

/// Dynasty value by overall pick (index 0 = pick 1): DRAFT_CLASS_DISCOUNT x the league's unproven
/// players' dynasty values, best first.
pub fn slot_curve(ctx: &LeagueContext, dyn_values: &Map<String, f64>) -> Vec<f64> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut anchors: Vec<f64> = Vec::new();
    for p in ctx.all_players() {
        if seen.contains(p.cid.as_str()) || !dyn_values.contains_key(&p.cid) {
            continue;
        }
        seen.insert(p.cid.as_str());
        if is_unproven(p) {
            anchors.push(dyn_values[&p.cid]);
        }
    }
    sort_desc_by(&mut anchors, |a, b| cmp_f64(*a, *b));
    anchors.into_iter().map(|v| DRAFT_CLASS_DISCOUNT * v).collect()
}

/// Dynasty value of one roster spot: the league's lowest replacement FPG held over the dynasty
/// horizon (the mode's year weights plus its terminal weight).
pub fn roster_spot_value(ctx: &LeagueContext, values: &Map<String, PlayerValue>) -> f64 {
    let levels = replacement_for(ctx, values, Horizon::Season);
    let Some(min) = levels.values().copied().reduce(|a, b| if b < a { b } else { a }) else { return 0.0 };
    let horizon = ctx.keeper_horizon_years.max(1);
    let (w, terminal) = mode_weights(Some(ctx.dynasty_mode.as_str()), horizon);
    min * (py_sum(w.iter().copied()) + DISCOUNT.powf(horizon as f64) * terminal)
}

// -- keeper leagues -------------------------------------------------------------------------

/// w_1, the mode's weight on next season (`DISCOUNT` when the horizon is one year).
pub fn next_season_weight(ctx: &LeagueContext) -> f64 {
    let years = ctx.keeper_horizon_years.max(1);
    let (w, _) = mode_weights(Some(ctx.dynasty_mode.as_str()), years);
    if w.len() > 1 {
        w[1]
    } else {
        DISCOUNT
    }
}

/// Next season's per-game value: healthy FPG x trajectory_factor(group, age, 1, pedigree), age and
/// pedigree from the DynastyValue `dv` (birth date and 1.0 without one).
pub fn next_season_fpg(ctx: &LeagueContext, pv: &PlayerValue, dv: Option<&DynastyValue>) -> f64 {
    next_season_fpg_with(&ProductionCurves::current(), ctx, pv, dv)
}

pub(crate) fn next_season_fpg_with(curves: &ProductionCurves, ctx: &LeagueContext, pv: &PlayerValue, dv: Option<&DynastyValue>) -> f64 {
    let p = &pv.player;
    let healthy = healthy_fpg(pv);
    let (age, ped) = match dv {
        Some(dv) => (dv.age, if dv.pedigree == 0.0 { 1.0 } else { dv.pedigree }),
        None => (age_on(p.birth_date, ctx.as_of), 1.0),
    };
    healthy * curves.trajectory_factor(position_group(p), age, 1.0, ped)
}

/// Keeper leagues: value by overall pick (index 0 = pick 1) of next season's redraft, w_1 x
/// next-season value of the pool (every valued player except rostered predicted keepers).
pub fn keeper_slot_curve(
    ctx: &LeagueContext,
    values: &Map<String, PlayerValue>,
    dynasty_values: &Map<String, DynastyValue>,
) -> Vec<f64> {
    keeper_pool(ctx, values, dynasty_values).into_iter().map(|(v, _)| v).collect()
}

/// The redraft pool of [`keeper_slot_curve`] as (w_1 x next-season value, w_1 x the replacement
/// level of the player's own VORP slot), best value first.
fn keeper_pool(
    ctx: &LeagueContext,
    values: &Map<String, PlayerValue>,
    dynasty_values: &Map<String, DynastyValue>,
) -> Vec<(f64, f64)> {
    let curves = ProductionCurves::current();
    let rostered: HashSet<&str> = ctx.teams.iter().flat_map(|t| t.players_iter()).map(|p| p.cid.as_str()).collect();
    let levels = replacement_for(ctx, values, Horizon::Season);
    let w1 = next_season_weight(ctx);
    let mut pool: Vec<(f64, f64)> = Vec::new();
    for (cid, pv) in values {
        let dv = dynasty_values.get(cid);
        if rostered.contains(cid.as_str()) && dv.is_some_and(|d| d.keeper) {
            continue;
        }
        pool.push((next_season_fpg_with(&curves, ctx, pv, dv), own_replacement(&pv.player, &levels).1));
    }
    sort_desc_by(&mut pool, |a, b| cmp_f64(a.0, b.0));
    pool.into_iter().map(|(v, r)| (w1 * v, w1 * r)).collect()
}

/// Keeper leagues: next season's value of the roster spot a draftee takes, repl x w_1.
pub fn keeper_spot_value(ctx: &LeagueContext, values: &Map<String, PlayerValue>) -> f64 {
    min_replacement_fpg(ctx, values) * next_season_weight(ctx)
}

/// Keeper leagues: the roster-spot cost of each redraft pick, aligned with [`keeper_slot_curve`]:
/// w_1 x the replacement level of the drafted player's own VORP slot, so a pick's net is measured
/// over the same positional spot a kept player's surplus is. Unlike Python, which netted every
/// pick against [`keeper_spot_value`] (the league's lowest level). None while the positional
/// dynasty rules are off, which means that flat spot.
pub fn keeper_spot_curve(
    ctx: &LeagueContext,
    values: &Map<String, PlayerValue>,
    dynasty_values: &Map<String, DynastyValue>,
) -> Option<Vec<f64>> {
    if positional_off() {
        return None;
    }
    Some(keeper_pool(ctx, values, dynasty_values).into_iter().map(|(_, s)| s).collect())
}

/// A pick's roster-spot cost: with `spot_curve`, the expected positional spot of its draftee,
/// priced like the pick itself ([`pick_value`]); without one, the flat `spot` discounted to the
/// pick's year.
pub fn pick_spot_cost(
    spot_curve: Option<&[f64]>,
    spot: f64,
    n_teams: i64,
    rnd: i64,
    slot: f64,
    years_ahead: i64,
    elapsed: f64,
) -> f64 {
    match spot_curve {
        Some(sc) => pick_value(sc, n_teams, rnd, slot, years_ahead, elapsed),
        None => spot * DISCOUNT.powf(years_ahead as f64),
    }
}

pub fn curve_at(curve: &[f64], overall: i64) -> f64 {
    if curve.is_empty() {
        return 0.0;
    }
    curve[(overall.max(1) as usize).min(curve.len()) - 1]
}

/// Expected dynasty value of round `rnd` at expected `slot` (module docstring 2-4).
pub fn pick_value(curve: &[f64], n_teams: i64, rnd: i64, slot: f64, years_ahead: i64, elapsed: f64) -> f64 {
    let early = if years_ahead == 0 { SPREAD_EARLY * (1.0 - elapsed) } else { 0.0 };
    let spread = SPREAD_NOW + SPREAD_PER_YEAR * years_ahead as f64 + early;
    let (mut num, mut den) = (0.0, 0.0);
    for t in 1..=n_teams {
        let w = (spread + 1.0 - (t as f64 - slot).abs()).max(0.0);
        if w > 0.0 {
            num += w * curve_at(curve, (rnd - 1) * n_teams + t);
            den += w;
        }
    }
    let v = if den != 0.0 { num / den } else { curve_at(curve, (rnd - 1) * n_teams + py_round(slot) as i64) };
    v * DISCOUNT.powf(years_ahead as f64)
}

/// (holder, year, round, original team, inferred) for every known pick. Teams the provider did not
/// report hold their own picks (rounds and years as in the reported teams) except those another
/// team is known to hold.
pub fn holdings(ctx: &LeagueContext) -> Vec<(String, i64, i64, String, bool)> {
    let known = &ctx.draft_picks;
    let mut out = Vec::new();
    if known.is_empty() {
        return out;
    }
    let mut taken: HashSet<(i64, i64, String)> = HashSet::new();
    let mut years: Vec<i64> = Vec::new();
    let mut rounds: Vec<i64> = Vec::new();
    for (holder, by_year) in known {
        for (year, picks) in by_year {
            years.push(*year);
            for pk in picks {
                let rnd = pk.round;
                rounds.push(rnd);
                let orig = pk.original_owner.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| holder.clone());
                taken.insert((*year, rnd, orig.clone()));
                out.push((holder.clone(), *year, rnd, orig, false));
            }
        }
    }
    years.sort_unstable();
    years.dedup();
    rounds.sort_unstable();
    rounds.dedup();
    for t in &ctx.teams {
        if known.contains_key(&t.team_id) {
            continue;
        }
        for &year in &years {
            for &rnd in &rounds {
                if !taken.contains(&(year, rnd, t.team_id.clone())) {
                    out.push((t.team_id.clone(), year, rnd, t.team_id.clone(), true));
                }
            }
        }
    }
    out
}

/// Every known draft pick with its value, best first ([] outside dynasty leagues or without pick
/// data). Keeper leagues value the redraft of everyone not kept. `dynasty_values` None (or empty):
/// `apply_dynasty(values, ctx)`.
pub fn value_picks(
    ctx: &LeagueContext,
    values: &Map<String, PlayerValue>,
    dynasty_values: Option<&Map<String, DynastyValue>>,
) -> Vec<PickAsset> {
    if !ctx.dynasty || ctx.draft_picks.is_empty() {
        return Vec::new();
    }
    let computed;
    let dynasty_values = match dynasty_values {
        Some(d) => d,
        None => {
            computed = apply_dynasty(values, ctx, None, None);
            &computed
        }
    };
    let dyn_values = dyn_floats(dynasty_values);
    let rows = holdings(ctx);
    if rows.is_empty() {
        return Vec::new();
    }
    let names: Map<&str, &str> = ctx.teams.iter().map(|t| (t.team_id.as_str(), t.name.as_str())).collect();
    let n = ctx.teams.len().max(1) as i64;
    let first = rows.iter().map(|r| r.1).min().expect("rows");
    let (curve, spot, spot_curve) = if keeper_count(ctx).is_none() {
        (slot_curve(ctx, &dyn_values), roster_spot_value(ctx, values), None)
    } else {
        // `_has_keeper_flags`: typed DynastyValues always carry the flag; an empty map is recomputed
        let recomputed;
        let full = if dynasty_values.is_empty() {
            recomputed = apply_dynasty(values, ctx, None, None);
            &recomputed
        } else {
            dynasty_values
        };
        (keeper_slot_curve(ctx, values, full), keeper_spot_value(ctx, values), keeper_spot_curve(ctx, values, full))
    };
    let el = elapsed(ctx);
    let mut slots: Map<i64, Map<String, f64>> = Map::new();
    let mut out: Vec<PickAsset> = Vec::with_capacity(rows.len());
    for (holder, year, rnd, orig, inferred) in rows {
        let ahead = year - first;
        if !slots.contains_key(&ahead) {
            slots.insert(ahead, expected_slots(ctx, values, &dyn_values, ahead));
        }
        let slot = slots[&ahead].get(&orig).copied().unwrap_or((n as f64 + 1.0) / 2.0);
        let value = pick_value(&curve, n, rnd, slot, ahead, el);
        out.push(PickAsset {
            year,
            round: rnd,
            holder,
            original_name: names.get(orig.as_str()).map(|s| s.to_string()).unwrap_or_else(|| orig.clone()),
            original: orig,
            years_ahead: ahead,
            expected_slot: slot,
            overall: (rnd - 1) as f64 * n as f64 + slot,
            value,
            inferred,
            net: (value - pick_spot_cost(spot_curve.as_deref(), spot, n, rnd, slot, ahead, el)).max(0.0),
        });
    }
    out.sort_by(|a, b| cmp_f64(-a.value, -b.value).then(a.year.cmp(&b.year)).then(a.round.cmp(&b.round)));
    out
}

pub fn picks_by_holder(picks: Vec<PickAsset>) -> Map<String, Vec<PickAsset>> {
    let mut out: Map<String, Vec<PickAsset>> = Map::new();
    for a in picks {
        out.entry(a.holder.clone()).or_default().push(a);
    }
    out
}

/// tests/test_picks.py (valuation parts) and the draft-pick half of tests/test_keepers_valuation.py.
#[cfg(test)]
mod tests {
    use super::super::dynasty::keeper_tests::{league, std_league};
    use super::super::dynasty::test_support::*;
    use super::*;
    use fm_core::models::Player;
    use serde_json::{json, Value};

    /// Three teams 'strong' / 'mid' / 'weak'; every player is unproven (career GP 10), so all of
    /// them anchor the slot curve. Dynasty values are `10 * f + i` (DynastyValue.value).
    fn build(picks: Option<Value>, record: Option<Value>) -> (LeagueContext, Map<String, PlayerValue>, Map<String, DynastyValue>) {
        let spec = [("strong", 3.0), ("mid", 2.0), ("weak", 1.0)];
        let mut vals = vec![];
        let mut dynv: Map<String, DynastyValue> = Map::new();
        let mut teams = vec![];
        for (tid, f) in spec {
            let entries: Vec<(&str, Player)> = [("C", "C"), ("LW", "LW"), ("D", "D"), ("D", "D"), ("G", "G"), ("BN", "C")]
                .iter()
                .enumerate()
                .map(|(i, (slot, pos))| {
                    let p = player(json!({"cid": format!("{tid}_{i}"), "team": "EDM", "positions": [pos],
                                          "status": "healthy", "career_gp": 10}));
                    let mut v = pv(&p, f + 0.1 * i as f64, None);
                    v.vorp = v.fpg - 1.0;
                    vals.push(v);
                    let dv = DynastyValue {
                        player: p.clone(), value: 10.0 * f + i as f64, age: None, age_mult: 1.0, upside: 0.0,
                        horizon_years: 3, model_value: None, market_value: None, pedigree: 1.0,
                        mode: "balanced".into(), season_part: 0.0, future: 0.0, keeper_surplus: 0.0, keeper: true,
                        reasons: vec![],
                    };
                    dynv.insert(p.cid.clone(), dv);
                    (*slot, p)
                })
                .collect();
            let slots: Vec<(&str, &Player)> = entries.iter().map(|(s, p)| (*s, p)).collect();
            let mut t = team(tid, &format!("Team {tid}"), tid == "mid", &slots);
            if let Some(r) = record.as_ref().and_then(|r| r.get(tid)) {
                t["record"] = r.clone();
            }
            teams.push(t);
        }
        let picks = picks.unwrap_or_else(|| {
            let mut m = serde_json::Map::new();
            for (tid, _) in spec {
                m.insert(tid.into(), json!({
                    "2027": [{"round": 1, "original_owner": tid}, {"round": 2, "original_owner": tid}],
                    "2028": [{"round": 1, "original_owner": tid}, {"round": 2, "original_owner": tid}]}));
            }
            Value::Object(m)
        });
        let c = ctx(json!({"scoring": {"kind": "points", "weights": {"G": 1.0}},
            "roster_shape": {"C": 1, "LW": 1, "D": 2, "G": 1, "BN": 2, "IR": 1}, "teams": teams,
            "matchup_period": 1, "as_of": "2026-11-01", "season_start": "2026-10-07", "dynasty": true,
            "dynasty_mode": "balanced", "draft_picks": picks}));
        (c, values(vals), dynv)
    }

    fn pick<'a>(picks: &'a [PickAsset], holder: &str, year: i64, rnd: i64) -> &'a PickAsset {
        picks.iter().find(|a| a.holder == holder && a.year == year && a.round == rnd && a.original == holder).unwrap()
    }

    #[test]
    fn elapsed_uses_the_schedules_season_length() {
        // Nov 1 is 25 days after the Oct 7 start
        let (mut c, _, _) = build(None, None);
        assert!((elapsed(&c) - 25.0 / 186.0).abs() < 1e-12);
        // a schedule running Oct 7 to Apr 18 (194 days, like 2026-27)
        let start = c.season_start.unwrap();
        c.schedule.insert("EDM".into(), vec![start, start + chrono::Duration::days(193)]);
        assert!((elapsed(&c) - 25.0 / 194.0).abs() < 1e-12);
    }

    #[test]
    fn weakest_team_picks_first_and_its_pick_is_worth_more() {
        let (c, vals, dynv) = build(None, None);
        let slots = expected_slots(&c, &vals, &dyn_floats(&dynv), 0);
        assert!(slots["weak"] < slots["mid"] && slots["mid"] < slots["strong"]);
        let picks = value_picks(&c, &vals, Some(&dynv));
        assert!(pick(&picks, "weak", 2027, 1).value > pick(&picks, "strong", 2027, 1).value);
        assert!(pick(&picks, "weak", 2027, 1).value > pick(&picks, "weak", 2027, 2).value);
        // best first
        assert!(picks.windows(2).all(|w| w[0].value >= w[1].value));
    }

    #[test]
    fn future_picks_are_discounted_and_pulled_to_the_middle() {
        let (c, vals, dynv) = build(None, None);
        let fl = dyn_floats(&dynv);
        let (now, later) = (expected_slots(&c, &vals, &fl, 0), expected_slots(&c, &vals, &fl, 1));
        assert!((later["weak"] - 2.0).abs() < (now["weak"] - 2.0).abs());
        let picks = value_picks(&c, &vals, Some(&dynv));
        assert!(pick(&picks, "weak", 2028, 1).value < pick(&picks, "weak", 2027, 1).value);
    }

    #[test]
    fn slot_curve_is_league_prospects_discounted() {
        let (c, _, dynv) = build(None, None);
        let curve = slot_curve(&c, &dyn_floats(&dynv));
        close(curve[0], DRAFT_CLASS_DISCOUNT * dynv.values().map(|d| d.value).fold(f64::MIN, f64::max));
        assert!(curve.windows(2).all(|w| w[0] >= w[1]));
    }

    #[test]
    fn record_moves_next_draft_order_late_in_the_season() {
        let rec = json!({"strong": [2, 18, 0], "mid": [10, 10, 0], "weak": [18, 2, 0]});
        let (mut c, vals, dynv) = build(None, Some(rec));
        c.as_of = d(2027, 4, 1);
        let slots = expected_slots(&c, &vals, &dyn_floats(&dynv), 0);
        assert!(slots["strong"] < slots["weak"]);
    }

    #[test]
    fn unreported_teams_hold_their_own_picks_minus_known_ones() {
        let picks = json!({"mid": {"2027": [{"round": 1, "original_owner": "mid"}, {"round": 1, "original_owner": "weak"}]}});
        let (c, _, _) = build(Some(picks), None);
        let rows = holdings(&c);
        assert!(rows.contains(&("mid".into(), 2027, 1, "weak".into(), false)));
        assert!(rows.contains(&("strong".into(), 2027, 1, "strong".into(), true)));
        assert!(!rows.iter().any(|r| r.0 == "weak" && r.3 == "weak"));
    }

    #[test]
    fn net_is_value_over_the_roster_spot() {
        let (c, vals, dynv) = build(None, None);
        let spot = roster_spot_value(&c, &vals);
        let picks = value_picks(&c, &vals, Some(&dynv));
        for a in &picks {
            close(a.net, (a.value - spot * 0.8f64.powi(a.years_ahead as i32)).max(0.0));
        }
    }

    #[test]
    fn no_picks_outside_dynasty() {
        let (mut c, vals, dynv) = build(None, None);
        c.dynasty = false;
        assert!(value_picks(&c, &vals, Some(&dynv)).is_empty());
    }

    #[test]
    fn pick_value_spread_and_rounding() {
        let curve = [10.0, 8.0, 6.0, 4.0, 2.0, 1.0];
        // years ahead 0, season over: spread 1 -> slot 2 weights t=1..3 as 1, 2, 1
        close(pick_value(&curve, 3, 1, 2.0, 0, 1.0), (10.0 + 2.0 * 8.0 + 6.0) / 4.0);
        // a later year is discounted
        // spread 3: weights 3, 4, 3 over overall picks 4..6
        close(pick_value(&curve, 3, 2, 2.0, 1, 1.0), (3.0 * 4.0 + 4.0 * 2.0 + 3.0 * 1.0) / 10.0 * 0.8);
        assert_eq!(curve_at(&curve, 0), 10.0);
        assert_eq!(curve_at(&curve, 99), 1.0);
        assert_eq!(curve_at(&[], 3), 0.0);
    }

    // -- keeper leagues ----------------------------------------------------------------------------

    fn nv(vals: &Map<String, PlayerValue>, dv: &Map<String, DynastyValue>, cid: &str) -> f64 {
        vals[cid].fpg * super::super::dynasty::trajectory_factor("F", dv[cid].age, 1.0, dv[cid].pedigree)
    }

    #[test]
    fn first_pick_is_the_best_non_keeper_next_season() {
        for (horizon, w1) in [(3, 0.8), (1, DISCOUNT)] {
            let (c, vals) = league(Some(3), "mine", horizon, &[], 0);
            let dv = apply_dynasty(&vals, &c, None, None);
            close(next_season_weight(&c), w1);
            let rostered: HashSet<&str> = c.teams.iter().flat_map(|t| t.players_iter()).map(|p| p.cid.as_str()).collect();
            let pool: Vec<&String> =
                vals.keys().filter(|cid| !(rostered.contains(cid.as_str()) && dv[*cid].keeper)).collect();
            assert!(pool.iter().any(|c| *c == "fa_star") && pool.iter().any(|c| *c == "kid"));
            assert!(!pool.iter().any(|c| *c == "s1"));
            let curve = keeper_slot_curve(&c, &vals, &dv);
            let best = pool.iter().map(|c| nv(&vals, &dv, c)).fold(f64::MIN, f64::max);
            close(curve[0], w1 * best);
            assert_eq!(curve.len(), pool.len());
            assert!(curve.windows(2).all(|w| w[0] >= w[1]));
            assert!(w1 * nv(&vals, &dv, "s1") > curve[0]);
        }
    }

    #[test]
    fn keeper_league_picks_are_worth_more_than_dynasty_picks() {
        let prospects = [("pros", 19, 0.4)];
        let (c, vals) = league(Some(3), "mine", 3, &prospects, 0);
        let (kc, _) = league(None, "mine", 3, &prospects, 0);
        let keeper: Map<String, PickAsset> = value_picks(&c, &vals, None).into_iter().map(|a| (a.key(), a)).collect();
        let dynasty: Map<String, PickAsset> = value_picks(&kc, &vals, None).into_iter().map(|a| (a.key(), a)).collect();
        let mut k1: Vec<&String> = keeper.keys().collect();
        let mut k2: Vec<&String> = dynasty.keys().collect();
        k1.sort();
        k2.sort();
        assert_eq!(k1, k2);
        for (key, a) in &keeper {
            assert_eq!(a.expected_slot, dynasty[key].expected_slot);
            if a.round == 1 {
                assert!(a.value > 1.5 * dynasty[key].value, "{key}");
                assert!(a.net > 0.0 && dynasty[key].net == 0.0, "{key}");
            }
        }
        let sum = |m: &Map<String, PickAsset>| m.values().map(|a| a.value).sum::<f64>();
        assert!(sum(&keeper) > 1.3 * sum(&dynasty));
    }

    #[test]
    fn keeper_pick_net_subtracts_next_seasons_roster_spot() {
        let (c, vals) = std_league(Some(3));
        let repl = min_replacement_fpg(&c, &vals);
        let picks = value_picks(&c, &vals, None);
        for a in &picks {
            close(a.net, (a.value - repl * 0.8 * DISCOUNT.powi(a.years_ahead as i32)).max(0.0));
        }
        assert!(picks.iter().any(|a| a.net > 0.0));
    }

    #[test]
    fn keeper_pick_net_is_over_the_draftees_own_positional_spot() {
        let (mut c, vals, mut dynv) = build(None, None);
        c.keepers_per_team = Some(1);
        for dv in dynv.values_mut() {
            dv.keeper = false;
        }
        let levels = replacement_for(&c, &vals, Horizon::Season);
        let w1 = next_season_weight(&c);
        let curve = keeper_slot_curve(&c, &vals, &dynv);
        let spots = keeper_spot_curve(&c, &vals, &dynv).unwrap();
        // every pool player's spot is w_1 x his own slot's level, in the slot curve's order
        let mut pairs: Vec<(f64, f64)> = vals
            .values()
            .map(|pv| (w1 * next_season_fpg(&c, pv, dynv.get(&pv.player.cid)), w1 * own_replacement(&pv.player, &levels).1))
            .collect();
        sort_desc_by(&mut pairs, |a, b| cmp_f64(a.0, b.0));
        assert_eq!((curve.len(), spots.len()), (pairs.len(), pairs.len()));
        for (i, (v, s)) in pairs.iter().enumerate() {
            close(curve[i], *v);
            close(spots[i], *s);
        }
        // the levels differ by position, so some draftees cost more than the league's lowest spot
        let lo = keeper_spot_value(&c, &vals);
        assert!(spots.iter().all(|s| *s >= lo - 1e-12) && spots.iter().any(|s| *s > lo + 1e-9));
        let picks = value_picks(&c, &vals, Some(&dynv));
        assert!(!picks.is_empty());
        for a in &picks {
            let cost = pick_value(&spots, 3, a.round, a.expected_slot, a.years_ahead, elapsed(&c));
            close(a.net, (a.value - cost).max(0.0));
        }
    }

    #[test]
    fn value_picks_in_keeper_mode_accepts_dynasty_values_or_none() {
        let (c, vals) = std_league(Some(3));
        let dv = apply_dynasty(&vals, &c, None, None);
        let rows = |d: Option<&Map<String, DynastyValue>>| -> Vec<(String, f64, f64)> {
            value_picks(&c, &vals, d).into_iter().map(|a| (a.key(), a.value, a.net)).collect()
        };
        let expected = rows(Some(&dv));
        assert!(!expected.is_empty());
        assert_eq!(rows(None), expected);
    }
}
