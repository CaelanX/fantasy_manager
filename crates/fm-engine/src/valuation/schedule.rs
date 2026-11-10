//! Rust port of `fantasy_manager/valuation/schedule.py`: the week window, games / off-night games
//! in it, goalie start shares (shrunk, ledger actuals, back-to-backs, absent teammates) and the
//! per-game skater opponent multiplier.
//! Owner: work package `W3-valuation-schedule` (wave 3).
//!
//! Deviations from the Python module (behaviour is the same):
//! * `_CTX_MEMO` / `ctx_memo` (a scratch dict keyed by `id(ctx)`) is an explicit [`ScheduleMemo`]
//!   the caller builds once per league context and passes down; it is `Sync` (rayon). Like Python it
//!   keys its entries by the inputs that can change (`as_of`, season start, schedule sizes), so a
//!   context copy with another `as_of` still gets its own window / opponent table.
//! * `opponent_adjust(ctx, odds=..., ga=..., enabled=...)` (replace the memoized one) is
//!   [`ScheduleMemo::install_opponent_adjust`]; the default odds source reads
//!   `<data_dir>/archive/odds-*.json` with the data dir given to [`ScheduleMemo::new`] (Python: the
//!   settings data dir). `FM_OPP_ADJUST` is read once per process ([`opp_adjust_env_enabled`]).
//! * `redistribute_shares` summed the open goalies' weights over a `set` (hash order, last-bit
//!   nondeterminism in Python); here the sum runs in sorted-key order.
//! * Python's `start_share(..., k=None, prior=None)` keyword defaults are `Option` arguments.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{Datelike, Duration, NaiveDate};
use dashmap::DashMap;
use fm_core::models::{LeagueContext, Player};
use fm_core::normalize::normalize_team;
use fm_core::values::Horizon;
use fm_core::Map;
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::{py_sum, round_to};
use serde::Serialize;
use serde_json::Value;

use super::adjust::{availability_multiplier, return_estimate, ReturnEstimate, SeasonLength};
use super::params;
use crate::analysis::team_factors::{odds_fn, team_ga_factors, DayOdds, OddsFn, TeamFactors};

pub const WEEK_DAYS: i64 = 7;
pub const OFFNIGHT_THRESHOLD: i64 = 8;
/// actual starts (harness ledger) shrunk toward the projection / prior share
pub const ACTUAL_SHARE_K: f64 = 10.0;
/// P(#1 starts a back-to-back's second night) until the ledger has `B2B_MIN_SAMPLE` of the team's
/// back-to-backs (then the observed rate shrunk toward it with k = `B2B_SHRINK_K`)
pub const B2B_SECOND_NIGHT_P: f64 = 0.35;
pub const B2B_MIN_SAMPLE: i64 = 5;
pub const B2B_SHRINK_K: f64 = 5.0;
/// share of a team's games that are the second night of a back-to-back
pub const SEASON_B2B_FRAC: f64 = 0.15;
pub const STARTER_SHARE: f64 = 0.5;
pub const BACKUP_MIN_SHARE: f64 = 0.25;

/// Import-time snapshot `OFFNIGHT_BONUS` (live code uses `params::offnight_bonus`).
pub fn offnight_bonus_snapshot() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| params::offnight_bonus(None))
}
/// Import-time snapshot `START_SHARE_K`.
pub fn start_share_k_snapshot() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| params::start_share_k(None))
}
/// Import-time snapshot `START_SHARE_PRIOR`.
pub fn start_share_prior_snapshot() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| params::start_share_prior(None))
}

fn in_window(d: NaiveDate, start: NaiveDate, days: i64) -> bool {
    start <= d && d < start + Duration::days(days)
}

