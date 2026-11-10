//! Rust port of `fantasy_manager/recommend/base.py`: shared helpers for the recommendation
//! engines (confidence, roster shape / IR / position limits, keeper-league dynasty math, move
//! budget and churn guard, game rate and gain units).
//!
//! Owner: work package `W5-recommend-base`.
//!
//! Deviations from Python (all behaviour-preserving):
//! * Python accepts `DynastyValue` objects *or* plain floats as `dyn`; here `dyn` is always
//!   `Option<&Map<Cid, DynastyValue>>` (float maps stay separate, see [`dynasty_floats`]).
//! * Default arguments are explicit parameters (`recently_added(.., RECENT_ADD_DAYS)`).
//! * `dynasty_objects` cannot fail (no warning path): `apply_dynasty` does not raise in Rust.
//! * `position_room` is split into [`position_room`] (a `Player`) and [`position_room_for`]
//!   (a list of position names); `NO_LIMIT` is `i64`.
//! * Hot-loop helpers take slices / iterators of `&Player`; `DynastyChange` borrows its players.
//! * `count_adds` / `recent_adds_from` / `RECENT_ADDS_WINDOW` live in `fm_core::activity`
//!   (re-exported here).

use std::borrow::Cow;

use chrono::{Datelike, Duration, NaiveDate};
use fm_core::models::{FantasyTeam, LeagueContext, Player, Reason, Status};
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::{Cid, CoreError, Map};
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::py_sum;
use fm_pycompat::order::cmp_f64;
use fm_pycompat::time::strftime_date;

use crate::valuation::adjust::SeasonLength;
use crate::valuation::dynasty::{apply_dynasty, keeper_total};
use crate::valuation::replacement::{eligible_slots, DEFAULT_SLOTS};

pub use fm_core::activity::{count_adds, recent_adds_from, RECENT_ADDS_WINDOW};

// Confidence = how much to trust a player's projection, from the NHL games behind it: this
// season's GP plus the prior three seasons weighted 1 / 0.8 / 0.6 (the baseline's 5/4/3).
pub const MIN_CONFIDENCE: f64 = 0.5;
pub const CONFIDENCE_K: f64 = 40.0;
pub const PRIOR_GP_WEIGHTS: [(&str, f64); 3] = [("prior", 1.0), ("prior2", 0.8), ("prior3", 0.6)];

/// NHL games behind the projection: this season's GP + weighted GP of the prior three.
pub fn evidence_games(player: &Player) -> f64 {
    let rest = py_sum(PRIOR_GP_WEIGHTS.iter().map(|(split, w)| w * player.gp(split) as f64));
    player.gp("season") as f64 + rest
}

/// `MIN_CONFIDENCE + (1 - MIN_CONFIDENCE) x E / (E + CONFIDENCE_K)`, E = [`evidence_games`].
pub fn confidence(player: &Player) -> f64 {
    let e = evidence_games(player);
    MIN_CONFIDENCE + (1.0 - MIN_CONFIDENCE) * e / (e + CONFIDENCE_K)
}

/// Rostered players outside IR slots.
pub fn droppable_players(team: &FantasyTeam) -> Vec<&Player> {
    team.slots.iter().filter(|s| s.slot != "IR").filter_map(|s| s.player.as_ref()).collect()
}

/// Do `a` and `b` share an eligible lineup slot (Python default `slots=DEFAULT_SLOTS`)?
pub fn shares_slot(a: &Player, b: &Player, slots: &[&str]) -> bool {
    let ea = eligible_slots(&a.positions, slots);
    let eb = eligible_slots(&b.positions, slots);
    ea.iter().any(|x| eb.contains(x))
}

/// [`shares_slot`] with `DEFAULT_SLOTS`.
pub fn shares_default_slot(a: &Player, b: &Player) -> bool {
    shares_slot(a, b, &DEFAULT_SLOTS)
}

pub const IR_SLOT_NAMES: [&str; 2] = ["IR", "IR+"];
/// most severe first
pub const IR_MOVE_STATUSES: [&str; 2] = ["ltir", "ir"];

/// IR slots in the league's roster shape not yet occupied on `team`.
pub fn free_ir_slots(team: &FantasyTeam, roster_shape: &Map<String, i64>) -> i64 {
    let cap: i64 = IR_SLOT_NAMES.iter().map(|s| roster_shape.get(*s).copied().unwrap_or(0)).sum();
    let used = team.slots.iter().filter(|s| s.player.is_some() && IR_SLOT_NAMES.contains(&s.slot.as_str())).count() as i64;
    (cap - used).max(0)
}

/// Players in active/bench slots whose status (IR/LTIR) lets them move to an IR slot, most severe
/// first, then most valuable first.
pub fn ir_movable<'a>(team: &'a FantasyTeam, values: Option<&Map<Cid, PlayerValue>>) -> Vec<&'a Player> {
    let fpg = |p: &Player| values.and_then(|v| v.get(&p.cid)).map(|pv| pv.fpg).unwrap_or(0.0);
    let idx = |p: &Player| IR_MOVE_STATUSES.iter().position(|s| *s == p.status.as_str()).unwrap_or(usize::MAX);
    let mut out: Vec<&Player> = team
        .slots
        .iter()
        .filter(|s| !IR_SLOT_NAMES.contains(&s.slot.as_str()))
        .filter_map(|s| s.player.as_ref())
        .filter(|p| matches!(p.status, Status::Ltir | Status::Ir))
        .collect();
    out.sort_by(|a, b| idx(a).cmp(&idx(b)).then_with(|| cmp_f64(-fpg(a), -fpg(b))));
    out
}

