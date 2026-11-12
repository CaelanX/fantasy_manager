//! Rust port of `fantasy_manager/recommend/plan.py`: the weekly action plan (`fm plan`, the
//! dashboard's /plan page) for daily-lineup leagues - today's items, add scheduling by beam
//! search within the moves left, one line per remaining day, win probabilities, the "Net this
//! week" lines and the plan archive. Owner: work package `W8-plan-claims-advise`.
//!
//! Deviations from Python (behaviour-preserving):
//! * The clock is a parameter (`now`); the schedule memo is an explicit [`ScheduleMemo`]; the
//!   local zone for game times is `FM_TZ` / `TZ` (chrono-tz) else the OS zone (`chrono::Local`).
//! * `_Timeline` keeps players in a registry (indices instead of `Player` objects); its day
//!   memo is keyed by the day and the roster in order (Python keys by the frozenset and returns
//!   the first order it saw: results only differ on exact lineup ties). Day values are computed
//!   once per player per day.
//! * Python's frozensets of chosen cids (day lines, the win-probability input order) iterate in
//!   hash-random order; here they iterate sorted by cid (ARCHITECTURE.md 4.3).
//! * The beam search evaluates each state's candidate moves in parallel (rayon) and collects them
//!   in Python's order before the same stable sorts, so results do not depend on threading.
//! * `matchup` / `goalies` / `recs` are explicit inputs ([`PlanOpts`]); when absent they are
//!   computed as in Python. `archive_plan` takes the clock.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use dashmap::DashMap;
use fm_core::models::{FantasyTeam, LeagueContext, Player, RecKind, Recommendation, RosterSlot};
use fm_core::normalize::normalize_team;
use fm_core::provider::LeagueProvider;
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::{Cid, CoreError, Map, PyDateTime};
use fm_pycompat::fmt::{fixed, percent};
use fm_pycompat::num::{py_sum, round_to};
use fm_pycompat::order::{cmp_f64, sort_desc_by};
use fm_pycompat::time::strftime_date;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::analysis::goalie_plan::{goalie_week, GoalieRow, GoalieWeek, GoalieWeekOpts};
use crate::analysis::matchup::{current_matchup, win_probability, Matchup, MatchupOpts};
use crate::recommend::advise::{advise, AdviseOpts};
use crate::recommend::base::{
    dynasty_delta, dynasty_floats, dynasty_objects, keeper_k, league_unit, moves_left, shares_default_slot, swap_values,
};
use crate::recommend::headline::market_suffix;
use crate::recommend::injuries::History;
use crate::recommend::lineup::{day_value, dp_idx, plan_days, started, starting_slots, unavailable};
use crate::recommend::lineup_lock;
use crate::recommend::waivers::{recommend_waivers, stream_dynasty_tolerance, WaiverOpts};
use crate::recommend::{claims, plan_memory};
use crate::valuation::schedule::ScheduleMemo;

/// matchup points a scheduled move must add, by stance, in ESPN points (the plan multiplies them by
/// [`league_unit`]; Python used them unscaled in every league)
pub fn min_move_pts(stance: &str) -> f64 {
    match stance {
        "chase" => 0.5,
        "even" => 1.0,
        "protect" => 2.0,
        _ => 1.0,
    }
}
/// moves the plan schedules when the league has no limit
pub const UNLIMITED_MOVES_CAP: i64 = 4;
/// this-matchup pickups considered (waivers.recommend_waivers limit)
pub const CANDIDATE_LIMIT: usize = 40;
pub const BEAM: usize = 3;
pub const EXPAND: usize = 6;
/// fixed seed: the three win probabilities use the same draws
pub const WIN_SEED: i64 = 7;
pub const LOCK_BONUS: f64 = 1e6;
/// a goalie start probability below this is worth a 5 pm check
pub const PREGAME_CONFIRMED: f64 = 0.9;
/// day points the best lineup must beat quick lineup by to call out a start, in ESPN points (times
/// [`league_unit`]; Python used the bare 0.5 in every league)
pub const QUICK_EDGE_MIN: f64 = 0.5;
pub const CHECKLIST_LIMIT: usize = 3;

// ------------------------------------------------------------------------------------ models

/// One scheduled acquisition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanMove {
    /// when to make it
    pub day: NaiveDate,
    /// first day it counts
    pub effective: NaiveDate,
    pub add: Player,
    #[serde(default)]
    pub drop: Option<Player>,
    #[serde(default)]
    pub to_ir: Option<Player>,
    /// matchup points it adds (with the moves before it applied)
    #[serde(default)]
    pub gain_pts: f64,
    #[serde(default)]
    pub win_delta: Option<f64>,
    /// days the add is in the best lineup
    #[serde(default)]
    pub starts: Vec<NaiveDate>,
    #[serde(default)]
    pub headline: String,
    /// "market agrees" / "model disagrees with market" / ...
    #[serde(default)]
    pub market: Option<String>,
    #[serde(default)]
    pub claim: bool,
    #[serde(default)]
    pub reasons: Vec<String>,
    /// moves sharing a rented player (rental then swap) share a number
    #[serde(default)]
    pub chain: Option<i64>,
    /// "rental" (a later step drops him) / "swap" (drops an earlier pickup)
    #[serde(default)]
    pub role: Option<String>,
    /// the chain story
    #[serde(default)]
    pub note: String,
    /// when to make it
    #[serde(default)]
    pub timing: String,
    /// vs the last plan shown: "new" / "kept" / "" unchanged
    #[serde(default)]
    pub status: String,
}

/// What changed since the last plan shown (`plan_memory::compare`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanSince {
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub done: Vec<String>,
    #[serde(default)]
    pub changed: Vec<String>,
    #[serde(default)]
    pub new: Vec<String>,
    #[serde(default)]
    pub kept: Vec<String>,
    #[serde(default)]
    pub roster: Vec<String>,
    #[serde(default)]
    pub long_term: Vec<String>,
}

impl PlanSince {
    pub fn empty(&self) -> bool {
        self.done.is_empty()
            && self.changed.is_empty()
            && self.new.is_empty()
            && self.kept.is_empty()
            && self.roster.is_empty()
            && self.long_term.is_empty()
    }
}

/// A today item taken from an existing recommendation (lineup / injury).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanAction {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub headline: Option<String>,
    #[serde(default)]
    pub gain: Option<f64>,
    #[serde(default)]
    pub gain_units: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanDayLine {
    pub day: NaiveDate,
    #[serde(default)]
    pub moves: Vec<PlanMove>,
    /// starts quick lineup would miss
    #[serde(default)]
    pub ins: Vec<String>,
    /// players quick lineup would start that should sit
    #[serde(default)]
    pub outs: Vec<String>,
    /// "Saros 62% vs BOS"
    #[serde(default)]
    pub goalies: Vec<String>,
    #[serde(default)]
    pub pregame: bool,
    /// quick lineup sets this day's best lineup
    #[serde(default = "yes")]
    pub quick_ok: bool,
    #[serde(default)]
    pub pts_plan: f64,
    /// quick lineup with today's roster (no moves)
    #[serde(default)]
    pub pts_as_set: f64,
    /// lineup left exactly as set
    #[serde(default)]
    pub pts_frozen: f64,
    /// why the plan is below quick lineup that day
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub text: String,
}

fn yes() -> bool {
    true
}

fn quick_lineup() -> String {
    "quick_lineup".to_string()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeekPlan {
    pub league: String,
    pub my_team: String,
    #[serde(default)]
    pub opponent: Option<String>,
    #[serde(default)]
    pub start: Option<NaiveDate>,
    #[serde(default)]
    pub end: Option<NaiveDate>,
    #[serde(default)]
    pub days_left: i64,
    #[serde(default)]
    pub my_so_far: f64,
    #[serde(default)]
    pub their_so_far: f64,
    #[serde(default)]
    pub their_remaining: f64,
    #[serde(default)]
    pub stance: Option<String>,
    #[serde(default)]
    pub moves_left: Option<i64>,
    #[serde(default)]
    pub moves_planned: i64,
    /// what "as set" means: quick lineup pressed each day
    #[serde(default = "quick_lineup")]
    pub baseline: String,
    #[serde(default)]
    pub proj_as_set: f64,
    #[serde(default)]
    pub proj_lineup: f64,
    #[serde(default)]
    pub proj_plan: f64,
    #[serde(default)]
    pub proj_frozen: f64,
    #[serde(default)]
    pub win_as_set: Option<f64>,
    #[serde(default)]
    pub win_lineup: Option<f64>,
    #[serde(default)]
    pub win_plan: Option<f64>,
    #[serde(default)]
    pub win_frozen: Option<f64>,
    #[serde(default)]
    pub today: Vec<PlanAction>,
    #[serde(default)]
    pub moves: Vec<PlanMove>,
    #[serde(default)]
    pub days: Vec<PlanDayLine>,
    #[serde(default)]
    pub watch: Vec<String>,
    #[serde(default)]
    pub claim_watch: Vec<String>,
    #[serde(default)]
    pub net_week: Vec<String>,
    #[serde(default)]
    pub since: Option<PlanSince>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default = "yes")]
    pub available: bool,
}

impl WeekPlan {
    pub fn new(league: &str, my_team: &str) -> Self {
        WeekPlan {
            league: league.to_string(),
            my_team: my_team.to_string(),
            opponent: None,
            start: None,
            end: None,
            days_left: 0,
            my_so_far: 0.0,
            their_so_far: 0.0,
            their_remaining: 0.0,
            stance: None,
            moves_left: None,
            moves_planned: 0,
            baseline: quick_lineup(),
            proj_as_set: 0.0,
            proj_lineup: 0.0,
            proj_plan: 0.0,
            proj_frozen: 0.0,
            win_as_set: None,
            win_lineup: None,
            win_plan: None,
            win_frozen: None,
            today: vec![],
            moves: vec![],
            days: vec![],
            watch: vec![],
            claim_watch: vec![],
            net_week: vec![],
            since: None,
            notes: vec![],
            available: true,
        }
    }

    pub fn lineup_gain(&self) -> f64 {
        self.proj_lineup - self.proj_as_set
    }

    pub fn moves_gain(&self) -> f64 {
        self.proj_plan - self.proj_lineup
    }

    /// `model_dump(mode="json")` + the rounded gains and projected totals.
    pub fn to_json(&self) -> Value {
        let mut d = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Value::Object(o) = &mut d {
            o.insert("lineup_gain".into(), json!(round_to(self.lineup_gain(), 2)));
            o.insert("moves_gain".into(), json!(round_to(self.moves_gain(), 2)));
            o.insert("my_projected_total".into(), json!(round_to(self.my_so_far + self.proj_plan, 2)));
            o.insert("their_projected_total".into(), json!(round_to(self.their_so_far + self.their_remaining, 2)));
        }
        d
    }
}

// ------------------------------------------------------------------------ timeline evaluation

/// `_Tx`: one scheduled move; players are indices into the [`Timeline`] registry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tx<'r> {
    pub day: NaiveDate,
    pub effective: NaiveDate,
    pub add: usize,
    pub drop: Option<usize>,
    pub to_ir: Option<usize>,
    pub rec: Option<&'r Recommendation>,
}

/// A candidate (add, drop, to_ir, rec) from the this-matchup pickups.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pair<'r> {
    pub add: usize,
    pub drop: Option<usize>,
    pub to_ir: Option<usize>,
    pub rec: &'r Recommendation,
}