/// `sorted(d for d in set(dates) if _in_window(d, start, days))`.
pub fn window_dates(dates: &[NaiveDate], start: NaiveDate, days: i64) -> Vec<NaiveDate> {
    let mut v: Vec<NaiveDate> = dates.iter().copied().filter(|d| in_window(*d, start, days)).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// `games_in_next(dates, as_of, days)`: distinct game dates in `[as_of, as_of + days)`.
pub fn games_in_next(dates: &[NaiveDate], as_of: NaiveDate, days: i64) -> i64 {
    window_dates(dates, as_of, days).len() as i64
}

/// `off_night_games(...)`: a team's games in the window played on nights with fewer than
/// `threshold` NHL games.
pub fn off_night_games(
    dates: &[NaiveDate],
    games_per_day: &Map<NaiveDate, i64>,
    as_of: NaiveDate,
    days: i64,
    threshold: i64,
) -> i64 {
    window_dates(dates, as_of, days)
        .into_iter()
        .filter(|d| {
            let n = games_per_day.get(d).copied().unwrap_or(0);
            0 < n && n < threshold
        })
        .count() as i64
}

/// `proj_week(fpg, avail_week, games_next7, offnight_games, params)`.
pub fn proj_week(fpg: f64, avail_week: f64, games_next7: i64, offnight_games: i64, params: Option<&Value>) -> f64 {
    fpg * avail_week * games_next7 as f64 * (1.0 + params::offnight_bonus(params) * offnight_games as f64)
}

/// `shrunk_share(starts, team_games, k, prior, params)`: `(GS + k*prior) / (team GP + k)` with k /
/// prior from `params` unless given.
pub fn shrunk_share(starts: f64, team_games: f64, k: Option<f64>, prior: Option<f64>, params: Option<&Value>) -> f64 {
    let k = k.unwrap_or_else(|| params::start_share_k(params));
    let prior = prior.unwrap_or_else(|| params::start_share_prior(params));
    (starts + k * prior) / (team_games + k)
}

/// `start_share(player, k, prior, team_games, actual, actual_k)`: shrunk goalie start share (1.0
/// for skaters). `actual` = (starts, team games) this season from the harness ledger: the result is
/// then `(starts + actual_k*base) / (team games + actual_k)` with `base` the shrunk share of the
/// prior line (else the projected line). `actual_k` is `ACTUAL_SHARE_K` in Python's default.
pub fn start_share(
    player: &Player,
    k: Option<f64>,
    prior: Option<f64>,
    team_games: Option<&Map<String, f64>>,
    actual: Option<(f64, f64)>,
    actual_k: f64,
) -> f64 {
    if !player.is_goalie() {
        return 1.0;
    }
    if let Some((starts, games)) = actual {
        if games > 0.0 {
            let base = start_share_parts(player, k, prior, team_games, &["prior"]).0;
            return actual_start_share(base, starts, games, actual_k);
        }
    }
    start_share_parts(player, k, prior, team_games, &["season", "prior"]).0
}

/// `actual_start_share(base, starts, team_games, k)`: this season's starts / team games shrunk
/// toward `base` with `k` games.
pub fn actual_start_share(base: f64, starts: f64, team_games: f64, k: f64) -> f64 {
    if team_games <= 0.0 {
        return base;
    }
    let starts = starts.max(0.0).min(team_games);
    (starts + k * base) / (team_games + k)
}

/// `start_share_parts(player, k, prior, team_games, splits)`: (share, starts, team games) pooled
/// over `splits` (Python default `("season", "prior")`), the projected line when those carry no
/// games. GS falls back to GP; the denominator is never below the goalie's own GP.
pub fn start_share_parts(
    player: &Player,
    k: Option<f64>,
    prior: Option<f64>,
    team_games: Option<&Map<String, f64>>,
    splits: &[&str],
) -> (f64, f64, f64) {
    let pooled = |splits: &[&str]| -> (f64, f64) {
        let (mut gs, mut tg) = (0.0_f64, 0.0_f64);
        for split in splits {
            let Some(line) = player.lines.get(*split) else { continue };
            let starts = line.stats.get("GS").copied().unwrap_or(line.gp as f64);
            let own = (line.gp as f64).max(starts);
            let denom = team_games.and_then(|t| t.get(*split).copied()).unwrap_or(own).max(own);
            if denom <= 0.0 {
                continue;
            }
            gs += starts;
            tg += denom;
        }
        (gs, tg)
    };
    let (mut gs, mut tg) = pooled(splits);
    if tg <= 0.0 {
        (gs, tg) = pooled(&["projected"]);
    }
    (shrunk_share(gs, tg, k, prior, None), gs, tg)
}

/// `WeekWindow`: the window to project over.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WeekWindow {
    pub start: NaiveDate,
    pub days: i64,
    pub preseason: bool,
    /// mean games per NHL team in the window
    pub avg_team_games: f64,
}

impl WeekWindow {
    /// `WeekWindow(start=..., days=...)` with the model defaults for the rest.
    pub fn new(start: NaiveDate, days: i64) -> Self {
        WeekWindow { start, days, preseason: false, avg_team_games: 0.0 }
    }
    pub fn end(&self) -> NaiveDate {
        self.start + Duration::days(self.days - 1)
    }
    /// `f"{start:%b %d}-{end:%b %d}"`
    pub fn label(&self) -> String {
        format!("{}-{}", self.start.format("%b %d"), self.end().format("%b %d"))
    }
}

