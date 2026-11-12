//! Started-games roster value (gap B1 of `docs/knowledge/09-engine-gap-analysis.md`).
//!
//! In a daily-lineup league a bench player scores whenever a starter's NHL team is idle, and a
//! third goalie fills a goalie slot whenever one of the top two has no game. The best starting
//! lineup's summed per-game value counts both at zero. Here a roster is worth, per team game,
//!
//! `sum over players of fpg_season x started / games`,
//!
//! where `games` is how many games his NHL team plays in the next [`START_WINDOW_DAYS`] days and
//! `started` is on how many of those days he is in the best lineup of the players whose team plays
//! that day. Each day's lineup is the trade engine's greedy assignment, which is exact for a total
//! because a player's value does not depend on his slot. A goalie's `fpg_season` is already per
//! team game (start share times points per start), so a goalie in a slot adds his share of that
//! game, and the solve never puts more goalies in than the league has goalie slots.
//!
//! An injured player keeps his availability-discounted rate on every game day (the discount is
//! already in `fpg_season`), so his absence is not counted twice. A weekly-lock league, a missing
//! schedule or a window with no games keep the best-lineup value: there the bench cannot be swapped
//! in during the week, or there is nothing to solve.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Duration;
use fm_core::models::{LeagueContext, Player};
use fm_core::normalize::normalize_team;
use fm_pycompat::num::py_sum;
use fm_pycompat::order::cmp_f64;
use smallvec::SmallVec;

use crate::recommend::lineup_lock;
use crate::recommend::trades::{augment, greedy_assignment, Held, Pid, SlotSpec};
use crate::valuation::params::START_WINDOW_DAYS;

static STARTED_OFF: AtomicBool = AtomicBool::new(false);

/// Turn the started-games value off (true: rosters are worth their best starting lineup's summed
/// per-game value, as in Python) or back on for every later call in this process. Only the Python
/// parity tests and the CLI's hidden `--from-snapshot` mode turn it off.
pub fn disable_started_games(off: bool) {
    STARTED_OFF.store(off, Ordering::Relaxed);
}

/// The window's game days per NHL team, one bit per day from `ctx.as_of`.
#[derive(Clone, Debug)]
pub struct StartWindow {
    masks: HashMap<String, u32>,
    /// the days any team plays
    any: u32,
}

impl StartWindow {
    /// The next [`START_WINDOW_DAYS`] days of `ctx.schedule`; None in a league whose lineups lock
    /// weekly, without a schedule, when no team plays in the window, or after
    /// [`disable_started_games`].
    pub fn of(ctx: &LeagueContext) -> Option<Self> {
        if STARTED_OFF.load(Ordering::Relaxed) || ctx.schedule.is_empty() || lineup_lock::lock(ctx) != "daily" {
            return None;
        }
        let end = ctx.as_of + Duration::days(START_WINDOW_DAYS.min(32));
        let masks: HashMap<String, u32> = ctx
            .schedule
            .iter()
            .map(|(team, dates)| {
                let m = dates
                    .iter()
                    .filter(|d| **d >= ctx.as_of && **d < end)
                    .fold(0u32, |m, d| m | 1 << (*d - ctx.as_of).num_days());
                (team.clone(), m)
            })
            .collect();
        let any = masks.values().fold(0, |a, m| a | m);
        (any != 0).then_some(StartWindow { masks, any })
    }

    /// The days `p`'s NHL team plays in the window: none for a player without a team (he plays no
    /// games), and every game day of the window for a team the schedule does not list (an alias
    /// the schedule lacks, or a partly loaded schedule), so that he keeps about his best-lineup
    /// value instead of vanishing, as `schedule_factor` leaves an unknown team unadjusted.
    pub fn mask(&self, p: &Player) -> u32 {
        match normalize_team(p.team.as_deref()) {
            Some(t) if !t.is_empty() => self.masks.get(&t).copied().unwrap_or(self.any),
            _ => 0,
        }
    }
}

