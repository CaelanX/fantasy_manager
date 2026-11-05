//! `_starting_slots`, `_greedy_assignment`, `_greedy_lineup_value` and the roster-keyed memos of
//! the trade engine (Python's `frozenset`-of-cids caches).
//!
//! Players are `u32` indices into the engine's per-league player table; a roster memo key is the
//! sorted, de-duplicated index list (`RosterKey`). Memoized results are computed from the players
//! in key order, so every memo is a pure function of its key and the shared (DashMap) memos give
//! the same results whatever thread fills them first. Python computes a cached roster from the
//! list order of the first call; the two can differ only when two players have exactly equal
//! positive season FPG at the lineup margin (which of them starts), never in a total.

use std::hash::{BuildHasherDefault, Hasher};

use fm_core::{Map, Player};
use fm_pycompat::num::py_sum;
use fm_pycompat::order::cmp_f64;
use smallvec::SmallVec;

use crate::valuation::replacement::eligible_slots;

/// Player index into the engine's player table.
pub type Pid = u32;
/// Sorted, de-duplicated player indices: the memo key of a roster.
pub(crate) type RosterKey = SmallVec<[Pid; 28]>;

pub(crate) const NON_STARTING: [&str; 4] = ["BN", "IR", "IR+", "NA"];

/// `_SLOT_RANK`: specific slots first, then F, then UTIL.
fn slot_rank(s: &str) -> u8 {
    match s {
        "F" => 1,
        "UTIL" => 2,
        _ => 0,
    }
}

/// `_starting_slots(roster_shape)`: starting slots, specific ones first, then F, then UTIL.
pub fn starting_slots(roster_shape: &Map<String, i64>) -> Vec<String> {
    let mut slots: Vec<String> = Vec::new();
    for (s, n) in roster_shape {
        if NON_STARTING.contains(&s.as_str()) {
            continue;
        }
        for _ in 0..(*n).max(0) {
            slots.push(s.clone());
        }
    }
    slots.sort_by_key(|s| slot_rank(s));
    slots
}

/// `_eligible(positions, slot)`.
pub fn eligible(positions: &[String], slot: &str) -> bool {
    !eligible_slots(positions, &[slot]).is_empty()
}

/// The starting slot structure: slot types in `_starting_slots` order with their capacities.
#[derive(Clone, Debug)]
pub struct SlotSpec {
    pub types: Vec<String>,
    pub caps: Vec<usize>,
    pub n_slots: usize,
}

impl SlotSpec {
    pub fn new(roster_shape: &Map<String, i64>) -> Self {
        let slots = starting_slots(roster_shape);
        let mut types: Vec<String> = Vec::new();
        let mut caps: Vec<usize> = Vec::new();
        for s in &slots {
            match types.iter().position(|t| t == s) {
                Some(i) => caps[i] += 1,
                None => {
                    types.push(s.clone());
                    caps.push(1);
                }
            }
        }
        assert!(types.len() <= 16, "at most 16 starting slot types");
        SlotSpec { types, caps, n_slots: slots.len() }
    }

    /// Bit mask of the slot types `p` may start at (`types_for(p)`).
    pub fn elig_mask(&self, p: &Player) -> u16 {
        let mut m = 0u16;
        for (i, t) in self.types.iter().enumerate() {
            if eligible(&p.positions, t) {
                m |= 1 << i;
            }
        }
        m
    }

    pub fn type_index(&self, t: &str) -> Option<usize> {
        self.types.iter().position(|x| x == t)
    }
}

/// Per-type starters: `held[t]` in assignment order.
pub type Held = Vec<SmallVec<[Pid; 8]>>;