/// The dynasty values as given in dynasty leagues, computed from `values` when not supplied;
/// None outside dynasty leagues. Keeper-league math ([`dynasty_change`]) needs these objects.
pub fn dynasty_objects<'a>(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&'a Map<Cid, DynastyValue>>,
) -> Option<Cow<'a, Map<Cid, DynastyValue>>> {
    if !ctx.dynasty {
        return None;
    }
    Some(match dynasty_values {
        Some(d) => Cow::Borrowed(d),
        None => Cow::Owned(apply_dynasty(values, ctx, None, None)),
    })
}

/// {cid: value} from DynastyValue objects (None when empty).
pub fn dynasty_floats(dynasty_values: Option<&Map<Cid, DynastyValue>>) -> Option<Map<Cid, f64>> {
    let out: Map<Cid, f64> = dynasty_values?.iter().map(|(c, dv)| (c.clone(), dv.value)).collect();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// {cid: dynasty value} in dynasty leagues (computed from `values` when not supplied), else None.
pub fn dynasty_map(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
) -> Option<Map<Cid, f64>> {
    dynasty_floats(dynasty_objects(ctx, values, dynasty_values).as_deref())
}

// -- keeper leagues (LeagueContext.keepers_per_team) ----------------------------------------
//
// Each team keeps K players into next season (free, no year limit); everyone else is redrafted.
// The exact dynasty-unit change of a roster move for a team is
//     sum(season_part, after) - sum(season_part, before) + keeper_total(after) - keeper_total(before)
// In keep-everyone dynasties (keepers_per_team None) the change stays the plain sum of values.

/// A team's dynasty-unit change from roster `before` to `after` ([`dynasty_change`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DynastyChange<'a> {
    /// what recommenders use (= season + keepers in keeper leagues)
    pub total: f64,
    /// keeper leagues: this season's part (sum of season_part)
    pub season: f64,
    /// keeper leagues: keeper_total(after) - keeper_total(before)
    pub keepers: f64,
    /// keeper leagues: players who join the top-K keepers
    pub kept_in: Vec<&'a Player>,
    /// ... and who leave them (traded / dropped / displaced)
    pub kept_out: Vec<&'a Player>,
    /// keepers per team; None = keep-everyone (additive)
    pub k: Option<i64>,
}

impl<'a> DynastyChange<'a> {
    pub fn plain(total: f64) -> Self {
        DynastyChange { total, ..Default::default() }
    }
}

/// Keepers per team when keeper-league math applies: a dynasty league with
/// `ctx.keepers_per_team` set and, when `dyn_` is given, non-empty dynasty values (which carry the
/// keeper fields). None = keep-everyone dynasty (or not a dynasty league).
pub fn keeper_k(ctx: &LeagueContext, dyn_: Option<&Map<Cid, DynastyValue>>) -> Option<i64> {
    let k = ctx.keepers_per_team?;
    if !ctx.dynasty {
        return None;
    }
    if dyn_.is_some_and(|d| d.is_empty()) {
        return None;
    }
    Some(k.max(0))
}

fn dyn_v(dyn_: Option<&Map<Cid, DynastyValue>>, cid: &str) -> f64 {
    dyn_.and_then(|d| d.get(cid)).map(|dv| dv.value).unwrap_or(0.0)
}

fn season_part(dyn_: &Map<Cid, DynastyValue>, cid: &str) -> f64 {
    dyn_.get(cid).map(|dv| dv.season_part).unwrap_or(0.0)
}

fn surplus(dyn_: &Map<Cid, DynastyValue>, cid: &str) -> f64 {
    dyn_.get(cid).map(|dv| dv.keeper_surplus).unwrap_or(0.0)
}

/// The keepers a team would choose: its top `k` players by keeper_surplus (positive surplus
/// only), best first; `keeper_total` sums their surpluses.
pub fn keeper_set<'a>(
    players: impl IntoIterator<Item = &'a Player>,
    dyn_: &Map<Cid, DynastyValue>,
    k: i64,
) -> Vec<&'a Player> {
    let mut ranked: Vec<&Player> = players.into_iter().filter(|p| surplus(dyn_, &p.cid) > 0.0).collect();
    ranked.sort_by(|a, b| cmp_f64(surplus(dyn_, &b.cid), surplus(dyn_, &a.cid)));
    ranked.truncate(k.max(0) as usize);
    ranked
}

/// `{p.cid: p for p in players}` as a Vec: first position of a cid, last value wins.
fn uniq<'a>(players: impl IntoIterator<Item = &'a Player>) -> Vec<&'a Player> {
    let mut out: Vec<&Player> = Vec::new();
    for p in players {
        match out.iter().position(|q| q.cid == p.cid) {
            Some(i) => out[i] = p,
            None => out.push(p),
        }
    }
    out
}