/// `week_window(as_of, season_start, schedule, games_per_day, days)`: None when no schedule is
/// loaded. Before the regular season the window is the first `days` days from `season_start`.
pub fn week_window(
    as_of: NaiveDate,
    season_start: Option<NaiveDate>,
    schedule: &Map<String, Vec<NaiveDate>>,
    games_per_day: &Map<NaiveDate, i64>,
    days: i64,
) -> Option<WeekWindow> {
    if schedule.is_empty() {
        return None;
    }
    let (mut start, mut preseason) = (as_of, false);
    let mut league_games: i64 =
        games_per_day.iter().filter(|(d, _)| in_window(**d, as_of, days)).map(|(_, n)| *n).sum();
    if league_games == 0 && games_per_day.is_empty() {
        league_games = schedule.values().map(|ds| games_in_next(ds, as_of, days)).sum();
    }
    if league_games == 0 {
        if let Some(s) = season_start {
            if s > as_of {
                start = s;
                preseason = true;
            }
        }
    }
    let counts: Vec<i64> = schedule.values().map(|ds| games_in_next(ds, start, days)).collect();
    let avg = if counts.is_empty() { 0.0 } else { counts.iter().sum::<i64>() as f64 / counts.len() as f64 };
    Some(WeekWindow { start, days, preseason, avg_team_games: avg })
}

/// `context_window(ctx, days)`.
pub fn context_window(ctx: &LeagueContext, days: i64) -> Option<WeekWindow> {
    week_window(ctx.as_of, ctx.season_start, &ctx.schedule, &ctx.games_per_day, days)
}

/// `ScheduleFactor`: games / off-night games / start share for one player in a window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScheduleFactor {
    pub games: i64,
    pub offnight: i64,
    pub start_share: f64,
    pub window: WeekWindow,
}

impl ScheduleFactor {
    /// Games-weighted multiplier applied to availability-adjusted FPG.
    pub fn multiplier(&self) -> f64 {
        self.games as f64 * (1.0 + params::offnight_bonus(None) * self.offnight as f64) * self.start_share
    }
}

/// `second_nights(dates, start, days)`: games in the window played the day after another game (the
/// previous game may fall before the window).
pub fn second_nights(dates: &[NaiveDate], start: NaiveDate, days: i64) -> i64 {
    let ds: std::collections::HashSet<NaiveDate> = dates.iter().copied().collect();
    window_dates(dates, start, days).into_iter().filter(|d| ds.contains(&(*d - Duration::days(1)))).count() as i64
}

/// `week_start_share(share, dates, start, days, b2b_p, season_b2b_frac)`: expected fraction of the
/// team's games in the window that the goalie starts given his season `share` and the window's
/// back-to-backs. Python defaults: `b2b_p = B2B_SECOND_NIGHT_P`, `season_b2b_frac = SEASON_B2B_FRAC`.
pub fn week_start_share(
    share: f64,
    dates: &[NaiveDate],
    start: NaiveDate,
    days: i64,
    b2b_p: f64,
    season_b2b_frac: f64,
) -> f64 {
    let games = games_in_next(dates, start, days);
    if games <= 0 || share < BACKUP_MIN_SHARE {
        return share;
    }
    let (p1, p2) = night_start_probs(share, b2b_p, season_b2b_frac);
    let b = second_nights(dates, start, days);
    ((games - b) as f64 * p1 + b as f64 * p2) / games as f64
}

/// `night_start_probs(share, b2b_p, season_b2b_frac)`: (P(start) on an ordinary night, P(start)
/// on a back-to-back's second night).
pub fn night_start_probs(share: f64, b2b_p: f64, season_b2b_frac: f64) -> (f64, f64) {
    let p2 = if share >= STARTER_SHARE {
        b2b_p
    } else if share >= BACKUP_MIN_SHARE {
        1.0 - b2b_p
    } else {
        return (share, share);
    };
    let f = season_b2b_frac.clamp(0.0, 0.9);
    let p1 = 1.0_f64.min(0.0_f64.max((share - f * p2) / (1.0 - f)));
    (p1, p2)
}

/// `schedule_factor(player, schedule, games_per_day, window, threshold, share, b2b_p)`: None for
/// an unknown team. `share` None = `start_share(player)`; `b2b_p` (goalies, opt-in) re-weights the
/// share for the window's back-to-backs ([`week_start_share`]).
pub fn schedule_factor(
    player: &Player,
    schedule: &Map<String, Vec<NaiveDate>>,
    games_per_day: &Map<NaiveDate, i64>,
    window: &WeekWindow,
    threshold: i64,
    share: Option<f64>,
    b2b_p: Option<f64>,
) -> Option<ScheduleFactor> {
    let team = normalize_team(player.team.as_deref())?;
    let dates = schedule.get(&team)?;
    let mut s = share.unwrap_or_else(|| start_share(player, None, None, None, None, ACTUAL_SHARE_K));
    if let Some(p) = b2b_p {
        if player.is_goalie() {
            s = week_start_share(s, dates, window.start, window.days, p, SEASON_B2B_FRAC);
        }
    }
    Some(ScheduleFactor {
        games: games_in_next(dates, window.start, window.days),
        offnight: off_night_games(dates, games_per_day, window.start, window.days, threshold),
        start_share: s,
        window: window.clone(),
    })
}