type Chosen = Arc<[u32]>;

/// `_Timeline`: day totals of my best daily lineup for a roster that changes with scheduled moves.
pub(crate) struct Timeline<'a> {
    pub ctx: &'a LeagueContext,
    pub values: &'a Map<Cid, PlayerValue>,
    memo: &'a ScheduleMemo,
    pub days: Vec<NaiveDate>,
    slots: Vec<String>,
    players: Vec<&'a Player>,
    idx: HashMap<&'a str, usize>,
    vals: Vec<OnceLock<Vec<f64>>>,
    /// players outside "IR" slots, in slot order (registry indices)
    pub base: Vec<usize>,
    starter: Vec<bool>,
    locked: Vec<bool>,
    day_memo: DashMap<(usize, Box<[u32]>), (f64, Chosen)>,
}

impl<'a> Timeline<'a> {
    pub fn new(
        ctx: &'a LeagueContext,
        team: &'a FantasyTeam,
        values: &'a Map<Cid, PlayerValue>,
        days: Vec<NaiveDate>,
        now: &PyDateTime,
        memo: &'a ScheduleMemo,
    ) -> Self {
        let mut players: Vec<&'a Player> = Vec::new();
        let mut idx: HashMap<&'a str, usize> = HashMap::new();
        let mut reg = |p: &'a Player, players: &mut Vec<&'a Player>| -> usize {
            *idx.entry(p.cid.as_str()).or_insert_with(|| {
                players.push(p);
                players.len() - 1
            })
        };
        let mut base = Vec::new();
        let mut starter_cids: HashSet<&str> = HashSet::new();
        for s in &team.slots {
            let Some(p) = s.player.as_ref() else { continue };
            let i = reg(p, &mut players);
            if s.starting {
                starter_cids.insert(p.cid.as_str());
            }
            if s.slot != "IR" {
                base.push(i);
            }
        }
        for p in ctx.all_players() {
            reg(p, &mut players);
        }
        let n = players.len();
        let starter: Vec<bool> = players.iter().map(|p| starter_cids.contains(p.cid.as_str())).collect();
        let mut locked = vec![false; n];
        for &i in &base {
            if started(ctx, players[i], ctx.as_of, now) {
                locked[i] = true;
            }
        }
        Timeline {
            ctx,
            values,
            memo,
            days,
            slots: starting_slots(&ctx.roster_shape),
            players,
            idx,
            vals: (0..n).map(|_| OnceLock::new()).collect(),
            base,
            starter,
            locked,
            day_memo: DashMap::new(),
        }
    }

    /// Registry index of a cid.
    pub fn id(&self, cid: &str) -> Option<usize> {
        self.idx.get(cid).copied()
    }

    pub fn p(&self, i: usize) -> &'a Player {
        self.players[i]
    }

    /// `value(p, days[k])`.
    pub fn val(&self, i: usize, k: usize) -> f64 {
        self.vals[i].get_or_init(|| {
            let p = self.players[i];
            let pv = self.values.get(&p.cid);
            self.days.iter().map(|&d| day_value(self.ctx, self.memo, p, pv, d)).collect()
        })[k]
    }

    /// `value(p, d)` by cid (0 for an unknown player).
    pub fn value_cid(&self, cid: &str, k: usize) -> f64 {
        self.id(cid).map_or(0.0, |i| self.val(i, k))
    }

    /// Active roster on `d`: a move's add, drop and IR move all count from its effective day.
    pub fn roster(&self, d: NaiveDate, txs: &[Tx<'_>]) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::with_capacity(self.base.len() + txs.len());
        for &b in &self.base {
            if !out.contains(&b) {
                out.push(b);
            }
        }
        for t in txs {
            if t.effective <= d {
                if !out.contains(&t.add) {
                    out.push(t.add);
                }
                for g in [t.drop, t.to_ir].into_iter().flatten() {
                    out.retain(|&x| x != g);
                }
            }
        }
        out
    }

    fn is_today(&self, k: usize) -> bool {
        self.days[k] == self.ctx.as_of
    }

    /// Best lineup on `days[k]` from `players`: (total, chosen registry indices sorted by cid).
    pub fn day(&self, k: usize, players: &[usize]) -> (f64, Chosen) {
        let key = (k, players.iter().map(|&i| i as u32).collect::<Box<[u32]>>());
        if let Some(hit) = self.day_memo.get(&key) {
            return hit.clone();
        }
        let today = self.is_today(k);
        let pool: Vec<usize> = players
            .iter()
            .copied()
            .filter(|&i| {
                (self.val(i, k) > 0.0 || (today && self.locked[i])) && !(today && self.locked[i] && !self.starter[i])
            })
            .collect();
        let weight: Vec<f64> =
            pool.iter().map(|&i| self.val(i, k) + if today && self.locked[i] { LOCK_BONUS } else { 0.0 }).collect();
        let pp: Vec<&Player> = pool.iter().map(|&i| self.players[i]).collect();
        let (_, assign) = dp_idx(&pp, &self.slots, |j, _| weight[j]);
        let mut chosen: Vec<u32> = assign.iter().map(|&(_, j)| pool[j] as u32).collect();
        chosen.sort_by(|a, b| self.players[*a as usize].cid.cmp(&self.players[*b as usize].cid));
        chosen.dedup();
        let total = py_sum(chosen.iter().map(|&c| self.val(c as usize, k)));
        let res: (f64, Chosen) = (total, chosen.into());
        self.day_memo.insert(key, res.clone());
        res
    }

    /// The lineup as set now, left unchanged on `days[k]`.
    pub fn as_set(&self, k: usize) -> f64 {
        let mut seen: Vec<usize> = Vec::new();
        for &b in &self.base {
            if self.starter[b] && !seen.contains(&b) {
                seen.push(b);
            }
        }
        py_sum(seen.iter().map(|&i| self.val(i, k)))
    }

    pub fn total(&self, txs: &[Tx<'_>]) -> f64 {
        py_sum((0..self.days.len()).map(|k| self.day(k, &self.roster(self.days[k], txs)).0))
    }

    pub fn per_day(&self, txs: &[Tx<'_>]) -> Vec<(f64, Chosen)> {
        (0..self.days.len()).map(|k| self.day(k, &self.roster(self.days[k], txs))).collect()
    }

    /// `_best_day`: best day to make a move on top of `txs`: (gain, effective day).
    pub fn best_day(
        &self,
        txs: &[Tx<'_>],
        add: usize,
        drop: Option<usize>,
        to_ir: Option<usize>,
        earliest: NaiveDate,
    ) -> Option<(f64, NaiveDate)> {
        let mut deltas: Vec<Option<f64>> = Vec::with_capacity(self.days.len());
        for (k, &d) in self.days.iter().enumerate() {
            if d < earliest {
                deltas.push(None);
                continue;
            }
            let before = self.roster(d, txs);
            let mut after: Vec<usize> =
                before.iter().copied().filter(|&i| Some(i) != drop && Some(i) != to_ir).collect();
            after.push(add);
            deltas.push(Some(self.day(k, &after).0 - self.day(k, &before).0));
        }
        let mut best: Option<(f64, NaiveDate)> = None;
        let mut run = 0.0;
        for i in (0..self.days.len()).rev() {
            let Some(x) = deltas[i] else { break };
            run += x;
            if best.map_or(true, |b| run > b.0 + 1e-9) {
                best = Some((run, self.days[i]));
            }
        }
        best
    }
}

/// `_upto(txs, i)`: moves 0..=i, plus removals of the players they add that a later move drops.
pub(crate) fn upto<'r>(txs: &[Tx<'r>], i: isize) -> Vec<Tx<'r>> {
    let cut = (i + 1).max(0) as usize;
    let cut = cut.min(txs.len());
    let head = &txs[..cut];
    let added: HashSet<usize> = head.iter().map(|t| t.add).collect();
    let mut out: Vec<Tx<'r>> = head.to_vec();
    for t in &txs[cut..] {
        if let Some(d) = t.drop {
            if added.contains(&d) {
                out.push(Tx { day: t.day, effective: t.effective, add: d, drop: Some(d), to_ir: None, rec: None });
            }
        }
    }
    out
}

// ------------------------------------------------------------------------ candidates / search

/// The day's first NHL game has started by `now` (Fantrax's daily period lock).
pub fn first_game_started(ctx: &LeagueContext, day: NaiveDate, now: &PyDateTime) -> bool {
    match first_game(ctx, day) {
        Some(first) => matches!(now.partial_cmp(&first), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)),
        None => false,
    }
}