/// Dynasty-unit change for a team going from roster `before` to `after` (IR players included in
/// both). Keeper leagues ([`keeper_k`]): the season_part change of the players who come and go plus
/// keeper_total(after) - keeper_total(before), with the keeper set's change. Keep-everyone:
/// V(added) - V(removed), the plain additive change.
pub fn dynasty_change<'a>(
    ctx: &LeagueContext,
    dyn_: Option<&Map<Cid, DynastyValue>>,
    before: impl IntoIterator<Item = &'a Player>,
    after: impl IntoIterator<Item = &'a Player>,
) -> DynastyChange<'a> {
    let b = uniq(before);
    let a = uniq(after);
    let added: Vec<&Player> = a.iter().copied().filter(|p| !b.iter().any(|q| q.cid == p.cid)).collect();
    let removed: Vec<&Player> = b.iter().copied().filter(|p| !a.iter().any(|q| q.cid == p.cid)).collect();
    let dyn_ = dyn_.filter(|d| !d.is_empty());
    let k = dyn_.and_then(|d| keeper_k(ctx, Some(d)));
    let (Some(k), Some(d)) = (k, dyn_) else {
        return DynastyChange::plain(
            py_sum(added.iter().map(|p| dyn_v(dyn_, &p.cid))) - py_sum(removed.iter().map(|p| dyn_v(dyn_, &p.cid))),
        );
    };
    let season = py_sum(added.iter().map(|p| season_part(d, &p.cid))) - py_sum(removed.iter().map(|p| season_part(d, &p.cid)));
    let keep = keeper_total(a.iter().copied(), d, Some(k)) - keeper_total(b.iter().copied(), d, Some(k));
    let kb = keeper_set(b.iter().copied(), d, k);
    let ka = keeper_set(a.iter().copied(), d, k);
    let kept_in: Vec<&Player> = ka.iter().copied().filter(|p| !kb.iter().any(|q| q.cid == p.cid)).collect();
    let kept_out: Vec<&Player> = kb.iter().copied().filter(|p| !ka.iter().any(|q| q.cid == p.cid)).collect();
    DynastyChange { total: season + keep, season, keepers: keep, kept_in, kept_out, k: Some(k) }
}

/// `dynasty_change(...).total`: the dynasty-unit change of a roster move for one team.
pub fn dynasty_delta<'a>(
    ctx: &LeagueContext,
    dyn_: Option<&Map<Cid, DynastyValue>>,
    before: impl IntoIterator<Item = &'a Player>,
    after: impl IntoIterator<Item = &'a Player>,
) -> f64 {
    dynasty_change(ctx, dyn_, before, after).total
}

/// (add value, drop value) in dynasty units for swapping `drop` for `add` on `roster` (my
/// players, IR included). Keep-everyone: (V(add), V(drop)). Keeper leagues: what `drop`
/// contributes to the roster and what `add` contributes without `drop`, so add - drop is the
/// swap's exact `dynasty_delta`. A `drop` not on `roster` is valued as if he were.
pub fn swap_values<'a>(
    ctx: &LeagueContext,
    dyn_: Option<&Map<Cid, DynastyValue>>,
    roster: impl IntoIterator<Item = &'a Player>,
    add: Option<&'a Player>,
    drop: Option<&'a Player>,
) -> (f64, f64) {
    let dyn_ = dyn_.filter(|d| !d.is_empty());
    if keeper_k(ctx, dyn_).is_none() || dyn_.is_none() {
        return (
            add.map_or(0.0, |p| dyn_v(dyn_, &p.cid)),
            drop.map_or(0.0, |p| dyn_v(dyn_, &p.cid)),
        );
    }
    let base: Vec<&Player> = roster
        .into_iter()
        .filter(|p| !add.is_some_and(|x| x.cid == p.cid) && !drop.is_some_and(|x| x.cid == p.cid))
        .collect();
    let with = |x: Option<&'a Player>| match x {
        Some(x) => dynasty_delta(ctx, dyn_, base.iter().copied(), base.iter().copied().chain(std::iter::once(x))),
        None => 0.0,
    };
    (with(add), with(drop))
}

/// "Bouchard replaces Sanderson as a keeper" / "Bouchard becomes a keeper" / "Sanderson is no
/// longer a keeper" / "keepers unchanged" for a keeper-league DynastyChange.
pub fn keepers_text(ch: &DynastyChange<'_>) -> String {
    let names = |ps: &[&Player]| ps.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(" + ");
    if !ch.kept_in.is_empty() && !ch.kept_out.is_empty() {
        let one = ch.kept_in.len() == 1;
        format!(
            "{} replace{} {} as {}",
            names(&ch.kept_in),
            if one { "s" } else { "" },
            names(&ch.kept_out),
            if one { "a keeper" } else { "keepers" }
        )
    } else if !ch.kept_in.is_empty() {
        let one = ch.kept_in.len() == 1;
        format!("{} become{}", names(&ch.kept_in), if one { "s a keeper" } else { " keepers" })
    } else if !ch.kept_out.is_empty() {
        let one = ch.kept_out.len() == 1;
        format!(
            "{} {} no longer {}",
            names(&ch.kept_out),
            if one { "is" } else { "are" },
            if one { "a keeper" } else { "keepers" }
        )
    } else {
        "keepers unchanged".to_string()
    }
}

