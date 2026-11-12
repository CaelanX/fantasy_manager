//! Rust port of `fantasy_manager/recommend/waivers.py`: waiver-wire pickups (open spot / IR move /
//! weakest same-slot drop, empty starting slots, dynasty and keeper-league long-term rules, the
//! move budget and churn guard, this-matchup pickups judged by the best lineup each day) and the
//! trending-ownership alerts. Owner: work package `W6-lineup-waivers`.
//!
//! Deviations from Python (behaviour-preserving):
//! * Keyword arguments are a [`WaiverOpts`]; the clock (`now`) and the schedule memo are explicit.
//! * Free agents are evaluated in parallel (rayon) and merged back in `ctx.free_agents` order
//!   before the same stable sort, so results and `debug` entries are in Python's order.
//! * Each free agent's day-by-day plan reuses the roster's per-day values ([`PlanCache`]) and
//!   applies the roster change on a borrowed [`RosterView`] instead of copying the team and the
//!   values dict (`{**values, fa.cid: fv}`).
//! * `debug` entries are [`WaiverDebug`] structs (same keys as the Python dicts).
//! * Where Python would raise `KeyError` (a displaced starter without a value), the comparison
//!   player is dropped instead.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use chrono::{Datelike, Duration, NaiveDate};
use fm_core::models::{GainUnits, LeagueContext, Player, Reason, RecKind, Recommendation, Status};
use fm_core::normalize::normalize_team;
use fm_core::values::{DynastyValue, Horizon, PlayerValue};
use fm_core::{Cid, CoreError, Map, PyDateTime};
use fm_pycompat::fmt::{fixed, general, percent};
use fm_pycompat::num::round_to;
use fm_pycompat::order::{cmp_f64, sort_desc_by};
use fm_pycompat::time::strftime_date;
use rayon::prelude::*;
use serde::Serialize;

use crate::recommend::base::{
    add_scarcity_threshold_at, churn_blocked_at, churn_text, confidence, droppable_players, dynasty_floats, dynasty_objects, evidence_games,
    free_ir_slots, has_data, ir_movable, keeper_k, move_scarcity_threshold_at, moves_budget_reason, open_roster_spots,
    protected_reason, recently_added, roster_legal_after, shares_default_slot, swap_values, CHURN_EXEMPT_STATUSES,
    CHURN_OVERRIDE_GAIN, INJURY_REPLACEMENT_STATUSES, league_unit, PROTECT_OVERRIDE, RECENT_ADD_DAYS,
};
use crate::recommend::lineup::{dp_idx, plan_days, starting_slots, DailyPlan, Entry, PlanCache, RosterView};
use crate::recommend::lineup_lock;
use crate::recommend::market::{market_check_reasons, MarketPool};
use crate::recommend::started::{roster_value, StartWindow};
use crate::recommend::trades::SlotSpec;
use crate::recommend::strength::{apply_ranks, apply_strength};
use crate::scoring::from_config;
use crate::valuation::params::START_WINDOW_DAYS;
use crate::valuation::schedule::{context_window, ScheduleMemo, WEEK_DAYS};
use crate::valuation::valuate::valuate_league;

pub const MIN_GAIN: f64 = 0.3;
pub const DYNASTY_MIN_RATIO: f64 = 1.15;
/// Contend mode may drop a protected player for an add that gains this many FPG (ESPN points;
/// times the league unit, as is CONTEND_LOW_FPG) when he projects to contribute little this season.
pub const CONTEND_CLEAR_GAIN: f64 = 1.0;
pub const CONTEND_LOW_GP: i64 = 40;
pub const CONTEND_LOW_FPG: f64 = 2.0;
// Market timing (% rostered change, in percentage points over the provider's trend window)
pub const RISER_MIN: f64 = 3.0;
pub const RISER_MAX_BOOST: f64 = 0.5;
pub const RISER_SCALE: f64 = 20.0;
pub const ALERT_MIN_CHANGE: f64 = 5.0;
/// minutes per game above the season baseline
pub const ALERT_TOI_TREND_MIN: f64 = 0.5;
pub const ALERT_PP_SHARE_TREND_MIN: f64 = 0.05;

/// This-matchup pickups: dynasty value the add may give up vs the drop, as a share of the drop's.
pub fn stream_dynasty_tolerance(mode: &str) -> f64 {
    match mode {
        "contend" => 0.25,
        "balanced" => 0.15,
        "rebuild" => 0.0,
        _ => 0.0,
    }
}

/// Score multiplier for a pickup whose % rostered moved `change` points this week.
pub fn market_multiplier(change: Option<f64>) -> f64 {
    match change {
        Some(c) if c >= RISER_MIN => 1.0 + RISER_MAX_BOOST.min(c / RISER_SCALE),
        _ => 1.0,
    }
}

/// OWNERSHIP_TREND: "X rostered 32% (+9.5 this week)", with a note for risers / fallers.
pub fn ownership_trend_reason(p: &Player) -> Option<Reason> {
    let chg = p.pct_owned_change?;
    let owned = match p.pct_owned {
        Some(o) => format!("rostered {}%", fixed(o, 0, false)),
        None => "% rostered".to_string(),
    };
    let mut text = format!("{} {owned} ({} this week)", p.name, fixed(chg, 1, true));
    let mult = market_multiplier(Some(chg));
    if mult > 1.0 {
        text += &format!(": rising, grab him before the league does (score x{})", fixed(mult, 2, false));
    } else if chg <= -RISER_MIN {
        text += ": falling league-wide, no rush (others are dropping him)";
    }
    Some(Reason::new("OWNERSHIP_TREND", text).with_value(Some(chg)).with_baseline(p.pct_owned))
}

/// A free agent still on waivers can't be added before the claim clears: WAIVER_CLAIM reason, and
/// `fv` with proj_week / fpg_week scaled to the games his team plays after the clear date.
pub fn waiver_adjust<'v>(ctx: &LeagueContext, fa: &Player, fv: &'v PlayerValue) -> (Cow<'v, PlayerValue>, Option<Reason>) {
    let Some(until) = fa.waiver_until.filter(|u| *u >= ctx.as_of) else { return (Cow::Borrowed(fv), None) };
    let mut text = format!("{} is on waivers until {}: claim, won't be available before then", fa.name, strftime_date(until, "%a"));
    let window = context_window(ctx, WEEK_DAYS);
    let team = normalize_team(fa.team.as_deref()).unwrap_or_default();
    let dates: &[NaiveDate] = match &window {
        Some(_) => ctx.schedule.get(&team).map(|v| v.as_slice()).unwrap_or(&[]),
        None => &[],
    };
    let (Some(window), Some(proj)) = (window, fv.proj_week) else {
        return (Cow::Borrowed(fv), Some(Reason::new("WAIVER_CLAIM", text)));
    };
    if dates.is_empty() {
        return (Cow::Borrowed(fv), Some(Reason::new("WAIVER_CLAIM", text)));
    }
    let end = window.start + Duration::days(window.days);
    let mut in_win: Vec<NaiveDate> = dates.iter().copied().filter(|d| window.start <= *d && *d < end).collect();
    in_win.sort();
    in_win.dedup();
    let left = in_win.iter().filter(|d| **d > until).count() as i64;
    let share = if in_win.is_empty() { 0.0 } else { left as f64 / in_win.len() as f64 };
    text += &format!(" ({left} of {} games {} count)", in_win.len(), window.label());
    let mut adj = fv.clone();
    adj.proj_week = Some(proj * share);
    adj.fpg_week = fv.fpg_week * share;
    adj.games_next7 = Some(left);
    (
        Cow::Owned(adj),
        Some(Reason::new("WAIVER_CLAIM", text).with_value(Some(left as f64)).with_baseline(Some(in_win.len() as f64))),
    )
}

/// Why `p` projects to contribute little this season, or None. The FPG bar is CONTEND_LOW_FPG x
/// `unit` (the league unit, [`crate::recommend::base::league_unit`]); Python used the bare 2.0 in
/// every league.
fn low_this_season(p: &Player, pv: &PlayerValue, unit: f64) -> Option<String> {
    let gp = p.lines.get("projected").map_or(0, |l| l.gp);
    if gp < CONTEND_LOW_GP {
        return Some(format!("projected {gp} GP this season"));
    }
    if pv.fpg_season < CONTEND_LOW_FPG * unit {
        return Some(format!("projected {} FPG this season", fixed(pv.fpg_season, 2, false)));
    }
    None
}

/// `_lineup_fpg`: (FPG total, starter cids) of the optimal starting lineup from `players`.
fn lineup_fpg<'p>(players: &[&'p Player], slots: &[String], val: &dyn Fn(&Player) -> f64) -> (f64, HashSet<&'p str>) {
    let w: Vec<f64> = players.iter().map(|p| val(p)).collect();
    let (total, assign) = dp_idx(players, slots, |i, _| w[i]);
    (total, assign.into_iter().map(|(_, i)| players[i].cid.as_str()).collect())
}