/// Days each of `roster` starts in the window: on each day, the best lineup (the greedy
/// assignment by `fpg`, eligibility `elig`) of the players whose team plays (`masks`), all three
/// indexed by [`Pid`]. Days with the same set of playing players are solved once. `roster` must be
/// sorted ascending without repeats, as a roster memo key is.
pub fn started_days(roster: &[Pid], spec: &SlotSpec, fpg: &[f64], elig: &[u16], masks: &[u32]) -> SmallVec<[u32; 32]> {
    debug_assert!(roster.windows(2).all(|w| w[0] < w[1]));
    let mut starts: SmallVec<[u32; 32]> = SmallVec::from_elem(0, roster.len());
    if roster.len() > 64 {
        // too many players for the bit sets below: solve each day on its own
        for day in 0..32u32 {
            let playing: Vec<Pid> = roster.iter().copied().filter(|&p| masks[p as usize] & (1 << day) != 0).collect();
            for p in greedy_assignment(&playing, spec, fpg, elig).into_iter().flatten() {
                if let Ok(i) = roster.binary_search(&p) {
                    starts[i] += 1;
                }
            }
        }
        return starts;
    }
    // the players playing each day, as a bit set over roster positions
    let mut by_day = [0u64; 32];
    for (i, &p) in roster.iter().enumerate() {
        let mut m = masks[p as usize];
        while m != 0 {
            by_day[m.trailing_zeros() as usize] |= 1 << i;
            m &= m - 1;
        }
    }
    // (set, number of days with that set)
    let mut sets: SmallVec<[(u64, u32); 32]> = SmallVec::new();
    for set in by_day.into_iter().filter(|s| *s != 0) {
        match sets.iter_mut().find(|(s, _)| *s == set) {
            Some((_, n)) => *n += 1,
            None => sets.push((set, 1)),
        }
    }
    // greedy_assignment day by day, sorting once: roster positions by descending fpg (stable, so
    // ties keep roster order as there), dropping players with no positive value, who never start
    let mut order: SmallVec<[usize; 32]> = (0..roster.len()).filter(|&i| fpg[roster[i] as usize] > 0.0).collect();
    order.sort_by(|&a, &b| cmp_f64(fpg[roster[b] as usize], fpg[roster[a] as usize]));
    // a player the greedy accepts stays in the lineup, so the starters are the accepted players
    let mut held: Held = vec![SmallVec::new(); spec.types.len()];
    for &(set, n) in &sets {
        held.iter_mut().for_each(|h| h.clear());
        let (mut dead, mut filled, mut chosen) = (0u32, 0usize, 0u64);
        for &i in &order {
            if filled >= spec.n_slots {
                break;
            }
            let p = roster[i];
            if set & (1 << i) == 0 || (elig[p as usize] as u32) & !dead == 0 {
                continue;
            }
            let mut seen = dead;
            if augment(p, &mut seen, &mut held, &spec.caps, elig) {
                filled += 1;
                chosen |= 1 << i;
            } else {
                dead = seen;
            }
        }
        while chosen != 0 {
            starts[chosen.trailing_zeros() as usize] += n;
            chosen &= chosen - 1;
        }
    }
    starts
}

/// `sum of fpg x started / games` over `roster` with its `starts` (see the module docs). A player
/// whose team has no game in the window adds nothing.
pub fn value_of_starts(roster: &[Pid], starts: &[u32], fpg: &[f64], masks: &[u32]) -> f64 {
    py_sum(
        roster
            .iter()
            .zip(starts.iter())
            .filter(|(_, s)| **s > 0)
            .map(|(&p, &s)| fpg[p as usize] * s as f64 / masks[p as usize].count_ones() as f64),
    )
}

/// The started-games value of `roster` (see the module docs).
pub fn started_value(roster: &[Pid], spec: &SlotSpec, fpg: &[f64], elig: &[u16], masks: &[u32]) -> f64 {
    value_of_starts(roster, &started_days(roster, spec, fpg, elig, masks), fpg, masks)
}

