//! Rust port of `fantasy_manager/valuation/replacement.py`: replacement level from the free-agent
//! pool and value over replacement (VORP).
//! Owner: work package `W2-valuation-base` (wave 2).
//!
//! `players: Mapping[str, Player]` is any [`PlayerLookup`] (`Map`/`HashMap` of owned or borrowed
//! players). Functions taking `positions: Iterable[str]` accept any slice of `AsRef<str>`.

use std::collections::HashMap;

use fm_core::models::Player;
use fm_core::{Map, Set};
use fm_pycompat::num::py_sum;
use fm_pycompat::order::{cmp_f64, first_min_by};

pub const DEFAULT_SLOTS: [&str; 5] = ["C", "LW", "RW", "D", "G"];
pub const FORWARDS: [&str; 3] = ["C", "LW", "RW"];
pub const TOP_N: usize = 3;
pub const ROSTERED_FALLBACK_PCT: f64 = 0.10;

/// `players.get(cid)` over the mapping types callers hold.
pub trait PlayerLookup {
    fn get_player(&self, cid: &str) -> Option<&Player>;
}
impl PlayerLookup for Map<String, Player> {
    fn get_player(&self, cid: &str) -> Option<&Player> {
        self.get(cid)
    }
}
impl PlayerLookup for HashMap<String, Player> {
    fn get_player(&self, cid: &str) -> Option<&Player> {
        self.get(cid)
    }
}
impl PlayerLookup for Map<String, &Player> {
    fn get_player(&self, cid: &str) -> Option<&Player> {
        self.get(cid).copied()
    }
}
impl PlayerLookup for HashMap<String, &Player> {
    fn get_player(&self, cid: &str) -> Option<&Player> {
        self.get(cid).copied()
    }
}

/// Slots a player with `positions` can fill. A generic 'F' counts for C/LW/RW.
pub fn eligible_slots<'a, P, S>(positions: &[P], slots: &'a [S]) -> Vec<&'a str>
where
    P: AsRef<str>,
    S: AsRef<str>,
{
    eligible_slots_iter(positions, slots.iter().map(|s| s.as_ref()))
}

/// [`eligible_slots`] over any iterator of slot names (e.g. a map's keys).
pub fn eligible_slots_iter<'a, P, I>(positions: &[P], slots: I) -> Vec<&'a str>
where
    P: AsRef<str>,
    I: IntoIterator<Item = &'a str>,
{
    let has = |x: &str, extra_f: bool| -> bool {
        positions.iter().any(|p| p.as_ref() == x) || (extra_f && FORWARDS.contains(&x) && has_generic_f(positions))
    };
    let is_skater = positions.iter().any(|p| p.as_ref() != "G");
    let mut out = Vec::new();
    for s in slots {
        let ok = if s == "F" {
            // pos & {C, LW, RW, F} (pos already widened by the generic-F rule)
            ["C", "LW", "RW", "F"].iter().any(|x| has(x, true))
        } else if s == "UTIL" {
            is_skater
        } else {
            has(s, true)
        };
        if ok {
            out.push(s);
        }
    }
    out
}

/// `"F" in pos and not pos & {C, LW, RW}`: the generic forward that fills C/LW/RW.
fn has_generic_f<P: AsRef<str>>(positions: &[P]) -> bool {
    positions.iter().any(|p| p.as_ref() == "F") && !positions.iter().any(|p| FORWARDS.contains(&p.as_ref()))
}

/// Mean of the top-3 free-agent values per slot; missing entries count as 0 (monotone).
pub fn replacement_levels<S: AsRef<str>>(
    free_agents_values: &Map<String, f64>,
    players: &dyn PlayerLookup,
    slots: &[S],
) -> Map<String, f64> {
    let mut by_slot: Map<String, Vec<f64>> = Map::new();
    for s in slots {
        by_slot.entry(s.as_ref().to_string()).or_default();
    }
    for (cid, val) in free_agents_values {
        let Some(p) = players.get_player(cid) else { continue };
        for s in eligible_slots(&p.positions, slots) {
            by_slot.get_mut(s).unwrap().push(*val);
        }
    }
    let mut out = Map::new();
    for (s, mut vals) in by_slot {
        vals.extend(std::iter::repeat(0.0).take(TOP_N));
        // sorted(..., reverse=True)[:3]
        vals.sort_by(|a, b| cmp_f64(*b, *a));
        vals.truncate(TOP_N);
        out.insert(s, py_sum(vals.iter().copied()) / TOP_N as f64);
    }
    out
}

pub fn default_slots() -> Vec<String> {
    DEFAULT_SLOTS.iter().map(|s| s.to_string()).collect()
}

/// Value minus the smallest replacement level among the player's eligible slots.
pub fn vorp<P: AsRef<str>>(value: f64, positions: &[P], repl: &Map<String, f64>) -> f64 {
    let elig = eligible_slots_iter(positions, repl.keys().map(|k| k.as_str()));
    if elig.is_empty() {
        return value;
    }
    let m = elig.iter().map(|s| repl[*s]).fold(f64::INFINITY, |a, b| if b < a { b } else { a });
    value - m
}

pub fn best_slot<P: AsRef<str>>(positions: &[P], repl: &Map<String, f64>) -> Option<String> {
    let elig = eligible_slots_iter(positions, repl.keys().map(|k| k.as_str()));
    first_min_by(elig, |a, b| cmp_f64(repl[*a], repl[*b])).map(|s| s.to_string())
}

/// Linear-interpolated q-quantile (0..1) of vals; 0.0 when empty.
pub fn percentile(vals: impl IntoIterator<Item = f64>, q: f64) -> f64 {
    let mut xs: Vec<f64> = vals.into_iter().collect();
    xs.sort_by(|a, b| cmp_f64(*a, *b));
    if xs.is_empty() {
        return 0.0;
    }
    let pos = q * (xs.len() - 1) as f64;
    let lo = pos.trunc() as usize;
    let hi = (lo + 1).min(xs.len() - 1);
    xs[lo] + (xs[hi] - xs[lo]) * (pos - lo as f64)
}

/// (levels, fallback slots). Top-3 free-agent mean per slot; a slot with no eligible free agent at
/// all falls back to the `pct` quantile of the rostered players eligible there instead of 0.
pub fn replacement_with_fallback<S: AsRef<str>>(
    free_agents_values: &Map<String, f64>,
    rostered_values: &Map<String, f64>,
    players: &dyn PlayerLookup,
    slots: &[S],
    pct: f64,
) -> (Map<String, f64>, Set<String>) {
    let mut levels = replacement_levels(free_agents_values, players, slots);
    let mut covered: Set<String> = Set::new();
    for cid in free_agents_values.keys() {
        if let Some(p) = players.get_player(cid) {
            for s in eligible_slots(&p.positions, slots) {
                covered.insert(s.to_string());
            }
        }
    }
    let mut fallback: Set<String> = Set::new();
    for s in slots {
        let s = s.as_ref();
        if covered.contains(s) {
            continue;
        }
        let one = [s];
        let vals: Vec<f64> = rostered_values
            .iter()
            .filter(|(cid, _)| {
                players.get_player(cid).map(|p| !eligible_slots(&p.positions, &one).is_empty()).unwrap_or(false)
            })
            .map(|(_, v)| *v)
            .collect();
        if !vals.is_empty() {
            levels.insert(s.to_string(), percentile(vals, pct));
            fallback.insert(s.to_string());
        }
    }
    (levels, fallback)
}