// --------------------------------------------------------------------------- teammate-aware shares

/// a goalie absorbing an absent teammate's starts never goes above this
pub const TEAMMATE_CAP: f64 = 0.9;
/// weight of an NHL roster goalie outside the league pool (or without any history)
pub const ROSTER_ONLY_SHARE: f64 = 0.10;
pub const ABSENT_STATUSES: [&str; 4] = ["out", "ir", "ltir", "suspended"];

fn status_words(status: &str) -> &str {
    match status {
        "out" => "out",
        "ir" => "on IR",
        "ltir" => "on LTIR",
        "suspended" => "suspended",
        other => other,
    }
}

/// `redistribute_shares(shares, absent, cap)`: new start shares of the goalies of one team not in
/// `absent` ({key: fraction of the horizon he misses}). Only an absent goalie whose share is above
/// every remaining goalie's donates; recipients take it in proportion to their shares, capped at
/// `max(cap, own share)`, the excess spilling to the others (lost when nobody can take it).
pub fn redistribute_shares(shares: &Map<String, f64>, absent: &Map<String, f64>, cap: f64) -> Map<String, f64> {
    let rest: Map<String, f64> =
        shares.iter().filter(|(k, _)| !absent.contains_key(*k)).map(|(k, v)| (k.clone(), v.max(0.0))).collect();
    if rest.is_empty() {
        return Map::new();
    }
    let top = rest.values().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut pool = py_sum(absent.iter().filter_map(|(k, f)| {
        let s = *shares.get(k)?;
        (s > top).then(|| s * f.clamp(0.0, 1.0))
    }));
    let mut out = rest.clone();
    // `open_` is a set in Python; iterate (and sum) it in sorted order
    let mut open: Vec<String> = rest.keys().cloned().collect();
    open.sort();
    while pool > 1e-12 && !open.is_empty() {
        let wsum = py_sum(open.iter().map(|k| rest[k]));
        let mut spill = 0.0;
        let n_open = open.len() as f64;
        let mut still: Vec<String> = Vec::with_capacity(open.len());
        for k in &open {
            let w = if wsum > 0.0 { rest[k] / wsum } else { 1.0 / n_open };
            let lim = cap.max(rest[k]);
            let new = out[k] + pool * w;
            if new >= lim {
                spill += new - lim;
                out[k] = lim;
            } else {
                out[k] = new;
                still.push(k.clone());
            }
        }
        open = still;
        pool = spill;
    }
    out
}

/// An absent goalie whose starts are redistributed: the fraction of the week window / of the rest
/// of the season he misses.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TeammateAbsence {
    pub name: String,
    pub status: String,
    pub return_date: Option<NaiveDate>,
    pub week_frac: f64,
    pub season_frac: f64,
    pub games_missed: Option<f64>,
    pub games_remaining: Option<f64>,
}

/// A goalie's start shares after his absent teammates' starts are redistributed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TeammateShare {
    pub base: f64,
    pub week: f64,
    pub season: f64,
    pub absent: Vec<TeammateAbsence>,
    pub window_label: Option<String>,
}

fn last_name(name: &str) -> String {
    name.split_whitespace().last().unwrap_or(name).to_string()
}

impl TeammateShare {
    /// The TEAMMATE_OUT reason text.
    pub fn text(&self, player: &Player) -> String {
        let me = last_name(&player.name);
        let mut parts: Vec<String> = Vec::new();
        for a in &self.absent {
            let mut who = format!("{} {}", last_name(&a.name), status_words(&a.status));
            if let Some(rd) = a.return_date {
                who += &format!(" until ~{} {}", rd.format("%b"), rd.day());
            } else if a.season_frac >= 0.999 {
                who += " for the rest of the season";
            }
            if let (Some(m), Some(r)) = (a.games_missed, a.games_remaining) {
                if r != 0.0 {
                    who += &format!(" (~{} of {} games)", fixed(m, 0, false), fixed(r, 0, false));
                }
            }
            parts.push(who);
        }
        let mut week = format!("{} -> {} this week", fixed(self.base, 2, false), fixed(self.week, 2, false));
        if let Some(l) = &self.window_label {
            week += &format!(" ({l})");
        }
        format!(
            "{}: {}'s start share {}, {} -> {} rest of season",
            parts.join(" and "),
            me,
            week,
            fixed(self.base, 2, false),
            fixed(self.season, 2, false)
        )
    }
}

