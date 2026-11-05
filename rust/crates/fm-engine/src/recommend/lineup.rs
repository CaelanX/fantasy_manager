//! Rust port of `fantasy_manager/recommend/lineup.py`: the exact lineup solver (a dynamic program
//! over slot-type fill counts), start/sit recommendations, and the day-by-day plan for
//! daily-lineup leagues. Owner: work package `W6-lineup-waivers`.
//!
//! Deviations from Python (behaviour-preserving):
//! * The clock is a parameter (`now`), and the per-context schedule memo (`memo_window`,
//!   `opponent_adjust`) is an explicit [`ScheduleMemo`].
//! * `_dp` keeps Python's dict semantics exactly (states visited in insertion order, the
//!   `cand > best + 1e-12` first-wins rule, first maximum wins) but stores states in a dense
//!   mixed-radix array with per-player back-pointers instead of copying assignment tuples.
//! * The plan works on a [`RosterView`] (borrowed slots) so callers (waivers, IR activation) can
//!   try roster changes without cloning a `FantasyTeam`; per-player day values are computed once
//!   per plan ([`DayInfo`]) and can be shared between plans ([`PlanCache`]).
//! * Python sets of cids (`playing`, `locked`, the starters summed into `current`) iterate in slot /
//!   pool order here; only float summation order can differ (Python's is hash-random).
//! * `recommend_lineup` returns `Err(CoreError::Lookup)` where Python raised `LookupError` (no
//!   team flagged as mine).

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use chrono::{Datelike, Duration, NaiveDate};
use fm_core::models::{FantasyTeam, GainUnits, LeagueContext, Player, Reason, RecKind, Recommendation, Status};
use fm_core::normalize::normalize_team;
use fm_core::values::{Horizon, PlayerValue};
use fm_core::{Cid, CoreError, Map, PyDateTime, Set};
use fm_pycompat::fmt::{fixed, percent};
use fm_pycompat::num::{py_sum, round_to};
use fm_pycompat::order::{cmp_f64, sort_desc_by};
use fm_pycompat::text::py_upper;
use fm_pycompat::time::strftime_date;
use regex::Regex;

use crate::analysis::schedule_grid::per_game_value;
use crate::analysis::team_factors::second_night_prob;
use crate::recommend::headline::day_swap_headline;
use crate::recommend::lineup_lock;
use crate::recommend::strength::{apply_ranks, apply_strength};
use crate::valuation::replacement::eligible_slots;
use crate::valuation::schedule::{
    memo_window, night_start_probs, skater_opp_game, week_start_share, OppGame, ScheduleMemo, B2B_SECOND_NIGHT_P,
    OPP_WEEK_REASON, SEASON_B2B_FRAC,
};

pub const SLOT_ORDER: [&str; 7] = ["C", "LW", "RW", "F", "D", "UTIL", "G"];
pub const NON_STARTING: [&str; 2] = ["BN", "IR"];
pub const MIN_GAIN_ABS: f64 = 0.5;
pub const MIN_GAIN_REL: f64 = 0.08;
/// tie-break toward current starters
pub const STABILITY_BONUS: f64 = 1e-6;
pub const UNAVAILABLE: [&str; 4] = ["out", "ir", "ltir", "suspended"];
/// = injuries.IR_ELIGIBLE
pub const IR_STASHABLE: [&str; 3] = ["ir", "ltir", "out"];
/// Start probability for today's game from a Daily Faceoff report: (starter, other goalie).
pub const START_PROB_CONFIRMED: (f64, f64) = (1.0, 0.0);
pub const START_PROB_LIKELY: (f64, f64) = (0.8, 0.2);
/// show OPP_ADJ when today's multiplier is at least this far from 1.0
pub const OPP_REASON_MIN: f64 = 0.03;
pub const WEEKLY_LOCK_TEXT: &str = "for this week's lineup (locks Monday)";

/// >> number of players: placing everyone dominates keeping slot types
const PLACE_ALL: f64 = 1000.0;
/// >> any lineup value: a locked starter is never benched
const LOCK_BONUS: f64 = 1e6;

pub(crate) fn unavailable(s: Status) -> bool {
    UNAVAILABLE.contains(&s.as_str())
}

/// `x or 1.0` for an optional float (None and 0.0 -> 1.0).
fn or_one(x: Option<f64>) -> f64 {
    match x {
        Some(v) if v != 0.0 => v,
        _ => 1.0,
    }
}

/// `f"{d:%a %b} {d.day}"`: "Mon Oct 5".
pub(crate) fn day_text(d: NaiveDate) -> String {
    format!("{} {}", strftime_date(d, "%a %b"), d.day())
}

/// Expanded starting slots in canonical order, e.g. ["C","C","LW","LW",...].
pub fn starting_slots(roster_shape: &Map<String, i64>) -> Vec<String> {
    let mut order: Vec<&str> = SLOT_ORDER.to_vec();
    let mut extra: Vec<&str> = roster_shape
        .keys()
        .map(|s| s.as_str())
        .filter(|s| !SLOT_ORDER.contains(s) && !NON_STARTING.contains(s))
        .collect();
    extra.sort();
    order.extend(extra);
    let mut out = Vec::new();
    for s in order {
        let n = roster_shape.get(s).copied().unwrap_or(0);
        for _ in 0..n.max(0) {
            out.push(s.to_string());
        }
    }
    out
}

fn value_of(pv: Option<&PlayerValue>, horizon: Horizon) -> f64 {
    pv.map_or(0.0, |pv| pv.lineup_value(horizon))
}

// ------------------------------------------------------------------------------------ solver

#[derive(Default)]
struct Scratch {
    old: Vec<f64>,
    new: Vec<f64>,
    present: Vec<bool>,
    order: Vec<u32>,
    back: Vec<u32>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

const NONE: u32 = u32::MAX;

/// Distinct slot types in first-appearance order and their counts.
fn slot_types(slots: &[String]) -> (Vec<&str>, Vec<usize>) {
    let mut types: Vec<&str> = Vec::new();
    let mut caps: Vec<usize> = Vec::new();
    for s in slots {
        match types.iter().position(|t| *t == s.as_str()) {
            Some(i) => caps[i] += 1,
            None => {
                types.push(s.as_str());
                caps.push(1);
            }
        }
    }
    (types, caps)
}

/// `_dp` on player indices: exact max-weight assignment of `players` to `slots` (weight per
/// player index and slot type) -> (score, [(slot index, player index)] sorted by slot index).
pub fn dp_idx<F>(players: &[&Player], slots: &[String], mut weight: F) -> (f64, Vec<(usize, usize)>)
where
    F: FnMut(usize, &str) -> f64,
{
    let (types, caps) = slot_types(slots);
    let nt = types.len();
    let mut strides = vec![0usize; nt];
    let mut nstates = 1usize;
    for i in 0..nt {
        strides[i] = nstates;
        nstates *= caps[i] + 1;
    }
    // eligible type indices and weights per player (Python computes `ws` before the state loop)
    let mut steps: Vec<(usize, Vec<(usize, f64)>)> = Vec::with_capacity(players.len());
    for (pi, p) in players.iter().enumerate() {
        let elig = eligible_slots(&p.positions, &types);
        let ws: Vec<(usize, f64)> = (0..nt)
            .filter(|i| elig.contains(&types[*i]))
            .map(|i| (i, weight(pi, types[i])))
            .collect();
        if !ws.is_empty() {
            steps.push((pi, ws));
        }
    }
    SCRATCH.with(|cell| {
        let mut guard = cell.borrow_mut();
        let sc = &mut *guard;
        sc.old.clear();
        sc.old.resize(nstates, 0.0);
        sc.new.clear();
        sc.new.resize(nstates, 0.0);
        sc.present.clear();
        sc.present.resize(nstates, false);
        sc.order.clear();
        sc.back.clear();
        sc.back.resize(steps.len() * nstates, NONE);
        sc.present[0] = true;
        sc.order.push(0);
        for (k, (_, ws)) in steps.iter().enumerate() {
            sc.new.copy_from_slice(&sc.old);
            let back = &mut sc.back[k * nstates..(k + 1) * nstates];
            let n_old = sc.order.len();
            for oi in 0..n_old {
                let s = sc.order[oi] as usize;
                let score = sc.old[s];
                for &(i, w) in ws {
                    let digit = (s / strides[i]) % (caps[i] + 1);
                    if digit >= caps[i] {
                        continue;
                    }
                    let ns = s + strides[i];
                    let cand = score + w;
                    if !sc.present[ns] {
                        sc.present[ns] = true;
                        sc.order.push(ns as u32);
                        sc.new[ns] = cand;
                        back[ns] = (s * nt + i) as u32;
                    } else if cand > sc.new[ns] + 1e-12 {
                        sc.new[ns] = cand;
                        back[ns] = (s * nt + i) as u32;
                    }
                }
            }
            std::mem::swap(&mut sc.old, &mut sc.new);
        }
        // max(dp.values(), key=score): first maximum in insertion order
        let mut best = sc.order[0] as usize;
        let mut best_score = sc.old[best];
        for &s in &sc.order[1..] {
            let v = sc.old[s as usize];
            if v > best_score {
                best_score = v;
                best = s as usize;
            }
        }
        // walk the back-pointers: (type index, player index) in player order
        let mut chain: Vec<(usize, usize)> = Vec::new();
        let mut s = best;
        for k in (0..steps.len()).rev() {
            let b = sc.back[k * nstates + s];
            if b != NONE {
                let b = b as usize;
                chain.push((b % nt, steps[k].0));
                s = b / nt;
            }
        }
        chain.reverse();
        // free slot indices per type, popped from the front
        let mut free: Vec<Vec<usize>> = vec![Vec::new(); nt];
        for (idx, sl) in slots.iter().enumerate() {
            let t = types.iter().position(|t| *t == sl.as_str()).unwrap_or(0);
            free[t].push(idx);
        }
        let mut next = vec![0usize; nt];
        let mut out: Vec<(usize, usize)> = chain
            .into_iter()
            .map(|(ti, pi)| {
                let slot = free[ti][next[ti]];
                next[ti] += 1;
                (slot, pi)
            })
            .collect();
        out.sort_by_key(|x| x.0);
        (best_score, out)
    })
}

/// `_dp(players, slots, weight)` -> (score, {slot index: cid}) (sorted by slot index).
pub fn dp<F>(players: &[&Player], slots: &[String], mut weight: F) -> (f64, Map<usize, Cid>)
where
    F: FnMut(&Player, &str) -> f64,
{
    let (score, assign) = dp_idx(players, slots, |i, t| weight(players[i], t));
    (score, assign.into_iter().map(|(s, i)| (s, players[i].cid.clone())).collect())
}

/// `_solve`: exact max-value assignment of `players` to `slots` (value for `horizon`, plus the
/// stability bonus for `current` starters) -> (score, {slot index: cid}).
pub fn solve(
    players: &[&Player],
    values: &Map<Cid, PlayerValue>,
    slots: &[String],
    horizon: Horizon,
    current: &HashSet<&str>,
) -> (f64, Map<usize, Cid>) {
    let vals: Vec<f64> = players
        .iter()
        .map(|p| value_of(values.get(&p.cid), horizon) + if current.contains(p.cid.as_str()) { STABILITY_BONUS } else { 0.0 })
        .collect();
    let (score, assign) = dp_idx(players, slots, |i, _| vals[i]);
    (score, assign.into_iter().map(|(s, i)| (s, players[i].cid.clone())).collect())
}

// ------------------------------------------------------------------------------ roster views

/// One filled roster slot (borrowed).
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    pub slot: &'a str,
    pub player: &'a Player,
    pub starting: bool,
}