/// `_earliest`: first day a pickup made today can count.
pub fn earliest(ctx: &LeagueContext, p: &Player, now: &PyDateTime) -> NaiveDate {
    let today = ctx.as_of;
    if let Some(w) = p.waiver_until.filter(|w| *w >= today) {
        return w + Duration::days(1);
    }
    let late = if ctx.provider == "fantrax" && !ctx.lock_per_game {
        first_game_started(ctx, today, now)
    } else {
        started(ctx, p, today, now)
    };
    if late {
        today + Duration::days(1)
    } else {
        today
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: a fixed UTC offset (seconds) instead of FM_TZ / the OS zone.
    pub(crate) static ZONE_OVERRIDE: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

/// `_local_zone`: FM_TZ / TZ when chrono-tz knows it, else None (the OS zone).
pub fn local_zone() -> Option<chrono_tz::Tz> {
    for var in ["FM_TZ", "TZ"] {
        let name = std::env::var(var).unwrap_or_default();
        let name = name.trim();
        if !name.is_empty() {
            if let Ok(tz) = name.parse::<chrono_tz::Tz>() {
                return Some(tz);
            }
        }
    }
    None
}

/// `t` (naive = UTC) as local wall-clock time in the display zone.
pub(crate) fn to_local(t: &PyDateTime) -> NaiveDateTime {
    let utc: DateTime<Utc> = t.to_utc().unwrap_or_else(|| Utc.from_utc_datetime(&t.local));
    #[cfg(test)]
    {
        if let Some(off) = ZONE_OVERRIDE.with(|z| z.get()) {
            return utc.naive_utc() + Duration::seconds(off as i64);
        }
    }
    match local_zone() {
        Some(tz) => utc.with_timezone(&tz).naive_local(),
        None => utc.with_timezone(&Local).naive_local(),
    }
}

/// `_clock`: `t` in local time, e.g. "11:00 AM".
pub fn clock(t: &PyDateTime) -> String {
    let loc = to_local(t);
    let h = loc.hour();
    format!("{}:{:02} {}", if h % 12 == 0 { 12 } else { h % 12 }, loc.minute(), if h < 12 { "AM" } else { "PM" })
}

fn game_time(ctx: &LeagueContext, p: &Player, d: NaiveDate) -> Option<PyDateTime> {
    let team = normalize_team(p.team.as_deref()).unwrap_or_default();
    ctx.game_starts.get(&team).and_then(|m| m.get(&d)).copied()
}

fn first_game(ctx: &LeagueContext, d: NaiveDate) -> Option<PyDateTime> {
    let mut best: Option<PyDateTime> = None;
    for s in ctx.game_starts.values() {
        if let Some(t) = s.get(&d) {
            if best.map_or(true, |b| matches!(t.partial_cmp(&b), Some(std::cmp::Ordering::Less))) {
                best = Some(*t);
            }
        }
    }
    best
}

/// `_wd`: "Mon".
pub(crate) fn wd(d: NaiveDate) -> String {
    strftime_date(d, "%a")
}

/// `_timing`: (when to make `mv`, the day to make it).
fn timing(ctx: &LeagueContext, mv: &PlanMove) -> (String, NaiveDate) {
    let (eff, today) = (mv.effective, ctx.as_of);
    if mv.claim {
        if let Some(w) = mv.add.waiver_until {
            return (format!("Place the claim now: it clears {} and counts from {}", wd(w), wd(eff)), mv.day);
        }
    }
    let prev = eff - Duration::days(1);
    if let Some(drop) = mv.drop.as_ref() {
        if prev >= today {
            if let Some(g) = game_time(ctx, drop, prev) {
                return (
                    format!(
                        "Make this move the evening before (after {}'s game starts {} at {} your time), so he keeps those \
                         points and {} counts {}",
                        drop.name,
                        wd(prev),
                        clock(&g),
                        mv.add.name,
                        wd(eff)
                    ),
                    prev,
                );
            }
        }
    }
    let when = if eff == today { "today".to_string() } else { wd(eff) };
    if ctx.lock_per_game {
        if let Some(g) = game_time(ctx, &mv.add, eff) {
            return (format!("Make this move before {}'s game {when} ({} your time)", mv.add.name, clock(&g)), mv.day);
        }
    }
    match first_game(ctx, eff) {
        None => (format!("Make this move by {when}"), mv.day),
        Some(first) => (format!("Make this move before {when}'s first game ({} your time)", clock(&first)), mv.day),
    }
}

/// `_move_day`: when to make a move that should count from `effective`.
pub(crate) fn move_day(ctx: &LeagueContext, p: &Player, effective: NaiveDate) -> NaiveDate {
    let today = ctx.as_of;
    if p.waiver_until.is_some_and(|w| w >= today) {
        return today;
    }
    today.max(effective)
}

/// `_candidates`: the this-matchup pickups (injured players are never the drop), or a note.
fn candidates(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> (Vec<Recommendation>, Vec<String>) {
    let hurt: Vec<Cid> = match ctx.my_team() {
        Ok(t) => t.players_iter().filter(|p| unavailable(p.status)).map(|p| p.cid.clone()).collect(),
        Err(e) => return (vec![], vec![format!("Pickup candidates unavailable: {e}")]),
    };
    let opts = WaiverOpts {
        limit: CANDIDATE_LIMIT,
        horizon: fm_core::values::Horizon::Week,
        dynasty_values,
        stream: true,
        exclude_drops: &hurt,
    };
    match recommend_waivers(ctx, values, &opts, None, now, memo) {
        Ok(recs) => (recs, vec![]),
        Err(e) => (vec![], vec![format!("Pickup candidates unavailable: {e}")]),
    }
}

/// (add, drop, to_ir, rec) pairs of the candidate recs.
fn pairs_of<'r>(tl: &Timeline<'_>, recs: &'r [Recommendation]) -> Vec<Pair<'r>> {
    let mut out = Vec::new();
    for r in recs {
        let Some(a) = r.add.first() else { continue };
        let Some(add) = tl.id(&a.cid) else { continue };
        let drop = r.drop.first().and_then(|p| tl.id(&p.cid));
        let to_ir = if r.drop.is_empty() { r.subjects.first().and_then(|p| tl.id(&p.cid)) } else { None };
        out.push(Pair { add, drop, to_ir, rec: r });
    }
    out
}

/// Long-term values the plan compares with (`lt` in Python): the DynastyValue objects in keeper
/// leagues, plain dynasty values in keep-everyone dynasties, else none (rest-of-season FPG).
#[derive(Clone, Copy)]
pub(crate) enum LongTerm<'d> {
    None,
    Floats(&'d Map<Cid, f64>),
    Keeper(&'d Map<Cid, DynastyValue>, i64),
}

fn fpg_season(values: &Map<Cid, PlayerValue>, cid: &str) -> f64 {
    values.get(cid).map_or(0.0, |pv| pv.fpg_season)
}

/// `_long_term`: (within tolerance, reason text) of swapping `drop` for `add` long term.
pub(crate) fn long_term(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    lt: LongTerm<'_>,
    add: &Player,
    drop: &Player,
) -> (bool, String) {
    let tol = stream_dynasty_tolerance(ctx.dynasty_mode.as_str());
    let (a, d, unit, lead, nd): (f64, f64, String, &str, usize) = match lt {
        LongTerm::Keeper(dobj, k) => {
            let roster: Vec<&Player> = ctx.my_team().map(|t| t.players()).unwrap_or_default();
            let (a, d) = swap_values(ctx, Some(dobj), roster, Some(add), Some(drop));
            (a, d, format!("value (this season + top-{k} keepers)"), "Long term", 1)
        }
        LongTerm::Floats(dy) => (
            dy.get(&add.cid).copied().unwrap_or(0.0),
            dy.get(&drop.cid).copied().unwrap_or(0.0),
            "dynasty value".to_string(),
            "Long term",
            1,
        ),
        LongTerm::None => {
            (fpg_season(values, &add.cid), fpg_season(values, &drop.cid), "FPG".to_string(), "Rest of season", 2)
        }
    };
    let mut text = format!("{lead}: {} {} vs {} {} {unit} (", add.name, fixed(a, nd, false), drop.name, fixed(d, nd, false));
    if a >= d {
        text += &format!("an upgrade, +{})", fixed(a - d, nd, false));
    } else {
        text += &format!("costs {}, {} of {}'s)", fixed(d - a, nd, false), percent((d - a) / d, 0, false), drop.name);
    }
    (a >= (1.0 - tol) * d, text)
}

/// `_search`: beam search over move sequences (see the Python module docstring).
#[allow(clippy::too_many_arguments)]
pub(crate) fn search<'r>(
    ctx: &LeagueContext,
    tl: &Timeline<'_>,
    pairs: &[Pair<'r>],
    max_moves: i64,
    min_pts: f64,
    now: &PyDateTime,
    values: &Map<Cid, PlayerValue>,
    lt: LongTerm<'_>,
) -> Vec<Tx<'r>> {
    let mut states: Vec<(f64, Vec<Tx<'r>>)> = vec![(tl.total(&[]), vec![])];
    let mut best: (f64, Vec<Tx<'r>>) = states[0].clone();
    for _ in 0..max_moves.max(0) {
        let per_state: Vec<Vec<(f64, Vec<Tx<'r>>)>> = states
            .par_iter()
            .map(|(total, txs)| {
                let mut options = state_options(ctx, tl, pairs, txs, min_pts, now, values, lt);
                sort_desc_by(&mut options, |a, b| cmp_f64(a.0, b.0));
                options
                    .into_iter()
                    .take(EXPAND)
                    .map(|(g, tx)| {
                        let mut next = txs.clone();
                        next.push(tx);
                        (total + g, next)
                    })
                    .collect()
            })
            .collect();
        let mut nxt: Vec<(f64, Vec<Tx<'r>>)> = per_state.into_iter().flatten().collect();
        if nxt.is_empty() {
            break;
        }
        sort_desc_by(&mut nxt, |a, b| cmp_f64(a.0, b.0));
        nxt.truncate(BEAM);
        states = nxt;
        if states[0].0 > best.0 + 1e-9 {
            best = states[0].clone();
        }
    }
    prune(tl, best.1, min_pts)
}

/// The candidate moves on top of one beam state, in Python's order.
#[allow(clippy::too_many_arguments)]
fn state_options<'r>(
    ctx: &LeagueContext,
    tl: &Timeline<'_>,
    pairs: &[Pair<'r>],
    txs: &[Tx<'r>],
    min_pts: f64,
    now: &PyDateTime,
    values: &Map<Cid, PlayerValue>,
    lt: LongTerm<'_>,
) -> Vec<(f64, Tx<'r>)> {
    let added: HashSet<usize> = txs.iter().map(|t| t.add).collect();
    let gone: HashSet<usize> = txs.iter().flat_map(|t| [t.drop, t.to_ir]).flatten().collect();
    let open_used = txs.iter().any(|t| t.drop.is_none() && t.to_ir.is_none());
    let mut options: Vec<(f64, Tx<'r>)> = pairs
        .par_iter()
        .filter_map(|pr| {
            if added.contains(&pr.add)
                || pr.drop.is_some_and(|d| gone.contains(&d))
                || pr.to_ir.is_some_and(|d| gone.contains(&d))
            {
                return None;
            }
            if pr.drop.is_none() && open_used && pr.to_ir.is_none() {
                return None; // one open roster spot is used once
            }
            let add = tl.p(pr.add);
            let hit = tl.best_day(txs, pr.add, pr.drop, pr.to_ir, earliest(ctx, add, now))?;
            if hit.0 < min_pts {
                return None;
            }
            Some((
                hit.0,
                Tx { day: move_day(ctx, add, hit.1), effective: hit.1, add: pr.add, drop: pr.drop, to_ir: pr.to_ir, rec: Some(pr.rec) },
            ))
        })
        .collect();
    // re-stream: drop a streamer this plan added for another pickup later in the week
    let mut combos: Vec<(usize, usize)> = Vec::new();
    for (pi, prev) in txs.iter().enumerate() {
        if gone.contains(&prev.add) {
            continue;
        }
        for (qi, pr) in pairs.iter().enumerate() {
            if added.contains(&pr.add) || !shares_default_slot(tl.p(pr.add), tl.p(prev.add)) {
                continue;
            }
            combos.push((pi, qi));
        }
    }
    let restream: Vec<(f64, Tx<'r>)> = combos
        .par_iter()
        .filter_map(|&(pi, qi)| {
            let prev = &txs[pi];
            let pr = &pairs[qi];
            let add = tl.p(pr.add);
            if !long_term(ctx, values, lt, add, tl.p(prev.add)).0 {
                return None;
            }
            let e = earliest(ctx, add, now).max(prev.effective + Duration::days(1));
            let hit = tl.best_day(txs, pr.add, Some(prev.add), None, e)?;
            if hit.0 < min_pts {
                return None;
            }
            Some((
                hit.0,
                Tx { day: move_day(ctx, add, hit.1), effective: hit.1, add: pr.add, drop: Some(prev.add), to_ir: None, rec: Some(pr.rec) },
            ))
        })
        .collect();
    options.extend(restream);
    options
}

/// `_prune`: drop moves whose own contribution is below `min_pts`; sorted by (day, effective).
pub(crate) fn prune<'r>(tl: &Timeline<'_>, txs: Vec<Tx<'r>>, min_pts: f64) -> Vec<Tx<'r>> {
    let mut txs = txs;
    let mut changed = true;
    while changed && !txs.is_empty() {
        changed = false;
        let full = tl.total(&txs);
        for i in 0..txs.len() {
            let t = txs[i];
            let rest: Vec<Tx<'r>> = txs
                .iter()
                .enumerate()
                .filter(|(j, x)| *j != i && x.drop != Some(t.add))
                .map(|(_, x)| *x)
                .collect();
            if full - tl.total(&rest) < min_pts {
                txs = rest;
                changed = true;
                break;
            }
        }
    }
    txs.sort_by(|a, b| (a.day, a.effective).cmp(&(b.day, b.effective)));
    txs
}

// ---------------------------------------------------------------------------- win probability

fn win(tl: &Timeline<'_>, txs: &[Tx<'_>], m: Option<&Matchup>, as_set: bool) -> Option<f64> {
    let m = m?;
    m.opponent_team.as_ref()?;
    let mut pts: Map<usize, Vec<f64>> = Map::new();
    let mut goalie: HashMap<usize, bool> = HashMap::new();
    for (k, &d) in tl.days.iter().enumerate() {
        if as_set {
            for &b in &tl.base {
                let v = tl.val(b, k);
                if tl.starter[b] && v > 0.0 {
                    pts.entry(b).or_default().push(v);
                    goalie.insert(b, tl.p(b).is_goalie());
                }
            }
            continue;
        }
        let players = tl.roster(d, txs);
        let (_, chosen) = tl.day(k, &players);
        for &c in chosen.iter() {
            let c = c as usize;
            let v = tl.val(c, k);
            if v > 0.0 {
                pts.entry(c).or_default().push(v);
                goalie.insert(c, tl.p(c).is_goalie());
            }
        }
    }
    let mine: Vec<(f64, f64)> = pts
        .iter()
        .map(|(c, xs)| {
            let mean = py_sum(xs.iter().copied());
            let n = xs.len() as f64;
            (mean, n.powf(0.5) * m.sd_model.game_sd(mean / n, goalie.get(c).copied().unwrap_or(false)))
        })
        .collect();
    let theirs: Vec<(f64, f64)> = m.their_players.iter().map(|x| (x.proj_remaining, x.sd)).collect();
    Some(win_probability(m.my_points_so_far, m.their_points_so_far, &mine, &theirs, m.draws, Some(WIN_SEED)))
}

// ------------------------------------------------------------------------------ text helpers

fn plural(n: usize) -> &'static str {
    if n != 1 {
        "s"
    } else {
        ""
    }
}

/// `_move_headline`. `lost`: nights the dropped player would have started from the move on.
fn move_headline(tl: &Timeline<'_>, mv: &PlanMove, lost: Option<&[NaiveDate]>) -> String {
    let days = mv.starts.iter().map(|d| wd(*d)).collect::<Vec<_>>().join(", ");
    let days = if days.is_empty() { "no nights".to_string() } else { days };
    let mut head = format!("{} starts {} night{} for you ({days})", mv.add.name, mv.starts.len(), plural(mv.starts.len()));
    if let Some(drop) = &mv.drop {
        let games = tl
            .days
            .iter()
            .enumerate()
            .filter(|(k, d)| **d >= mv.effective && tl.value_cid(&drop.cid, *k) > 0.0)
            .count();
        if games == 0 {
            head += &format!("; {} has no games left this matchup", drop.name);
        } else {
            match lost {
                None => head += &format!("; {} has {games} game{} left after that", drop.name, plural(games)),
                Some(lost) => {
                    let n = lost.len();
                    head += &format!("; {} would have started {n} of his {games} game{} left", drop.name, plural(games));
                    if !lost.is_empty() {
                        head += &format!(" ({})", lost.iter().map(|d| wd(*d)).collect::<Vec<_>>().join(", "));
                    }
                }
            }
        }
    } else if let Some(ir) = &mv.to_ir {
        head += &format!("; {} goes to IR, no drop", ir.name);
    } else {
        head += "; open roster spot, no drop";
    }
    head
}

/// Up to `limit` concrete actions for `day` from the plan, most important first.
pub fn today_checklist(plan: &WeekPlan, day: NaiveDate, limit: usize) -> Vec<String> {
    if !plan.available {
        return vec![];
    }
    let mut out: Vec<String> = Vec::new();
    for m in &plan.moves {
        if m.day == day {
            let drop = drop_text(m);
            let what = if m.headline.is_empty() { format!("+{} pts", fixed(m.gain_pts, 1, false)) } else { m.headline.clone() };
            out.push(format!("{} {}{drop}: {what}", if m.claim { "Claim" } else { "Add" }, m.add.name));
        }
    }
    for a in &plan.today {
        match a.headline.as_deref().filter(|h| !h.is_empty()) {
            Some(h) => out.push(format!("{}: {h}", a.title)),
            None => out.push(a.title.clone()),
        }
    }
    if let Some(line) = plan.days.iter().find(|x| x.day == day) {
        if !line.ins.is_empty() {
            out.push(format!("After quick lineup, start {}", line.ins.join(", ")));
        }
        if !line.outs.is_empty() {
            out.push(format!("After quick lineup, bench {}", line.outs.join(", ")));
        }
        if line.pregame && !line.goalies.is_empty() {
            out.push(format!("Check goalie confirmations: {}", line.goalies.join(", ")));
        }
    }
    out.truncate(limit);
    out
}

fn drop_text(m: &PlanMove) -> String {
    if let Some(d) = &m.drop {
        format!(", drop {}", d.name)
    } else if let Some(i) = &m.to_ir {
        format!(", {} to IR", i.name)
    } else {
        String::new()
    }
}

/// Morning-digest lead lines (see the Python docstring).
pub fn plan_summary(plan: &WeekPlan, day: NaiveDate) -> Vec<String> {
    if !plan.available {
        return vec![];
    }
    let mut head = format!("This week vs {}", plan.opponent.as_deref().filter(|s| !s.is_empty()).unwrap_or("opponent"));
    if let (Some(a), Some(p)) = (plan.win_as_set, plan.win_plan) {
        head += &format!(": win {} with quick lineup -> {} with the plan", percent(a, 0, false), percent(p, 0, false));
    }
    head += &format!(
        " ({} pts remaining vs {} with quick lineup",
        fixed(plan.proj_plan, 1, false),
        fixed(plan.proj_as_set, 1, false)
    );
    head += &match plan.moves_left {
        Some(n) => format!("; moves left {n})"),
        None => ")".to_string(),
    };
    let mut lead = vec![head];
    if let Some(s) = plan_memory::since_line(plan) {
        lead.push(s);
    }
    let actions = today_checklist(plan, day, CHECKLIST_LIMIT);
    if actions.is_empty() {
        lead.push("Today: quick lineup is enough".to_string());
        return lead;
    }
    lead.push("Quick lineup each day; also do:".to_string());
    lead.extend(actions.into_iter().map(|a| format!("Today: {a}")));
    lead
}

/// `_pair_outs`: pair each player coming into the lineup with one leaving it.
fn pair_outs(tl: &Timeline<'_>, ins: &[usize], prev: &[usize], playing: &HashSet<usize>) -> Vec<(usize, Option<usize>)> {
    let mut pool: Vec<usize> = prev.to_vec();
    pool.sort_by(|a, b| (playing.contains(a), &tl.p(*a).name).cmp(&(playing.contains(b), &tl.p(*b).name)));
    let mut out = Vec::new();
    for &p in ins {
        let mate = pool.iter().position(|&q| shares_default_slot(tl.p(p), tl.p(q)));
        let mate = mate.map(|i| pool.remove(i));
        out.push((p, mate));
    }
    out
}

// ------------------------------------------------------------------------------ entry point

/// Keyword arguments of `build_week_plan`.
pub struct PlanOpts<'a> {
    pub dynasty_values: Option<&'a Map<Cid, DynastyValue>>,
    /// the advise output (today's lineup / injury items); computed when None
    pub recs: Option<&'a [Recommendation]>,
    /// `analysis::matchup::current_matchup`; computed with `provider` when None
    pub matchup: Option<&'a Matchup>,
    pub provider: Option<&'a dyn LeagueProvider>,
    pub history: &'a History<'a>,
    /// `analysis::goalie_plan::goalie_week`; computed when None
    pub goalies: Option<&'a GoalieWeek>,
    /// plan memory (and the goalie odds archive); None = no memory
    pub data_dir: Option<&'a Path>,
}

impl Default for PlanOpts<'_> {
    fn default() -> Self {
        PlanOpts {
            dynasty_values: None,
            recs: None,
            matchup: None,
            provider: None,
            history: &History::None,
            goalies: None,
            data_dir: None,
        }
    }
}

fn err_type(e: &CoreError) -> &'static str {
    match e {
        CoreError::Lookup(_) => "LookupError",
        CoreError::Value(_) => "ValueError",
    }
}

/// The weekly action plan (see the Python module docstring).
pub fn build_week_plan(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    opts: &PlanOpts<'_>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> WeekPlan {
    let Ok(team) = ctx.my_team() else {
        let mut plan = WeekPlan::new(&ctx.provider, "");
        plan.available = false;
        plan.notes.push("No team flagged as mine.".to_string());
        return plan;
    };
    let mut plan = WeekPlan::new(&ctx.provider, &team.name);
    if ctx.schedule.is_empty() || lineup_lock::lock(ctx) != "daily" {
        plan.available = false;
        plan.notes.push("The weekly plan needs a daily-lineup league with the NHL schedule loaded.".to_string());
        return plan;
    }
    let mut days = plan_days(ctx);
    let owned_matchup;
    let matchup: Option<&Matchup> = match opts.matchup {
        Some(m) => Some(m),
        None => {
            let mopts = MatchupOpts { streaming: false, ..MatchupOpts::default() };
            match current_matchup(ctx, opts.provider, Some(values), mopts, now, memo) {
                Ok(m) => {
                    owned_matchup = m;
                    Some(&owned_matchup)
                }
                Err(e) => {
                    plan.notes.push(format!("Matchup unavailable ({}: {e}); no win probabilities", err_type(&e)));
                    None
                }
            }
        }
    };
    if let Some(m) = matchup {
        plan.opponent = m.opponent_team.clone();
        plan.start = m.start;
        plan.end = m.end;
        plan.my_so_far = m.my_points_so_far;
        plan.their_so_far = m.their_points_so_far;
        plan.their_remaining = m.their_projected_remaining;
        plan.stance = m.stance.clone();
        if let Some(end) = m.end {
            let kept: Vec<NaiveDate> = days.iter().copied().filter(|d| *d <= end).collect();
            if !kept.is_empty() {
                days = kept;
            }
        }
    }
    plan.days_left = days.len() as i64;
    let stance = plan.stance.clone().unwrap_or_else(|| "even".to_string());

    // today's items from the existing recommenders
    let owned_recs;
    let recs: &[Recommendation] = match opts.recs {
        Some(r) => r,
        None => {
            let aopts = AdviseOpts {
                dynasty_values: opts.dynasty_values,
                history: opts.history,
                include: &["lineup", "injuries"],
                limit: None,
            };
            owned_recs = advise(ctx, values, &aopts, now, memo);
            &owned_recs
        }
    };
    let mut ir_items: Vec<usize> = Vec::new();
    for r in recs {
        if !matches!(r.kind, RecKind::Lineup | RecKind::Injury) {
            continue;
        }
        let has = |c: &str| r.reasons.iter().any(|x| x.code == c);
        if r.title.starts_with("Hold ") || (has("HOLD_VERDICT") && r.drop.is_empty() && r.add.is_empty()) {
            plan.watch.push(r.headline.clone().filter(|h| !h.is_empty()).unwrap_or_else(|| r.title.clone()));
            continue;
        }
        let ir = has("IR_SWAP") || r.title.starts_with("Activate ");
        if r.kind == RecKind::Lineup && !(ir || has("RETURN_DUE")) {
            continue; // start/sit and benching the injured: quick lineup does it
        }
        plan.today.push(PlanAction {
            kind: r.kind.as_str().to_string(),
            title: r.title.clone(),
            headline: r.headline.clone(),
            gain: if ir { None } else { r.predicted_gain },
            gain_units: if ir { None } else { r.gain_units.map(|g| g.as_str().to_string()) },
        });
        if ir {
            ir_items.push(plan.today.len() - 1);
        }
    }

    // IR swaps / activations change who is available: apply them before scheduling
    let swapped = apply_ir_actions(team, recs);
    let tl_team: &FantasyTeam = swapped.as_ref().unwrap_or(team);
    let tl = Timeline::new(ctx, tl_team, values, days.clone(), now, memo);
    plan.proj_frozen = py_sum((0..days.len()).map(|k| tl.as_set(k)));
    plan.proj_lineup = tl.total(&[]);
    plan.proj_as_set = plan.proj_lineup;
    if !ir_items.is_empty() && swapped.is_some() {
        let before = Timeline::new(ctx, team, values, days.clone(), now, memo).total(&[]);
        let item = &mut plan.today[ir_items[0]];
        item.gain = Some(round_to(plan.proj_lineup - before, 2));
        item.gain_units = Some("matchup_pts".to_string());
    }

    let dobj = dynasty_objects(ctx, values, opts.dynasty_values);
    let dyn_ = dynasty_floats(dobj.as_deref());
    let lt: LongTerm<'_> = match (&dyn_, dobj.as_deref().and_then(|d| keeper_k(ctx, Some(d)).map(|k| (d, k)))) {
        (Some(_), Some((d, k))) => LongTerm::Keeper(d, k),
        (Some(f), None) => LongTerm::Floats(f),
        (None, _) => LongTerm::None,
    };
    let left = moves_left(ctx);
    plan.moves_left = left;
    let mut max_moves = left.unwrap_or(UNLIMITED_MOVES_CAP);
    if stance == "protect" && max_moves >= 2 {
        max_moves -= 1;
        plan.notes.push("Protect stance: one move is kept in reserve for an injury.".to_string());
    }
    // plan memory: earlier plans for this matchup period
    let snaps: Option<Vec<Value>> = opts.data_dir.map(|dir| plan_memory::load_snapshots(dir, &plan, ctx));
    let mut kept: Map<plan_memory::MoveKey, String> = Map::new();
    let mut txs: Vec<Tx<'_>> = Vec::new();
    let cand_recs: Vec<Recommendation>;
    let pairs: Vec<Pair<'_>>;
    if max_moves > 0 {
        let (r, notes) = candidates(ctx, values, opts.dynasty_values, now, memo);
        cand_recs = r;
        plan.notes.extend(notes);
        pairs = pairs_of(&tl, &cand_recs);
        let min_pts = min_move_pts(&stance) * league_unit(ctx);
        txs = search(ctx, &tl, &pairs, max_moves, min_pts, now, values, lt);
        if let Some(prev) = snaps.as_ref().and_then(|s| s.last()) {
            let (t, k) = plan_memory::stabilize(ctx, &tl, txs, prev, &pairs, max_moves, min_pts, now);
            txs = t;
            kept = k;
        }
    } else if left == Some(0) {
        plan.notes.push("No moves left this period: the plan only sets lineups.".to_string());
    }
    plan.moves_planned = txs.len() as i64;
    plan.proj_plan = tl.total(&txs);
    if left != Some(0) {
        let exclude: HashSet<String> = txs.iter().map(|t| tl.p(t.add).cid.clone()).collect();
        match claims::claim_watch_lines(ctx, values, opts.dynasty_values, &exclude, 2, now, memo) {
            Ok(lines) => plan.claim_watch.extend(lines),
            Err(e) => plan.notes.push(format!("Claim check unavailable: {e}")),
        }
    }

    // win probabilities: quick lineup (the baseline) -> each move in order
    plan.win_frozen = win(&tl, &[], matchup, true);
    plan.win_lineup = win(&tl, &[], matchup, false);
    plan.win_as_set = plan.win_lineup;
    let per_day = tl.per_day(&txs);
    let (mut prev_total, mut prev_win) = (plan.proj_lineup, plan.win_lineup);
    for (i, t) in txs.iter().enumerate() {
        let up = upto(&txs, i as isize);
        let tot = tl.total(&up);
        let w = win(&tl, &up, matchup, false);
        let starts: Vec<NaiveDate> =
            days.iter().zip(&per_day).filter(|(_, (_, ch))| ch.contains(&(t.add as u32))).map(|(d, _)| *d).collect();
        // nights the dropped player would have started without this move
        let lost: Option<Vec<NaiveDate>> = t.drop.map(|drop| {
            let others: Vec<Tx<'_>> = txs.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, x)| *x).collect();
            let without = tl.per_day(&upto(&others, i as isize - 1));
            days.iter()
                .zip(&without)
                .filter(|(d, (_, ch))| **d >= t.effective && ch.contains(&(drop as u32)))
                .map(|(d, _)| *d)
                .collect()
        });
        let add = tl.p(t.add);
        let mut mv = PlanMove {
            day: t.day,
            effective: t.effective,
            add: add.clone(),
            drop: t.drop.map(|d| tl.p(d).clone()),
            to_ir: t.to_ir.map(|d| tl.p(d).clone()),
            gain_pts: round_to(tot - prev_total, 2),
            win_delta: match (w, prev_win) {
                (Some(a), Some(b)) => Some(a - b),
                _ => None,
            },
            starts,
            headline: String::new(),
            market: t.rec.and_then(|r| {
                let s = market_suffix(r).trim_matches(|c| c == ' ' || c == '\u{b7}');
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            }),
            claim: add.waiver_until.is_some_and(|w| w >= ctx.as_of),
            reasons: vec![],
            chain: None,
            role: None,
            note: String::new(),
            timing: String::new(),
            status: String::new(),
        };
        mv.headline = move_headline(&tl, &mv, lost.as_deref());
        if let Some(r) = t.rec {
            if !r.reasons.is_empty() {
                mv.reasons =
                    r.reasons.iter().filter(|x| x.code == "WAIVER_CLAIM" || x.code == "CHURN_GUARD").map(|x| x.text.clone()).collect();
            }
        }
        if let Some(d) = t.drop {
            mv.reasons.insert(0, long_term(ctx, values, lt, add, tl.p(d)).1);
        }
        let (tm, day) = timing(ctx, &mv);
        mv.timing = tm;
        mv.day = day;
        if kept.contains_key(&plan_memory::move_key_move(&mv)) {
            mv.status = "kept".to_string();
        }
        plan.moves.push(mv);
        prev_total = tot;
        prev_win = w;
    }
    plan.win_plan = prev_win;
    chains(&mut plan, ctx, values, lt, &tl);
    if let (Some(snaps), Some(dir)) = (snaps.as_ref(), opts.data_dir) {
        let since = plan_memory::compare(&mut plan, ctx, snaps, &kept, &tl, &txs, values, dyn_.as_ref(), now);
        if let Some(s) = &since {
            plan.watch.extend(s.long_term.iter().cloned());
        }
        plan.since = since;
        plan_memory::save_snapshot(dir, &plan, ctx, now);
    }

    // goalie lines
    let owned_goalies;
    let goalies: &GoalieWeek = match opts.goalies {
        Some(g) => g,
        None => {
            let gopts = GoalieWeekOpts { data_dir: opts.data_dir.map(|d| d.to_path_buf()), streams: false, ..GoalieWeekOpts::default() };
            owned_goalies = goalie_week(ctx, values, &gopts, now, memo);
            &owned_goalies
        }
    };
    day_lines(&mut plan, ctx, &tl, &txs, &per_day, goalies);
    plan.notes.extend(assumption_notes(ctx));
    plan
}

/// `_slot_label`: "G", "C/LW", "D", or "F".
pub(crate) fn slot_label(p: &Player) -> String {
    let pos: Vec<&str> = p.positions.iter().map(|s| s.as_str()).filter(|x| !["F", "UTIL", "BN"].contains(x)).collect();
    if p.is_goalie() {
        "G".to_string()
    } else if pos.is_empty() {
        "F".to_string()
    } else {
        pos[..pos.len().min(2)].join("/")
    }
}

/// `_chains`: re-streams as one story, plus the "Net this week" line.
fn chains(plan: &mut WeekPlan, ctx: &LeagueContext, values: &Map<Cid, PlayerValue>, lt: LongTerm<'_>, tl: &Timeline<'_>) {
    let roster: Vec<&Player> = ctx.my_team().map(|t| t.players()).unwrap_or_default();
    let mut by_add: HashMap<String, usize> = HashMap::new();
    for (i, m) in plan.moves.iter().enumerate() {
        by_add.insert(m.add.cid.clone(), i);
    }
    let mut n = 0i64;
    let total = plan.moves.len();
    for i in 0..total {
        let Some(j) = plan.moves[i].drop.as_ref().and_then(|d| by_add.get(&d.cid).copied()) else { continue };
        if j == i {
            continue;
        }
        if plan.moves[j].chain.is_none() {
            n += 1;
            let prev = &mut plan.moves[j];
            prev.chain = Some(n);
            prev.role = Some("rental".to_string());
            let span = match (prev.starts.first(), prev.starts.last()) {
                (Some(a), Some(b)) => format!("{}-{}", wd(*a), wd(*b)),
                _ => wd(prev.effective),
            };
            let k = prev.starts.len();
            prev.note = format!(
                "Rent {} {span}: {k} night{} your {} slot would otherwise miss",
                prev.add.name,
                plural(k),
                slot_label(&prev.add)
            );
        }
        let (prev_chain, prev_add) = (plan.moves[j].chain, plan.moves[j].add.clone());
        let m_eff = plan.moves[i].effective;
        let later = tl.days.iter().enumerate().filter(|(k, d)| **d >= m_eff && tl.value_cid(&prev_add.cid, *k) > 0.0).count();
        let mut why = vec![if later == 0 {
            format!("{} has no games left", prev_add.name)
        } else {
            format!("{} has {later} game{} left", prev_add.name, plural(later))
        }];
        let last = i == total - 1 && plan.moves_left.is_some_and(|l| l == total as i64);
        if last {
            let mut s = "this is your last move this week and unused moves don't carry over".to_string();
            if let Some(end) = ctx.period_end {
                let reset = end + Duration::days(1);
                s += &format!(" (reset {} {} {})", wd(reset), strftime_date(reset, "%b"), reset.day());
            }
            why.push(s);
        }
        let m_add = plan.moves[i].add.clone();
        match lt {
            LongTerm::Keeper(dobj, _) => {
                let (a, d) = swap_values(ctx, Some(dobj), roster.iter().copied(), Some(&m_add), Some(&prev_add));
                if a > d {
                    why.push(format!("{} is the better keeper", m_add.name));
                }
            }
            LongTerm::Floats(dy) => {
                if dy.get(&m_add.cid).copied().unwrap_or(0.0) > dy.get(&prev_add.cid).copied().unwrap_or(0.0) {
                    why.push(format!("{} is the better keeper", m_add.name));
                }
            }
            LongTerm::None => {}
        }
        let m = &mut plan.moves[i];
        m.chain = prev_chain;
        m.role = Some("swap".to_string());
        m.note = format!("Then swap {} for {} {}: {}", prev_add.name, m_add.name, wd(m_eff), why.join("; "));
    }
    if plan.moves.is_empty() {
        return;
    }
    let mut added: Map<String, Player> = Map::new();
    for m in &plan.moves {
        added.insert(m.add.cid.clone(), m.add.clone());
    }
    let mut dropped: Map<String, Player> = Map::new();
    for m in &plan.moves {
        if let Some(d) = &m.drop {
            dropped.insert(d.cid.clone(), d.clone());
        }
    }
    let stays: Vec<&Player> = added.iter().filter(|(c, _)| !dropped.contains_key(*c)).map(|(_, p)| p).collect();
    let gone: Vec<&Player> = dropped.iter().filter(|(c, _)| !added.contains_key(*c)).map(|(_, p)| p).collect();
    let rented: Vec<&Player> = added.iter().filter(|(c, _)| dropped.contains_key(*c)).map(|(_, p)| p).collect();
    let ltext = match lt {
        LongTerm::Keeper(dobj, k) => {
            let out: HashSet<&str> = gone.iter().map(|p| p.cid.as_str()).collect();
            let after: Vec<&Player> =
                roster.iter().copied().filter(|p| !out.contains(p.cid.as_str())).chain(stays.iter().copied()).collect();
            let delta = dynasty_delta(ctx, Some(dobj), roster.iter().copied(), after);
            format!("long term {} value (this season + top-{k} keepers)", fixed(delta, 1, true))
        }
        LongTerm::Floats(dy) => {
            let delta = py_sum(stays.iter().map(|p| dy.get(&p.cid).copied().unwrap_or(0.0)))
                - py_sum(gone.iter().map(|p| dy.get(&p.cid).copied().unwrap_or(0.0)));
            format!("long term {} dynasty value", fixed(delta, 1, true))
        }
        LongTerm::None => {
            let delta = py_sum(stays.iter().map(|p| fpg_season(values, &p.cid))) - py_sum(gone.iter().map(|p| fpg_season(values, &p.cid)));
            format!("rest of season {} FPG", fixed(delta, 2, true))
        }
    };
    let names = |ps: &[&Player]| {
        let s = ps.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ");
        if s.is_empty() {
            "nobody".to_string()
        } else {
            s
        }
    };
    let mut line = format!(
        "Net this week: {} gone; {} stay{}",
        names(&gone),
        names(&stays),
        if stays.len() == 1 { "s" } else { "" }
    );
    if !rented.is_empty() {
        line += &format!("; {} rented and released", rented.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "));
    }
    let mg = plan.moves_gain();
    plan.net_week.push(format!("{line}; {ltext}; {} pts this matchup", fixed(mg, 1, true)));
}

/// `_apply_ir_actions`: `team` after today's IR swaps and plain activations with room (None =
/// unchanged).
fn apply_ir_actions(team: &FantasyTeam, recs: &[Recommendation]) -> Option<FantasyTeam> {
    let mut act: HashSet<&str> = HashSet::new();
    let mut to_ir: HashSet<&str> = HashSet::new();
    for r in recs {
        if r.kind != RecKind::Lineup || r.add.is_empty() {
            continue;
        }
        let has = |c: &str| r.reasons.iter().any(|x| x.code == c);
        if has("IR_SWAP") && !r.subjects.is_empty() {
            act.insert(r.add[0].cid.as_str());
            to_ir.insert(r.subjects[0].cid.as_str());
        } else if r.title.starts_with("Activate ") && !has("ROSTER_FULL") {
            act.insert(r.add[0].cid.as_str());
        }
    }
    if act.is_empty() && to_ir.is_empty() {
        return None;
    }
    let slots = team
        .slots
        .iter()
        .map(|s| match &s.player {
            Some(p) if act.contains(p.cid.as_str()) && s.slot == "IR" => {
                RosterSlot { slot: "BN".to_string(), player: Some(p.clone()), starting: false }
            }
            Some(p) if to_ir.contains(p.cid.as_str()) => RosterSlot { slot: "IR".to_string(), player: Some(p.clone()), starting: false },
            _ => s.clone(),
        })
        .collect();
    Some(FantasyTeam { slots, ..team.clone() })
}

/// `_quick_choice`: what quick lineup would start on `days[k]` (ranked by season value).
fn quick_choice(tl: &Timeline<'_>, k: usize, players: &[usize]) -> (f64, Vec<usize>) {
    let today = tl.is_today(k);
    let pool: Vec<usize> = players
        .iter()
        .copied()
        .filter(|&i| (tl.val(i, k) > 0.0 || (today && tl.locked[i])) && !(today && tl.locked[i] && !tl.starter[i]))
        .collect();
    let rank: Vec<f64> = pool
        .iter()
        .map(|&i| fpg_season(tl.values, &tl.p(i).cid) + if today && tl.locked[i] { LOCK_BONUS } else { 0.0 })
        .collect();
    let pp: Vec<&Player> = pool.iter().map(|&i| tl.p(i)).collect();
    let (_, assign) = dp_idx(&pp, &tl.slots, |j, _| rank[j]);
    let mut chosen: Vec<usize> = assign.iter().map(|&(_, j)| pool[j]).collect();
    chosen.sort_by(|a, b| tl.p(*a).cid.cmp(&tl.p(*b).cid));
    chosen.dedup();
    let total = py_sum(pool.iter().filter(|i| chosen.contains(i)).map(|&i| tl.val(i, k)));
    (total, chosen)
}

fn day_lines(
    plan: &mut WeekPlan,
    ctx: &LeagueContext,
    tl: &Timeline<'_>,
    txs: &[Tx<'_>],
    per_day: &[(f64, Chosen)],
    goalies: &GoalieWeek,
) {
    let mut rows: HashMap<&str, &GoalieRow> = HashMap::new();
    for r in goalies.mine.iter().chain(goalies.free_agents.iter()) {
        rows.insert(r.cid.as_str(), r);
    }
    let base_days = tl.per_day(&[]);
    let quick_edge = QUICK_EDGE_MIN * league_unit(ctx);
    for (i, &d) in tl.days.iter().enumerate() {
        let mut line = PlanDayLine {
            day: d,
            moves: plan.moves.iter().filter(|m| m.day == d).cloned().collect(),
            ins: vec![],
            outs: vec![],
            goalies: vec![],
            pregame: false,
            quick_ok: true,
            pts_plan: round_to(per_day[i].0, 2),
            pts_as_set: round_to(base_days[i].0, 2),
            pts_frozen: round_to(tl.as_set(i), 2),
            note: String::new(),
            text: String::new(),
        };
        let roster = tl.roster(d, txs);
        let chosen: Vec<usize> = per_day[i].1.iter().map(|&c| c as usize).collect();
        let playing: HashSet<usize> = roster.iter().copied().filter(|&c| tl.val(c, i) > 0.0).collect();
        // starts quick lineup would get wrong: only when the best lineup beats it by the scaled QUICK_EDGE_MIN
        let (q_pts, quick) = quick_choice(tl, i, &roster);
        if per_day[i].0 - q_pts >= quick_edge {
            let ins: Vec<usize> = chosen.iter().copied().filter(|c| !quick.contains(c)).collect();
            let mut outs: Vec<usize> = quick.iter().copied().filter(|c| !chosen.contains(c)).collect();
            for (p, mate) in pair_outs(tl, &ins, &outs, &playing) {
                let Some(mate) = mate else {
                    line.ins.push(format!("{} (open slot)", tl.p(p).name));
                    continue;
                };
                line.ins.push(format!(
                    "{} over {} ({} vs {} pts)",
                    tl.p(p).name,
                    tl.p(mate).name,
                    fixed(tl.val(p, i), 1, false),
                    fixed(tl.val(mate, i), 1, false)
                ));
                if let Some(pos) = outs.iter().position(|&x| x == mate) {
                    outs.remove(pos);
                }
            }
            line.outs = outs.iter().map(|&p| tl.p(p).name.clone()).collect();
            line.quick_ok = false;
        }
        // goalies in the plan's lineup that day, with the goalie planner's start probability
        for &c in &chosen {
            let p = tl.p(c);
            if !p.is_goalie() {
                continue;
            }
            let row = rows.get(p.cid.as_str());
            let game = row.and_then(|r| r.games.iter().find(|g| g.day == d));
            match (row, game) {
                (Some(row), Some(game)) => {
                    line.goalies.push(format!("{} {} {}", row.name, percent(game.start_prob, 0, false), game.opponent));
                    if game.start_prob < PREGAME_CONFIRMED {
                        line.pregame = true;
                    }
                }
                _ => {
                    line.goalies.push(p.name.clone());
                    line.pregame = true;
                }
            }
        }
        let quick_base: Vec<usize> = base_days[i].1.iter().map(|&c| c as usize).collect();
        line.note = below_note(plan, ctx, tl, d, &line, &quick_base, &roster);
        line.text = day_text(&line);
        plan.days.push(line);
    }
}

/// `_below_note`: why the plan projects below quick lineup on `d`.
fn below_note(
    plan: &WeekPlan,
    ctx: &LeagueContext,
    tl: &Timeline<'_>,
    d: NaiveDate,
    line: &PlanDayLine,
    quick: &[usize],
    roster: &[usize],
) -> String {
    if line.pts_plan >= line.pts_as_set - 0.05 {
        return String::new();
    }
    let in_quick = |cid: &str| quick.iter().any(|&q| tl.p(q).cid == cid);
    let in_roster = |cid: &str| roster.iter().any(|&q| tl.p(q).cid == cid);
    let gone: Vec<&PlanMove> = plan
        .moves
        .iter()
        .filter(|m| m.effective <= d && m.drop.as_ref().is_some_and(|x| in_quick(&x.cid) && !in_roster(&x.cid)))
        .collect();
    if gone.is_empty() {
        return String::new();
    }
    let when = if d == ctx.as_of { "today".to_string() } else { wd(d) };
    let who = gone
        .iter()
        .map(|m| format!("{} (dropped {})", m.drop.as_ref().map_or("", |x| x.name.as_str()), wd(m.day)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut text = format!("Lower than quick lineup {when}: {who} would have played");
    let mg = plan.moves_gain();
    if mg > 0.0 {
        let mut adds: Vec<&str> = Vec::new();
        for m in &gone {
            if !adds.contains(&m.add.name.as_str()) {
                adds.push(m.add.name.as_str());
            }
        }
        let whose = if adds.len() == 1 {
            format!("{}'s extra nights make", adds[0])
        } else {
            "the pickups' extra nights make".to_string()
        };
        text += &format!("; {whose} up for it ({} for the week)", fixed(mg, 1, true));
    }
    text
}

fn day_text(line: &PlanDayLine) -> String {
    let mut parts: Vec<String> = Vec::new();
    for m in &line.moves {
        let when = if m.effective == m.day { String::new() } else { format!(" (counts from {})", wd(m.effective)) };
        parts.push(format!("{} {}{}{when}, then quick lineup", if m.claim { "Claim" } else { "Add" }, m.add.name, drop_text(m)));
    }
    if !line.ins.is_empty() {
        parts.push(format!("after quick lineup start {}", line.ins.join(", ")));
    }
    if !line.outs.is_empty() {
        parts.push(format!("bench {}", line.outs.join(", ")));
    }
    if !line.goalies.is_empty() {
        parts.push(format!("G: {}", line.goalies.join(", ")));
    }
    if line.moves.is_empty() && line.quick_ok && !line.pregame {
        parts.insert(0, "Quick lineup is fine".to_string());
    }
    let mut text = if parts.is_empty() { "Quick lineup is fine".to_string() } else { parts.join("; ") };
    if line.pregame {
        text += " \u{b7} check goalies at 5 pm";
    }
    if !line.note.is_empty() {
        text += &format!(" \u{b7} {}", line.note);
    }
    text
}

fn assumption_notes(ctx: &LeagueContext) -> Vec<String> {
    let mut out = Vec::new();
    if ctx.provider == "fantrax" {
        let lock = if ctx.lock_per_game { "the player's own game locks" } else { "the day's first game" };
        out.push(format!(
            "Fantrax free-agent adds count the same day when made before {lock} (else from the next day); \
             waiver claims from the day after they clear."
        ));
    }
    out
}

// ------------------------------------------------------------------------------------ archive

/// Write `archive/plan-<league>-<date>.json` (one per day; rewritten when it changes). Returns
/// the write status or None.
pub fn archive_plan(plan: &WeekPlan, data_dir: &Path, league_id: Option<&str>, now: &PyDateTime) -> Option<&'static str> {
    if !plan.available {
        return None;
    }
    let day = plan.days.first().map(|d| d.day).unwrap_or_else(|| to_local(now).date());
    let dir = crate::archive::archive_dir(data_dir).ok()?;
    let path = dir.join(format!("plan-{}-{}.json", plan.league, day.format("%Y-%m-%d")));
    let payload = json!({
        "version": 1,
        "provider": plan.league,
        "league_id": league_id,
        "as_of": day.format("%Y-%m-%d").to_string(),
        "created": utc_seconds(now),
        "plan": plan.to_json(),
    });
    crate::archive::write_dedup(&path, &payload, "plan", Some(1)).ok()
}

/// `now.astimezone(timezone.utc).isoformat(timespec="seconds")` (naive = local time).
pub(crate) fn utc_seconds(now: &PyDateTime) -> String {
    let utc: DateTime<Utc> = match now.to_utc() {
        Some(u) => u,
        None => Local.from_local_datetime(&now.local).earliest().map(|d| d.with_timezone(&Utc)).unwrap_or_else(|| Utc.from_utc_datetime(&now.local)),
    };
    utc.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    //! Port of tests/test_plan.py (intent).
    use super::*;
    use crate::analysis::matchup::PlayerProjection;
    use crate::scoring::PointsScoring;
    use crate::valuation::valuate::valuate_league;

    pub(crate) fn mon() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 5).unwrap()
    }

    pub(crate) fn days7() -> Vec<NaiveDate> {
        (0..7).map(|k| mon() + Duration::days(k)).collect()
    }

    pub(crate) fn now0() -> PyDateTime {
        PyDateTime::from_utc(Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap())
    }

    pub(crate) fn mk(cid: &str, team: &str, goals_pg: f64, status: &str) -> Player {
        serde_json::from_value(json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": team,
            "positions": ["C"], "status": status,
            "lines": {"projected": {"split": "projected", "gp": 80, "stats": {"G": goals_pg * 80.0, "GP": 80.0}},
                      "season": {"split": "season", "gp": 20, "stats": {"G": goals_pg * 20.0, "GP": 20.0}}}}))
        .unwrap()
    }

    pub(crate) fn ctx_for(
        mine: &[(&str, Player)],
        fas: Vec<Player>,
        sched: &[(&str, Vec<NaiveDate>)],
        moves: i64,
        provider: &str,
    ) -> LeagueContext {
        let slots: Vec<RosterSlot> = mine
            .iter()
            .map(|(s, p)| RosterSlot { slot: s.to_string(), player: Some(p.clone()), starting: !["BN", "IR"].contains(s) })
            .collect();
        let mut schedule: Map<String, Vec<NaiveDate>> = Map::new();
        for i in 0..4 {
            schedule.insert(format!("Z{i}"), days7());
        }
        for (t, ds) in sched {
            schedule.insert(t.to_string(), ds.clone());
        }
        let gpd: Map<String, i64> = days7().iter().map(|d| (d.format("%Y-%m-%d").to_string(), 8)).collect();
        let t = FantasyTeam { team_id: "1".into(), name: "me".into(), owner_is_me: true, slots, record: None };
        serde_json::from_value(json!({"provider": provider, "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {"G": 1.0}}, "roster_shape": {"C": 1, "BN": 1, "IR": 1},
            "teams": [t], "free_agents": fas, "matchup_period": 1, "as_of": "2026-10-05", "period_end": "2026-10-11",
            "schedule": schedule, "games_per_day": gpd, "moves_limit_per_period": moves,
            "moves_used_this_period": 0, "lineup_lock": "daily"}))
        .unwrap()
    }

    pub(crate) fn matchup(stance: &str) -> Matchup {
        Matchup {
            league: "test".into(),
            my_team: "me".into(),
            opponent_team: None,
            opponent_id: None,
            period: None,
            start: Some(mon()),
            end: Some(days7()[6]),
            from_day: None,
            days_left: 7,
            playoffs: false,
            my_points_so_far: 0.0,
            their_points_so_far: 0.0,
            my_projected_remaining: 0.0,
            their_projected_remaining: 0.0,
            my_games_left: 0,
            their_games_left: 0,
            my_sd: 0.0,
            their_sd: 0.0,
            win_probability: None,
            stance: Some(stance.into()),
            key_players_theirs: vec![],
            gap_by_position: Map::new(),
            advice: vec![],
            my_players: vec![],
            their_players: Vec::<PlayerProjection>::new(),
            source: String::new(),
            lineup_basis: String::new(),
            warnings: vec![],
            draws: 200,
            sd_model: crate::analysis::matchup::SdModel::REFERENCE,
        }
    }

    pub(crate) fn no_goalies() -> GoalieWeek {
        GoalieWeek { days: vec![], g_slots: 0, mine: vec![], free_agents: vec![], plan: vec![], streams: vec![], notes: vec![] }
    }

    pub(crate) fn valuate(ctx: &LeagueContext) -> Map<Cid, PlayerValue> {
        let mut c = ctx.clone();
        let w: Map<String, f64> = [("G".to_string(), 1.0)].into_iter().collect();
        valuate_league(&mut c, &PointsScoring::new(w, None))
    }

    pub(crate) fn plan_with(ctx: &LeagueContext, values: &Map<Cid, PlayerValue>, stance: &str, data_dir: Option<&Path>) -> WeekPlan {
        let m = matchup(stance);
        let g = no_goalies();
        let opts = PlanOpts { recs: Some(&[]), matchup: Some(&m), goalies: Some(&g), data_dir, ..PlanOpts::default() };
        build_week_plan(ctx, values, &opts, &now0(), &ScheduleMemo::new(None))
    }

    pub(crate) fn plan_for(ctx: &LeagueContext, stance: &str) -> WeekPlan {
        plan_with(ctx, &valuate(ctx), stance, None)
    }

    fn d(k: usize) -> NaiveDate {
        days7()[k]
    }

    fn streaming_ctx(moves: i64, provider: &str) -> LeagueContext {
        let mine = [("C", mk("my_c", "MON", 2.0, "healthy")), ("BN", mk("weak", "NONE", 0.1, "healthy"))];
        let fas = vec![mk("x", "TW", 2.0, "healthy"), mk("y", "FS", 2.0, "healthy")];
        let sched = [("MON", vec![d(0)]), ("TW", days7()[1..3].to_vec()), ("FS", days7()[4..7].to_vec()), ("NONE", vec![])];
        ctx_for(&mine, fas, &sched, moves, provider)
    }

    fn pairs_of_plan(p: &WeekPlan) -> Vec<(String, Option<String>)> {
        p.moves.iter().map(|m| (m.add.cid.clone(), m.drop.as_ref().map(|x| x.cid.clone()))).collect()
    }

    #[test]
    fn sequence_streams_two_players_when_it_beats_one_add() {
        let p = plan_for(&streaming_ctx(2, "test"), "even");
        assert_eq!(pairs_of_plan(&p), vec![("x".into(), Some("weak".into())), ("y".into(), Some("x".into()))]);
        let (first, second) = (&p.moves[0], &p.moves[1]);
        assert_eq!((first.effective, second.effective), (d(1), d(4)));
        assert_eq!(first.starts, days7()[1..3].to_vec());
        assert_eq!(second.starts, days7()[4..7].to_vec());
        let sum: f64 = p.moves.iter().map(|m| m.gain_pts).sum();
        assert!((p.proj_plan - p.proj_lineup - sum).abs() < 0.02);
        assert!((p.moves_gain() - 10.0).abs() <= 0.5, "{}", p.moves_gain());
    }

    #[test]
    fn never_uses_more_moves_than_left() {
        let p = plan_for(&streaming_ctx(1, "test"), "even");
        assert_eq!(p.moves.len(), 1);
        assert_eq!(p.moves[0].add.cid, "y");
        assert!(plan_for(&streaming_ctx(0, "test"), "even").moves.is_empty());
    }

    #[test]
    fn protect_stance_keeps_a_move_in_reserve() {
        let p = plan_for(&streaming_ctx(2, "test"), "protect");
        assert_eq!(p.moves.len(), 1);
        assert!(p.notes.iter().any(|n| n.contains("reserve")));
    }

    #[test]
    fn bench_games_do_not_count() {
        let mine = [("C", mk("my_c", "ALL", 3.0, "healthy")), ("BN", mk("weak", "NONE", 0.1, "healthy"))];
        let ctx = ctx_for(&mine, vec![mk("fa", "ALL", 2.0, "healthy")], &[("ALL", days7()), ("NONE", vec![])], 2, "test");
        let p = plan_for(&ctx, "even");
        assert!(p.moves.is_empty());
        assert!((p.proj_plan - p.proj_lineup).abs() < 1e-9);
    }

    #[test]
    fn injured_player_is_never_the_drop() {
        let mine = [("C", mk("my_c", "MON", 2.0, "healthy")), ("BN", mk("hurt", "NONE", 0.05, "out"))];
        let ctx = ctx_for(
            &mine,
            vec![mk("x", "TW", 2.0, "healthy")],
            &[("MON", vec![d(0)]), ("TW", days7()[1..5].to_vec()), ("NONE", vec![])],
            2,
            "test",
        );
        let p = plan_for(&ctx, "even");
        assert!(p.moves.iter().all(|m| m.drop.as_ref().map_or(true, |x| x.cid != "hurt")));
    }

    #[test]
    fn fantrax_adds_count_the_same_day_before_the_first_game() {
        let p = plan_for(&streaming_ctx(1, "fantrax"), "even");
        assert_eq!(p.moves.len(), 1);
        assert_eq!(p.moves[0].effective, p.moves[0].day);
        assert!(p.notes.iter().any(|n| n.contains("same day")));
    }

    #[test]
    fn fantrax_add_after_todays_first_game_counts_tomorrow() {
        let mut ctx = streaming_ctx(1, "fantrax");
        let first = Utc.with_ymd_and_hms(2026, 10, 5, 23, 0, 0).unwrap();
        let mut gs: Map<String, Map<NaiveDate, PyDateTime>> = Map::new();
        gs.insert("MON".into(), [(mon(), PyDateTime::from_utc(first))].into_iter().collect());
        gs.insert("TW".into(), [(d(1), PyDateTime::from_utc(first + Duration::days(1)))].into_iter().collect());
        ctx.game_starts = gs;
        let fa = ctx.free_agents[0].clone();
        assert_eq!(earliest(&ctx, &fa, &PyDateTime::from_utc(first - Duration::hours(1))), mon());
        assert_eq!(earliest(&ctx, &fa, &PyDateTime::from_utc(first + Duration::minutes(5))), d(1));
    }

    #[test]
    fn fantrax_per_game_lock_add_counts_today_until_his_own_game() {
        let mut ctx = streaming_ctx(1, "fantrax");
        ctx.lock_per_game = true;
        let first = Utc.with_ymd_and_hms(2026, 10, 5, 17, 0, 0).unwrap();
        let mut gs: Map<String, Map<NaiveDate, PyDateTime>> = Map::new();
        gs.insert("MON".into(), [(mon(), PyDateTime::from_utc(first))].into_iter().collect());
        gs.insert("TW".into(), [(mon(), PyDateTime::from_utc(first + Duration::hours(6)))].into_iter().collect());
        ctx.game_starts = gs;
        let fa = ctx.free_agents[0].clone();
        // the day's first game has started but his own has not: the add still counts today
        assert_eq!(earliest(&ctx, &fa, &PyDateTime::from_utc(first + Duration::hours(1))), mon());
        assert_eq!(earliest(&ctx, &fa, &PyDateTime::from_utc(first + Duration::hours(7))), d(1));
    }

    #[test]
    fn day_lines_and_archive() {
        let p = plan_for(&streaming_ctx(2, "test"), "even");
        assert_eq!(p.days.len(), 7);
        let tue = &p.days[1];
        assert!(!tue.moves.is_empty() && tue.text.contains("Add x, drop weak, then quick lineup"), "{}", tue.text);
        assert_eq!(p.days[2].text, "Quick lineup is fine");
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(archive_plan(&p, dir.path(), Some("1"), &now0()), Some("created"));
        assert_eq!(archive_plan(&p, dir.path(), Some("1"), &now0()), Some("unchanged"));
        let n = std::fs::read_dir(dir.path().join("archive"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("plan-test-"))
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn weekly_lock_leagues_get_no_plan() {
        let mut ctx = streaming_ctx(2, "test");
        ctx.provider = "fantrax".into();
        ctx.lineup_lock = Some(serde_json::from_value(json!("weekly")).unwrap());
        let p = plan_for(&ctx, "even");
        assert!(!p.available && !p.notes.is_empty());
    }

    #[test]
    fn restreamed_pickup_is_credited_only_with_the_nights_it_keeps() {
        let mine = [("C", mk("my_c", "MON", 2.0, "healthy")), ("BN", mk("weak", "NONE", 0.1, "healthy"))];
        let sched = [("MON", vec![d(0)]), ("TWS", vec![d(1), d(2), d(5)]), ("TS", days7()[3..7].to_vec()), ("NONE", vec![])];
        let ctx = ctx_for(&mine, vec![mk("x", "TWS", 2.0, "healthy"), mk("y", "TS", 2.0, "healthy")], &sched, 2, "test");
        let p = plan_for(&ctx, "even");
        assert_eq!(pairs_of_plan(&p), vec![("x".into(), Some("weak".into())), ("y".into(), Some("x".into()))]);
        let (x, y) = (&p.moves[0], &p.moves[1]);
        assert_eq!(x.starts, days7()[1..3].to_vec());
        assert_eq!(y.starts, days7()[3..7].to_vec());
        for m in &p.moves {
            let want = 2.0 * m.starts.len() as f64;
            assert!((m.gain_pts - want).abs() <= 0.05 * want, "{} vs {want}", m.gain_pts);
        }
        assert!((p.proj_plan - p.proj_lineup - (x.gain_pts + y.gain_pts)).abs() < 0.02);
        assert!(y.headline.contains("x would have started 1 of his 1 game left (Sat)"), "{}", y.headline);
    }

    #[test]
    fn baseline_is_quick_lineup_and_no_lineup_setting_step() {
        let p = plan_for(&streaming_ctx(2, "test"), "even");
        assert_eq!(p.baseline, "quick_lineup");
        assert_eq!(p.proj_as_set, p.proj_lineup);
        assert_eq!(p.win_as_set, p.win_lineup);
        assert_eq!(p.lineup_gain(), 0.0);
        assert!(p.proj_frozen <= p.proj_lineup);
        assert!(p.today.iter().all(|a| !a.title.to_lowercase().contains("best lineup each day")));
        let s: f64 = p.days.iter().map(|d| d.pts_as_set).sum();
        assert!((s - p.proj_lineup).abs() < 0.05);
    }

    #[test]
    fn day_line_calls_out_a_start_quick_lineup_would_miss() {
        let mine = [("C", mk("a", "ALL", 1.0, "healthy")), ("BN", mk("b", "ALL", 2.0, "healthy"))];
        let ctx = ctx_for(&mine, vec![], &[("ALL", days7())], 0, "test");
        let mut values = valuate(&ctx);
        values.get_mut("a").unwrap().fpg_season = 9.0;
        let p = plan_with(&ctx, &values, "even", None);
        let line = &p.days[1];
        assert!(!line.quick_ok && line.ins.first().is_some_and(|s| s.starts_with("b over a")), "{:?}", line.ins);
        assert!(line.text.contains("after quick lineup start b over a"));
    }

    #[test]
    fn restream_chain_story_and_long_term_against_the_actual_drop() {
        let p = plan_for(&streaming_ctx(2, "test"), "even");
        let (first, second) = (&p.moves[0], &p.moves[1]);
        assert_eq!((first.role.as_deref(), second.role.as_deref()), (Some("rental"), Some("swap")));
        assert_eq!((first.chain, second.chain), (Some(1), Some(1)));
        assert!(first.note.starts_with("Rent x Tue-Wed: 2 nights your C slot would otherwise miss"), "{}", first.note);
        assert!(second.note.starts_with("Then swap x for y Fri: x has no games left"), "{}", second.note);
        assert!(second.note.contains("last move this week"));
        assert!(second.reasons[0].starts_with("Rest of season: y") && second.reasons[0].contains("vs x "), "{:?}", second.reasons);
        assert!(!second.reasons.join(" ").contains("vs weak"));
        assert!(p.net_week[0].starts_with("Net this week: weak gone; y stays; x rented and released"), "{}", p.net_week[0]);
    }

    fn starts_at(sched: &Map<String, Vec<NaiveDate>>) -> Map<String, Map<NaiveDate, PyDateTime>> {
        sched
            .iter()
            .map(|(t, ds)| {
                let m = ds
                    .iter()
                    .map(|d| {
                        let at = Utc.with_ymd_and_hms(d.year(), d.month(), d.day(), 0, 0, 0).unwrap() + Duration::days(1) + Duration::hours(1);
                        (*d, PyDateTime::from_utc(at))
                    })
                    .collect();
                (t.clone(), m)
            })
            .collect()
    }

    #[test]
    fn move_timing_evening_before_and_before_first_game() {
        ZONE_OVERRIDE.with(|z| z.set(Some(-6 * 3600)));
        let mine = [("C", mk("my_c", "MON", 2.0, "healthy")), ("BN", mk("weak", "NONE", 0.1, "healthy"))];
        let sched = [("MON", vec![d(0)]), ("TWT", days7()[1..4].to_vec()), ("FS", days7()[4..7].to_vec()), ("NONE", vec![])];
        let mut ctx = ctx_for(&mine, vec![mk("x", "TWT", 2.0, "healthy"), mk("y", "FS", 2.0, "healthy")], &sched, 2, "test");
        ctx.game_starts = starts_at(&ctx.schedule);
        let p = plan_for(&ctx, "even");
        let (first, second) = (&p.moves[0], &p.moves[1]);
        assert_eq!(first.timing, "Make this move before Tue's first game (7:00 PM your time)");
        assert_eq!((second.effective, second.day), (d(4), d(3)));
        assert_eq!(
            second.timing,
            "Make this move the evening before (after x's game starts Thu at 7:00 PM your time), so he keeps those points and y counts Fri"
        );
        assert!(p.days[3].moves.iter().any(|m| m.day == d(3)));
        ZONE_OVERRIDE.with(|z| z.set(None));
    }

    #[test]
    fn clock_formats_twelve_hour_time() {
        ZONE_OVERRIDE.with(|z| z.set(Some(-6 * 3600)));
        assert_eq!(clock(&PyDateTime::from_utc(Utc.with_ymd_and_hms(2026, 10, 31, 17, 0, 0).unwrap())), "11:00 AM");
        assert_eq!(clock(&PyDateTime::from_utc(Utc.with_ymd_and_hms(2026, 10, 31, 18, 5, 0).unwrap())), "12:05 PM");
        assert_eq!(clock(&PyDateTime::from_utc(Utc.with_ymd_and_hms(2026, 10, 31, 6, 0, 0).unwrap())), "12:00 AM");
        ZONE_OVERRIDE.with(|z| z.set(None));
        // a named zone uses that instant's offset (Mountain time falls back on Nov 1 2026)
        let tz: chrono_tz::Tz = "America/Denver".parse().unwrap();
        let a = Utc.with_ymd_and_hms(2026, 10, 31, 17, 0, 0).unwrap().with_timezone(&tz);
        let b = Utc.with_ymd_and_hms(2026, 11, 1, 18, 0, 0).unwrap().with_timezone(&tz);
        assert_eq!((a.hour(), b.hour()), (11, 11));
    }

    #[test]
    fn day_note_when_the_plan_is_below_quick_lineup() {
        let mine = [("C", mk("my_c", "MON", 2.0, "healthy")), ("BN", mk("weak", "THU", 0.5, "healthy"))];
        let sched = [("MON", vec![d(0)]), ("THU", vec![d(3)]), ("X", vec![d(1), d(2), d(4), d(5)])];
        let p = plan_for(&ctx_for(&mine, vec![mk("x", "X", 2.0, "healthy")], &sched, 1, "test"), "even");
        assert_eq!(pairs_of_plan(&p), vec![("x".into(), Some("weak".into()))]);
        let thu = &p.days[3];
        assert!(thu.pts_plan < thu.pts_as_set);
        assert_eq!(
            thu.note,
            format!(
                "Lower than quick lineup Thu: weak (dropped Tue) would have played; x's extra nights make up for it ({} for the week)",
                fixed(p.moves_gain(), 1, true)
            )
        );
        assert!(thu.text.contains(&thu.note));
        assert!(p.days.iter().all(|x| x.day == d(3) || x.note.is_empty()));
    }

    #[test]
    fn restream_must_stay_within_long_term_tolerance() {
        let mine = [("C", mk("my_c", "MON", 2.0, "healthy")), ("BN", mk("weak", "NONE", 0.1, "healthy"))];
        let sched = [("MON", vec![d(0)]), ("TW", days7()[1..3].to_vec()), ("FS", days7()[4..7].to_vec()), ("NONE", vec![])];
        let p = plan_for(&ctx_for(&mine, vec![mk("x", "TW", 2.0, "healthy"), mk("y", "FS", 0.5, "healthy")], &sched, 2, "test"), "even");
        assert!(p.moves.iter().all(|m| m.drop.as_ref().map_or(true, |x| x.cid != "x")));
    }
}