/// `_week_fraction(status, est, team_dates, window)`: fraction of the team's games in `window` an
/// absent goalie misses (the flat week availability when nothing can be inferred).
fn week_fraction(status: &str, est: Option<&ReturnEstimate>, team_dates: &[NaiveDate], window: Option<&WeekWindow>) -> f64 {
    let flat = (1.0 - availability_multiplier(status, Horizon::Week, None)).clamp(0.0, 1.0);
    let Some(window) = window else { return flat };
    let games = window_dates(team_dates, window.start, window.days);
    let Some(est) = est else { return flat };
    if games.is_empty() {
        return flat;
    }
    match est.return_date {
        None => {
            if est.games_missed > 0.0 {
                1.0
            } else {
                0.0
            }
        }
        Some(rd) => games.iter().filter(|d| **d < rd).count() as f64 / games.len() as f64,
    }
}

/// `teammate_shares(ctx, base, window, weights, cap)`: cid -> teammate-adjusted week / season start
/// shares of the goalies whose team's #1 (a goalie with a higher share) is out / IR / LTIR /
/// suspended. `weights` overrides a goalie's share for the redistribution. Python default `cap`
/// is `TEAMMATE_CAP`.
pub fn teammate_shares(
    ctx: &LeagueContext,
    base: &Map<String, f64>,
    window: Option<&WeekWindow>,
    weights: Option<&Map<String, f64>>,
    cap: f64,
) -> Map<String, TeammateShare> {
    let mut by_team: Map<String, Vec<&Player>> = Map::new();
    for p in ctx.all_players() {
        if p.is_goalie() && base.contains_key(&p.cid) {
            if let Some(team) = normalize_team(p.team.as_deref()) {
                by_team.entry(team).or_default().push(p);
            }
        }
    }
    let empty_roster: Map<i64, String> = Map::new();
    let mut out: Map<String, TeammateShare> = Map::new();
    for (team, goalies) in &by_team {
        let absent: Map<&str, &Player> = goalies
            .iter()
            .filter(|g| g.status.is_in(&ABSENT_STATUSES))
            .map(|g| (g.cid.as_str(), *g))
            .collect();
        if absent.is_empty() {
            continue;
        }
        let roster = ctx.nhl_goalies.get(team).unwrap_or(&empty_roster);
        let team_dates: &[NaiveDate] = ctx.schedule.get(team).map(|v| v.as_slice()).unwrap_or(&[]);
        let mut shares: Map<String, f64> = Map::new();
        for g in goalies {
            let nid = g.nhl_id();
            if absent.contains_key(g.cid.as_str())
                || roster.is_empty()
                || nid.is_none()
                || nid.is_some_and(|n| roster.contains_key(&n))
            {
                let w = weights.and_then(|w| w.get(&g.cid).copied()).unwrap_or(base[&g.cid]);
                shares.insert(g.cid.clone(), w);
            }
        }
        let pool_ids: std::collections::HashSet<i64> = goalies.iter().filter_map(|g| g.nhl_id()).collect();
        for nid in roster.keys() {
            if !pool_ids.contains(nid) {
                shares.insert(format!("nhl:{nid}"), ROSTER_ONLY_SHARE);
            }
        }
        let mut info: Map<String, TeammateAbsence> = Map::new();
        for (cid, a) in &absent {
            let status = a.status.as_str();
            let est = return_estimate(
                status,
                a.status_note.as_deref(),
                ctx.as_of,
                Some(team_dates),
                ctx.season_start,
                SeasonLength::of(&ctx.schedule),
            );
            let season_frac = match &est {
                Some(e) => {
                    if e.games_remaining > 0.0 {
                        e.games_missed / e.games_remaining
                    } else {
                        1.0
                    }
                }
                None => 1.0 - availability_multiplier(status, Horizon::Season, None),
            };
            info.insert(
                cid.to_string(),
                TeammateAbsence {
                    name: a.name.clone(),
                    status: status.to_string(),
                    return_date: est.as_ref().and_then(|e| e.return_date),
                    week_frac: week_fraction(status, est.as_ref(), team_dates, window),
                    season_frac: season_frac.clamp(0.0, 1.0),
                    games_missed: est.as_ref().map(|e| round_to(e.games_missed, 2)),
                    games_remaining: est.as_ref().map(|e| round_to(e.games_remaining, 2)),
                },
            );
        }
        let week_abs: Map<String, f64> = info.iter().map(|(c, i)| (c.clone(), i.week_frac)).collect();
        let season_abs: Map<String, f64> = info.iter().map(|(c, i)| (c.clone(), i.season_frac)).collect();
        let week = redistribute_shares(&shares, &week_abs, cap);
        let season = redistribute_shares(&shares, &season_abs, cap);
        let rest_top = shares
            .iter()
            .filter(|(k, _)| !info.contains_key(*k))
            .map(|(_, v)| *v)
            .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| if v > m { v } else { m })))
            .unwrap_or(0.0);
        let donors: Vec<TeammateAbsence> =
            info.iter().filter(|(c, _)| shares[*c] > rest_top).map(|(_, i)| i.clone()).collect();
        for g in goalies {
            if info.contains_key(&g.cid) {
                continue;
            }
            let Some(wk_new) = week.get(&g.cid) else { continue };
            let b = base[&g.cid];
            let w0 = shares[&g.cid];
            let lim = cap.max(b);
            let wk = lim.min(b + wk_new - w0);
            let sn = lim.min(b + season[&g.cid] - w0);
            if wk - b <= 1e-4 && sn - b <= 1e-4 {
                continue;
            }
            out.insert(
                g.cid.clone(),
                TeammateShare {
                    base: b,
                    week: wk,
                    season: sn,
                    absent: donors.clone(),
                    window_label: window.map(|w| w.label()),
                },
            );
        }
    }
    out
}