/// One drop / add the guards blocked (`debug` of [`recommend_waivers`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WaiverDebug {
    pub add: String,
    pub drop: Option<String>,
    pub add_value: Option<f64>,
    pub drop_value: Option<f64>,
    pub reason: Reason,
}

/// Keyword arguments of `recommend_waivers`.
#[derive(Clone, Debug)]
pub struct WaiverOpts<'a> {
    pub limit: usize,
    pub horizon: Horizon,
    pub dynasty_values: Option<&'a Map<Cid, DynastyValue>>,
    pub stream: bool,
    /// cids that are never the drop
    pub exclude_drops: &'a [Cid],
}

impl Default for WaiverOpts<'_> {
    fn default() -> Self {
        WaiverOpts { limit: 10, horizon: Horizon::Season, dynasty_values: None, stream: false, exclude_drops: &[] }
    }
}

/// (gain, units, horizon days) of a pickup (see the Python docstring).
pub fn predicted_gain(add: &PlayerValue, cmp: Option<&PlayerValue>, fpg_gain: f64, horizon: Horizon) -> (f64, GainUnits, Option<i64>) {
    if horizon == Horizon::Week {
        if let Some(pw) = add.proj_week {
            match cmp {
                None => return (pw, GainUnits::WeekPts, Some(7)),
                Some(c) if c.proj_week.is_some() => return (pw - c.proj_week.unwrap_or(0.0), GainUnits::WeekPts, Some(7)),
                _ => {}
            }
        }
        return (fpg_gain, GainUnits::SeasonFpg, Some(7));
    }
    (fpg_gain, GainUnits::SeasonFpg, None)
}

fn cmp_key(a: (f64, f64), b: (f64, f64)) -> std::cmp::Ordering {
    cmp_f64(a.0, b.0).then_with(|| cmp_f64(a.1, b.1))
}

fn key_le(a: (f64, f64), b: (f64, f64)) -> bool {
    cmp_key(a, b) != std::cmp::Ordering::Greater
}

fn status_text(p: &Player) -> String {
    let note = p.status_note.as_deref().map_or(String::new(), |n| format!(" ({n})"));
    format!("{} is {}{note}", p.name, p.status.as_str())
}