/// Empty non-IR roster spots (starting + bench/reserve) on `team`.
pub fn open_roster_spots(team: &FantasyTeam, roster_shape: &Map<String, i64>) -> i64 {
    let cap: i64 = roster_shape.iter().filter(|(s, _)| !IR_SLOT_NAMES.contains(&s.as_str())).map(|(_, n)| *n).sum();
    let used = team.slots.iter().filter(|s| s.player.is_some() && !IR_SLOT_NAMES.contains(&s.slot.as_str())).count() as i64;
    (cap - used).max(0)
}

// -- per-position roster maximums (LeagueContext.position_limits / max_roster_size) ----------

pub const NO_LIMIT: i64 = 1_000_000;
pub const FORWARD_POSITIONS: [&str; 3] = ["C", "LW", "RW"];
/// Slots that do not count toward `LeagueContext.max_roster_size` (IR and minors / not-active).
pub const SIZE_EXEMPT_SLOTS: [&str; 4] = ["IR", "IR+", "MIN", "NA"];

pub fn position_plural(key: &str) -> Option<&'static str> {
    Some(match key {
        "C" => "centers",
        "LW" => "left wings",
        "RW" => "right wings",
        "F" => "forwards",
        "D" => "defensemen",
        "G" => "goalies",
        _ => return None,
    })
}

/// The capped positions `p` counts toward (keys of `limits`): his platform default position when
/// the provider reports one, else every position he is eligible at. Any forward (C/LW/RW) also
/// counts toward an "F" limit. In `limits` order.
pub fn limit_positions<'a>(p: &Player, limits: &'a Map<String, i64>) -> Vec<&'a str> {
    let primary = p.primary_position.as_deref().filter(|s| !s.is_empty());
    let has = |x: &str| match primary {
        Some(pr) => pr == x,
        None => p.positions.iter().any(|q| q == x),
    };
    let fwd = FORWARD_POSITIONS.iter().any(|x| has(x));
    limits.keys().map(|k| k.as_str()).filter(|k| has(k) || (fwd && *k == "F")).collect()
}

fn counts_for<'a>(players: impl Iterator<Item = &'a Player>, limits: &Map<String, i64>) -> Vec<i64> {
    let mut out = vec![0i64; limits.len()];
    for p in players {
        for k in limit_positions(p, limits) {
            if let Some(i) = limits.get_index_of(k) {
                out[i] += 1;
            }
        }
    }
    out
}

/// {capped position: players on `team` (every slot, IR included) counting toward it}.
pub fn position_counts(team: &FantasyTeam, ctx: &LeagueContext) -> Map<String, i64> {
    let c = counts_for(team.players_iter(), &ctx.position_limits);
    ctx.position_limits.keys().cloned().zip(c).collect()
}

fn room_for_keys(team: &FantasyTeam, ctx: &LeagueContext, keys: &[&str]) -> i64 {
    if keys.is_empty() {
        return NO_LIMIT;
    }
    let counts = counts_for(team.players_iter(), &ctx.position_limits);
    let m = keys
        .iter()
        .filter_map(|k| ctx.position_limits.get_full(*k))
        .map(|(i, _, lim)| *lim - counts[i])
        .min();
    m.map_or(NO_LIMIT, |m| m.max(0))
}

/// How many more players like `p` (see [`limit_positions`]) `team` can add before hitting a
/// per-position roster maximum; `NO_LIMIT` when none applies.
pub fn position_room(team: &FantasyTeam, ctx: &LeagueContext, p: &Player) -> i64 {
    let keys = limit_positions(p, &ctx.position_limits);
    room_for_keys(team, ctx, &keys)
}

/// [`position_room`] for a list of position names.
pub fn position_room_for(team: &FantasyTeam, ctx: &LeagueContext, positions: &[&str]) -> i64 {
    let fwd = positions.iter().any(|p| FORWARD_POSITIONS.contains(p));
    let keys: Vec<&str> = ctx
        .position_limits
        .keys()
        .map(|k| k.as_str())
        .filter(|k| positions.contains(k) || (fwd && *k == "F"))
        .collect();
    room_for_keys(team, ctx, &keys)
}

/// The capped positions of `p` at which `team` is already full (sorted).
pub fn capped_positions<'a>(team: &FantasyTeam, ctx: &'a LeagueContext, p: &Player) -> Vec<&'a str> {
    let limits = &ctx.position_limits;
    let counts = counts_for(team.players_iter(), limits);
    let mut out: Vec<&str> = limit_positions(p, limits)
        .into_iter()
        .filter(|k| limits.get_full(*k).is_some_and(|(i, _, lim)| counts[i] >= *lim))
        .collect();
    out.sort();
    out
}

/// "G limit 3" (+ ": would roster 4 goalies" when `n` is given).
pub fn cap_text(ctx: &LeagueContext, key: &str, n: Option<i64>) -> String {
    let lim = ctx.position_limits.get(key).map_or("None".to_string(), |v| v.to_string());
    let txt = format!("{key} limit {lim}");
    match n {
        Some(n) => format!("{txt}: would roster {n} {}", position_plural(key).unwrap_or(key)),
        None => txt,
    }
}