// --------------------------------------------------------------------------- opponent strength (skaters)
//
// A skater's per-game value is scaled per game by how many goals his team is expected to score in
// it: with odds archived for that day, `(implied team total / day's average implied total) **
// OPP_BETA_ODDS` (the day's average shrunk toward `LEAGUE_IMPLIED_TOTAL` with `IMPLIED_AVG_K`
// teams); without odds, the opponent's goals against per game (`team_ga_factors`) ** `OPP_BETA_GA`.
// Both clamped to [0.80, 1.25]; no data -> 1.0. Goalies are never adjusted here.
// `FM_OPP_ADJUST=0` turns it off.

pub const OPP_BETA_ODDS: f64 = 0.7;
pub const OPP_BETA_GA: f64 = 0.5;
pub const OPP_MULT_MIN: f64 = 0.80;
pub const OPP_MULT_MAX: f64 = 1.25;
/// NHL goals per team-game, recent seasons (approximate)
pub const LEAGUE_IMPLIED_TOTAL: f64 = 3.05;
/// teams of shrinkage of a day's average implied total toward it
pub const IMPLIED_AVG_K: f64 = 8.0;
pub const OPP_WEEK_REASON: &str = "OPP_ADJ_WEEK";

/// `opponent_multiplier(ratio, beta)`: `clamp(ratio ** beta, OPP_MULT_MIN, OPP_MULT_MAX)`; 1.0
/// without a usable ratio.
pub fn opponent_multiplier(ratio: Option<f64>, beta: f64) -> f64 {
    match ratio {
        Some(r) if r > 0.0 => OPP_MULT_MAX.min(OPP_MULT_MIN.max(r.powf(beta))),
        _ => 1.0,
    }
}

/// `implied_ratio(team_total, day_totals)`: a team's implied goals over the day's average implied
/// total (shrunk toward `LEAGUE_IMPLIED_TOTAL` with `IMPLIED_AVG_K` teams).
pub fn implied_ratio(team_total: Option<f64>, day_totals: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    let t = team_total.filter(|t| *t > 0.0)?;
    let ts: Vec<f64> = day_totals.into_iter().flatten().filter(|x| *x != 0.0 && *x > 0.0).collect();
    let avg = (py_sum(ts.iter().copied()) + IMPLIED_AVG_K * LEAGUE_IMPLIED_TOTAL) / (ts.len() as f64 + IMPLIED_AVG_K);
    Some(t / avg)
}

/// `OppGame`: one game's opponent multiplier and where it came from.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OppGame {
    pub mult: f64,
    /// "odds" / "goals against" / "neutral"
    pub source: String,
    /// "BOS" at home, "@BOS" away
    pub opponent: Option<String>,
    /// his team's implied goals (odds)
    pub implied_total: Option<f64>,
    /// opponent GA per game / league average (fallback)
    pub ga_factor: Option<f64>,
}

impl Default for OppGame {
    fn default() -> Self {
        OppGame { mult: 1.0, source: "neutral".into(), opponent: None, implied_total: None, ga_factor: None }
    }
}

impl OppGame {
    pub fn text(&self, name: &str) -> String {
        let opp = match &self.opponent {
            Some(o) if !o.is_empty() => format!(" {o}"),
            _ => String::new(),
        };
        let why = match self.source.as_str() {
            "odds" => format!("market implies {} goals for his team", fixed(self.implied_total.unwrap_or(f64::NAN), 2, false)),
            "goals against" => format!(
                "opponent allows {}x the league's goals per game",
                fixed(self.ga_factor.unwrap_or(f64::NAN), 2, false)
            ),
            _ => "no matchup data".to_string(),
        };
        format!("{name}: today{opp} - {why}, per-game value x{}", fixed(self.mult, 2, false))
    }
}