/// A team's filled slots in slot order: what the lineup code reads from a `FantasyTeam`, cheap
/// to modify for "what if" rosters (`_plan_team`, IR activation).
#[derive(Clone, Debug, Default)]
pub struct RosterView<'a> {
    pub entries: Vec<Entry<'a>>,
}

impl<'a> RosterView<'a> {
    pub fn of(team: &'a FantasyTeam) -> Self {
        RosterView {
            entries: team
                .slots
                .iter()
                .filter_map(|s| s.player.as_ref().map(|p| Entry { slot: s.slot.as_str(), player: p, starting: s.starting }))
                .collect(),
        }
    }

    /// `_candidates`: players outside IR slots, one per cid (first position, last object).
    pub fn candidates(&self) -> Vec<&'a Player> {
        let mut out: Vec<&'a Player> = Vec::new();
        let mut pos: HashMap<&str, usize> = HashMap::new();
        for e in &self.entries {
            if e.slot == "IR" {
                continue;
            }
            match pos.get(e.player.cid.as_str()) {
                Some(&i) => out[i] = e.player,
                None => {
                    pos.insert(e.player.cid.as_str(), out.len());
                    out.push(e.player);
                }
            }
        }
        out
    }

    /// `_starters`: cids in starting slots.
    pub fn starters(&self) -> HashSet<&'a str> {
        self.entries.iter().filter(|e| e.starting).map(|e| e.player.cid.as_str()).collect()
    }

    /// Starters in slot order (first occurrence of each cid).
    fn starters_ordered(&self) -> Vec<&'a str> {
        let mut out: Vec<&'a str> = Vec::new();
        for e in &self.entries {
            if e.starting && !out.contains(&e.player.cid.as_str()) {
                out.push(e.player.cid.as_str());
            }
        }
        out
    }

    /// `_current_types`: {cid: starting slot type}.
    pub fn current_types(&self) -> HashMap<&'a str, &'a str> {
        self.entries.iter().filter(|e| e.starting).map(|e| (e.player.cid.as_str(), e.slot)).collect()
    }
}

/// `_best` over an explicit pool: `vals[i]` / `locked[i]` per pool index ->
/// ([(slot index, pool index)] sorted by slot, total value).
fn best_pool(
    pool: &[&Player],
    slots: &[String],
    vals: &[f64],
    locked: &[bool],
    starters: &HashSet<&str>,
    current_types: &HashMap<&str, &str>,
) -> (Vec<(usize, usize)>, f64) {
    let is_cur: Vec<bool> = pool.iter().map(|p| starters.contains(p.cid.as_str())).collect();
    let sub: Vec<usize> = (0..pool.len()).filter(|&i| !locked[i] || is_cur[i]).collect();
    let sub_players: Vec<&Player> = sub.iter().map(|&i| pool[i]).collect();
    let v: Vec<f64> = sub
        .iter()
        .map(|&i| vals[i] + if is_cur[i] { STABILITY_BONUS } else { 0.0 } + if locked[i] { LOCK_BONUS } else { 0.0 })
        .collect();
    let (_, best) = dp_idx(&sub_players, slots, |k, _| v[k]);
    let mut chosen: Vec<usize> = best.iter().map(|&(_, k)| sub[k]).collect();
    chosen.sort_unstable();
    // _arrange: the chosen players, in pool order, placed to keep current slot types
    let arr: Vec<usize> = sub.iter().copied().filter(|i| chosen.binary_search(i).is_ok()).collect();
    let arr_players: Vec<&Player> = arr.iter().map(|&i| pool[i]).collect();
    let (_, arranged) = dp_idx(&arr_players, slots, |k, t| {
        let i = arr[k];
        PLACE_ALL
            + if current_types.get(pool[i].cid.as_str()) == Some(&t) {
                if locked[i] {
                    100.0
                } else {
                    1.0
                }
            } else {
                0.0
            }
    });
    let mut placed: Vec<usize> = arranged.iter().map(|&(_, k)| arr[k]).collect();
    placed.sort_unstable();
    let assign: Vec<(usize, usize)> = if placed != chosen {
        best.iter().map(|&(s, k)| (s, sub[k])).collect()
    } else {
        arranged.iter().map(|&(s, k)| (s, arr[k])).collect()
    };
    let total = py_sum(assign.iter().map(|&(_, i)| vals[i]));
    (assign, total)
}

/// `_best(team, slots, vals, locked)`: max-value assignment {slot index: cid} arranged so starters
/// keep their slot type where possible, and its total value (`vals.get(cid, 0.0)`).
pub fn best(view: &RosterView<'_>, slots: &[String], vals: &dyn Fn(&str) -> f64, locked: &HashSet<&str>) -> (Map<usize, Cid>, f64) {
    let pool = view.candidates();
    let v: Vec<f64> = pool.iter().map(|p| vals(&p.cid)).collect();
    let l: Vec<bool> = pool.iter().map(|p| locked.contains(p.cid.as_str())).collect();
    let (assign, total) = best_pool(&pool, slots, &v, &l, &view.starters(), &view.current_types());
    (assign.into_iter().map(|(s, i)| (s, pool[i].cid.clone())).collect(), total)
}

/// Best assignment {index into starting_slots(roster_shape): cid} and its total value.
pub fn optimal_lineup(
    team: &FantasyTeam,
    values: &Map<Cid, PlayerValue>,
    roster_shape: &Map<String, i64>,
    horizon: Horizon,
) -> (Map<usize, Cid>, f64) {
    let vals = |c: &str| value_of(values.get(c), horizon);
    best(&RosterView::of(team), &starting_slots(roster_shape), &vals, &HashSet::new())
}

pub fn current_total(team: &FantasyTeam, values: &Map<Cid, PlayerValue>, horizon: Horizon) -> f64 {
    py_sum(
        team.slots
            .iter()
            .filter(|s| s.starting)
            .filter_map(|s| s.player.as_ref())
            .map(|p| value_of(values.get(&p.cid), horizon)),
    )
}

// ---------------------------------------------------- daily-lineup leagues: one lineup per day

#[derive(Clone, Debug, PartialEq)]
pub struct PlanDay {
    pub day: NaiveDate,
    /// {slot index: cid}, players with a game that day
    pub assign: Map<usize, Cid>,
    /// cid -> expected points still to come that day (0 without a game / locked)
    pub values: Map<Cid, f64>,
    /// cids whose NHL team plays that day (unavailable players excluded)
    pub playing: Set<Cid>,
    /// cids whose game has already started (today only)
    pub locked: Set<Cid>,
    pub optimal: f64,
    /// the lineup as set now, left unchanged that day
    pub current: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DailyPlan {
    pub days: Vec<PlanDay>,
}

impl DailyPlan {
    pub fn optimal(&self) -> f64 {
        py_sum(self.days.iter().map(|d| d.optimal))
    }
    pub fn current(&self) -> f64 {
        py_sum(self.days.iter().map(|d| d.current))
    }
    fn assigned(d: &PlanDay, cid: &str) -> bool {
        d.assign.values().any(|c| c == cid)
    }
    pub fn starts(&self, cid: &str) -> i64 {
        self.days.iter().filter(|d| Self::assigned(d, cid)).count() as i64
    }
    /// Starts whose game has not begun yet (today's locked games are already in the score).
    pub fn remaining_starts(&self, cid: &str) -> i64 {
        self.days.iter().filter(|d| Self::assigned(d, cid) && !d.locked.contains(cid)).count() as i64
    }
    pub fn points(&self, cid: &str) -> f64 {
        py_sum(self.days.iter().filter(|d| Self::assigned(d, cid)).map(|d| d.values.get(cid).copied().unwrap_or(0.0)))
    }
}

/// Days left in the matchup period (today through `period_end`), else the next 7 days.
pub fn plan_days(ctx: &LeagueContext) -> Vec<NaiveDate> {
    let end = match ctx.period_end {
        Some(e) if e >= ctx.as_of => e,
        _ => ctx.as_of + Duration::days(6),
    };
    (0..=(end - ctx.as_of).num_days()).map(|k| ctx.as_of + Duration::days(k)).collect()
}

fn team_dates<'c>(ctx: &'c LeagueContext, p: &Player) -> &'c [NaiveDate] {
    let team = normalize_team(p.team.as_deref()).unwrap_or_default();
    ctx.schedule.get(&team).map(|v| v.as_slice()).unwrap_or(&[])
}

fn plays(ctx: &LeagueContext, p: &Player, day: NaiveDate) -> bool {
    team_dates(ctx, p).contains(&day)
}

/// False while an out / IR / suspended player is out: before his estimated return date
/// (`PlayerValue.return_date`), or every day when there is none; and for a waiver claim until the
/// day after `waiver_until`.
pub fn available_on(p: &Player, pv: Option<&PlayerValue>, day: NaiveDate) -> bool {
    if p.waiver_until.is_some_and(|w| day <= w) {
        return false;
    }
    if !unavailable(p.status) {
        return true;
    }
    pv.and_then(|pv| pv.return_date).is_some_and(|ret| day >= ret)
}

/// The start share inside his per-game value: valuate's week share, re-weighted for the window's
/// back-to-backs when the harness ledger gave his team a rate.
fn week_share(ctx: &LeagueContext, memo: &ScheduleMemo, p: &Player, pv: &PlayerValue) -> f64 {
    let share = pv.start_share.unwrap_or(0.0);
    let team = normalize_team(p.team.as_deref()).unwrap_or_default();
    let Some(b2b) = ctx.b2b_second_night.get(&team) else { return share };
    if pv.proj_week.is_none() {
        return share;
    }
    let Some(window) = memo_window(ctx, memo) else { return share };
    let dates = ctx.schedule.get(&team).map(|v| v.as_slice()).unwrap_or(&[]);
    week_start_share(share, dates, window.start, window.days, b2b.0, SEASON_B2B_FRAC)
}