/// (ok, reason): is `team` within the league's per-position maximums and maximum roster size
/// after adding `add`, dropping `drop` and moving `to_ir` into IR slots? A limit the roster
/// already exceeds only fails when the move makes it worse. Position limits count every rostered
/// player (IR included); the roster size excludes IR / minors slots.
pub fn roster_legal_after(
    team: &FantasyTeam,
    ctx: &LeagueContext,
    add: &[&Player],
    drop: &[&Player],
    to_ir: &[&Player],
) -> (bool, Option<String>) {
    let limits = &ctx.position_limits;
    let have = |cid: &str| team.players_iter().any(|p| p.cid == cid);
    let gone = |cid: &str| drop.iter().any(|p| p.cid == cid);
    let new: Vec<&Player> = add.iter().copied().filter(|p| !have(&p.cid)).collect();
    if !limits.is_empty() {
        let before = counts_for(team.players_iter(), limits);
        let after = counts_for(team.players_iter().filter(|p| !gone(&p.cid)).chain(new.iter().copied()), limits);
        let mut order: Vec<usize> = (0..limits.len()).collect();
        order.sort_by(|a, b| limits.get_index(*a).unwrap().0.cmp(limits.get_index(*b).unwrap().0));
        for i in order {
            let (k, lim) = limits.get_index(i).unwrap();
            if after[i] > *lim && after[i] > before[i] {
                return (false, Some(cap_text(ctx, k, Some(after[i]))));
            }
        }
    }
    if let Some(cap) = ctx.max_roster_size.filter(|c| *c != 0) {
        let moved = |cid: &str| to_ir.iter().any(|p| p.cid == cid);
        let counted = |s: &&fm_core::models::RosterSlot| s.player.is_some() && !SIZE_EXEMPT_SLOTS.contains(&s.slot.as_str());
        let size_before = team.slots.iter().filter(counted).count() as i64;
        let size_after = team
            .slots
            .iter()
            .filter(counted)
            .filter(|s| s.player.as_ref().is_some_and(|p| !gone(&p.cid) && !moved(&p.cid)))
            .count() as i64
            + new.len() as i64;
        if size_after > cap && size_after > size_before {
            return (false, Some(format!("roster limit {cap}: would roster {size_after} players")));
        }
    }
    (true, None)
}