/// [`started_value`] of a list of players valued by `val` (per team game), for callers outside the
/// trade engine (season waivers, the outlook).
pub fn roster_value(win: &StartWindow, spec: &SlotSpec, players: &[&Player], val: impl Fn(&Player) -> f64) -> f64 {
    let fpg: Vec<f64> = players.iter().map(|p| val(p)).collect();
    let elig: Vec<u16> = players.iter().map(|p| spec.elig_mask(p)).collect();
    let masks: Vec<u32> = players.iter().map(|p| win.mask(p)).collect();
    let ids: Vec<Pid> = (0..players.len() as Pid).collect();
    started_value(&ids, spec, &fpg, &elig, &masks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fm_core::Map;

    fn spec(xs: &[(&str, i64)]) -> SlotSpec {
        let shape: Map<String, i64> = xs.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        SlotSpec::new(&shape)
    }

    /// Two goalie slots, three goalies. Days 0-3: A and B play; days 4-5: A and C; day 6: all three.
    /// A starts all 7 of his games, B all 5 of his, C 2 of 3 (C, the weakest, sits on day 6).
    #[test]
    fn third_goalie_starts_when_a_top_goalie_is_idle() {
        let s = spec(&[("G", 2), ("BN", 1)]);
        let fpg = [1.2, 1.0, 0.75];
        let elig = [1u16, 1, 1];
        let masks = [0b111_1111u32, 0b100_1111, 0b111_0000];
        let roster = [0, 1, 2];
        assert_eq!(started_days(&roster, &s, &fpg, &elig, &masks).as_slice(), &[7, 5, 2]);
        // 1.2 x 7/7 + 1.0 x 5/5 + 0.75 x 2/3 = 2.7
        assert!((started_value(&roster, &s, &fpg, &elig, &masks) - 2.7).abs() < 1e-12);
        // the best-lineup sum counted the third goalie at zero: 1.2 + 1.0 = 2.2
        assert!((started_value(&roster[..2], &s, &fpg, &elig, &masks) - 2.2).abs() < 1e-12);
    }

    #[test]
    fn a_player_without_games_adds_nothing() {
        let s = spec(&[("C", 1), ("BN", 1)]);
        let fpg = [1.0, 2.0];
        let elig = [1u16, 1];
        let masks = [0b1u32, 0];
        assert_eq!(started_value(&[0, 1], &s, &fpg, &elig, &masks), 1.0);
    }

    /// The once-sorted solve agrees with a fresh greedy lineup of each day's players, over
    /// pseudo-random rosters with ties, zero values, shared and multi-position eligibility.
    #[test]
    fn started_days_match_a_greedy_lineup_each_day() {
        let s = spec(&[("C", 2), ("LW", 2), ("D", 3), ("G", 2), ("UTIL", 1), ("BN", 4)]);
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let elig_of = |p: &str| s.elig_mask(&crate::recommend::lineup::tests::pl("x", &[p], "healthy"));
        let kinds = [elig_of("C"), elig_of("LW"), elig_of("D"), elig_of("G"), elig_of("C") | elig_of("LW")];
        for _ in 0..300 {
            let n = 6 + next(14) as usize;
            let fpg: Vec<f64> = (0..n).map(|_| next(8) as f64 * 0.25).collect();
            let elig: Vec<u16> = (0..n).map(|_| kinds[next(kinds.len() as u64) as usize]).collect();
            let masks: Vec<u32> = (0..n).map(|_| next(1 << 28) as u32).collect();
            let roster: Vec<Pid> = (0..n as Pid).collect();
            let mut want = vec![0u32; n];
            for day in 0..28 {
                let playing: Vec<Pid> = roster.iter().copied().filter(|&p| masks[p as usize] & (1 << day) != 0).collect();
                for p in greedy_assignment(&playing, &s, &fpg, &elig).into_iter().flatten() {
                    want[p as usize] += 1;
                }
            }
            assert_eq!(started_days(&roster, &s, &fpg, &elig, &masks).as_slice(), want.as_slice());
        }
    }

    /// A team the schedule does not list plays every game day of the window; no team, no days.
    #[test]
    fn unknown_team_plays_every_game_day() {
        let w = StartWindow { masks: [("EDM".to_string(), 0b0101u32), ("TOR".to_string(), 0b0011)].into(), any: 0b0111 };
        let mut p = crate::recommend::lineup::tests::pl("x", &["C"], "healthy");
        assert_eq!(w.mask(&p), 0b0101);
        p.team = Some("XYZ".into());
        assert_eq!(w.mask(&p), 0b0111);
        p.team = None;
        assert_eq!(w.mask(&p), 0);
    }
}