/// `FM_OPP_ADJUST` parsing: off for "0" / "false" / "no" / "off" (trimmed, case-insensitive).
pub fn opp_adjust_enabled_from(value: Option<&str>) -> bool {
    let v = value.unwrap_or("1").trim().to_lowercase();
    !matches!(v.as_str(), "0" | "false" | "no" | "off")
}

/// `FM_OPP_ADJUST` read once per process.
pub fn opp_adjust_env_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| opp_adjust_enabled_from(std::env::var("FM_OPP_ADJUST").ok().as_deref()))
}

/// Per-game skater opponent multipliers for one league context (see the section comment). Results
/// are cached per (team, day); the GA table is built from the context on first use. `Sync`.
pub struct OpponentAdjust {
    odds: OddsFn,
    ga: OnceLock<TeamFactors>,
    days: DashMap<NaiveDate, Arc<DayOdds>>,
    games: DashMap<(String, NaiveDate), OppGame>,
    pub enabled: bool,
}

impl std::fmt::Debug for OpponentAdjust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpponentAdjust").field("enabled", &self.enabled).finish_non_exhaustive()
    }
}

impl OpponentAdjust {
    /// `OpponentAdjust(ctx, odds, ga, enabled)`: `odds` None = the archive in `data_dir`
    /// ([`odds_fn`]); `ga` None = `team_ga_factors(ctx)` on first use; `enabled` None = the
    /// `FM_OPP_ADJUST` switch.
    pub fn new(odds: Option<OddsFn>, ga: Option<TeamFactors>, enabled: Option<bool>, data_dir: Option<PathBuf>) -> Self {
        let cell = OnceLock::new();
        if let Some(g) = ga {
            let _ = cell.set(g);
        }
        OpponentAdjust {
            odds: odds.unwrap_or_else(|| odds_fn(data_dir)),
            ga: cell,
            days: DashMap::new(),
            games: DashMap::new(),
            enabled: enabled.unwrap_or_else(opp_adjust_env_enabled),
        }
    }

    fn odds_for(&self, day: NaiveDate) -> Arc<DayOdds> {
        if let Some(v) = self.days.get(&day) {
            return v.clone();
        }
        let v = Arc::new((self.odds)(day));
        self.days.entry(day).or_insert(v).clone()
    }

    fn ga_factors(&self, ctx: &LeagueContext) -> &TeamFactors {
        self.ga.get_or_init(|| team_ga_factors(ctx))
    }

    /// The multiplier for `team`'s game on `day` (neutral when disabled or unknown).
    pub fn game(&self, ctx: &LeagueContext, team: Option<&str>, day: NaiveDate) -> OppGame {
        let team = normalize_team(team).unwrap_or_default();
        if !self.enabled || team.is_empty() {
            return OppGame::default();
        }
        let key = (team, day);
        if let Some(g) = self.games.get(&key) {
            return g.clone();
        }
        let team = &key.0;
        let opp = ctx.opponents.get(team).and_then(|m| m.get(&day)).cloned();
        let odds = self.odds_for(day);
        let o = odds.get(team);
        let ratio = implied_ratio(o.and_then(|o| o.implied_total), odds.values().map(|v| v.implied_total));
        let g = if let Some(r) = ratio {
            OppGame {
                mult: opponent_multiplier(Some(r), OPP_BETA_ODDS),
                source: "odds".into(),
                opponent: opp,
                implied_total: o.and_then(|o| o.implied_total),
                ga_factor: None,
            }
        } else {
            let f = match opp.as_deref() {
                Some(s) if !s.is_empty() => {
                    let t = normalize_team(Some(s.trim_start_matches('@'))).unwrap_or_default();
                    self.ga_factors(ctx).get(&t).copied()
                }
                _ => None,
            };
            match f {
                Some((fac, _)) => OppGame {
                    mult: opponent_multiplier(Some(fac), OPP_BETA_GA),
                    source: "goals against".into(),
                    opponent: opp,
                    implied_total: None,
                    ga_factor: Some(fac),
                },
                None => OppGame { opponent: opp, ..OppGame::default() },
            }
        };
        self.games.entry(key).or_insert(g).clone()
    }

    pub fn mult(&self, ctx: &LeagueContext, team: Option<&str>, day: NaiveDate) -> f64 {
        self.game(ctx, team, day).mult
    }
}

// --------------------------------------------------------------------------- per-context memo

type WindowKey = (NaiveDate, Option<NaiveDate>, usize, usize);
type OppKey = (NaiveDate, usize, usize);