/// P(`p` starts in goal on `day`), None for skaters / without a start share.
pub fn night_start_probability(ctx: &LeagueContext, p: &Player, pv: Option<&PlayerValue>, day: NaiveDate) -> Option<f64> {
    let pv = pv?;
    if !p.is_goalie() {
        return None;
    }
    let share = pv.start_share.filter(|s| *s != 0.0)?;
    if day == ctx.as_of {
        if let Some(prob) = confirmed_start_probability(p) {
            return Some(prob);
        }
    }
    let team = normalize_team(p.team.as_deref()).unwrap_or_default();
    let dates = ctx.schedule.get(&team).map(|v| v.as_slice()).unwrap_or(&[]);
    let b2b_p = ctx.b2b_second_night.get(&team).map_or(B2B_SECOND_NIGHT_P, |b| b.0);
    let prev = day - Duration::days(1);
    if !dates.contains(&prev) {
        return Some(night_start_probs(share, b2b_p, SEASON_B2B_FRAC).0);
    }
    if prev == ctx.as_of {
        if let Some(q) = confirmed_start_probability(p) {
            return Some(second_night_prob(share, q, b2b_p));
        }
    }
    Some(night_start_probs(share, b2b_p, SEASON_B2B_FRAC).1)
}

/// His per-game opponent multiplier on `day` (neutral for goalies).
pub fn opp_game(ctx: &LeagueContext, memo: &ScheduleMemo, p: &Player, day: NaiveDate) -> OppGame {
    skater_opp_game(ctx, memo, p, day)
}

/// `_week_opp`: the opponent factor already inside `proj_week` (a Reason `OPP_WEEK_REASON`), else 1.0.
pub fn week_opp(pv: &PlayerValue) -> f64 {
    match pv.reasons.iter().find(|r| r.code == OPP_WEEK_REASON && r.value.is_some_and(|v| v != 0.0)) {
        Some(r) if r.value.unwrap_or(0.0) > 0.0 => r.value.unwrap_or(1.0),
        _ => 1.0,
    }
}

/// Expected points from `p` on `day` (see the Python docstring): 0 without a game or while he is
/// out, else his per-game value adjusted for the night's start probability (goalies) or the
/// game's opponent (skaters).
pub fn day_value(ctx: &LeagueContext, memo: &ScheduleMemo, p: &Player, pv: Option<&PlayerValue>, day: NaiveDate) -> f64 {
    let Some(pv) = pv else { return 0.0 };
    if !plays(ctx, p, day) || !available_on(p, Some(pv), day) {
        return 0.0;
    }
    let mut v = per_game_value(Some(pv)).unwrap_or(0.0);
    let mut from_week = true;
    if unavailable(p.status) {
        // back from injury: undo the week's averaged availability
        if pv.avail_week > 0.0 {
            v /= pv.avail_week;
        } else {
            v = pv.fpg * or_one(pv.start_share);
            from_week = false;
        }
    }
    if let Some(prob) = night_start_probability(ctx, p, Some(pv), day) {
        let share = if from_week && pv.proj_week.is_some() {
            week_share(ctx, memo, p, pv)
        } else {
            pv.start_share.unwrap_or(0.0)
        };
        if share > 0.0 {
            v = v / share * prob;
        }
    } else if !p.is_goalie() {
        v = v * opp_game(ctx, memo, p, day).mult / if from_week { week_opp(pv) } else { 1.0 };
    }
    v.max(0.0)
}

/// His team's game on `day` has started by `now`.
pub fn started(ctx: &LeagueContext, p: &Player, day: NaiveDate, now: &PyDateTime) -> bool {
    let team = normalize_team(p.team.as_deref()).unwrap_or_default();
    ctx.game_starts.get(&team).and_then(|m| m.get(&day)).is_some_and(|start| now >= start)
}

/// One player's numbers for one plan day.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DayInfo {
    /// `0.0` when locked, else `day_value`
    pub value: f64,
    /// plays that day and is available
    pub playing: bool,
    /// today's game already started
    pub locked: bool,
}

/// [`DayInfo`] of `p` (valued at `pv`) for each of `days`.
pub fn player_days(
    ctx: &LeagueContext,
    memo: &ScheduleMemo,
    p: &Player,
    pv: Option<&PlayerValue>,
    days: &[NaiveDate],
    now: &PyDateTime,
) -> Vec<DayInfo> {
    let dates = team_dates(ctx, p);
    days.iter()
        .map(|&d| {
            let locked = d == ctx.as_of && started(ctx, p, d, now);
            let value = if locked { 0.0 } else { day_value(ctx, memo, p, pv, d) };
            let playing = dates.contains(&d) && available_on(p, pv, d);
            DayInfo { value, playing, locked }
        })
        .collect()
}

/// Per-cid [`DayInfo`]s for a fixed set of days, now and values: shared by every plan of one
/// recommender run (waivers evaluates one plan per free agent over the same roster).
pub struct PlanCache<'a> {
    pub days: Vec<NaiveDate>,
    pub slots: Vec<String>,
    now: PyDateTime,
    ctx: &'a LeagueContext,
    memo: &'a ScheduleMemo,
    values: &'a Map<Cid, PlayerValue>,
    info: HashMap<Cid, Vec<DayInfo>>,
}

impl<'a> PlanCache<'a> {
    /// Precompute the day infos of `players` (valued from `values`).
    pub fn new<'p>(
        ctx: &'a LeagueContext,
        memo: &'a ScheduleMemo,
        values: &'a Map<Cid, PlayerValue>,
        days: Vec<NaiveDate>,
        now: &PyDateTime,
        players: impl IntoIterator<Item = &'p Player>,
    ) -> Self {
        let mut c = PlanCache {
            days,
            slots: starting_slots(&ctx.roster_shape),
            now: *now,
            ctx,
            memo,
            values,
            info: HashMap::new(),
        };
        for p in players {
            if !c.info.contains_key(&p.cid) {
                let v = player_days(ctx, memo, p, values.get(&p.cid), &c.days, now);
                c.info.insert(p.cid.clone(), v);
            }
        }
        c
    }

    /// Day infos of `p`: precomputed from the cache's values, or `pv` when given (an override).
    pub fn infos(&self, p: &Player, pv: Option<&PlayerValue>) -> Cow<'_, [DayInfo]> {
        if pv.is_none() {
            if let Some(v) = self.info.get(&p.cid) {
                return Cow::Borrowed(v.as_slice());
            }
        }
        let pv = pv.or_else(|| self.values.get(&p.cid));
        Cow::Owned(player_days(self.ctx, self.memo, p, pv, &self.days, &self.now))
    }

    /// The plan for `view` with `overrides` (cid -> value used instead of the cache's).
    pub fn plan(&self, view: &RosterView<'_>, overrides: &[(&str, &PlayerValue)], full: bool) -> DailyPlan {
        let pool = view.candidates();
        let infos: Vec<Cow<'_, [DayInfo]>> = pool
            .iter()
            .map(|p| self.infos(p, overrides.iter().find(|(c, _)| *c == p.cid.as_str()).map(|x| x.1)))
            .collect();
        let refs: Vec<&[DayInfo]> = infos.iter().map(|c| c.as_ref()).collect();
        plan_from(view, &self.slots, &self.days, &pool, &refs, full)
    }
}

/// The plan core: `pool` = `view.candidates()`, `info[i][k]` = pool player i on day k. `full`
/// false skips the per-day maps (only `optimal` / `current` are filled).
fn plan_from(
    view: &RosterView<'_>,
    slots: &[String],
    days: &[NaiveDate],
    pool: &[&Player],
    info: &[&[DayInfo]],
    full: bool,
) -> DailyPlan {
    let starters = view.starters();
    let starters_ordered = view.starters_ordered();
    let current_types = view.current_types();
    let pool_idx: HashMap<&str, usize> = pool.iter().enumerate().map(|(i, p)| (p.cid.as_str(), i)).collect();
    let mut out = Vec::with_capacity(days.len());
    let mut vals = vec![0.0; pool.len()];
    let mut locked = vec![false; pool.len()];
    for (k, &d) in days.iter().enumerate() {
        for i in 0..pool.len() {
            vals[i] = info[i][k].value;
            locked[i] = info[i][k].locked;
        }
        let (assign, total) = best_pool(pool, slots, &vals, &locked, &starters, &current_types);
        let current = py_sum(starters_ordered.iter().map(|c| pool_idx.get(c).map_or(0.0, |&i| vals[i])));
        let day = if full {
            PlanDay {
                day: d,
                assign: assign
                    .iter()
                    .filter(|&&(_, i)| info[i][k].playing)
                    .map(|&(s, i)| (s, pool[i].cid.clone()))
                    .collect(),
                values: pool.iter().enumerate().map(|(i, p)| (p.cid.clone(), vals[i])).collect(),
                playing: pool.iter().enumerate().filter(|(i, _)| info[*i][k].playing).map(|(_, p)| p.cid.clone()).collect(),
                locked: pool.iter().enumerate().filter(|(i, _)| locked[*i]).map(|(_, p)| p.cid.clone()).collect(),
                optimal: total,
                current,
            }
        } else {
            PlanDay {
                day: d,
                assign: Map::new(),
                values: Map::new(),
                playing: Set::new(),
                locked: Set::new(),
                optimal: total,
                current,
            }
        };
        out.push(day);
    }
    DailyPlan { days: out }
}