fn augment(p: Pid, seen: &mut u32, held: &mut Held, caps: &[usize], elig: &[u16]) -> bool {
    let nt = caps.len();
    let ts = (elig[p as usize] as u32) & !*seen;
    for t in 0..nt {
        if ts & (1 << t) != 0 && held[t].len() < caps[t] {
            held[t].push(p);
            return true;
        }
    }
    for t in 0..nt {
        if ts & (1 << t) == 0 || *seen & (1 << t) != 0 {
            continue;
        }
        *seen |= 1 << t;
        let mut i = 0;
        while i < held[t].len() {
            let q = held[t][i];
            if augment(q, seen, held, caps, elig) {
                held[t][i] = p;
                return true;
            }
            i += 1;
        }
    }
    false
}

/// `_greedy_assignment(players)`: the starting lineup as per-type lists (BN/IR never start).
/// Greedy by season FPG with an augmenting-path feasibility check over slot types with
/// capacities, pruning saturated ("dead") types - the Python algorithm step by step.
pub fn greedy_assignment(players: &[Pid], spec: &SlotSpec, fpg: &[f64], elig: &[u16]) -> Held {
    let nt = spec.types.len();
    let mut held: Held = vec![SmallVec::new(); nt];
    let mut order: SmallVec<[Pid; 32]> = SmallVec::from_slice(players);
    // sorted(..., reverse=True): stable descending
    order.sort_by(|a, b| cmp_f64(fpg[*b as usize], fpg[*a as usize]));
    let mut dead: u32 = 0;
    let mut filled = 0usize;
    for &p in &order {
        if filled >= spec.n_slots || fpg[p as usize] <= 0.0 {
            break;
        }
        if (elig[p as usize] as u32) & !dead == 0 {
            continue;
        }
        let mut seen = dead;
        if augment(p, &mut seen, &mut held, &spec.caps, elig) {
            filled += 1;
        } else {
            dead = seen;
        }
    }
    held
}

/// Memoized summary of a roster's greedy lineup.
#[derive(Clone, Debug)]
pub(crate) struct GreedySummary {
    /// `_greedy_lineup_value`
    pub value: f64,
    /// starters (`_starters`), sorted
    pub starters: SmallVec<[Pid; 24]>,
    /// starters per slot type
    pub counts: SmallVec<[u8; 8]>,
}

impl GreedySummary {
    pub fn is_starter(&self, p: Pid) -> bool {
        self.starters.binary_search(&p).is_ok()
    }
}

pub(crate) fn summarize(held: &Held, fpg: &[f64]) -> GreedySummary {
    let value = py_sum(held.iter().flat_map(|h| h.iter()).map(|&p| fpg[p as usize]));
    let mut starters: SmallVec<[Pid; 24]> = held.iter().flat_map(|h| h.iter().copied()).collect();
    starters.sort_unstable();
    let counts = held.iter().map(|h| h.len() as u8).collect();
    GreedySummary { value, starters, counts }
}

/// The memo key of a roster: sorted, de-duplicated indices.
pub(crate) fn key_of<'x>(parts: impl IntoIterator<Item = &'x [Pid]>) -> RosterKey {
    let mut k: RosterKey = SmallVec::new();
    for part in parts {
        k.extend_from_slice(part);
    }
    k.sort_unstable();
    k.dedup();
    k
}

/// FxHash (rustc's hasher): fast hashing of small integer keys for the shared memos.
#[derive(Default, Clone, Copy)]
pub(crate) struct FxHasher {
    hash: u64,
}

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.hash = (self.hash.rotate_left(5) ^ w).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().expect("8 bytes")));
        }
        for &b in chunks.remainder() {
            self.add(b as u64);
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub(crate) type FxBuild = BuildHasherDefault<FxHasher>;
pub(crate) type Memo<K, V> = dashmap::DashMap<K, V, FxBuild>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_slots_order_specific_then_f_then_util() {
        let mut shape: Map<String, i64> = Map::new();
        for (k, v) in [("UTIL", 1), ("F", 2), ("BN", 3), ("D", 2), ("G", 1), ("IR", 1)] {
            shape.insert(k.to_string(), v);
        }
        assert_eq!(starting_slots(&shape), vec!["D", "D", "G", "F", "F", "UTIL"]);
    }
}