/// Replacement for Python's `ctx_memo(ctx)` (`_CTX_MEMO` keyed by `id(ctx)`): per-context caches
/// of values that are expensive to rebuild per player-day (the week window, the opponent table).
/// Build one per league context (per run) and pass it down; it is `Send + Sync`.
#[derive(Debug, Default)]
pub struct ScheduleMemo {
    data_dir: Option<PathBuf>,
    windows: Mutex<HashMap<WindowKey, Option<WeekWindow>>>,
    opp: Mutex<HashMap<OppKey, Arc<OpponentAdjust>>>,
}

impl ScheduleMemo {
    /// `data_dir`: where the default odds source reads `archive/odds-*.json` (Python: the settings
    /// data dir); None = no odds (opponent goals against only).
    pub fn new(data_dir: Option<PathBuf>) -> Self {
        ScheduleMemo { data_dir, ..Default::default() }
    }

    /// An empty memo with the same odds data dir: Python's `_CTX_MEMO` entry of a new context.
    pub fn fresh(&self) -> ScheduleMemo {
        ScheduleMemo::new(self.data_dir.clone())
    }

    /// `memo_window(ctx)`: [`context_window`] cached per (as_of, season start, schedule size).
    pub fn window(&self, ctx: &LeagueContext) -> Option<WeekWindow> {
        let k = (ctx.as_of, ctx.season_start, ctx.schedule.len(), ctx.games_per_day.len());
        let mut m = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(k).or_insert_with(|| context_window(ctx, WEEK_DAYS)).clone()
    }

    fn opp_key(ctx: &LeagueContext) -> OppKey {
        (ctx.as_of, ctx.opponents.len(), ctx.schedule.len())
    }

    /// `opponent_adjust(ctx)`: the context's [`OpponentAdjust`], built once per (context, as_of).
    pub fn opponent_adjust(&self, ctx: &LeagueContext) -> Arc<OpponentAdjust> {
        let mut m = self.opp.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(Self::opp_key(ctx))
            .or_insert_with(|| Arc::new(OpponentAdjust::new(None, None, None, self.data_dir.clone())))
            .clone()
    }

    /// `opponent_adjust(ctx, odds=..., ga=..., enabled=...)`: replace the context's table (tests,
    /// or a caller with its own odds source).
    pub fn install_opponent_adjust(&self, ctx: &LeagueContext, adj: OpponentAdjust) -> Arc<OpponentAdjust> {
        let a = Arc::new(adj);
        let mut m = self.opp.lock().unwrap_or_else(|e| e.into_inner());
        m.insert(Self::opp_key(ctx), a.clone());
        a
    }
}

/// `memo_window(ctx)`.
pub fn memo_window(ctx: &LeagueContext, memo: &ScheduleMemo) -> Option<WeekWindow> {
    memo.window(ctx)
}

/// `opponent_adjust(ctx)` (memoized).
pub fn opponent_adjust(ctx: &LeagueContext, memo: &ScheduleMemo) -> Arc<OpponentAdjust> {
    memo.opponent_adjust(ctx)
}

/// `skater_opp_game(ctx, player, day)`: neutral for goalies (they have their own matchup model in
/// `analysis.goalie_plan`).
pub fn skater_opp_game(ctx: &LeagueContext, memo: &ScheduleMemo, player: &Player, day: NaiveDate) -> OppGame {
    if player.is_goalie() {
        return OppGame::default();
    }
    memo.opponent_adjust(ctx).game(ctx, player.team.as_deref(), day)
}

/// `opp_week_factor(ctx, player, window)`: mean opponent multiplier over `player`'s games in the
/// week window (1.0 for goalies, no games or no window). `window` None = the memoized context
/// window. A caller applying it to `proj_week` records it as a Reason with code `OPP_WEEK_REASON`.
pub fn opp_week_factor(ctx: &LeagueContext, memo: &ScheduleMemo, player: &Player, window: Option<&WeekWindow>) -> f64 {
    let owned;
    let window = match window {
        Some(w) => Some(w),
        None => {
            owned = memo.window(ctx);
            owned.as_ref()
        }
    };
    let team = normalize_team(player.team.as_deref());
    let (Some(window), Some(team)) = (window, team) else { return 1.0 };
    if player.is_goalie() {
        return 1.0;
    }
    let days = window_dates(ctx.schedule.get(&team).map(|v| v.as_slice()).unwrap_or(&[]), window.start, window.days);
    if days.is_empty() {
        return 1.0;
    }
    let adj = memo.opponent_adjust(ctx);
    py_sum(days.iter().map(|d| adj.mult(ctx, Some(&team), *d))) / days.len() as f64
}