/// The best lineup for each of `days` (default [`plan_days`]) in daily-lineup leagues with the
/// NHL schedule loaded; None otherwise. Today, players whose game has started by `now` keep their
/// place and are worth 0 (their points are already in the provider's score).
pub fn daily_plan(
    ctx: &LeagueContext,
    team: &FantasyTeam,
    values: &Map<Cid, PlayerValue>,
    days: Option<&[NaiveDate]>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Option<DailyPlan> {
    daily_plan_view(ctx, &RosterView::of(team), values, days, now, memo)
}

/// [`daily_plan`] on a [`RosterView`].
pub fn daily_plan_view(
    ctx: &LeagueContext,
    view: &RosterView<'_>,
    values: &Map<Cid, PlayerValue>,
    days: Option<&[NaiveDate]>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Option<DailyPlan> {
    if ctx.schedule.is_empty() || lineup_lock::lock(ctx) != "daily" {
        return None;
    }
    let days: Vec<NaiveDate> = match days {
        Some(d) => d.to_vec(),
        None => plan_days(ctx),
    };
    let slots = starting_slots(&ctx.roster_shape);
    let pool = view.candidates();
    let infos: Vec<Vec<DayInfo>> = pool.iter().map(|p| player_days(ctx, memo, p, values.get(&p.cid), &days, now)).collect();
    let refs: Vec<&[DayInfo]> = infos.iter().map(|v| v.as_slice()).collect();
    Some(plan_from(view, &slots, &days, &pool, &refs, true))
}

// ------------------------------------------------------------------------------- reasons

fn status_reason(p: &Player) -> Reason {
    let note = p.status_note.as_deref().map_or(String::new(), |n| format!(" ({n})"));
    Reason::new("STATUS", format!("{} is {}{note}", p.name, p.status.as_str()))
}

fn not_healthy(p: &Player) -> bool {
    !matches!(p.status, Status::Healthy | Status::Unknown)
}

fn week_reasons(p: &Player, pv: Option<&PlayerValue>, horizon: Horizon) -> Vec<Reason> {
    let mut out = Vec::new();
    let Some(pv) = pv else { return out };
    if let Some(g) = pv.games_next7 {
        let off = match pv.offnight_next7 {
            Some(o) if o != 0 => format!(" ({o} off-night)"),
            _ => String::new(),
        };
        out.push(Reason::new("GAMES_NEXT7", format!("{}: {g} games next 7 days{off}", p.name)).with_value(Some(g as f64)));
    }
    match (horizon, pv.proj_week) {
        (Horizon::Week, Some(pw)) => out.push(
            Reason::new("PROJ_WEEK", format!("{}: {} projected pts this week", p.name, fixed(pw, 1, false))).with_value(Some(pw)),
        ),
        _ => {
            let lv = pv.lineup_value(horizon);
            let code = if horizon == Horizon::Week { "PROJ_WEEK" } else { "FPG" };
            out.push(
                Reason::new(code, format!("{}: {} FPG ({})", p.name, fixed(lv, 2, false), horizon.as_str())).with_value(Some(lv)),
            );
        }
    }
    if not_healthy(p) {
        out.push(status_reason(p));
    }
    out
}

/// Today's expected points, today's opponent adjustment (OPP_ADJ, skaters) and his games left in
/// the matchup (daily-lineup leagues).
fn day_reasons(p: &Player, plan: &DailyPlan, ctx: Option<(&LeagueContext, &ScheduleMemo)>) -> Vec<Reason> {
    let day0 = &plan.days[0];
    let today = day0.values.get(&p.cid).copied().unwrap_or(0.0);
    let games = plan.days.iter().filter(|d| d.playing.contains(&p.cid)).count();
    let text = if today > 0.0 {
        format!("{}: {} projected pts today", p.name, fixed(today, 2, false))
    } else if day0.locked.contains(&p.cid) && day0.playing.contains(&p.cid) {
        format!("{}: game already started today", p.name)
    } else if day0.playing.contains(&p.cid) {
        format!("{}: not expected to play today", p.name)
    } else {
        format!("{}: no game today", p.name)
    };
    let mut out = vec![Reason::new("DAY_VALUE", text).with_value(Some(today))];
    if let Some((ctx, memo)) = ctx {
        if day0.playing.contains(&p.cid) && !day0.locked.contains(&p.cid) {
            let g = opp_game(ctx, memo, p, day0.day);
            if (g.mult - 1.0).abs() >= OPP_REASON_MIN {
                out.push(Reason::new("OPP_ADJ", g.text(&p.name)).with_value(Some(g.mult)).with_baseline(Some(1.0)));
            }
        }
    }
    let last = plan.days[plan.days.len() - 1].day;
    out.push(
        Reason::new(
            "GAMES_LEFT",
            format!(
                "{}: {games} game{} left this matchup (through {})",
                p.name,
                if games != 1 { "s" } else { "" },
                day_text(last)
            ),
        )
        .with_value(Some(games as f64)),
    );
    if not_healthy(p) {
        out.push(status_reason(p));
    }
    out
}

// --------------------------------------------------------------------------- slot changes

/// One starting-slot type gets `incoming` (None: left empty) in place of `outgoing` (None: the
/// slot was empty).
#[derive(Clone, Debug, PartialEq)]
pub struct SlotChange {
    pub slot: String,
    pub incoming: Option<Cid>,
    pub outgoing: Option<Cid>,
}

/// Per slot type, pair the players entering it with the players leaving it (or with its empty
/// slots); `value` orders the pairing (best incoming with worst outgoing first).
pub fn slot_changes(view: &RosterView<'_>, assign: &Map<usize, Cid>, slots: &[String], value: &dyn Fn(&str) -> f64) -> Vec<SlotChange> {
    let mut cur: Map<&str, Vec<&str>> = Map::new();
    for e in &view.entries {
        if e.starting {
            cur.entry(e.slot).or_default().push(e.player.cid.as_str());
        }
    }
    let mut sorted_assign: Vec<(&usize, &Cid)> = assign.iter().collect();
    sorted_assign.sort_by_key(|x| *x.0);
    let mut opt: Map<&str, Vec<&str>> = Map::new();
    for (i, cid) in sorted_assign {
        opt.entry(slots[*i].as_str()).or_default().push(cid.as_str());
    }
    let cap = |t: &str| slots.iter().filter(|s| s.as_str() == t).count();
    let mut types: Vec<&str> = Vec::new();
    for t in slots.iter().map(|s| s.as_str()).chain(cur.keys().copied()) {
        if !types.contains(&t) {
            types.push(t);
        }
    }
    let empty: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for t in types {
        let now = cur.get(t).unwrap_or(&empty);
        let then = opt.get(t).unwrap_or(&empty);
        let mut ins: Vec<&str> = then.iter().copied().filter(|c| !now.contains(c)).collect();
        ins.sort_by(|a, b| cmp_f64(-value(a), -value(b)).then_with(|| a.cmp(b)));
        let mut outs: Vec<&str> = now.iter().copied().filter(|c| !then.contains(c)).collect();
        outs.sort_by(|a, b| cmp_f64(value(a), value(b)).then_with(|| a.cmp(b)));
        let mut partners: Vec<Option<&str>> = outs.iter().map(|c| Some(*c)).collect();
        partners.extend(std::iter::repeat(None).take(cap(t).saturating_sub(now.len())));
        for (k, c) in ins.iter().enumerate() {
            out.push(SlotChange {
                slot: t.to_string(),
                incoming: Some(c.to_string()),
                outgoing: partners.get(k).copied().flatten().map(str::to_string),
            });
        }
        for c in outs.iter().skip(ins.len()) {
            out.push(SlotChange { slot: t.to_string(), incoming: None, outgoing: Some(c.to_string()) });
        }
    }
    out
}

/// Link slot changes into chains (bench player entering ... starter leaving).
pub fn chains(changes: &[SlotChange], starters: &HashSet<&str>) -> Vec<Vec<SlotChange>> {
    let mut by_in: HashMap<&str, usize> = HashMap::new();
    for (k, c) in changes.iter().enumerate() {
        if let Some(i) = &c.incoming {
            by_in.insert(i.as_str(), k);
        }
    }
    let mut used = vec![false; changes.len()];
    let mut out: Vec<Vec<SlotChange>> = Vec::new();
    let follow = |k: usize, used: &mut Vec<bool>| -> Vec<SlotChange> {
        let mut chain = vec![changes[k].clone()];
        used[k] = true;
        loop {
            let nxt = chain.last().and_then(|c| c.outgoing.as_deref()).and_then(|o| by_in.get(o)).copied();
            match nxt {
                Some(n) if !used[n] => {
                    chain.push(changes[n].clone());
                    used[n] = true;
                }
                _ => break,
            }
        }
        chain
    };
    let heads: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.incoming.as_deref().is_none_or(|i| !starters.contains(i)))
        .map(|(k, _)| k)
        .collect();
    for k in heads {
        if !used[k] {
            out.push(follow(k, &mut used));
        }
    }
    for k in 0..changes.len() {
        if !used[k] {
            out.push(follow(k, &mut used));
        }
    }
    out
}

fn leaving(p: &Player) -> String {
    if unavailable(p.status) {
        format!("{} ({})", p.name, p.status.as_str())
    } else {
        p.name.clone()
    }
}

fn chain_title(chain: &[SlotChange], by_cid: &HashMap<&str, &Player>) -> String {
    let pl = |c: &Option<Cid>| by_cid[c.as_deref().unwrap_or("")];
    let first = &chain[0];
    if chain.len() == 1 {
        if first.incoming.is_none() {
            return format!("Bench {} (leave {} empty)", leaving(pl(&first.outgoing)), first.slot);
        }
        if first.outgoing.is_none() {
            return format!("Start {} at {} (empty slot)", pl(&first.incoming).name, first.slot);
        }
        return format!("Start {} over {}", pl(&first.incoming).name, leaving(pl(&first.outgoing)));
    }
    let mut parts = Vec::new();
    for (k, c) in chain.iter().enumerate() {
        let head = if c.incoming.is_none() {
            format!("leave {} empty", c.slot)
        } else {
            let verb = if k == 0 { "Start" } else { "move" };
            format!("{verb} {} {} {}", pl(&c.incoming).name, if k == 0 { "at" } else { "to" }, c.slot)
        };
        let tail = if c.outgoing.is_none() {
            " (empty slot)".to_string()
        } else if k == chain.len() - 1 {
            format!(" over {}", leaving(pl(&c.outgoing)))
        } else {
            format!(" for {}", pl(&c.outgoing).name)
        };
        parts.push(head + &tail);
    }
    let title = parts.join("; ");
    let mut chars = title.chars();
    match chars.next() {
        Some(f) => py_upper(&f.to_string()) + chars.as_str(),
        None => title,
    }
}

fn start_strength_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(Confirmed|Likely|Expected)\b").expect("regex"))
}

/// Today's start probability from `confirmed_start` (None when unknown): 1.0 / 0.0 for a
/// confirmed report, 0.8 / 0.2 for likely / expected (read from `start_source`).
pub fn confirmed_start_probability(p: &Player) -> Option<f64> {
    if !p.is_goalie() {
        return None;
    }
    let cs = p.confirmed_start?;
    let m = start_strength_re().captures(p.start_source.as_deref().unwrap_or(""));
    let likely = m.and_then(|m| m.get(1)).is_some_and(|g| g.as_str().to_lowercase() != "confirmed");
    let (starter, other) = if likely { START_PROB_LIKELY } else { START_PROB_CONFIRMED };
    Some(if cs { starter } else { other })
}

/// Daily-lineup leagues, week horizon: re-project my goalies whose start tonight is known.
/// Returns (values, {cid: (prob, share)}).
pub fn apply_confirmed_starts<'v>(
    ctx: &LeagueContext,
    values: &'v Map<Cid, PlayerValue>,
    horizon: Horizon,
) -> (Cow<'v, Map<Cid, PlayerValue>>, Map<Cid, (f64, f64)>) {
    let mut used = Map::new();
    if horizon != Horizon::Week || lineup_lock::lock(ctx) != "daily" {
        return (Cow::Borrowed(values), used);
    }
    let Ok(team) = ctx.my_team() else { return (Cow::Borrowed(values), used) };
    let mut out: Option<Map<Cid, PlayerValue>> = None;
    for p in team.players_iter() {
        let Some(prob) = confirmed_start_probability(p) else { continue };
        let Some(pv) = values.get(&p.cid) else { continue };
        let (Some(proj), Some(games)) = (pv.proj_week, pv.games_next7.filter(|g| *g != 0)) else { continue };
        if !plays(ctx, p, ctx.as_of) {
            continue;
        }
        let share = pv.start_share.unwrap_or(1.0);
        if share <= 0.0 {
            continue;
        }
        let per_start = proj / (games as f64 * share);
        let new = (proj + per_start * (prob - share)).max(0.0);
        let m = out.get_or_insert_with(|| values.clone());
        if let Some(x) = m.get_mut(&p.cid) {
            x.proj_week = Some(new);
        }
        used.insert(p.cid.clone(), (prob, share));
    }
    (out.map_or(Cow::Borrowed(values), Cow::Owned), used)
}