/// "max G 3 (incl. IR); roster max 22 (excl. IR)" for settings / footers, or None when nothing
/// is known.
pub fn position_limits_text(ctx: &LeagueContext) -> Option<String> {
    let order = |k: &str| ["C", "LW", "RW", "F", "D", "G"].iter().position(|x| *x == k).unwrap_or(99);
    let mut parts: Vec<String> = Vec::new();
    if !ctx.position_limits.is_empty() {
        let mut items: Vec<(&String, &i64)> = ctx.position_limits.iter().collect();
        items.sort_by_key(|(k, _)| order(k));
        let body = items.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ");
        parts.push(format!("max {body} (incl. IR)"));
    }
    if let Some(n) = ctx.max_roster_size.filter(|n| *n != 0) {
        parts.push(format!("roster max {n} (excl. IR)"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

/// False when valuation found no stats at all (NO_DATA): the value is unknown, not zero
/// (typically a prospect), so dynasty drop logic must not treat it as the weakest asset.
pub fn has_data(pv: Option<&PlayerValue>) -> bool {
    pv.is_some_and(|pv| !pv.reasons.iter().any(|r| r.code == "NO_DATA"))
}

// -- dynasty prospect protection ----------------------------------------------------

pub const PROTECT_MAX_AGE: f64 = 23.0;
pub const PROTECT_MAX_PICK: i64 = 32;
pub const PROTECT_MIN_OWNED: f64 = 80.0;
/// career NHL GP below this = still unproven
pub const PROTECT_UNPROVEN_GP: i64 = 82;
/// ... and rostered at least this widely
pub const PROTECT_UNPROVEN_OWNED: f64 = 60.0;
/// dynasty(add) must be at least this multiple of dynasty(drop)
pub const PROTECT_OVERRIDE: f64 = 1.5;

/// Age from the dynasty value object (which includes league-reported ages), else the birth date.
pub fn player_age(p: &Player, ctx: &LeagueContext, dynasty_values: Option<&Map<Cid, DynastyValue>>) -> Option<f64> {
    if let Some(age) = dynasty_values.and_then(|d| d.get(&p.cid)).and_then(|dv| dv.age) {
        return Some(age);
    }
    p.birth_date.map(|b| (ctx.as_of - b).num_days() as f64 / 365.25)
}

/// Why `p` is a protected young dynasty asset, or None. Protected = age <= 23 AND (a top-32 pick,
/// OR rostered in >= 80% of leagues, OR unproven (< 82 career NHL GP) and rostered in >= 60%).
pub fn protection_reason(p: &Player, age: Option<f64>) -> Option<String> {
    let age = age?;
    if age > PROTECT_MAX_AGE {
        return None;
    }
    let a0 = fixed(age, 0, false);
    if let Some(pick) = p.draft_overall.filter(|d| *d <= PROTECT_MAX_PICK) {
        return Some(format!("{} is {a0} and a #{pick} overall pick", p.name));
    }
    let owned = p.pct_owned?;
    let o0 = fixed(owned, 0, false);
    if owned >= PROTECT_MIN_OWNED {
        return Some(format!("{} is {a0} and rostered in {o0}% of leagues", p.name));
    }
    if owned >= PROTECT_UNPROVEN_OWNED {
        if let Some(gp) = p.career_gp.filter(|g| *g < PROTECT_UNPROVEN_GP) {
            return Some(format!("{} is {a0}, {gp} NHL GP and rostered in {o0}% of leagues", p.name));
        }
    }
    None
}

/// [`protection_reason`] (age from `dynasty_values`, see [`player_age`]) and, in keeper leagues
/// ([`keeper_k`]), only for a keeper on his team (`DynastyValue.keeper` in `dyn_`, default
/// `dynasty_values`). A player without a dynasty value keeps the plain rule.
pub fn protected_reason(
    p: &Player,
    ctx: &LeagueContext,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    dyn_: Option<&Map<Cid, DynastyValue>>,
) -> Option<String> {
    let why = protection_reason(p, player_age(p, ctx, dynasty_values))?;
    let dyn_ = dyn_.or(dynasty_values).filter(|d| !d.is_empty());
    let Some(k) = dyn_.and_then(|d| keeper_k(ctx, Some(d))) else {
        return Some(why);
    };
    if dyn_.and_then(|d| d.get(&p.cid)).is_some_and(|dv| !dv.keeper) {
        return None;
    }
    Some(format!("{why}, {}", if k == 1 { "his team's keeper".to_string() } else { format!("a top-{k} keeper") }))
}

// -- transaction budget and churn guard (LeagueContext.moves_* / recent_adds) -----------------

/// FPG a waiver add must clear with plenty of moves (waivers.MIN_GAIN)
pub const BASE_MIN_GAIN: f64 = 0.3;
/// 2-3 moves left
pub const SCARCE_MIN_GAIN: f64 = 0.6;
/// 1 move left (unless it replaces an injured player)
pub const LAST_MOVE_MIN_GAIN: f64 = 1.0;
pub const PLENTY_MOVES: i64 = 4;
/// churn guard: a player I added this recently is not dropped ...
pub const RECENT_ADD_DAYS: i64 = 7;
/// ... unless the add beats him by this much FPG ...
pub const CHURN_OVERRIDE_GAIN: f64 = 1.5;
/// ... or he is unavailable
pub const CHURN_EXEMPT_STATUSES: [&str; 4] = ["out", "ir", "ltir", "suspended"];
/// An add that replaces one of these (IR move, or dropping him) may use the last move.
pub const INJURY_REPLACEMENT_STATUSES: [&str; 3] = ["out", "ir", "ltir"];

/// Acquisitions I can still make now: the per-period limit minus my adds this period, capped by
/// what is left of a season limit. None = unlimited / unknown. Unknown usage counts as 0.
pub fn moves_left(ctx: &LeagueContext) -> Option<i64> {
    let mut lefts: Vec<i64> = Vec::new();
    if let Some(per) = ctx.moves_limit_per_period.filter(|p| *p >= 0) {
        lefts.push((per - ctx.moves_used_this_period.unwrap_or(0)).max(0));
    }
    if let Some(season) = ctx.moves_limit_season.filter(|s| *s >= 0) {
        lefts.push((season - ctx.moves_used_season.unwrap_or(0)).max(0));
    }
    lefts.into_iter().min()
}

/// True when I added `player` fewer than `days` days before `ctx.as_of`.
pub fn recently_added(ctx: &LeagueContext, player: &Player, days: i64) -> bool {
    ctx.recent_adds.get(&player.cid).is_some_and(|when| (ctx.as_of - *when).num_days() < days)
}

pub fn added_days_ago(ctx: &LeagueContext, player: &Player) -> Option<i64> {
    ctx.recent_adds.get(&player.cid).map(|when| (ctx.as_of - *when).num_days().max(0))
}

/// Churn guard: never drop a player I added < RECENT_ADD_DAYS days ago unless the add gains >=
/// CHURN_OVERRIDE_GAIN FPG over him or he is out / IR / suspended.
pub fn churn_blocked(ctx: &LeagueContext, drop: &Player, gain: f64) -> bool {
    recently_added(ctx, drop, RECENT_ADD_DAYS)
        && gain < CHURN_OVERRIDE_GAIN
        && !CHURN_EXEMPT_STATUSES.contains(&drop.status.as_str())
}

/// "added 1 day ago" / "added today".
pub fn churn_text(ctx: &LeagueContext, drop: &Player) -> String {
    match added_days_ago(ctx, drop) {
        None => "added recently".to_string(),
        Some(0) => "added today".to_string(),
        Some(n) => format!("added {n} day{} ago", if n != 1 { "s" } else { "" }),
    }
}

/// Minimum FPG gain a waiver add must clear given the moves left: unlimited / unknown or >= 4
/// left -> 0.3; 2-3 -> 0.6; 1 -> 1.0 (0.3 when the add replaces an injured player); 0 -> None.
pub fn move_scarcity_threshold(ctx: &LeagueContext, injury_replacement: bool) -> Option<f64> {
    match moves_left(ctx) {
        None => Some(BASE_MIN_GAIN),
        Some(left) if left >= PLENTY_MOVES => Some(BASE_MIN_GAIN),
        Some(left) if left >= 2 => Some(SCARCE_MIN_GAIN),
        Some(1) => Some(if injury_replacement { BASE_MIN_GAIN } else { LAST_MOVE_MIN_GAIN }),
        Some(_) => None,
    }
}

/// `f"{d:%a %b} {d.day}"`: "Mon Oct 5".
fn day_text(d: NaiveDate) -> String {
    format!("{} {}", strftime_date(d, "%a %b"), d.day())
}

/// First day of the next period (when the per-period count resets).
pub fn moves_reset(ctx: &LeagueContext) -> Option<NaiveDate> {
    ctx.period_end.map(|end| end + Duration::days(1))
}

fn period_word(ctx: &LeagueContext) -> &'static str {
    match (ctx.period_start, ctx.period_end) {
        (Some(s), Some(e)) if (e - s).num_days() > 6 => "this period",
        _ => "this week",
    }
}

/// (used, limit, "this week"/"this season") of the limit that binds now, or None.
fn binding(ctx: &LeagueContext) -> Option<(i64, i64, &'static str)> {
    let per = ctx.moves_limit_per_period;
    let season = ctx.moves_limit_season;
    let per_left = per.filter(|p| *p >= 0).map(|p| (p - ctx.moves_used_this_period.unwrap_or(0)).max(0));
    let season_left = season.filter(|s| *s >= 0).map(|s| (s - ctx.moves_used_season.unwrap_or(0)).max(0));
    if let Some(sl) = season_left {
        if per_left.map_or(true, |pl| sl < pl) {
            return Some((ctx.moves_used_season.unwrap_or(0), season.unwrap_or(0), "this season"));
        }
    }
    if per_left.is_some() {
        return Some((ctx.moves_used_this_period.unwrap_or(0), per.unwrap_or(0), period_word(ctx)));
    }
    None
}

/// "4 per matchup period (2 used, 2 left, resets Mon Oct 5)" or "unlimited" (plus the adds made
/// this period when known, and any season limit).
pub fn moves_text(ctx: &LeagueContext) -> String {
    let used = ctx.moves_used_this_period;
    let reset = moves_reset(ctx);
    let label = ctx.moves_period_label.as_deref().filter(|s| !s.is_empty()).unwrap_or("period");
    let mut text;
    match ctx.moves_limit_per_period.filter(|p| *p >= 0) {
        Some(per) => {
            let mut bits = vec![match used {
                Some(u) => format!("{u} used, {} left", (per - u).max(0)),
                None => "usage unknown".to_string(),
            }];
            if let Some(r) = reset {
                bits.push(format!("resets {}", day_text(r)));
            }
            text = format!("{per} per {label} ({})", bits.join(", "));
        }
        None => {
            text = "unlimited".to_string();
            if let Some(u) = used {
                text += &format!(" ({u} add{} {})", if u != 1 { "s" } else { "" }, period_word(ctx));
            }
        }
    }
    if let Some(season) = ctx.moves_limit_season.filter(|s| *s >= 0) {
        match ctx.moves_used_season {
            Some(su) => text += &format!("; season limit {season} ({su} used, {} left)", (season - su).max(0)),
            None => text += &format!("; season limit {season}"),
        }
    }
    text
}

/// "2 of 4 moves left this week" when a limit applies, else None.
pub fn moves_note(ctx: &LeagueContext) -> Option<String> {
    let (used, limit, when) = binding(ctx)?;
    Some(format!("{} of {limit} moves left {when}", (limit - used).max(0)))
}

/// MOVE_BUDGET reason for waiver / streaming recs (None when moves are unlimited).
pub fn moves_budget_reason(ctx: &LeagueContext) -> Option<Reason> {
    let mut note = moves_note(ctx)?;
    let left = moves_left(ctx);
    let thr = move_scarcity_threshold(ctx, false);
    if let (Some(l), Some(t)) = (left, thr) {
        if 0 < l && l < PLENTY_MOVES {
            note += &format!(": adds must gain >= {} FPG", fixed(t, 1, false));
            if l == 1 {
                note += " (or replace an injured player)";
            }
        }
    }
    let b = binding(ctx);
    Some(Reason {
        code: "MOVE_BUDGET".to_string(),
        text: note,
        value: left.map(|l| l as f64),
        baseline: b.map(|b| b.1 as f64),
    })
}

/// "No moves left this week (4/4 used); waiver suggestions resume Mon Oct 5".
pub fn no_moves_text(ctx: &LeagueContext) -> String {
    let Some((used, limit, when)) = binding(ctx) else {
        return "No moves left".to_string();
    };
    let mut text = format!("No moves left {when} ({used}/{limit} used)");
    if when != "this season" {
        if let Some(r) = moves_reset(ctx) {
            text += &format!("; waiver suggestions resume {}", day_text(r));
        }
    }
    text
}

// -- schedule-based gain units (trades: lineup FPG -> pts/week, pts rest of season) -----------

/// an NHL team's typical games per fantasy week (no schedule loaded)
pub const DEFAULT_GAMES_PER_WEEK: f64 = 3.4;
pub const SEASON_GAMES: i64 = 82;
/// used only when neither the schedule nor ctx.season_start is known
pub const DEFAULT_SEASON_START: (u32, u32) = (10, 7);
pub const DEFAULT_SEASON_END: (u32, u32) = (4, 16);
pub const LINEUP_EXEMPT_SLOTS: [&str; 5] = ["BN", "IR", "IR+", "NA", "MIN"];

/// My starters' average NHL games per week and games left in the regular season; source
/// "schedule" (ctx.schedule), "default" or "season over".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GameRate {
    pub per_week: f64,
    pub remaining: f64,
    pub source: &'static str,
}

/// (first, last) regular-season day: the loaded schedule's first and last game dates, else
/// `ctx.season_start` (or Oct 7) through Apr 16 of the following year.
pub fn season_bounds(ctx: &LeagueContext) -> (NaiveDate, NaiveDate) {
    let mut it = ctx.schedule.values().flatten();
    if let Some(&first) = it.next() {
        let (mut lo, mut hi) = (first, first);
        for &d in it {
            lo = lo.min(d);
            hi = hi.max(d);
        }
        return (lo, hi);
    }
    let y = if ctx.as_of.month() >= 7 { ctx.as_of.year() } else { ctx.as_of.year() - 1 };
    let start = ctx
        .season_start
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(y, DEFAULT_SEASON_START.0, DEFAULT_SEASON_START.1).expect("valid date"));
    let end = NaiveDate::from_ymd_opt(start.year() + 1, DEFAULT_SEASON_END.0, DEFAULT_SEASON_END.1).expect("valid date");
    (start, end)
}

/// `team`'s current starters (starting slots), else every player outside IR / minors.
pub fn lineup_players(team: &FantasyTeam) -> Vec<&Player> {
    let starters: Vec<&Player> = team
        .slots
        .iter()
        .filter(|s| s.starting && !LINEUP_EXEMPT_SLOTS.contains(&s.slot.as_str()))
        .filter_map(|s| s.player.as_ref())
        .collect();
    if !starters.is_empty() {
        return starters;
    }
    team.slots
        .iter()
        .filter(|s| !SIZE_EXEMPT_SLOTS.contains(&s.slot.as_str()))
        .filter_map(|s| s.player.as_ref())
        .collect()
}

/// My starters' average NHL games per week and games remaining ([`GameRate`]). From
/// `ctx.schedule`: each starter's team games from max(as_of, first game) to the last regular-season
/// game, averaged over the starters whose team is on the schedule; per week = that / max(1, weeks
/// left). When no starter's team is on it: DEFAULT_GAMES_PER_WEEK and the fraction of the season
/// still to play x the schedule's games per team (SEASON_GAMES without a schedule; Python always
/// used SEASON_GAMES). `players` None = my current starters (errors, like Python's `LookupError`,
/// when no team is mine).
pub fn game_rate(ctx: &LeagueContext, players: Option<&[&Player]>) -> Result<GameRate, CoreError> {
    let (first, last) = season_bounds(ctx);
    let start = ctx.as_of.max(first);
    let days = (last - start).num_days() + 1;
    if days <= 0 {
        return Ok(GameRate { per_week: 0.0, remaining: 0.0, source: "season over" });
    }
    let weeks = days as f64 / 7.0;
    let mine;
    let ps: &[&Player] = match players {
        Some(p) => p,
        None => {
            mine = lineup_players(ctx.my_team()?);
            &mine
        }
    };
    let counts: Vec<i64> = ps
        .iter()
        .filter_map(|p| p.team.as_deref())
        .filter_map(|t| ctx.schedule.get(t))
        .map(|ds| ds.iter().filter(|d| start <= **d && **d <= last).count() as i64)
        .collect();
    if !counts.is_empty() {
        let remaining = counts.iter().sum::<i64>() as f64 / counts.len() as f64;
        return Ok(GameRate { per_week: remaining / weeks.max(1.0), remaining, source: "schedule" });
    }
    let frac = (days as f64 / ((last - first).num_days() + 1).max(1) as f64).max(0.0).min(1.0);
    let season_games = SeasonLength::of(&ctx.schedule).games;
    Ok(GameRate { per_week: DEFAULT_GAMES_PER_WEEK, remaining: season_games * frac, source: "default" })
}

/// Lineup FPG -> fantasy points per week.
pub fn gain_week(fpg: f64, rate: &GameRate) -> f64 {
    fpg * rate.per_week
}

/// Lineup FPG -> fantasy points over the rest of the regular season.
pub fn gain_season(fpg: f64, rate: &GameRate) -> f64 {
    fpg * rate.remaining
}

/// "+0.66/g · +2.3/wk · +55/season".
pub fn gain_units_text(fpg: f64, rate: &GameRate) -> String {
    format!(
        "{}/g \u{b7} {}/wk \u{b7} {}/season",
        fixed(fpg, 2, true),
        fixed(gain_week(fpg, rate), 1, true),
        fixed(gain_season(fpg, rate), 0, true)
    )
}