/// Ranked waiver pickups (see the module docs). `debug` collects drops that prospect protection,
/// the churn guard or a position cap blocked. [] when the league's move limit is used up. Unlike
/// Python, a season-horizon pickup in a daily-lineup league is scored, and its drop chosen, by the
/// roster's started-games value (`recommend::started`) rather than by best-lineup FPG.
pub fn recommend_waivers(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    opts: &WaiverOpts<'_>,
    debug: Option<&mut Vec<WaiverDebug>>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Result<Vec<Recommendation>, CoreError> {
    // the league unit, computed once: the bars below are checked for every add / drop pair
    let unit = league_unit(ctx);
    if move_scarcity_threshold_at(ctx, true, unit).is_none() {
        return Ok(vec![]); // no moves left this period
    }
    let churn_gain = CHURN_OVERRIDE_GAIN * unit;
    let contend_gain = CONTEND_CLEAR_GAIN * unit;
    let horizon = opts.horizon;
    let stream = opts.stream;
    let want_debug = debug.is_some();
    let budget = moves_budget_reason(ctx);
    let team = ctx.my_team()?;
    let dobj = dynasty_objects(ctx, values, opts.dynasty_values);
    let dobj_ref: Option<&Map<Cid, DynastyValue>> = dobj.as_deref();
    let dyn_ = dynasty_floats(dobj_ref);
    let keepers = if dyn_.is_some() { keeper_k(ctx, dobj_ref) } else { None };
    let my_roster: Vec<&Player> = team.players();
    let mode = ctx.dynasty_mode.as_str();
    let contend = mode == "contend";
    let free_ir = free_ir_slots(team, &ctx.roster_shape);
    let to_ir: Vec<&Player> =
        ir_movable(team, Some(values)).into_iter().filter(|p| values.contains_key(&p.cid)).take(free_ir.max(0) as usize).collect();
    let open_spots = open_roster_spots(team, &ctx.roster_shape);
    let ir_move: Option<&Player> = if open_spots == 0 { to_ir.first().copied() } else { None };
    let moving: HashSet<&str> = to_ir.iter().map(|p| p.cid.as_str()).collect();
    let active: Vec<&Player> = droppable_players(team);
    let mine: Vec<&Player> = active
        .iter()
        .copied()
        .filter(|p| values.contains_key(&p.cid) && !moving.contains(p.cid.as_str()) && !opts.exclude_drops.contains(&p.cid))
        .collect();
    let slots = starting_slots(&ctx.roster_shape);
    let fpg_of = |p: &Player| values.get(&p.cid).map_or(0.0, |pv| pv.fpg_for(horizon));

    // optimal lineup without the FA, per IR-move player (or None)
    let base_pool = |to_ir: Option<&Player>| -> Vec<&Player> {
        active.iter().copied().filter(|p| to_ir.is_none_or(|x| p.cid != x.cid)).collect()
    };
    let mut base_lineup: HashMap<Option<&str>, (f64, HashSet<&str>)> = HashMap::new();
    base_lineup.insert(None, lineup_fpg(&base_pool(None), &slots, &fpg_of));
    if let Some(x) = ir_move {
        base_lineup.insert(Some(x.cid.as_str()), lineup_fpg(&base_pool(Some(x)), &slots, &fpg_of));
    }

    // daily lineups: the best lineup each day over the rest of the matchup
    let daily = horizon == Horizon::Week && !ctx.schedule.is_empty() && lineup_lock::lock(ctx) == "daily";
    let view = RosterView::of(team);
    let cache = if daily {
        Some(PlanCache::new(ctx, memo, values, plan_days(ctx), now, team.players_iter()))
    } else {
        None
    };
    let base_plan: Option<DailyPlan> = cache.as_ref().map(|c| c.plan(&view, &[], true));
    let mut plan_games = 0.0;
    if let Some(bp) = &base_plan {
        let days: HashSet<NaiveDate> = bp.days.iter().map(|d| d.day).collect();
        let n: usize = ctx.schedule.values().map(|ds| ds.iter().filter(|d| days.contains(d)).count()).sum();
        plan_games = n as f64 / ctx.schedule.len().max(1) as f64;
    }
    let base_optimal = base_plan.as_ref().map(|p| p.optimal());

    let plan_gain = |fa: &Player, fv: &PlayerValue, drop: Option<&Player>, to_ir: Option<&Player>| -> Option<f64> {
        let (cache, base) = (cache.as_ref()?, base_optimal?);
        if plan_games <= 0.0 {
            return None;
        }
        let mut v = RosterView {
            entries: view
                .entries
                .iter()
                .filter(|e| drop.is_none_or(|d| e.player.cid != d.cid))
                .map(|e| {
                    if to_ir.is_some_and(|x| x.cid == e.player.cid) {
                        Entry { slot: "IR", player: e.player, starting: false }
                    } else {
                        *e
                    }
                })
                .collect(),
        };
        v.entries.push(Entry { slot: "BN", player: fa, starting: false });
        Some(cache.plan(&v, &[(fa.cid.as_str(), fv)], false).optimal() - base)
    };

    let lt_pair = |add: Option<&Player>, drop: Option<&Player>| -> (f64, f64) {
        match keepers {
            None => {
                let d = dyn_.as_ref();
                (
                    add.and_then(|p| d.and_then(|m| m.get(&p.cid))).copied().unwrap_or(0.0),
                    drop.and_then(|p| d.and_then(|m| m.get(&p.cid))).copied().unwrap_or(0.0),
                )
            }
            Some(_) => {
                // swap_values needs one lifetime for roster, add and drop
                let roster: Vec<&Player> = my_roster.clone();
                swap_values(ctx, dobj_ref, roster.iter().copied(), add, drop)
            }
        }
    };

    let mut protected: HashMap<&str, String> = HashMap::new();
    let mut contrib: HashMap<&str, f64> = HashMap::new();
    if dyn_.is_some() {
        for p in &mine {
            if let Some(why) = protected_reason(p, ctx, opts.dynasty_values, dobj_ref) {
                protected.insert(p.cid.as_str(), why);
            }
            if keepers.is_some() {
                contrib.insert(p.cid.as_str(), lt_pair(None, Some(p)).1);
            }
        }
    }

    // empty starting slots: lineup pool incl. IR-slot players (their value counts the games they miss)
    let in_ir: Vec<&Player> = team
        .slots
        .iter()
        .filter(|s| s.slot == "IR")
        .filter_map(|s| s.player.as_ref())
        .filter(|p| values.contains_key(&p.cid))
        .collect();
    let fill_pool: Vec<&Player> = active.iter().copied().filter(|p| values.contains_key(&p.cid)).chain(in_ir).collect();
    let (fill_before, fill_starters) = lineup_fpg(&fill_pool, &slots, &fpg_of);
    let slots_empty = fill_starters.len() < slots.len();
    // season horizon in a daily-lineup league: a pickup's gain is the change in the roster's
    // started-games value (`recommend::started`), so a bench add counts the games he would start
    let start_win = if horizon == Horizon::Season { StartWindow::of(ctx) } else { None };
    let spec = SlotSpec::new(&ctx.roster_shape);
    let started_before = start_win.as_ref().map(|w| roster_value(w, &spec, &fill_pool, &fpg_of));
    // what dropping each player costs in started-games value; it replaces his own FPG in the drop
    // order below, so the drop is the player whose starts the roster misses least (a rarely used
    // bench player before a third goalie who starts most of his team's games)
    let started_loss: HashMap<&str, f64> = match (start_win.as_ref(), started_before) {
        (Some(w), Some(before)) => mine
            .iter()
            .map(|d| {
                let pool: Vec<&Player> = fill_pool.iter().copied().filter(|p| p.cid != d.cid).collect();
                (d.cid.as_str(), before - roster_value(w, &spec, &pool, &fpg_of))
            })
            .collect(),
        _ => HashMap::new(),
    };

    let drop_key = |p: &Player| -> (f64, f64) {
        let pv = &values[&p.cid];
        let fpg = started_loss.get(p.cid.as_str()).copied().unwrap_or_else(|| pv.fpg_for(horizon));
        if let Some(d) = dyn_.as_ref().and_then(|d| d.get(&p.cid)) {
            return (if keepers.is_none() { *d } else { contrib.get(p.cid.as_str()).copied().unwrap_or(0.0) }, fpg);
        }
        if stream {
            return (pv.fpg_season, fpg);
        }
        (fpg, fpg)
    };

    let market = MarketPool::new(ctx, Some(values), None);
    let tol = stream_dynasty_tolerance(mode);

    let evaluate = |fa: &Player| -> (Option<Recommendation>, Vec<WaiverDebug>) {
        let mut dbg: Vec<WaiverDebug> = Vec::new();
        if normalize_team(fa.team.as_deref()).is_none_or(|t| t.is_empty()) {
            return (None, dbg); // unsigned: his value assumes games he won't play
        }
        let Some(fv0) = values.get(&fa.cid) else { return (None, dbg) };
        if fv0.vorp_for(horizon) <= 0.0 && !stream {
            return (None, dbg);
        }
        let (fv, waiver) = waiver_adjust(ctx, fa, fv0);
        let fv: &PlayerValue = &fv;
        let fa_fpg = fv.fpg_for(horizon);
        let with_fa_val = |p: &Player| if p.cid == fa.cid { fa_fpg } else { fpg_of(p) };
        let mut candidates: Vec<&Player> = mine.iter().copied().filter(|p| shares_default_slot(fa, p)).collect();
        let mut empty_fill = false;
        if slots_empty {
            let pool: Vec<&Player> = fill_pool.iter().copied().chain(std::iter::once(fa)).collect();
            let (with_fa, _) = lineup_fpg(&pool, &slots, &with_fa_val);
            if with_fa - fill_before >= fa_fpg - 1e-9 && fa_fpg - 1e-9 > 0.0 {
                empty_fill = true;
                candidates = mine.iter().copied().filter(|p| !fill_starters.contains(p.cid.as_str())).collect();
            }
        }
        let mut drop: Option<&Player> = None;
        let mut extra: Vec<Reason> = Vec::new();
        let mut cap_why: Option<String> = None;
        let (mut spot_open, mut spot_ir) = (open_spots > 0, ir_move);
        if spot_open || spot_ir.is_some() {
            let ir: Vec<&Player> = spot_ir.into_iter().collect();
            let (ok, why) = roster_legal_after(team, ctx, &[fa], &[], &ir);
            if !ok {
                cap_why = why;
                spot_open = false;
                spot_ir = None;
            }
        }
        if !spot_open && spot_ir.is_none() {
            let mut pool: Vec<&Player> =
                candidates.iter().copied().filter(|p| dyn_.is_none() || has_data(values.get(&p.cid))).collect();
            pool.sort_by(|a, b| cmp_key(drop_key(a), drop_key(b)));
            let legal: Vec<&Player> = pool.iter().copied().filter(|p| roster_legal_after(team, ctx, &[fa], &[p], &[]).0).collect();
            if legal.len() < pool.len() && cap_why.is_none() {
                cap_why = roster_legal_after(team, ctx, &[fa], &[], &[]).1;
            }
            let pool_legal = legal;
            // churn guard: keep players I just added unless the upgrade is big or they are hurt
            let mut kept_recent: Vec<&Player> = Vec::new();
            let mut unguarded: Vec<&Player> = Vec::new();
            for p in pool_legal {
                let pv = &values[&p.cid];
                let g_p = fa_fpg - pv.fpg_for(horizon);
                if churn_blocked_at(ctx, p, g_p, churn_gain) {
                    kept_recent.push(p);
                    if want_debug {
                        dbg.push(WaiverDebug {
                            add: fa.name.clone(),
                            drop: Some(p.name.clone()),
                            add_value: Some(fa_fpg),
                            drop_value: Some(pv.fpg_for(horizon)),
                            reason: Reason::new(
                                "CHURN_GUARD",
                                format!(
                                    "Not dropping {} ({}): {} is only {} FPG over him (needs +{} or an injury)",
                                    p.name,
                                    churn_text(ctx, p),
                                    fa.name,
                                    fixed(g_p, 2, true),
                                    general(round_to(churn_gain, 2), None, false)
                                ),
                            )
                            .with_value(Some(g_p))
                            .with_baseline(Some(churn_gain)),
                        });
                    }
                } else {
                    unguarded.push(p);
                }
            }
            let mut pool = unguarded;
            if pool.is_empty() && want_debug {
                if let Some(why) = &cap_why {
                    dbg.push(WaiverDebug {
                        add: fa.name.clone(),
                        drop: None,
                        add_value: None,
                        drop_value: None,
                        reason: Reason::new(
                            "POSITION_CAP",
                            format!("Not adding {}: {why} and no droppable same-position player", fa.name),
                        ),
                    });
                }
            }
            if dyn_.is_some() {
                let mut chosen: Option<&Player> = None;
                for &p in &pool {
                    let (a_val, d_val) = lt_pair(Some(fa), Some(p));
                    let why = protected.get(p.cid.as_str());
                    let Some(why) = why.filter(|_| !(a_val >= PROTECT_OVERRIDE * d_val)) else {
                        chosen = Some(p);
                        break;
                    };
                    let pv = &values[&p.cid];
                    let gain_p = fa_fpg - pv.fpg_for(horizon);
                    let low = low_this_season(p, pv, unit);
                    if contend && gain_p >= contend_gain {
                        if let Some(low) = low {
                            chosen = Some(p);
                            extra.push(
                                Reason::new(
                                    "CONTEND_DROP",
                                    format!(
                                        "Contend mode: {why}, but {low}; {} is a clear this-season upgrade (+{} FPG)",
                                        fa.name,
                                        fixed(gain_p, 2, false)
                                    ),
                                )
                                .with_value(Some(gain_p))
                                .with_baseline(Some(contend_gain)),
                            );
                            break;
                        }
                    }
                    if want_debug {
                        dbg.push(WaiverDebug {
                            add: fa.name.clone(),
                            drop: Some(p.name.clone()),
                            add_value: Some(a_val),
                            drop_value: Some(d_val),
                            reason: Reason::new(
                                "PROSPECT_PROTECTED",
                                format!(
                                    "Not dropping {}: {why} ({} {} < {} x {})",
                                    p.name,
                                    fa.name,
                                    fixed(a_val, 2, false),
                                    general(PROTECT_OVERRIDE, None, false),
                                    fixed(d_val, 2, false)
                                ),
                            )
                            .with_value(Some(a_val))
                            .with_baseline(Some(d_val)),
                        });
                    }
                }
                pool = chosen.into_iter().collect();
            }
            let Some(&d) = pool.first() else { return (None, dbg) };
            drop = Some(d);
            if !kept_recent.is_empty() {
                let dk = drop_key(d);
                let better: Vec<&Player> = kept_recent.iter().copied().filter(|p| key_le(drop_key(p), dk)).collect();
                if !better.is_empty() {
                    let names = better.iter().map(|p| format!("{} ({})", p.name, churn_text(ctx, p))).collect::<Vec<_>>().join(", ");
                    extra.push(Reason::new(
                        "CHURN_GUARD",
                        format!(
                            "Keeping {names}: just added, so {} is the drop instead (a recent add is only dropped for +{} FPG or an injury)",
                            d.name,
                            general(round_to(churn_gain, 2), None, false)
                        ),
                    ));
                }
            }
            if recently_added(ctx, d, RECENT_ADD_DAYS) {
                let g_d = fa_fpg - values[&d.cid].fpg_for(horizon);
                let why_ok = if CHURN_EXEMPT_STATUSES.contains(&d.status.as_str()) {
                    format!("he is {}", d.status.as_str())
                } else {
                    format!("{} is +{} FPG over him", fa.name, fixed(g_d, 2, false))
                };
                extra.push(
                    Reason::new("CHURN_GUARD", format!("{} was {}, dropping him anyway: {why_ok}", d.name, churn_text(ctx, d)))
                        .with_value(Some(g_d))
                        .with_baseline(Some(churn_gain)),
                );
            }
        }
        let mut gain: f64;
        let mut cmp: Option<&Player>;
        if let (Some(d), true) = (drop, empty_fill) {
            cmp = Some(d);
            let pool: Vec<&Player> = fill_pool.iter().copied().filter(|p| p.cid != d.cid).chain(std::iter::once(fa)).collect();
            let (after_fill, _) = lineup_fpg(&pool, &slots, &with_fa_val);
            gain = after_fill - fill_before;
        } else if let Some(d) = drop {
            cmp = Some(d);
            gain = fa_fpg - values[&d.cid].fpg_for(horizon);
        } else {
            // no_drop_gain: marginal lineup FPG of adding fa without a drop
            let key = spot_ir.map(|x| x.cid.as_str());
            let (before, starters) = match base_lineup.get(&key) {
                Some(b) => (b.0, b.1.clone()),
                None => lineup_fpg(&base_pool(spot_ir), &slots, &fpg_of),
            };
            let pool: Vec<&Player> = base_pool(spot_ir).into_iter().chain(std::iter::once(fa)).collect();
            let (after, new_starters) = lineup_fpg(&pool, &slots, &with_fa_val);
            let out: Vec<&Player> = base_pool(spot_ir)
                .into_iter()
                .filter(|p| starters.contains(p.cid.as_str()) && !new_starters.contains(p.cid.as_str()))
                .collect();
            gain = after - before;
            cmp = fm_pycompat::order::first_min_by(out, |a, b| cmp_f64(fpg_of(a), fpg_of(b)));
        }
        if let (Some(w), Some(before)) = (start_win.as_ref(), started_before) {
            let pool: Vec<&Player> =
                fill_pool.iter().copied().filter(|p| drop.is_none_or(|d| p.cid != d.cid)).chain(std::iter::once(fa)).collect();
            gain = roster_value(w, &spec, &pool, &with_fa_val) - before;
        }
        let mut cv: Option<&PlayerValue> = cmp.and_then(|c| values.get(&c.cid));
        if cv.is_none() {
            cmp = None;
        }
        let matchup_pts = plan_gain(fa, fv, drop, if drop.is_some() || spot_open { None } else { spot_ir });
        if let Some(mp) = matchup_pts {
            gain = mp / plan_games;
            if drop.is_none() {
                // daily lineups: nobody is benched for the week
                cmp = None;
                cv = None;
            }
        }
        let injury_replacement =
            spot_ir.is_some() || drop.is_some_and(|d| INJURY_REPLACEMENT_STATUSES.contains(&d.status.as_str()));
        let threshold = match add_scarcity_threshold_at(ctx, fa, injury_replacement, unit) {
            Some(t) if gain > t => t,
            _ => return (None, dbg),
        };
        let mut dyn_gain: Option<f64> = None;
        if dyn_.is_some() {
            let (a_val, d_val) = lt_pair(Some(fa), drop);
            if drop.is_some() {
                // this matchup only, or a season-long empty starting slot: a small, stated long-term cost is allowed
                if stream || empty_fill {
                    if a_val < (1.0 - tol) * d_val {
                        return (None, dbg);
                    }
                } else if a_val < DYNASTY_MIN_RATIO * d_val || a_val <= d_val {
                    return (None, dbg); // never cut a comparable long-term asset for a short-term bump
                }
                dyn_gain = Some(a_val - d_val);
            } else {
                dyn_gain = Some(a_val);
            }
        } else if let (true, Some(d)) = (stream, drop) {
            if fv.fpg_season < (1.0 - tol) * values[&d.cid].fpg_season {
                return (None, dbg);
            }
        }
        let conf = confidence(fa);
        let gp = fa.gp("season");
        let fill = format!("fills empty {} slot", fa.positions.join("/"));
        let bench_drop = match (empty_fill, drop) {
            (true, Some(d)) => {
                let pos: Vec<&str> = d.positions.iter().map(|s| s.as_str()).filter(|x| *x != "F").collect();
                let pos = if pos.is_empty() { "F".to_string() } else { pos.join("/") };
                format!("; drop bench {pos} {}", d.name)
            }
            _ => String::new(),
        };
        let h = horizon.as_str();
        let g2 = fixed(gain, 2, false);
        let delta = if matchup_pts.is_some() {
            format!("+{g2} FPG ({h}, best lineup each day)")
                + &if !bench_drop.is_empty() {
                    format!(" ({fill}{bench_drop})")
                } else if let Some(d) = drop {
                    format!(" over dropping {}", d.name)
                } else {
                    String::new()
                }
        } else if !bench_drop.is_empty() {
            format!("{fill} (+{g2} FPG, {h}){bench_drop}")
        } else if let Some(c) = cmp {
            let over = match drop {
                Some(d) => format!("over dropping {}", d.name),
                None => format!("over benching {}", c.name),
            };
            format!("+{g2} FPG ({h}) {over}")
        } else {
            format!("{fill} (+{g2} FPG, {h})")
        };
        let mut reasons = vec![
            Reason::new("VORP_DELTA", delta).with_value(Some(gain)).with_baseline(Some(threshold)),
            Reason::new(
                "FPG_ADD",
                format!("{}: {} FPG, VORP {}", fa.name, fixed(fa_fpg, 2, false), fixed(fv.vorp_for(horizon), 2, true)),
            )
            .with_value(Some(fa_fpg)),
        ];
        if started_before.is_some() {
            reasons.push(Reason::new(
                "STARTED_GAMES",
                format!(
                    "Gain counts the games each player would start: the best lineup each day over the next {START_WINDOW_DAYS} days of the NHL schedule"
                ),
            ));
        }
        if let (Some(c), Some(cvv)) = (cmp, cv) {
            reasons.push(
                Reason::new(
                    "FPG_DROP",
                    format!(
                        "{}: {} FPG, VORP {}",
                        c.name,
                        fixed(cvv.fpg_for(horizon), 2, false),
                        fixed(cvv.vorp_for(horizon), 2, true)
                    ),
                )
                .with_value(Some(cvv.fpg_for(horizon))),
            );
        }
        if let (Some(mp), Some(bp)) = (matchup_pts, base_plan.as_ref()) {
            let last = bp.days[bp.days.len() - 1].day;
            reasons.insert(
                1,
                Reason::new(
                    "MATCHUP_GAIN",
                    format!(
                        "{} pts through {} {} with the best lineup set each day",
                        fixed(mp, 1, true),
                        strftime_date(last, "%a %b"),
                        last.day()
                    ),
                )
                .with_value(Some(mp)),
            );
        }
        let prior = fm_pycompat::num::py_round_i64(evidence_games(fa) - gp as f64);
        reasons.push(
            Reason::new(
                "GP",
                format!(
                    "{gp} GP this season{} -> confidence {}",
                    if prior != 0 {
                        format!(" + {prior} weighted from the last 3 seasons")
                    } else {
                        ", no prior NHL games".to_string()
                    },
                    fixed(conf, 2, false)
                ),
            )
            .with_value(Some(gp as f64))
            .with_baseline(Some(conf)),
        );
        if let Some(w) = waiver {
            reasons.insert(1, w);
        }
        if let (Some(why), Some(d)) = (&cap_why, drop) {
            reasons.push(Reason::new(
                "POSITION_CAP",
                format!(
                    "League roster maximum ({why}): drop {} ({}) to make room",
                    d.name,
                    if empty_fill { "bench" } else { "same position" }
                ),
            ));
        }
        if spot_open {
            let tail = if matchup_pts.is_some() {
                String::new()
            } else if let Some(c) = cmp {
                format!(" ({} goes to the bench)", c.name)
            } else {
                format!(" ({fill})")
            };
            reasons.push(
                Reason::new(
                    "OPEN_SPOT",
                    format!(
                        "{open_spots} open roster spot{}: no drop needed{tail}",
                        if open_spots != 1 { "s" } else { "" }
                    ),
                )
                .with_value(Some(open_spots as f64)),
            );
        } else if let Some(x) = spot_ir {
            reasons.push(
                Reason::new(
                    "IR_MOVE",
                    format!(
                        "Move {} ({}) to IR ({free_ir} IR slot{} free): no drop needed",
                        x.name,
                        fm_pycompat::text::py_upper(x.status.as_str()),
                        if free_ir != 1 { "s" } else { "" }
                    ),
                )
                .with_value(Some(free_ir as f64)),
            );
        }
        match drop {
            Some(d) if dyn_.is_some() && (stream || empty_fill) => {
                let (a_val, d_val) = lt_pair(Some(fa), Some(d));
                let unit = match keepers {
                    None => "dynasty value".to_string(),
                    Some(k) => format!("value (this season + top-{k} keepers)"),
                };
                let tail = if a_val >= d_val {
                    format!("an upgrade, {})", fixed(a_val - d_val, 1, true))
                } else {
                    format!(
                        "costs {}, {} of {}'s value)",
                        fixed(d_val - a_val, 1, false),
                        percent((d_val - a_val) / d_val, 0, false),
                        d.name
                    )
                };
                reasons.push(
                    Reason::new(
                        "DYNASTY_COST",
                        format!(
                            "Long term: {} {} vs {} {} {unit} ({tail}",
                            fa.name,
                            fixed(a_val, 1, false),
                            d.name,
                            fixed(d_val, 1, false)
                        ),
                    )
                    .with_value(Some(a_val - d_val))
                    .with_baseline(Some(d_val)),
                );
            }
            Some(d) if stream => {
                let (d_val, a_val) = (values[&d.cid].fpg_season, fv.fpg_season);
                let tail = if a_val >= d_val {
                    format!("an upgrade, {})", fixed(a_val - d_val, 2, true))
                } else {
                    format!(
                        "costs {}, {} of {}'s)",
                        fixed(d_val - a_val, 2, false),
                        percent((d_val - a_val) / d_val, 0, false),
                        d.name
                    )
                };
                reasons.push(
                    Reason::new(
                        "SEASON_COST",
                        format!(
                            "Rest of season: {} {} vs {} {} FPG ({tail}",
                            fa.name,
                            fixed(a_val, 2, false),
                            d.name,
                            fixed(d_val, 2, false)
                        ),
                    )
                    .with_value(Some(a_val - d_val))
                    .with_baseline(Some(d_val)),
                );
            }
            Some(d) if dyn_.is_some() && keepers.is_some() => {
                let (a_val, d_val) = lt_pair(Some(fa), Some(d));
                reasons.push(
                    Reason::new(
                        "DYNASTY_DROP",
                        format!(
                            "Keeper-league value {} {} vs {} {} (this season + the change in your top-{} keepers; lowest-value \
                             droppable same-slot player; needs x{})",
                            fa.name,
                            fixed(a_val, 2, false),
                            d.name,
                            fixed(d_val, 2, false),
                            keepers.unwrap_or(0),
                            general(DYNASTY_MIN_RATIO, None, false)
                        ),
                    )
                    .with_value(Some(a_val))
                    .with_baseline(Some(d_val)),
                );
            }
            Some(d) if dyn_.is_some() => {
                let dm = dyn_.as_ref();
                let a = dm.and_then(|m| m.get(&fa.cid)).copied().unwrap_or(0.0);
                let b = dm.and_then(|m| m.get(&d.cid)).copied().unwrap_or(0.0);
                reasons.push(
                    Reason::new(
                        "DYNASTY_DROP",
                        format!(
                            "Dynasty value {} {} vs {} {} (lowest-dynasty droppable same-slot player; needs x{})",
                            fa.name,
                            fixed(a, 2, false),
                            d.name,
                            fixed(b, 2, false),
                            general(DYNASTY_MIN_RATIO, None, false)
                        ),
                    )
                    .with_value(Some(a))
                    .with_baseline(Some(b)),
                );
            }
            _ => {}
        }
        reasons.extend(extra);
        if let Some(b) = &budget {
            reasons.push(b.clone());
        }
        if let (Some(dg), false, false) = (dyn_gain, stream, empty_fill) {
            let gain_text = match drop {
                None => format!("Dynasty value added {} (no drop, so the whole value counts; score basis)", fixed(dg, 2, true)),
                Some(d) => format!("Dynasty gain {} over {} (score basis)", fixed(dg, 2, true), d.name),
            };
            reasons.push(Reason::new("DYNASTY_GAIN", gain_text).with_value(Some(dg)));
        }
        for (pl, pv) in [(Some(fa), Some(fv)), (cmp, cv)] {
            if let (Some(pl), Some(pv)) = (pl, pv) {
                if !matches!(pl.status, Status::Healthy | Status::Unknown) {
                    let v = if pv.fpg != 0.0 { Some(pv.fpg_for(horizon) / pv.fpg) } else { None };
                    reasons.push(Reason::new("STATUS", status_text(pl)).with_value(v));
                }
            }
        }
        if let Some(trend) = ownership_trend_reason(fa) {
            reasons.push(trend);
        } else if let Some(po) = fa.pct_owned {
            reasons.push(
                Reason::new("OWNED", format!("{} rostered in {}% of leagues", fa.name, fixed(po, 1, false))).with_value(Some(po)),
            );
        }
        let mut checks: Vec<(&Player, &str)> = vec![(fa, "add")];
        if let Some(d) = drop {
            checks.push((d, "drop"));
        }
        reasons.extend(market_check_reasons(&market, checks));
        let title = match (drop, spot_open, spot_ir) {
            (Some(d), _, _) => format!("Add {}, drop {}", fa.name, d.name),
            (None, true, _) => format!("Add {} (open roster spot)", fa.name),
            (None, false, Some(x)) => format!("Add {}, move {} to IR", fa.name, x.name),
            (None, false, None) => format!("Add {}, move None to IR", fa.name),
        };
        // this-matchup pickups rank by the matchup points shown
        let score = if stream {
            gain
        } else {
            (match dyn_gain {
                Some(dg) if !empty_fill => dg,
                _ => gain,
            }) * conf
                * market_multiplier(fa.pct_owned_change)
        };
        let (mut pred, mut units, mut days) = predicted_gain(fv, cv, gain, horizon);
        if let (Some(mp), Some(bp)) = (matchup_pts, base_plan.as_ref()) {
            pred = mp;
            units = GainUnits::MatchupPts;
            days = Some(bp.days.len() as i64);
        }
        let mut r = Recommendation::new(RecKind::Waiver, score, title);
        r.add = vec![fa.clone()];
        r.drop = drop.into_iter().cloned().collect();
        r.reasons = reasons;
        r.predicted_gain = Some(pred);
        r.gain_units = Some(units);
        r.horizon_days = days;
        if drop.is_none() && !spot_open {
            r.subjects = spot_ir.into_iter().cloned().collect();
        }
        (Some(r), dbg)
    };

    let results: Vec<(Option<Recommendation>, Vec<WaiverDebug>)> = ctx.free_agents.par_iter().map(evaluate).collect();
    let mut recs: Vec<Recommendation> = Vec::new();
    let mut all_dbg: Vec<WaiverDebug> = Vec::new();
    for (r, d) in results {
        recs.extend(r);
        all_dbg.extend(d);
    }
    if let Some(out) = debug {
        out.extend(all_dbg);
    }
    sort_desc_by(&mut recs, |a, b| cmp_f64(a.score, b.score));
    recs.truncate(opts.limit);
    apply_strength(&mut recs);
    apply_ranks(&mut recs);
    Ok(recs)
}

/// Pickups for the rest of this matchup (week horizon, best lineup each day) in daily-lineup
/// leagues; [] elsewhere. Titles end "for this matchup".
pub fn recommend_matchup_waivers(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    limit: usize,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    exclude_drops: &[Cid],
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Result<Vec<Recommendation>, CoreError> {
    if ctx.schedule.is_empty() || lineup_lock::lock(ctx) != "daily" {
        return Ok(vec![]);
    }
    let opts = WaiverOpts { limit, horizon: Horizon::Week, dynasty_values, stream: true, exclude_drops };
    let mut recs = recommend_waivers(ctx, values, &opts, None, now, memo)?;
    for r in recs.iter_mut() {
        r.title = format!("{} for this matchup", r.title);
    }
    Ok(recs)
}

/// Kind "alert": free agents whose % rostered rose >= ALERT_MIN_CHANGE points this week and who
/// back it up with positive VORP (`values`; computed from `ctx` when None) or a rising deployment.
pub fn recommend_trending_alerts(
    ctx: &LeagueContext,
    values: Option<&Map<Cid, PlayerValue>>,
    limit: usize,
    horizon: Horizon,
) -> Vec<Recommendation> {
    let owned;
    let values: &Map<Cid, PlayerValue> = match values {
        Some(v) => v,
        None => {
            let mut c = ctx.clone();
            owned = valuate_league(&mut c, &from_config(&ctx.scoring));
            &owned
        }
    };
    let mut recs: Vec<Recommendation> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for fa in &ctx.free_agents {
        let Some(chg) = fa.pct_owned_change else { continue };
        if seen.contains(fa.cid.as_str()) || chg < ALERT_MIN_CHANGE {
            continue;
        }
        seen.insert(fa.cid.as_str());
        let pv = values.get(&fa.cid);
        let mut support: Vec<Reason> = Vec::new();
        if let Some(pv) = pv {
            let vorp = pv.vorp_for(horizon);
            if vorp > 0.0 {
                support.push(
                    Reason::new(
                        "VORP",
                        format!(
                            "VORP {} ({}), {} FPG",
                            fixed(vorp, 2, true),
                            horizon.as_str(),
                            fixed(pv.fpg_for(horizon), 2, false)
                        ),
                    )
                    .with_value(Some(vorp))
                    .with_baseline(Some(0.0)),
                );
            }
        }
        if let Some(toi) = fa.toi_trend.filter(|t| *t >= ALERT_TOI_TREND_MIN) {
            support.push(
                Reason::new("TOI_TREND", format!("TOI {} min/game vs his season baseline", fixed(toi, 1, true)))
                    .with_value(Some(toi))
                    .with_baseline(Some(ALERT_TOI_TREND_MIN)),
            );
        }
        if let Some(pp) = fa.pp_share_trend.filter(|p| *p >= ALERT_PP_SHARE_TREND_MIN) {
            support.push(
                Reason::new("PP_TREND", format!("PP share {} pts vs his season baseline", fixed(pp * 100.0, 0, true)))
                    .with_value(Some(pp))
                    .with_baseline(Some(ALERT_PP_SHARE_TREND_MIN)),
            );
        }
        if support.is_empty() {
            continue;
        }
        let mut reasons: Vec<Reason> = ownership_trend_reason(fa).into_iter().collect();
        reasons.extend(support);
        if !matches!(fa.status, Status::Healthy | Status::Unknown) {
            reasons.push(Reason::new("STATUS", status_text(fa)));
        }
        let mut r = Recommendation::new(
            RecKind::Alert,
            chg,
            format!("Rising: {} ({}% rostered this week)", fa.name, fixed(chg, 1, true)),
        );
        r.counterparty = Some("FA".to_string());
        r.subjects = vec![fa.clone()];
        r.reasons = reasons;
        r.horizon_days = Some(7);
        r.strength = Some(round_to(10.0_f64.min(3.0 + chg / 3.0), 2));
        recs.push(r);
    }
    sort_desc_by(&mut recs, |a, b| cmp_f64(a.score, b.score));
    recs.truncate(limit);
    apply_ranks(&mut recs);
    recs
}

#[cfg(test)]
mod tests {
    //! Port of tests/test_waivers.py (intent).
    use super::*;
    use crate::analysis::schedule_grid::per_game_value;
    use crate::recommend::lineup::tests::now_at;
    use crate::scoring::PointsScoring;
    use fm_core::models::{FantasyTeam, RosterSlot};
    use serde_json::json;

    fn mk(cid: &str, pos: &[&str], goals_pg: f64, gp: i64, status: &str) -> Player {
        let mut lines = json!({"projected": {"split": "projected", "gp": 80, "stats": {"G": goals_pg * 80.0, "GP": 80.0}}});
        if gp > 0 {
            lines["season"] = json!({"split": "season", "gp": gp, "stats": {"G": goals_pg * gp as f64, "GP": gp as f64}});
        }
        serde_json::from_value(json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": "EDM",
                                      "positions": pos, "status": status, "lines": lines}))
        .unwrap()
    }

    fn league(fas: Vec<Player>, mine: &[(&str, &Player)], shape: Option<&[(&str, i64)]>) -> LeagueContext {
        let slots: Vec<RosterSlot> = mine
            .iter()
            .map(|(s, p)| RosterSlot { slot: s.to_string(), player: Some((*p).clone()), starting: !["BN", "IR"].contains(s) })
            .collect();
        let mut sh: Map<String, i64> = Map::new();
        match shape {
            Some(xs) => sh.extend(xs.iter().map(|(k, v)| (k.to_string(), *v))),
            None => {
                sh.insert("IR".into(), 1);
                for (s, _) in mine {
                    *sh.entry(s.to_string()).or_insert(0) += i64::from(*s != "IR");
                }
            }
        }
        let t = FantasyTeam { team_id: "1".into(), name: "me".into(), owner_is_me: true, slots, record: None };
        serde_json::from_value(json!({"provider": "test", "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {"G": 1.0}}, "roster_shape": sh, "teams": [t],
            "free_agents": fas, "matchup_period": 1, "as_of": "2026-10-01"}))
        .unwrap()
    }

    fn valuate(ctx: &LeagueContext) -> Map<Cid, PlayerValue> {
        let mut c = ctx.clone();
        let w: Map<String, f64> = [("G".to_string(), 1.0)].into_iter().collect();
        valuate_league(&mut c, &PointsScoring::new(w, None))
    }

    fn dynv(ctx: &LeagueContext, xs: &[(&str, f64)]) -> Map<Cid, DynastyValue> {
        let players: HashMap<String, Player> = ctx.all_players().into_iter().map(|p| (p.cid.clone(), p.clone())).collect();
        xs.iter()
            .map(|(c, v)| {
                let dv: DynastyValue = serde_json::from_value(json!({"player": players[*c], "value": v, "age": null,
                    "age_mult": 1.0, "upside": 0.0, "horizon_years": 3}))
                .unwrap();
                (c.to_string(), dv)
            })
            .collect()
    }

    fn run(ctx: &LeagueContext, values: &Map<Cid, PlayerValue>, opts: &WaiverOpts<'_>) -> Vec<Recommendation> {
        recommend_waivers(ctx, values, opts, None, &now_at(12, 0, 1), &ScheduleMemo::new(None)).unwrap()
    }

    fn matchup(ctx: &LeagueContext, values: &Map<Cid, PlayerValue>, dynv: Option<&Map<Cid, DynastyValue>>) -> Vec<Recommendation> {
        recommend_matchup_waivers(ctx, values, 5, dynv, &[], &now_at(12, 0, 1), &ScheduleMemo::new(None)).unwrap()
    }

    fn codes(r: &Recommendation) -> Vec<&str> {
        r.reasons.iter().map(|x| x.code.as_str()).collect()
    }

    fn sched_league(fa: Player, my: Player) -> LeagueContext {
        let mut ctx = league(vec![fa], &[("C", &my)], None);
        let days: Vec<NaiveDate> = [1, 3, 5, 7, 8].iter().map(|d| NaiveDate::from_ymd_opt(2026, 10, *d).unwrap()).collect();
        ctx.games_per_day = days.iter().map(|d| (*d, 4)).collect();
        ctx.schedule.insert("EDM".into(), days);
        ctx
    }

    #[test]
    fn waiver_pick_and_drop_candidate() {
        let fa_good = mk("fa_c", &["C"], 1.0, 20, "healthy");
        let fa_meh = mk("fa_d", &["D"], 0.2, 0, "healthy");
        let my_c = mk("my_c", &["C"], 0.2, 0, "healthy");
        let my_lw = mk("my_lw", &["LW"], 0.1, 0, "healthy");
        let my_ir = mk("my_ir", &["C"], 0.0, 0, "ir");
        let ctx = league(vec![fa_good, fa_meh], &[("C", &my_c), ("LW", &my_lw), ("IR", &my_ir)], None);
        let values = valuate(&ctx);
        let recs = run(&ctx, &values, &WaiverOpts::default());
        assert_eq!(recs.len(), 1);
        assert_eq!((recs[0].add[0].cid.as_str(), recs[0].drop[0].cid.as_str()), ("fa_c", "my_c"));
        assert_eq!(codes(&recs[0])[..4], ["VORP_DELTA", "FPG_ADD", "FPG_DROP", "GP"]);
        assert!((recs[0].score - 0.8 * (0.5 + 0.5 * 20.0 / 60.0)).abs() < 1e-9);
    }

    #[test]
    fn open_spot_ir_move_and_excluded_drop() {
        let fa = mk("fa_c", &["C"], 1.0, 20, "healthy");
        let my_c = mk("my_c", &["C"], 0.2, 0, "healthy");
        let ctx = league(vec![fa.clone()], &[("C", &my_c)], Some(&[("C", 1), ("BN", 1), ("IR", 1)]));
        let r = run(&ctx, &valuate(&ctx), &WaiverOpts::default());
        assert_eq!(r.len(), 1);
        assert!(r[0].drop.is_empty() && r[0].reason("OPEN_SPOT").is_some());
        assert!(r[0].reason("VORP_DELTA").unwrap().text.contains("my_c"));

        let hurt = mk("hurt", &["LW"], 0.9, 0, "ir");
        let ctx = league(vec![fa.clone()], &[("C", &my_c), ("LW", &hurt)], Some(&[("C", 1), ("LW", 1), ("IR", 1)]));
        let r = &run(&ctx, &valuate(&ctx), &WaiverOpts::default())[0];
        assert!(r.drop.is_empty() && r.title == "Add fa_c, move hurt to IR" && r.reason("IR_MOVE").is_some());
        let ctx2 = league(vec![fa.clone()], &[("C", &my_c), ("LW", &hurt)], Some(&[("C", 1), ("LW", 1)]));
        let r2 = &run(&ctx2, &valuate(&ctx2), &WaiverOpts::default())[0];
        assert_eq!(r2.drop[0].cid, "my_c");

        let hurt_c = mk("hurt_c", &["C"], 0.05, 0, "out");
        let meh = mk("meh_c", &["C"], 0.2, 0, "healthy");
        let ctx3 = league(vec![fa], &[("C", &meh), ("BN", &hurt_c)], None);
        let v3 = valuate(&ctx3);
        assert_eq!(run(&ctx3, &v3, &WaiverOpts::default())[0].drop[0].cid, "hurt_c");
        let excl = vec!["hurt_c".to_string()];
        let r3 = run(&ctx3, &v3, &WaiverOpts { exclude_drops: &excl, ..WaiverOpts::default() });
        assert_eq!(r3.len(), 1);
        assert_eq!(r3[0].drop[0].cid, "meh_c");
    }

    #[test]
    fn dynasty_drop_is_lowest_dynasty_value() {
        let fa = mk("fa_c", &["C"], 1.0, 20, "healthy");
        let vet = mk("vet", &["C"], 0.2, 0, "healthy");
        let kid = mk("kid", &["C"], 0.3, 0, "healthy");
        let mut ctx = league(vec![fa], &[("C", &vet), ("BN", &kid)], None);
        ctx.dynasty = true;
        let values = valuate(&ctx);
        let dv = dynv(&ctx, &[("fa_c", 5.0), ("vet", 9.0), ("kid", 1.0)]);
        let r = &run(&ctx, &values, &WaiverOpts { dynasty_values: Some(&dv), ..WaiverOpts::default() })[0];
        assert_eq!(r.drop[0].cid, "kid");
        assert!(r.reason("DYNASTY_DROP").is_some());
        let dv2 = dynv(&ctx, &[("fa_c", 0.5), ("vet", 9.0), ("kid", 1.0)]);
        assert!(run(&ctx, &values, &WaiverOpts { dynasty_values: Some(&dv2), ..WaiverOpts::default() }).is_empty());
    }

    #[test]
    fn market_timing() {
        assert_eq!(market_multiplier(None), 1.0);
        assert_eq!(market_multiplier(Some(2.9)), 1.0);
        assert_eq!(market_multiplier(Some(-8.0)), 1.0);
        assert!((market_multiplier(Some(3.0)) - 1.15).abs() < 1e-12);
        assert!((market_multiplier(Some(9.5)) - 1.475).abs() < 1e-12);
        assert!((market_multiplier(Some(40.0)) - 1.5).abs() < 1e-12);
        let trend = |chg: Option<f64>| {
            let mut fa = mk("fa_c", &["C"], 1.0, 20, "healthy");
            fa.pct_owned = Some(32.0);
            fa.pct_owned_change = chg;
            let ctx = league(vec![fa], &[("C", &mk("my_c", &["C"], 0.2, 0, "healthy"))], None);
            let v = valuate(&ctx);
            run(&ctx, &v, &WaiverOpts::default()).remove(0)
        };
        let base = trend(None);
        assert!(base.reason("OWNERSHIP_TREND").is_none());
        assert_eq!(base.reason("OWNED").unwrap().text, "fa_c rostered in 32.0% of leagues");
        let r = trend(Some(9.5));
        assert!((r.score - base.score * 1.475).abs() < 1e-9);
        let t = r.reason("OWNERSHIP_TREND").unwrap();
        assert!(t.text.contains("rostered 32% (+9.5 this week)") && t.value == Some(9.5) && t.baseline == Some(32.0));
        assert!(r.reason("OWNED").is_none());
        let f = trend(Some(-4.0));
        assert!((f.score - base.score).abs() < 1e-12 && f.reason("OWNERSHIP_TREND").unwrap().text.contains("falling"));
    }

    #[test]
    fn trending_alerts_thresholds() {
        let mut riser = mk("riser", &["C"], 1.0, 20, "healthy");
        riser.pct_owned = Some(32.0);
        riser.pct_owned_change = Some(9.5);
        let mut edge = mk("edge", &["C"], 1.0, 20, "healthy");
        edge.pct_owned = Some(20.0);
        edge.pct_owned_change = Some(5.0);
        let mut small = mk("small", &["C"], 1.0, 20, "healthy");
        small.pct_owned_change = Some(4.9);
        let mut dud = mk("dud", &["C"], 0.0, 0, "healthy");
        dud.pct_owned_change = Some(12.0);
        let mut deployed = mk("deployed", &["C"], 0.0, 0, "healthy");
        deployed.pct_owned_change = Some(6.0);
        deployed.toi_trend = Some(1.8);
        let mut pp_guy = mk("pp_guy", &["C"], 0.0, 0, "healthy");
        pp_guy.pct_owned_change = Some(30.0);
        pp_guy.pp_share_trend = Some(0.2);
        let fas = vec![riser, edge, small, dud, deployed, pp_guy];
        let ctx = league(fas.clone(), &[("C", &mk("my_c", &["C"], 0.2, 0, "healthy"))], None);
        let vals: Map<Cid, PlayerValue> = fas
            .iter()
            .map(|p| {
                let v = if ["riser", "edge", "small"].contains(&p.cid.as_str()) { 0.5 } else { -0.5 };
                let pv: PlayerValue = serde_json::from_value(json!({"player": p, "fpg": 1.0 + v, "fpg_season": 1.0 + v,
                    "fpg_week": 1.0 + v, "vorp": v, "vorp_week": v}))
                .unwrap();
                (p.cid.clone(), pv)
            })
            .collect();
        let recs = recommend_trending_alerts(&ctx, Some(&vals), 10, Horizon::Season);
        let order: Vec<&str> = recs.iter().map(|r| r.subjects[0].cid.as_str()).collect();
        assert_eq!(order, vec!["pp_guy", "riser", "deployed", "edge"]);
        let top = &recs[1];
        assert_eq!(top.title, "Rising: riser (+9.5% rostered this week)");
        assert_eq!((top.counterparty.as_deref(), top.horizon_days), (Some("FA"), Some(7)));
        assert!((top.strength.unwrap() - (3.0 + 9.5 / 3.0)).abs() < 0.01);
        assert_eq!(recs[0].strength, Some(10.0));
        assert!(recs[2].reason("TOI_TREND").is_some());
        assert_eq!((recs[0].rank_in_kind, recs[0].kind_total), (Some(1), Some(4)));
        assert_eq!(recommend_trending_alerts(&ctx, Some(&vals), 2, Horizon::Season).len(), 2);
    }

    #[test]
    fn matchup_waivers_only_in_daily_leagues_and_gain_is_matchup_points() {
        let ctx = sched_league(mk("fa_c", &["C"], 3.0, 20, "healthy"), mk("my_c", &["C"], 0.2, 0, "healthy"));
        let values = valuate(&ctx);
        let recs = matchup(&ctx, &values, None);
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert!(r.title.ends_with(" for this matchup"));
        let pts = r.reason("MATCHUP_GAIN").unwrap().value.unwrap();
        assert_eq!((r.gain_units, r.horizon_days), (Some(GainUnits::MatchupPts), Some(7)));
        assert!((r.predicted_gain.unwrap() - pts).abs() < 1e-12);
        let mut weekly = ctx.clone();
        weekly.provider = "fantrax".into();
        weekly.lineup_lock = Some(fm_core::models::LineupLock::Weekly);
        assert!(matchup(&weekly, &values, None).is_empty());
    }

    #[test]
    fn waiver_claim_counts_only_games_after_clear_date() {
        let go = |until: Option<NaiveDate>| {
            let mut fa = mk("fa_c", &["C"], 3.0, 20, "healthy");
            fa.waiver_until = until;
            let ctx = sched_league(fa, mk("my_c", &["C"], 0.2, 0, "healthy"));
            let values = valuate(&ctx);
            let mut r = run(&ctx, &values, &WaiverOpts { horizon: Horizon::Week, ..WaiverOpts::default() });
            assert_eq!(r.len(), 1);
            (r.remove(0), values)
        };
        let (free, values) = go(None);
        let (waiv, _) = go(NaiveDate::from_ymd_opt(2026, 10, 5));
        assert!(free.reason("WAIVER_CLAIM").is_none());
        let claim = waiv.reason("WAIVER_CLAIM").unwrap();
        assert!(claim.text.contains("on waivers until Mon: claim, won't be available before then"));
        assert_eq!((claim.value, claim.baseline), (Some(1.0), Some(4.0)));
        assert_eq!(free.gain_units, Some(GainUnits::MatchupPts));
        let pg = per_game_value(values.get("fa_c")).unwrap();
        assert!((free.predicted_gain.unwrap() - waiv.predicted_gain.unwrap() - 3.0 * pg).abs() < 1e-9);
    }

    fn stream_league(fa: Player, mine: &[(&str, &Player)], shape: &[(&str, i64)]) -> LeagueContext {
        let mut ctx = league(vec![fa], mine, Some(shape));
        let start = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let days: Vec<NaiveDate> = (0..80).map(|i| start + Duration::days(2 * i)).collect();
        ctx.games_per_day = days.iter().map(|d| (*d, 4)).collect();
        ctx.schedule.insert("EDM".into(), days);
        ctx
    }

    #[test]
    fn matchup_pickup_drop_uses_rest_of_season_value() {
        let fa = mk("fa_c", &["C"], 1.0, 20, "healthy");
        let weak = mk("weak_c", &["C"], 0.5, 5, "healthy");
        let hurt = mk("hurt_c", &["C"], 2.0, 5, "ir");
        let ctx = stream_league(fa.clone(), &[("C", &weak), ("BN", &hurt)], &[("C", 1), ("BN", 1)]);
        let values = valuate(&ctx);
        assert!(values["hurt_c"].fpg_week < values["weak_c"].fpg_week);
        assert!(values["hurt_c"].fpg_season > values["weak_c"].fpg_season);
        let r = matchup(&ctx, &values, None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].drop[0].cid, "weak_c");
        let cost = r[0].reason("SEASON_COST").unwrap();
        assert!(cost.text.starts_with("Rest of season: fa_c") && cost.text.contains("an upgrade"));
        // never cut a much better rest-of-season player
        let star = mk("star_c", &["C"], 3.0, 5, "ir");
        let ctx2 = stream_league(fa, &[("C", &star)], &[("C", 1)]);
        let v2 = valuate(&ctx2);
        assert!(matchup(&ctx2, &v2, None).is_empty());
    }

    #[test]
    fn matchup_pickups_allow_a_small_stated_dynasty_cost() {
        let mut ctx = sched_league(mk("fa_c", &["C"], 3.0, 20, "healthy"), mk("my_c", &["C"], 0.2, 0, "healthy"));
        ctx.dynasty = true;
        let values = valuate(&ctx);
        let dv = dynv(&ctx, &[("fa_c", 9.0), ("my_c", 10.0)]);
        let go = |mode: fm_core::models::DynastyMode| {
            let mut c = ctx.clone();
            c.dynasty_mode = mode;
            let s = matchup(&c, &values, Some(&dv));
            let p = run(&c, &values, &WaiverOpts { horizon: Horizon::Week, dynasty_values: Some(&dv), ..WaiverOpts::default() });
            (s, p)
        };
        let (stream, plain) = go(fm_core::models::DynastyMode::Balanced);
        assert!(plain.is_empty() && !stream.is_empty());
        assert!(stream[0].reason("DYNASTY_COST").unwrap().text.contains("costs 1.0, 10% of my_c's value"));
        assert!(go(fm_core::models::DynastyMode::Rebuild).0.is_empty());
    }

    #[test]
    fn empty_starting_slot_is_filled_by_dropping_a_bench_player() {
        let fa_d = mk("fa_d", &["D"], 0.4, 20, "healthy");
        let my_c = mk("my_c", &["C"], 0.5, 0, "healthy");
        let bench_lw = mk("bench_lw", &["LW"], 0.6, 0, "healthy");
        let mut ctx = league(vec![fa_d], &[("C", &my_c), ("BN", &bench_lw)], Some(&[("C", 1), ("D", 1), ("BN", 1), ("IR", 1)]));
        ctx.max_roster_size = Some(2);
        let values = valuate(&ctx);
        let r = &run(&ctx, &values, &WaiverOpts::default())[0];
        assert_eq!(r.title, "Add fa_d, drop bench_lw");
        let delta = r.reason("VORP_DELTA").unwrap();
        assert!((delta.value.unwrap() - values["fa_d"].fpg).abs() < 1e-9);
        assert!(delta.text.contains("fills empty D slot") && delta.text.contains("drop bench LW bench_lw"));
        ctx.dynasty = true;
        ctx.dynasty_mode = fm_core::models::DynastyMode::Balanced;
        let dv = dynv(&ctx, &[("fa_d", 9.0), ("bench_lw", 10.0), ("my_c", 20.0)]);
        let r = &run(&ctx, &values, &WaiverOpts { dynasty_values: Some(&dv), ..WaiverOpts::default() })[0];
        assert_eq!(r.drop[0].cid, "bench_lw");
        assert!(r.reason("DYNASTY_COST").is_some());
        let dv2 = dynv(&ctx, &[("fa_d", 8.0), ("bench_lw", 10.0), ("my_c", 20.0)]);
        assert!(run(&ctx, &values, &WaiverOpts { dynasty_values: Some(&dv2), ..WaiverOpts::default() }).is_empty());
    }

    /// Season horizon, daily lineups (C 1, BN 1): the bench centre (CAR) starts both of his games
    /// because my_c's team (EDM) is idle those days. A free agent whose team plays only on EDM's
    /// days would never start, so swapping him in loses the bench player's games, whatever his FPG;
    /// one who plays on CAR's days gains exactly his FPG over the dropped player's.
    #[test]
    fn season_pickup_gain_counts_the_games_he_would_start() {
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        let with_team = |mut p: Player, t: &str| {
            p.team = Some(t.to_string());
            p
        };
        let my_c = mk("my_c", &["C"], 1.5, 0, "healthy");
        let bench = with_team(mk("bench", &["C"], 0.2, 0, "healthy"), "CAR");
        let build = |fa_team: &str| {
            let fa = with_team(mk("fa", &["C"], 1.0, 20, "healthy"), fa_team);
            let mut ctx = league(vec![fa], &[("C", &my_c), ("BN", &bench)], Some(&[("C", 1), ("BN", 1)]));
            for (t, ds) in [("EDM", [1, 2, 3].as_slice()), ("TOR", &[1, 2, 3]), ("CAR", &[4, 5]), ("BOS", &[4, 5])] {
                ctx.schedule.insert(t.into(), ds.iter().map(|&d| day(d)).collect());
            }
            ctx
        };
        // the best-lineup rule called this +0.8 FPG (fa 1.0 over bench 0.2); he would never start
        let ctx = build("TOR");
        assert!(run(&ctx, &valuate(&ctx), &WaiverOpts::default()).is_empty());

        let ctx = build("BOS");
        let values = valuate(&ctx);
        let r = run(&ctx, &values, &WaiverOpts::default());
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].add[0].cid.as_str(), r[0].drop[0].cid.as_str()), ("fa", "bench"));
        let want = values["fa"].fpg_season - values["bench"].fpg_season;
        assert!((r[0].reason("VORP_DELTA").and_then(|x| x.value).unwrap() - want).abs() < 1e-9);
        assert!(r[0].reason("STARTED_GAMES").is_some());
    }

    /// Season horizon, daily lineups (C 1, BN 2). bench_a (CAR) starts both of his games, on days
    /// my_c's team (EDM) is idle; bench_b (TOR, more FPG) plays only on EDM's days and never
    /// starts. The free agent (BOS) plays on days nobody else does, so the drop that costs least
    /// is bench_b, not the lowest-FPG bench_a, and the gain is the free agent's whole FPG.
    #[test]
    fn season_drop_is_the_player_whose_starts_are_missed_least() {
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        let with_team = |mut p: Player, t: &str| {
            p.team = Some(t.to_string());
            p
        };
        let my_c = mk("my_c", &["C"], 1.5, 0, "healthy");
        let bench_a = with_team(mk("bench_a", &["C"], 0.2, 0, "healthy"), "CAR");
        let bench_b = with_team(mk("bench_b", &["C"], 0.4, 0, "healthy"), "TOR");
        let fa = with_team(mk("fa", &["C"], 1.0, 20, "healthy"), "BOS");
        let mut ctx = league(vec![fa], &[("C", &my_c), ("BN", &bench_a), ("BN", &bench_b)], Some(&[("C", 1), ("BN", 2)]));
        for (t, ds) in [("EDM", [1, 2, 3].as_slice()), ("TOR", &[1, 2, 3]), ("CAR", &[4, 5]), ("BOS", &[6, 7])] {
            ctx.schedule.insert(t.into(), ds.iter().map(|&d| day(d)).collect());
        }
        let values = valuate(&ctx);
        assert!(values["bench_a"].fpg_season < values["bench_b"].fpg_season);
        let r = run(&ctx, &values, &WaiverOpts::default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].drop[0].cid.as_str(), "bench_b");
        let want = values["fa"].fpg_season;
        assert!((r[0].reason("VORP_DELTA").and_then(|x| x.value).unwrap() - want).abs() < 1e-9);
    }
}