fn start_reason(p: &Player, prob: f64, share: f64) -> Reason {
    Reason::new(
        "CONFIRMED_START",
        format!(
            "{}: start probability today {} (season share {}); {}",
            p.name,
            percent(prob, 0, false),
            percent(share, 0, false),
            p.start_source.as_deref().filter(|s| !s.is_empty()).unwrap_or("Daily Faceoff")
        ),
    )
    .with_value(Some(prob))
    .with_baseline(Some(share))
}

/// Units of a lineup gain: week points when every player involved has a projected week, else FPG.
fn gain_units(players: &[&Player], values: &Map<Cid, PlayerValue>, horizon: Horizon) -> (GainUnits, Option<i64>) {
    if horizon == Horizon::Week {
        if !players.is_empty() && players.iter().all(|p| values.get(&p.cid).is_some_and(|pv| pv.proj_week.is_some())) {
            return (GainUnits::WeekPts, Some(7));
        }
        return (GainUnits::SeasonFpg, Some(7));
    }
    (GainUnits::SeasonFpg, None)
}

/// Weekly-lock leagues: the lineup is set once per scoring period, locking Monday.
fn weekly_lock(mut r: Recommendation) -> Recommendation {
    r.title = format!("{} {WEEKLY_LOCK_TEXT}", r.title);
    r.reasons.push(Reason::new(
        "LINEUP_LOCK",
        "Fantrax lineup changes are executed weekly (Rules page): the lineup locks Monday for the whole scoring period, \
         so this uses the week horizon",
    ));
    r
}

/// Start/sit changes that beat the current lineup, plus injured starters and healthy IR players
/// (see the Python docstring). `now` is the clock the daily plan locks started games with.
pub fn recommend_lineup(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    horizon: Horizon,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Result<Vec<Recommendation>, CoreError> {
    let team = ctx.my_team()?;
    let view = RosterView::of(team);
    let plan = if horizon == Horizon::Week { daily_plan_view(ctx, &view, values, None, now, memo) } else { None };
    let (values, starts): (Cow<'_, Map<Cid, PlayerValue>>, Map<Cid, (f64, f64)>) = match &plan {
        None => apply_confirmed_starts(ctx, values, horizon),
        Some(_) => {
            let mut s = Map::new();
            for p in team.players_iter() {
                let Some(prob) = confirmed_start_probability(p) else { continue };
                let Some(pv) = values.get(&p.cid) else { continue };
                if !plays(ctx, p, ctx.as_of) {
                    continue;
                }
                s.insert(p.cid.clone(), (prob, or_one(pv.start_share)));
            }
            (Cow::Borrowed(values), s)
        }
    };
    let values: &Map<Cid, PlayerValue> = &values;
    let mut recs: Vec<Recommendation> = Vec::new();
    let slots = starting_slots(&ctx.roster_shape);
    let mut by_cid: HashMap<&str, &Player> = HashMap::new();
    for p in team.players_iter() {
        by_cid.insert(p.cid.as_str(), p);
    }
    let starters = view.starters();
    let (assign, today_vals): (Map<usize, Cid>, Option<&Map<Cid, f64>>) = match &plan {
        None => (optimal_lineup(team, values, &ctx.roster_shape, horizon).0, None),
        Some(plan) => {
            let d0 = &plan.days[0];
            let today = &d0.values;
            let locked: HashSet<&str> = d0.locked.iter().map(|c| c.as_str()).collect();
            let vf = |c: &str| today.get(c).copied().unwrap_or(0.0);
            (best(&view, &slots, &vf, &locked).0, Some(today))
        }
    };
    let vc = |c: &str| -> f64 {
        match today_vals {
            None => value_of(values.get(c), horizon),
            Some(t) => t.get(c).copied().unwrap_or(0.0),
        }
    };
    let after: HashSet<&str> = assign.values().map(|c| c.as_str()).collect();
    for chain in chains(&slot_changes(&view, &assign, &slots, &vc), &starters) {
        let head = chain[0].incoming.as_deref();
        let tail = chain[chain.len() - 1].outgoing.as_deref();
        let pin = head.filter(|h| !starters.contains(h)).map(|h| by_cid[h]);
        let pout = tail.filter(|t| !after.contains(t)).map(|t| by_cid[t]);
        let moved: Vec<&Player> = chain
            .iter()
            .filter_map(|c| c.incoming.as_deref())
            .filter(|c| starters.contains(c))
            .map(|c| by_cid[c])
            .collect();
        let gain = pin.map_or(0.0, |p| vc(&p.cid)) - pout.map_or(0.0, |p| vc(&p.cid));
        let v_out = pout.map_or(0.0, |p| vc(&p.cid));
        let threshold = MIN_GAIN_ABS.max(MIN_GAIN_REL * v_out.abs());
        if gain <= threshold {
            continue;
        }
        let mut reasons = vec![Reason::new(
            "LINEUP_GAIN",
            format!(
                "+{} ({}) from this change",
                fixed(gain, 2, false),
                if plan.is_none() { horizon.as_str() } else { "today" }
            ),
        )
        .with_value(Some(gain))
        .with_baseline(Some(threshold))];
        for x in [pin, pout].into_iter().flatten() {
            match &plan {
                None => reasons.extend(week_reasons(x, values.get(&x.cid), horizon)),
                Some(plan) => reasons.extend(day_reasons(x, plan, Some((ctx, memo)))),
            }
        }
        for c in &chain {
            if let Some(i) = c.incoming.as_deref().filter(|i| starters.contains(i)) {
                let repl = c.outgoing.as_deref().map_or(String::new(), |o| format!(" (replacing {})", by_cid[o].name));
                reasons.push(Reason::new("LINEUP_MOVE", format!("{} moves to {}{repl}", by_cid[i].name, c.slot)));
            }
        }
        let mut headline = None;
        let (units, days, title) = match &plan {
            None => {
                let involved: Vec<&Player> = [pin, pout].into_iter().flatten().collect();
                let (u, d) = gain_units(if involved.is_empty() { &moved } else { &involved }, values, horizon);
                (u, d, chain_title(&chain, &by_cid))
            }
            Some(plan) => {
                let day0 = &plan.days[0];
                let idle = pout.is_some_and(|p| !day0.playing.contains(&p.cid));
                let title = match (pin, pout) {
                    (Some(pi), Some(po)) if chain.len() == 1 && idle => {
                        format!("Start {} in {}'s slot today ({} has no game)", pi.name, po.name, po.name)
                    }
                    _ => chain_title(&chain, &by_cid) + " today",
                };
                let opp = pin.and_then(|p| {
                    let team = normalize_team(p.team.as_deref()).unwrap_or_default();
                    ctx.opponents.get(&team).and_then(|m| m.get(&day0.day)).map(|s| s.as_str())
                });
                let h = day_swap_headline(
                    pin,
                    pout,
                    pin.map_or(0.0, |p| vc(&p.cid)),
                    v_out,
                    !idle,
                    Some(chain[0].slot.as_str()),
                    opp,
                );
                headline = Some(h).filter(|h| !h.is_empty());
                (GainUnits::DayPts, Some(1), title)
            }
        };
        let mut r = Recommendation::new(RecKind::Lineup, gain, title);
        r.headline = headline;
        r.add = pin.into_iter().cloned().collect();
        r.drop = pout.into_iter().cloned().collect();
        r.subjects = moved.into_iter().cloned().collect();
        r.reasons = reasons;
        r.predicted_gain = Some(gain);
        r.gain_units = Some(units);
        r.horizon_days = days;
        recs.push(r);
    }

    // IR swaps: a healthy / dtd player in an IR slot comes out and an injured active player takes
    // his slot (players covered by a free IR slot get "Move to IR" from recommend.injuries instead)
    let ir_cap = ctx.roster_shape.get("IR").copied().unwrap_or(0);
    let ir_used = team.slots.iter().filter(|s| s.slot == "IR" && s.player.is_some()).count() as i64;
    let free_ir = (ir_cap - ir_used).max(0) as usize;
    let fpg = |p: &Player| values.get(&p.cid).map_or(0.0, |pv| pv.fpg);
    let mut stash: Vec<&Player> = team
        .slots
        .iter()
        .filter(|s| s.slot != "IR")
        .filter_map(|s| s.player.as_ref())
        .filter(|p| IR_STASHABLE.contains(&p.status.as_str()))
        .collect();
    stash.sort_by(|a, b| cmp_f64(-fpg(a), -fpg(b)));
    let stash: Vec<&Player> = stash.into_iter().skip(free_ir).collect();
    let mut back: Vec<&Player> = team
        .slots
        .iter()
        .filter(|s| s.slot == "IR")
        .filter_map(|s| s.player.as_ref())
        .filter(|p| matches!(p.status, Status::Healthy | Status::Dtd))
        .collect();
    back.sort_by(|a, b| cmp_f64(-fpg(a), -fpg(b)));
    let swaps: HashMap<&str, &Player> = back.iter().zip(stash.iter()).map(|(b, x)| (b.cid.as_str(), *x)).collect();
    let swapped: HashSet<&str> = swaps.values().map(|x| x.cid.as_str()).collect();
    let full = team.slots.iter().filter(|s| s.player.is_some() && s.slot != "IR").count() as i64
        >= ctx.roster_shape.iter().filter(|(k, _)| k.as_str() != "IR").map(|(_, n)| *n).sum::<i64>();

    let activation_gain = |p: &Player, pv: Option<&PlayerValue>, to_ir: Option<&Player>| -> (Option<f64>, Option<GainUnits>, Option<i64>) {
        let Some(pv) = pv else { return (None, None, None) };
        if let Some(plan) = &plan {
            let mut v = view.clone();
            for e in v.entries.iter_mut() {
                if e.player.cid == p.cid {
                    e.slot = "BN";
                    e.starting = false;
                } else if to_ir.is_some_and(|x| x.cid == e.player.cid) {
                    e.slot = "IR";
                    e.starting = false;
                }
            }
            if let Some(after) = daily_plan_view(ctx, &v, values, None, now, memo) {
                return (
                    Some(round_to(after.optimal() - plan.optimal(), 2)),
                    Some(GainUnits::MatchupPts),
                    Some(plan.days.len() as i64),
                );
            }
        }
        let (u, d) = gain_units(&[p], values, horizon);
        (Some(value_of(Some(pv), horizon)), Some(u), d)
    };

    for s in &team.slots {
        let Some(p) = s.player.as_ref() else { continue };
        let pv = values.get(&p.cid);
        if swapped.contains(p.cid.as_str()) {
            continue; // leaves the lineup through the swap below
        }
        if s.slot == "IR" {
            if let Some(x) = swaps.get(p.cid.as_str()) {
                let (gain, units, days) = activation_gain(p, pv, Some(x));
                let mut r = Recommendation::new(
                    RecKind::Lineup,
                    pv.map_or(0.0, |pv| pv.fpg_season),
                    format!(
                        "Swap IR slots: activate {} ({}), move {} ({}) to IR",
                        p.name,
                        p.status.as_str(),
                        x.name,
                        x.status.as_str()
                    ),
                );
                r.add = vec![p.clone()];
                r.subjects = vec![(*x).clone()];
                r.predicted_gain = gain;
                r.gain_units = units;
                r.horizon_days = days;
                r.reasons = vec![
                    Reason::new(
                        "IR_SWAP",
                        format!(
                            "{} is {} but sits in an IR slot, {} is {} on the active roster: swapping them needs no drop",
                            p.name,
                            p.status.as_str(),
                            x.name,
                            x.status.as_str()
                        ),
                    ),
                    status_reason(x),
                ];
                r.reasons.extend(week_reasons(p, pv, horizon).into_iter().take(1));
                recs.push(r);
                continue;
            }
        }
        let dropped: HashSet<&str> = recs.iter().filter_map(|r| r.drop.first()).map(|p| p.cid.as_str()).collect();
        if s.starting && unavailable(p.status) && !dropped.contains(p.cid.as_str()) {
            let mut r = Recommendation::new(
                RecKind::Lineup,
                pv.map_or(0.0, |pv| pv.fpg),
                format!("Bench {}: {} but in a starting slot ({})", p.name, p.status.as_str(), s.slot),
            );
            r.drop = vec![p.clone()];
            r.reasons = vec![status_reason(p)];
            r.reasons.extend(week_reasons(p, pv, horizon).into_iter().take(1));
            recs.push(r);
        } else if s.slot == "IR" && matches!(p.status, Status::Healthy | Status::Dtd) {
            let (gain, units, days) = activation_gain(p, pv, None);
            let note = p.status_note.as_deref().map_or(String::new(), |n| format!(" ({n})"));
            let mut r = Recommendation::new(
                RecKind::Lineup,
                pv.map_or(0.0, |pv| pv.fpg_season),
                format!("Activate {} from IR ({})", p.name, p.status.as_str()),
            );
            r.add = vec![p.clone()];
            r.predicted_gain = gain;
            r.gain_units = units;
            r.horizon_days = days;
            r.reasons = vec![Reason::new(
                "STATUS",
                format!("{} is {} but sits in an IR slot{note}", p.name, p.status.as_str()),
            )];
            if full {
                r.reasons.push(Reason::new(
                    "ROSTER_FULL",
                    format!("Roster is full: activating {} needs a drop (or another player moved to IR)", p.name),
                ));
            }
            r.reasons.extend(week_reasons(p, pv, horizon).into_iter().take(1));
            recs.push(r);
        } else if s.slot == "IR" {
            let (Some(plan), Some(pv)) = (&plan, pv) else { continue };
            let Some(ret) = pv.return_date else { continue };
            let last = plan.days[plan.days.len() - 1].day;
            if !(plan.days[0].day < ret && ret <= last) {
                continue;
            }
            let back: Vec<NaiveDate> = plan.days.iter().map(|d| d.day).filter(|d| *d >= ret && plays(ctx, p, *d)).collect();
            if back.is_empty() {
                continue;
            }
            let pts = py_sum(back.iter().map(|d| day_value(ctx, memo, p, Some(pv), *d)));
            let note = p.status_note.as_deref().map_or(String::new(), |n| format!(" ({n})"));
            let mut r = Recommendation::new(
                RecKind::Lineup,
                pts,
                format!("{} due back {}: activate him from IR then", p.name, day_text(ret)),
            );
            r.subjects = vec![p.clone()];
            r.reasons = vec![
                Reason::new(
                    "RETURN_DUE",
                    format!("{} is {}, expected back {}{note}", p.name, p.status.as_str(), day_text(ret)),
                )
                .with_value(Some(pts)),
                Reason::new(
                    "GAMES_LEFT",
                    format!(
                        "{}: {} game{} from then to {}, ~{} pts if he plays",
                        p.name,
                        back.len(),
                        if back.len() != 1 { "s" } else { "" },
                        day_text(last),
                        fixed(pts, 1, false)
                    ),
                )
                .with_value(Some(back.len() as f64)),
            ];
            recs.push(r);
        }
    }
    if !starts.is_empty() {
        for r in recs.iter_mut() {
            let mut seen: Vec<String> = Vec::new();
            for x in r.add.iter().chain(r.drop.iter()).chain(r.subjects.iter()) {
                if !seen.contains(&x.cid) {
                    seen.push(x.cid.clone());
                }
            }
            for cid in seen {
                if let Some(&(prob, share)) = starts.get(&cid) {
                    if let Some(p) = by_cid.get(cid.as_str()) {
                        r.reasons.push(start_reason(p, prob, share));
                    }
                }
            }
        }
    }
    if lineup_lock::lock(ctx) == "weekly" {
        recs = recs.into_iter().map(weekly_lock).collect();
    }
    sort_desc_by(&mut recs, |a, b| cmp_f64(a.score, b.score));
    apply_strength(&mut recs);
    apply_ranks(&mut recs);
    Ok(recs)
}

#[cfg(test)]
pub(crate) mod tests {
    //! Port of tests/test_lineup.py (intent).
    use super::*;
    use chrono::{TimeZone, Utc};
    use fm_core::models::RosterSlot;
    use serde_json::json;

    pub(crate) fn pl(cid: &str, pos: &[&str], status: &str) -> Player {
        serde_json::from_value(json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": "EDM",
                                      "positions": pos, "status": status}))
        .unwrap()
    }

    fn named(name: &str, pos: &[&str], status: &str) -> Player {
        let cid = name.split_whitespace().last().unwrap().to_lowercase();
        let mut p = pl(&cid, pos, status);
        p.name = name.to_string();
        p
    }

    pub(crate) fn pv(p: &Player, val: f64, proj: Option<f64>, games: Option<i64>) -> PlayerValue {
        serde_json::from_value(json!({"player": p, "fpg": val, "fpg_season": val, "fpg_week": val, "vorp": 0.0,
                                      "proj_week": proj, "games_next7": games}))
        .unwrap()
    }

    pub(crate) fn team(slotted: &[(&str, &Player)]) -> FantasyTeam {
        FantasyTeam {
            team_id: "1".into(),
            name: "me".into(),
            owner_is_me: true,
            slots: slotted
                .iter()
                .map(|(s, p)| RosterSlot { slot: s.to_string(), player: Some((*p).clone()), starting: !["BN", "IR"].contains(s) })
                .collect(),
            record: None,
        }
    }

    pub(crate) fn shape(xs: &[(&str, i64)]) -> Map<String, i64> {
        xs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    pub(crate) fn ctx_for(t: &FantasyTeam, shape: &Map<String, i64>, provider: &str, as_of: NaiveDate) -> LeagueContext {
        serde_json::from_value(json!({"provider": provider, "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {"G": 1.0}}, "roster_shape": shape, "teams": [t],
            "free_agents": [], "matchup_period": 1, "as_of": as_of}))
        .unwrap()
    }

    pub(crate) fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }

    pub(crate) fn now_at(h: u32, mi: u32, day: u32) -> PyDateTime {
        PyDateTime::from_utc(Utc.with_ymd_and_hms(2026, 10, day, h, mi, 0).unwrap())
    }

    pub(crate) fn vals(xs: Vec<PlayerValue>) -> Map<Cid, PlayerValue> {
        xs.into_iter().map(|v| (v.player.cid.clone(), v)).collect()
    }

    fn recs(ctx: &LeagueContext, v: &Map<Cid, PlayerValue>) -> Vec<Recommendation> {
        recommend_lineup(ctx, v, Horizon::Week, &now_at(12, 0, 1), &ScheduleMemo::new(None)).unwrap()
    }

    pub(crate) fn titles(r: &[Recommendation]) -> Vec<String> {
        r.iter().map(|r| r.title.clone()).collect()
    }

    #[test]
    fn starting_slots_expand_and_skip_bench() {
        let s = starting_slots(&shape(&[("G", 2), ("C", 1), ("BN", 4), ("IR", 1), ("UTIL", 1), ("F", 1)]));
        assert_eq!(s, vec!["C", "F", "UTIL", "G", "G"]);
    }

    #[test]
    fn known_optimum_beats_greedy() {
        let (a, b, c) = (pl("A", &["C", "LW"], "healthy"), pl("B", &["C"], "healthy"), pl("C", &["LW"], "healthy"));
        let t = team(&[("C", &a), ("LW", &c), ("BN", &b)]);
        let v = vals(vec![pv(&a, 5.0, None, None), pv(&b, 4.0, None, None), pv(&c, 1.0, None, None)]);
        let (assign, total) = optimal_lineup(&t, &v, &shape(&[("C", 1), ("LW", 1), ("BN", 1)]), Horizon::Season);
        assert!((total - 9.0).abs() < 1e-9);
        assert_eq!(assign, [(0, "B".to_string()), (1, "A".to_string())].into_iter().collect::<Map<_, _>>());
    }

    #[test]
    fn eligibility_rules() {
        let (g1, g2) = (pl("g1", &["G"], "healthy"), pl("g2", &["G"], "healthy"));
        let (c, dd, lw) = (pl("c", &["C", "F"], "healthy"), pl("d", &["D"], "healthy"), pl("lw", &["LW"], "healthy"));
        let ir = pl("irstar", &["C"], "healthy");
        let t = team(&[("G", &g2), ("C", &c), ("F", &lw), ("UTIL", &dd), ("BN", &g1), ("IR", &ir)]);
        let v = vals(vec![
            pv(&g1, 9.0, None, None),
            pv(&g2, 1.0, None, None),
            pv(&c, 3.0, None, None),
            pv(&dd, 2.0, None, None),
            pv(&lw, 4.0, None, None),
            pv(&ir, 50.0, None, None),
        ]);
        let sh = shape(&[("C", 1), ("F", 1), ("UTIL", 1), ("G", 1), ("BN", 1), ("IR", 1)]);
        let (assign, total) = optimal_lineup(&t, &v, &sh, Horizon::Season);
        let slots = starting_slots(&sh);
        let got: HashMap<&str, &str> = assign.iter().map(|(i, c)| (slots[*i].as_str(), c.as_str())).collect();
        let want: HashMap<&str, &str> = [("C", "c"), ("F", "lw"), ("UTIL", "d"), ("G", "g1")].into_iter().collect();
        assert_eq!(got, want);
        assert!((total - 18.0).abs() < 1e-9);
    }

    fn brute(players: &[&Player], v: &Map<Cid, PlayerValue>, slots: &[String], k: usize, used: &mut Vec<String>) -> f64 {
        if k == slots.len() {
            return 0.0;
        }
        let mut best = brute(players, v, slots, k + 1, used);
        for p in players {
            if !used.contains(&p.cid) && !eligible_slots(&p.positions, &[slots[k].as_str()]).is_empty() {
                used.push(p.cid.clone());
                best = best.max(v[&p.cid].fpg_season + brute(players, v, slots, k + 1, used));
                used.pop();
            }
        }
        best
    }

    #[test]
    fn matches_brute_force() {
        let pos_pool: [&[&str]; 7] = [&["C"], &["LW"], &["RW"], &["C", "LW"], &["LW", "RW"], &["D"], &["G"]];
        let mut rng = fm_pycompat::random::PyRandom::new(7);
        for _ in 0..12 {
            let players: Vec<Player> =
                (0..8).map(|i| pl(&format!("p{i}"), pos_pool[rng.randrange_to(7) as usize], "healthy")).collect();
            let v = vals(players.iter().map(|p| pv(p, round_to(rng.uniform(0.0, 5.0), 2), None, None)).collect());
            let sh = shape(&[("C", 1), ("LW", 1), ("F", 1), ("D", 1), ("UTIL", 1), ("G", 1)]);
            let refs: Vec<&Player> = players.iter().collect();
            let t = team(&refs.iter().map(|p| ("BN", *p)).collect::<Vec<_>>());
            let (_, total) = optimal_lineup(&t, &v, &sh, Horizon::Season);
            let want = brute(&refs, &v, &starting_slots(&sh), 0, &mut vec![]);
            assert!((total - want).abs() < 1e-9, "{total} vs {want}");
        }
    }

    #[test]
    fn week_horizon_prefers_projected_points_and_ties_keep_starter() {
        let (a, b) = (pl("a", &["C"], "healthy"), pl("b", &["C"], "healthy"));
        let t = team(&[("C", &a), ("BN", &b)]);
        let v = vals(vec![pv(&a, 3.0, Some(6.0), Some(2)), pv(&b, 2.0, Some(8.0), Some(4))]);
        let sh = shape(&[("C", 1), ("BN", 1)]);
        assert_eq!(optimal_lineup(&t, &v, &sh, Horizon::Week).0[&0], "b");
        assert_eq!(optimal_lineup(&t, &v, &sh, Horizon::Season).0[&0], "a");
        let t2 = team(&[("BN", &a), ("C", &b)]);
        let v2 = vals(vec![pv(&a, 2.0, None, None), pv(&b, 2.0, None, None)]);
        assert_eq!(optimal_lineup(&t2, &v2, &sh, Horizon::Season).0[&0], "b");
    }

    #[test]
    fn recommend_lineup_swaps_and_thresholds() {
        let (s1, s2, b1, b2) =
            (pl("s1", &["C"], "healthy"), pl("s2", &["D"], "healthy"), pl("b1", &["C"], "healthy"), pl("b2", &["D"], "healthy"));
        let t = team(&[("C", &s1), ("D", &s2), ("BN", &b1), ("BN", &b2)]);
        let sh = shape(&[("C", 1), ("D", 1), ("BN", 2)]);
        let v = vals(vec![
            pv(&s1, 1.0, Some(4.0), Some(2)),
            pv(&b1, 1.0, Some(8.0), Some(4)),
            pv(&s2, 1.0, Some(10.0), Some(3)),
            pv(&b2, 1.0, Some(10.4), Some(4)),
        ]);
        let r = recs(&ctx_for(&t, &sh, "t", d(10, 7)), &v);
        assert_eq!(titles(&r), vec!["Start b1 over s1"]);
        let codes: Vec<&str> = r[0].reasons.iter().map(|x| x.code.as_str()).collect();
        assert_eq!(codes[0], "LINEUP_GAIN");
        assert!(codes.contains(&"GAMES_NEXT7") && codes.contains(&"PROJ_WEEK"));
        assert!((r[0].score - 4.0).abs() < 1e-9);
        assert_eq!((r[0].gain_units, r[0].horizon_days), (Some(GainUnits::WeekPts), Some(7)));
    }

    #[test]
    fn injured_starter_and_healthy_ir() {
        let (hurt, bench, ir_ok) = (pl("hurt", &["C"], "suspended"), pl("bench", &["LW"], "healthy"), pl("irok", &["C"], "healthy"));
        let t = team(&[("C", &hurt), ("BN", &bench), ("IR", &ir_ok)]);
        let v = vals(vec![pv(&hurt, 0.0, None, None), pv(&bench, 2.0, None, None), pv(&ir_ok, 3.0, None, None)]);
        let r = recs(&ctx_for(&t, &shape(&[("C", 1), ("BN", 1), ("IR", 1)]), "t", d(10, 7)), &v);
        let ts = titles(&r);
        assert!(ts.iter().any(|t| t.starts_with("Bench hurt")));
        let act = r.iter().find(|r| r.title == "Activate irok from IR (healthy)").unwrap();
        assert!(act.reasons.iter().any(|x| x.code == "ROSTER_FULL"));
        let repl = pl("repl", &["C"], "healthy");
        let t2 = team(&[("C", &hurt), ("BN", &repl)]);
        let v2 = vals(vec![pv(&hurt, 0.0, None, None), pv(&repl, 2.0, None, None)]);
        let r2 = recs(&ctx_for(&t2, &shape(&[("C", 1), ("BN", 1)]), "t", d(10, 7)), &v2);
        assert_eq!(titles(&r2), vec!["Start repl over hurt (suspended)"]);
    }

    #[test]
    fn dtd_in_ir_and_injured_starter_become_one_swap() {
        let (hurt, bench, dtd) = (pl("hurt", &["C"], "ir"), pl("bench", &["LW"], "healthy"), pl("dtd", &["C"], "dtd"));
        let t = team(&[("C", &hurt), ("BN", &bench), ("IR", &dtd)]);
        let v = vals(vec![pv(&hurt, 0.0, None, None), pv(&bench, 2.0, None, None), pv(&dtd, 3.0, None, None)]);
        let r = recs(&ctx_for(&t, &shape(&[("C", 1), ("BN", 1), ("IR", 1)]), "t", d(10, 7)), &v);
        assert_eq!(titles(&r), vec!["Swap IR slots: activate dtd (dtd), move hurt (ir) to IR"]);
        let r2 = recs(&ctx_for(&t, &shape(&[("C", 1), ("BN", 1), ("IR", 2)]), "t", d(10, 7)), &v);
        assert!(titles(&r2).contains(&"Activate dtd from IR (dtd)".to_string()));
        assert!(!titles(&r2).iter().any(|t| t.starts_with("Swap")));
    }

    fn espn_like() -> (FantasyTeam, Map<Cid, PlayerValue>, Map<String, i64>) {
        let hel = named("Connor Hellebuyck", &["G"], "suspended");
        let mar = named("Brad Marchand", &["LW", "RW", "F"], "ir");
        let pin = named("Shane Pinto", &["C", "RW", "F"], "healthy");
        let dra = named("Leon Draisaitl", &["C", "F"], "healthy");
        let bus = named("Brandon Bussi", &["G"], "healthy");
        let fro = named("Morgan Frost", &["C", "F"], "healthy");
        let jen = named("Boone Jenner", &["C", "LW", "F"], "healthy");
        let ann = named("Justus Annunen", &["G"], "healthy");
        let t = team(&[("F", &dra), ("F", &mar), ("UTIL", &pin), ("G", &bus), ("G", &hel), ("BN", &fro), ("BN", &jen), ("BN", &ann)]);
        let v = vals(vec![
            pv(&hel, 0.0, Some(0.0), None),
            pv(&mar, 0.0, Some(0.0), None),
            pv(&pin, 1.0, Some(1.65), None),
            pv(&dra, 3.0, Some(7.86), None),
            pv(&bus, 1.0, Some(3.46), None),
            pv(&fro, 1.0, Some(4.33), None),
            pv(&jen, 1.0, Some(3.65), None),
            pv(&ann, 1.0, Some(1.10), None),
        ]);
        (t, v, shape(&[("F", 2), ("UTIL", 1), ("G", 2), ("BN", 3)]))
    }

    #[test]
    fn suspended_goalie_is_replaced_by_a_goalie_and_gains_add_up() {
        let (t, v, sh) = espn_like();
        let r = recs(&ctx_for(&t, &sh, "t", d(10, 7)), &v);
        assert_eq!(
            titles(&r),
            vec![
                "Start Morgan Frost over Brad Marchand (ir)",
                "Start Boone Jenner over Shane Pinto",
                "Start Justus Annunen over Connor Hellebuyck (suspended)"
            ]
        );
        assert!((r[2].predicted_gain.unwrap() - 1.10).abs() < 1e-9);
        let (_, opt) = optimal_lineup(&t, &v, &sh, Horizon::Week);
        let sum: f64 = r.iter().filter_map(|r| r.predicted_gain).sum();
        assert!((sum - (opt - current_total(&t, &v, Horizon::Week))).abs() < 1e-6);
    }

    #[test]
    fn chain_move_and_empty_slot_titles() {
        let z = named("Zed Out", &["C"], "out");
        let y = named("Yan Mover", &["C"], "healthy");
        let x = named("Xav Bench", &["LW"], "healthy");
        let t = team(&[("C", &z), ("UTIL", &y), ("BN", &x)]);
        let v = vals(vec![pv(&z, 0.0, Some(0.0), None), pv(&y, 1.0, Some(5.0), None), pv(&x, 1.0, Some(3.0), None)]);
        let sh = shape(&[("C", 1), ("UTIL", 1), ("BN", 1)]);
        let r: Vec<Recommendation> =
            recs(&ctx_for(&t, &sh, "t", d(10, 7)), &v).into_iter().filter(|r| r.reason("LINEUP_GAIN").is_some()).collect();
        assert_eq!(titles(&r), vec!["Start Xav Bench at UTIL for Yan Mover; move Yan Mover to C over Zed Out (out)"]);
        assert_eq!(r[0].subjects.iter().map(|p| p.cid.as_str()).collect::<Vec<_>>(), vec!["mover"]);
        // weekly lock wording (Fantrax)
        let rf = recs(&ctx_for(&t, &sh, "fantrax", d(10, 7)), &v);
        assert!(rf[0].title.ends_with(WEEKLY_LOCK_TEXT) && rf[0].reason("LINEUP_LOCK").is_some());
        // empty slot
        let (g1, g2) = (named("Goalie One", &["G"], "healthy"), named("Goalie Two", &["G"], "healthy"));
        let t2 = team(&[("G", &g1), ("BN", &g2)]);
        let v2 = vals(vec![pv(&g1, 1.0, Some(3.0), None), pv(&g2, 1.0, Some(2.5), None)]);
        let r2 = recs(&ctx_for(&t2, &shape(&[("G", 2), ("BN", 1)]), "t", d(10, 7)), &v2);
        assert_eq!(titles(&r2), vec!["Start Goalie Two at G (empty slot)"]);
    }

    pub(crate) fn daily(provider: &str) -> (LeagueContext, FantasyTeam, Map<Cid, PlayerValue>) {
        let a = pl("a", &["C"], "healthy");
        let mut b = pl("b", &["C"], "healthy");
        b.team = Some("CGY".into());
        let t = team(&[("C", &a), ("BN", &b)]);
        let mut ctx = ctx_for(&t, &shape(&[("C", 1), ("BN", 1)]), provider, d(10, 6));
        ctx.period_end = Some(d(10, 11));
        ctx.schedule.insert("EDM".into(), vec![d(10, 6), d(10, 8)]);
        ctx.schedule.insert("CGY".into(), vec![d(10, 7), d(10, 8), d(10, 12)]);
        let v = vals(vec![pv(&a, 2.0, Some(4.0), Some(2)), pv(&b, 3.0, Some(9.0), Some(3))]);
        (ctx, t, v)
    }

    fn plan(ctx: &LeagueContext, t: &FantasyTeam, v: &Map<Cid, PlayerValue>, now: &PyDateTime) -> Option<DailyPlan> {
        daily_plan(ctx, t, v, None, now, &ScheduleMemo::new(None))
    }

    #[test]
    fn daily_plan_counts_both_players_on_different_nights() {
        let (ctx, t, v) = daily("espn");
        let p = plan(&ctx, &t, &v, &now_at(12, 0, 1)).unwrap();
        assert_eq!(p.days.iter().map(|x| x.day).collect::<Vec<_>>(), (6..12).map(|k| d(10, k)).collect::<Vec<_>>());
        let firsts: Vec<Vec<&str>> = p.days[..3].iter().map(|x| x.assign.values().map(|c| c.as_str()).collect()).collect();
        assert_eq!(firsts, vec![vec!["a"], vec!["b"], vec!["b"]]);
        assert!((p.optimal() - 8.0).abs() < 1e-9 && (p.current() - 4.0).abs() < 1e-9);
        assert_eq!((p.starts("a"), p.starts("b")), (1, 2));
        // weekly lock: no daily plan
        let (cf, tf, vf) = daily("fantrax");
        assert!(plan(&cf, &tf, &vf, &now_at(12, 0, 1)).is_none());
    }

    #[test]
    fn daily_recs_are_for_today_only() {
        let (ctx, _, v) = daily("espn");
        assert!(recs(&ctx, &v).is_empty());
        let mut c2 = ctx.clone();
        c2.as_of = d(10, 7);
        let r = &recs(&c2, &v)[0];
        assert_eq!(r.title, "Start b in a's slot today (a has no game)");
        assert!((r.predicted_gain.unwrap() - 3.0).abs() < 1e-9);
        assert_eq!(r.headline.as_deref(), Some("a has no game today; b has a game"));
        assert_eq!((r.gain_units, r.horizon_days), (Some(GainUnits::DayPts), Some(1)));
        let mut c3 = ctx.clone();
        c3.as_of = d(10, 8);
        c3.opponents.insert("CGY".into(), [(d(10, 8), "@VAN".to_string())].into_iter().collect());
        let r = &recs(&c3, &v)[0];
        assert_eq!(r.title, "Start b over a today");
        assert_eq!(r.headline.as_deref(), Some("b projects 3.0 pts today vs 2.0 for a"));
    }

    #[test]
    fn injured_player_returns_mid_matchup() {
        let (ctx, mut t, mut v) = daily("espn");
        t.slots[1].player.as_mut().unwrap().status = Status::Out;
        let b = v.get_mut("b").unwrap();
        b.player.status = Status::Out;
        b.proj_week = Some(3.0);
        b.return_date = Some(d(10, 8));
        b.avail_week = 1.0 / 3.0;
        let p = plan(&ctx, &t, &v, &now_at(12, 0, 1)).unwrap();
        assert!(!p.days[1].playing.contains("b"));
        assert!((p.days[2].values["b"] - 3.0).abs() < 1e-9);
        assert_eq!(p.days[2].assign.values().collect::<Vec<_>>(), vec!["b"]);
    }

    #[test]
    fn started_games_lock_players_and_are_worth_nothing_more() {
        let (mut ctx, t, v) = daily("espn");
        let tue = d(10, 6);
        ctx.schedule.get_mut("CGY").unwrap().push(tue);
        let gs = |h: u32, day: u32| PyDateTime::from_utc(Utc.with_ymd_and_hms(2026, 10, day, h, 0, 0).unwrap());
        ctx.game_starts.insert("EDM".into(), [(tue, gs(23, 6))].into_iter().collect());
        ctx.game_starts.insert("CGY".into(), [(tue, gs(2, 7))].into_iter().collect());
        let before = plan(&ctx, &t, &v, &now_at(22, 0, 6)).unwrap();
        assert!(before.days[0].locked.is_empty());
        assert!((before.days[0].values["a"] - 2.0).abs() < 1e-9);
        let live = plan(&ctx, &t, &v, &now_at(23, 30, 6)).unwrap();
        let d0 = &live.days[0];
        assert_eq!(d0.locked.iter().collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(d0.assign.values().collect::<Vec<_>>(), vec!["a"]);
        assert_eq!((d0.values["a"], d0.optimal, d0.current), (0.0, 0.0, 0.0));
        assert!(d0.playing.contains("a"));
        assert_eq!((live.starts("a"), live.remaining_starts("a")), (1, 0));
        assert!((live.optimal() - (before.optimal() - 3.0)).abs() < 1e-9);
        let late = plan(&ctx, &t, &v, &now_at(2, 30, 7)).unwrap();
        assert_eq!(late.days[0].assign.values().collect::<Vec<_>>(), vec!["a"]);
        // the locked starter never gets a swap rec today
        let memo = ScheduleMemo::new(None);
        let r = recommend_lineup(&ctx, &v, Horizon::Week, &now_at(22, 0, 6), &memo).unwrap();
        assert_eq!(r[0].title, "Start b over a today");
        assert!(recommend_lineup(&ctx, &v, Horizon::Week, &now_at(23, 30, 6), &memo).unwrap().is_empty());
    }

    #[test]
    fn ir_return_reminder_and_daily_ir_swap_gain() {
        let (mut ctx, mut t, mut v) = daily("espn");
        let hurt = pl("h", &["C"], "out");
        t.slots.push(RosterSlot { slot: "IR".into(), player: Some(hurt.clone()), starting: false });
        ctx.teams = vec![t.clone()];
        ctx.schedule.get_mut("EDM").unwrap().push(d(10, 10));
        let mut hv = pv(&hurt, 2.0, Some(2.0), Some(1));
        hv.return_date = Some(d(10, 8));
        hv.avail_week = 2.0 / 3.0;
        v.insert("h".into(), hv);
        let r = recs(&ctx, &v);
        let r = r.iter().find(|r| r.subjects.first().is_some_and(|p| p.cid == "h")).unwrap();
        assert_eq!(r.title, "h due back Thu Oct 8: activate him from IR then");
        assert_eq!(r.reason("GAMES_LEFT").unwrap().value, Some(2.0));

        let (mut ctx, mut t, mut v) = daily("espn");
        t.slots[0].player.as_mut().unwrap().status = Status::Ir;
        let dd = pl("d", &["C"], "healthy");
        t.slots.push(RosterSlot { slot: "IR".into(), player: Some(dd.clone()), starting: false });
        ctx.teams = vec![t];
        ctx.roster_shape = shape(&[("C", 1), ("BN", 1), ("IR", 1)]);
        v.insert("d".into(), pv(&dd, 4.0, Some(8.0), Some(2)));
        let rs = recs(&ctx, &v);
        let r = rs.iter().find(|r| r.title.starts_with("Swap IR slots")).unwrap();
        assert_eq!((r.gain_units, r.horizon_days), (Some(GainUnits::MatchupPts), Some(6)));
        assert!((r.predicted_gain.unwrap() - 5.0).abs() < 1e-9);
    }

    fn goalie_ctx(share: f64, b2b: Option<(f64, i64)>) -> (LeagueContext, Player, Map<Cid, PlayerValue>) {
        let g = pl("g", &["G"], "healthy");
        let t = team(&[("G", &g)]);
        let mut ctx = ctx_for(&t, &shape(&[("G", 1)]), "espn", d(10, 5));
        ctx.period_end = Some(d(10, 11));
        ctx.schedule.insert("EDM".into(), vec![d(10, 6), d(10, 7), d(10, 10)]);
        if let Some(b) = b2b {
            ctx.b2b_second_night.insert("EDM".into(), b);
        }
        let mut gv = pv(&g, 3.0, Some(9.0 * share), Some(3));
        gv.fpg_season = 3.0 * share;
        gv.fpg_week = 3.0 * share;
        gv.offnight_next7 = Some(0);
        gv.start_share = Some(share);
        (ctx, g, vals(vec![gv]))
    }

    #[test]
    fn goalie_day_value_uses_per_night_start_probability() {
        let memo = ScheduleMemo::new(None);
        let (ctx, g, v) = goalie_ctx(0.6, None);
        let (p1, p2) = night_start_probs(0.6, B2B_SECOND_NIGHT_P, SEASON_B2B_FRAC);
        assert!((p2 - 0.35).abs() < 1e-12 && p1 > 0.6);
        let (tue, wed, sat) = (d(10, 6), d(10, 7), d(10, 10));
        let pvg = v.get("g");
        assert!((night_start_probability(&ctx, &g, pvg, tue).unwrap() - p1).abs() < 1e-12);
        assert!((night_start_probability(&ctx, &g, pvg, wed).unwrap() - p2).abs() < 1e-12);
        assert!((day_value(&ctx, &memo, &g, pvg, wed) - 3.0 * p2).abs() < 1e-9);
        assert!((day_value(&ctx, &memo, &g, pvg, tue) - 3.0 * p1).abs() < 1e-9);
        assert!((day_value(&ctx, &memo, &g, pvg, sat) - 3.0 * p1).abs() < 1e-9);
        let (cb, gb, vb) = goalie_ctx(0.4, None);
        assert!(day_value(&cb, &memo, &gb, vb.get("g"), wed) > 1.2 && 1.2 > day_value(&cb, &memo, &gb, vb.get("g"), tue));
        // the ledger's rate replaces 0.35; valuate's B2B-weighted week share is divided out
        let (cl, gl, vl) = goalie_ctx(0.6, Some((0.2, 9)));
        let w = week_start_share(0.6, &cl.schedule["EDM"], cl.as_of, 7, 0.2, SEASON_B2B_FRAC);
        let mut pvl = vl["g"].clone();
        pvl.proj_week = Some(9.0 * w);
        assert!((night_start_probability(&cl, &gl, Some(&pvl), wed).unwrap() - 0.2).abs() < 1e-12);
        assert!((day_value(&cl, &memo, &gl, Some(&pvl), wed) - 0.6).abs() < 1e-9);
    }

    #[test]
    fn goalie_second_night_is_conditioned_on_tonights_confirmed_starter() {
        let memo = ScheduleMemo::new(None);
        let (mut ctx, mut g, v) = goalie_ctx(0.6, None);
        ctx.as_of = d(10, 6);
        g.confirmed_start = Some(false);
        g.start_source = Some("Confirmed".into());
        assert_eq!(day_value(&ctx, &memo, &g, v.get("g"), d(10, 6)), 0.0);
        let rested = crate::analysis::team_factors::RESTED_STARTER_P;
        assert!((day_value(&ctx, &memo, &g, v.get("g"), d(10, 7)) - 3.0 * rested).abs() < 1e-9);
        g.confirmed_start = Some(true);
        assert!((day_value(&ctx, &memo, &g, v.get("g"), d(10, 6)) - 3.0).abs() < 1e-9);
        assert!((day_value(&ctx, &memo, &g, v.get("g"), d(10, 7)) - 3.0 * 0.35).abs() < 1e-9);
    }
}
