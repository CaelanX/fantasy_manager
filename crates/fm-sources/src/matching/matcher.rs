//! Rust port of `fantasy_manager/matching/matcher.py`: fuzzy player matching across sources.
//! Owner: work package `W2-nhl-matching` (wave 2).
//!
//! Rules (`PlayerIndex::match_`):
//!   1. exact normalized name, team agrees               -> "exact"
//!   2. exact normalized name, unique among candidates   -> "exact"
//!      (several exact-name hits are tie-broken by team, then position -> "high"; else "pending")
//!   3. rapidfuzz token_sort_ratio >= 92 and team or position agrees -> "high"
//!   4. score in [85, 92), or >= 92 without any agreement            -> "pending"
//!   5. otherwise                                                     -> "none"
//!
//! The fuzzy step reproduces `rapidfuzz.process.extract(q, norm, scorer=fuzz.token_sort_ratio,
//! limit=8, score_cutoff=85)` (rapidfuzz 3.14): tokens split on whitespace and sorted, then the
//! normalized Indel similarity (`1 - (len1 + len2 - 2 * LCS) / (len1 + len2)`) times 100; the
//! cutoff test is done on the 0..1 similarity (`>= 0.85`) like rapidfuzz; hits are ordered by
//! score descending then candidate index ascending. Python's `match` method is `match_` here
//! (`match` is a Rust keyword).

use std::collections::HashMap;

use fm_core::normalize::{normalize_name, normalize_team, positions_agree};
use fm_pycompat::num::round_to;
use serde::{Deserialize, Serialize};

pub const HIGH_THRESHOLD: f64 = 92.0;
pub const PENDING_THRESHOLD: f64 = 85.0;
pub const FUZZY_LIMIT: usize = 8;

/// `Candidate.key` is `Any` in Python: an NHL player id (int) or a cid (str).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Key {
    Int(i64),
    Str(String),
}

impl Key {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Key::Int(i) => Some(*i),
            Key::Str(s) => s.trim().parse().ok(),
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Key::Str(s) => Some(s),
            Key::Int(_) => None,
        }
    }
}

impl From<i64> for Key {
    fn from(v: i64) -> Self {
        Key::Int(v)
    }
}
impl From<&str> for Key {
    fn from(v: &str) -> Self {
        Key::Str(v.to_string())
    }
}
impl From<String> for Key {
    fn from(v: String) -> Self {
        Key::Str(v)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Exact,
    High,
    Pending,
    #[default]
    None,
}

impl Confidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Confidence::Exact => "exact",
            Confidence::High => "high",
            Confidence::Pending => "pending",
            Confidence::None => "none",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub key: Key,
    pub name: String,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub position: Option<String>,
}

impl Candidate {
    pub fn new(key: impl Into<Key>, name: &str, team: Option<&str>, position: Option<&str>) -> Self {
        Candidate {
            key: key.into(),
            name: name.to_string(),
            team: team.map(str::to_string),
            position: position.map(str::to_string),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct MatchResult {
    pub key: Option<Key>,
    pub name: Option<String>,
    pub score: f64,
    pub confidence: Confidence,
}

impl MatchResult {
    /// True for confident matches that can be auto-accepted.
    pub fn matched(&self) -> bool {
        matches!(self.confidence, Confidence::Exact | Confidence::High)
    }
}

fn team_agrees(a: Option<&str>, b: Option<&str>) -> bool {
    match (normalize_team(a), normalize_team(b)) {
        (Some(ta), Some(tb)) => !ta.is_empty() && !tb.is_empty() && ta == tb,
        _ => false,
    }
}

// --------------------------------------------------------------------------- token_sort_ratio

/// `" ".join(sorted(s.split()))` (rapidfuzz `sorted_split`; tokens ordered by code point).
pub fn sorted_tokens(s: &str) -> String {
    let mut toks: Vec<&str> = s.split_whitespace().collect();
    toks.sort_unstable();
    toks.join(" ")
}

/// Length of the longest common subsequence of two strings (by code point).
fn lcs_len(a: &[u8], b: &[u8], a_masks: Option<&[u64; 256]>) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    if a.len() <= 64 {
        let local;
        let pm = match a_masks {
            Some(m) => m,
            None => {
                local = pattern_masks(a);
                &local
            }
        };
        // Hyyro bit-parallel LCS
        let mut v: u64 = !0;
        for &c in b {
            let u = v & pm[c as usize];
            v = v.wrapping_add(u) | (v & !pm[c as usize]);
        }
        let mask = if a.len() == 64 { !0u64 } else { (1u64 << a.len()) - 1 };
        return (!v & mask).count_ones() as usize;
    }
    // plain DP
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for &ca in a {
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = if ca == cb { prev[j] + 1 } else { cur[j].max(prev[j + 1]) };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn pattern_masks(a: &[u8]) -> [u64; 256] {
    let mut pm = [0u64; 256];
    for (i, &c) in a.iter().enumerate() {
        pm[c as usize] |= 1u64 << i;
    }
    pm
}

fn char_lcs(a: &str, b: &str) -> (usize, usize, usize) {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let mut prev = vec![0usize; bc.len() + 1];
    let mut cur = vec![0usize; bc.len() + 1];
    for &ca in &ac {
        for (j, &cb) in bc.iter().enumerate() {
            cur[j + 1] = if ca == cb { prev[j] + 1 } else { cur[j].max(prev[j + 1]) };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    (prev[bc.len()], ac.len(), bc.len())
}

/// Normalized Indel similarity in 0..1 as rapidfuzz computes it (`1.0 - dist / (len1 + len2)`).
fn indel_norm_sim(a: &str, b: &str, a_masks: Option<&[u64; 256]>) -> f64 {
    let (lcs, la, lb) = if a.is_ascii() && b.is_ascii() {
        (lcs_len(a.as_bytes(), b.as_bytes(), a_masks), a.len(), b.len())
    } else {
        char_lcs(a, b)
    };
    let maximum = la + lb;
    let dist = maximum - 2 * lcs;
    let norm_dist = if maximum == 0 { 0.0 } else { dist as f64 / maximum as f64 };
    1.0 - norm_dist
}

/// `fuzz.token_sort_ratio(a, b)` (processor=None), 0..100.
pub fn token_sort_ratio(a: &str, b: &str) -> f64 {
    indel_norm_sim(&sorted_tokens(a), &sorted_tokens(b), None) * 100.0
}

/// `fuzz.token_sort_ratio(a, b, score_cutoff=c)`: 0.0 below the cutoff (c in 0..100).
pub fn token_sort_ratio_cutoff(a: &str, b: &str, score_cutoff: f64) -> f64 {
    let sim = indel_norm_sim(&sorted_tokens(a), &sorted_tokens(b), None);
    if sim >= score_cutoff / 100.0 {
        sim * 100.0
    } else {
        0.0
    }
}

// --------------------------------------------------------------------------- index

/// Pre-normalized candidate pool for repeated lookups.
#[derive(Clone, Debug)]
pub struct PlayerIndex {
    pub candidates: Vec<Candidate>,
    pub norm: Vec<String>,
    sorted: Vec<String>,
    by_norm: HashMap<String, Vec<usize>>,
}

impl PlayerIndex {
    pub fn new<I: IntoIterator<Item = Candidate>>(candidates: I) -> Self {
        let candidates: Vec<Candidate> = candidates.into_iter().collect();
        let norm: Vec<String> = candidates.iter().map(|c| normalize_name(Some(&c.name))).collect();
        let sorted: Vec<String> = norm.iter().map(|n| sorted_tokens(n)).collect();
        let mut by_norm: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, n) in norm.iter().enumerate() {
            by_norm.entry(n.clone()).or_default().push(i);
        }
        PlayerIndex { candidates, norm, sorted, by_norm }
    }

    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    fn result(&self, i: usize, score: f64, conf: Confidence) -> MatchResult {
        let c = &self.candidates[i];
        MatchResult { key: Some(c.key.clone()), name: Some(c.name.clone()), score: round_to(score, 2), confidence: conf }
    }

    /// `process.extract(q, self.norm, scorer=token_sort_ratio, limit, score_cutoff)` ->
    /// `(score, index)` ordered by score descending, then index ascending.
    fn extract(&self, q: &str, limit: usize, score_cutoff: f64) -> Vec<(f64, usize)> {
        let qs = sorted_tokens(q);
        let cut = score_cutoff / 100.0;
        let masks = if qs.is_ascii() && qs.len() <= 64 { Some(pattern_masks(qs.as_bytes())) } else { None };
        let mut hits: Vec<(f64, usize)> = Vec::new();
        for (i, cs) in self.sorted.iter().enumerate() {
            let (la, lb) = (qs.len(), cs.len());
            let lensum = la + lb;
            // |len diff| is a lower bound of the Indel distance: skip hopeless candidates
            if lensum > 0 && (la.abs_diff(lb) as f64 / lensum as f64) > (1.0 - cut) + 1e-9 {
                continue;
            }
            let sim = indel_norm_sim(&qs, cs, masks.as_ref());
            if sim >= cut {
                hits.push((sim * 100.0, i));
            }
        }
        hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
        hits.truncate(limit);
        hits
    }

    /// `PlayerIndex.match(query_name, team, position, fuzzy_limit)`.
    pub fn match_(&self, query_name: &str, team: Option<&str>, position: Option<&str>, fuzzy_limit: usize) -> MatchResult {
        let q = normalize_name(Some(query_name));
        if q.is_empty() || self.candidates.is_empty() {
            return MatchResult::default();
        }

        // ---- exact normalized-name hits
        if let Some(exact) = self.by_norm.get(&q).filter(|v| !v.is_empty()) {
            if exact.len() == 1 {
                return self.result(exact[0], 100.0, Confidence::Exact);
            }
            let team_hits: Vec<usize> =
                exact.iter().copied().filter(|&i| team_agrees(team, self.candidates[i].team.as_deref())).collect();
            if team_hits.len() == 1 {
                return self.result(team_hits[0], 100.0, Confidence::Exact);
            }
            let pool: &Vec<usize> = if team_hits.is_empty() { exact } else { &team_hits };
            let pos_hits: Vec<usize> =
                pool.iter().copied().filter(|&i| positions_agree(position, self.candidates[i].position.as_deref())).collect();
            if pos_hits.len() == 1 {
                let conf = if !team_hits.is_empty() { Confidence::Exact } else { Confidence::High };
                return self.result(pos_hits[0], 100.0, conf);
            }
            // genuinely ambiguous duplicate name
            let pick = if pos_hits.is_empty() { pool[0] } else { pos_hits[0] };
            return self.result(pick, 100.0, Confidence::Pending);
        }

        // ---- fuzzy
        let hits = self.extract(&q, fuzzy_limit, PENDING_THRESHOLD);
        if hits.is_empty() {
            return MatchResult::default();
        }
        // max(hits, key=rank): first maximum wins
        let mut best: Option<(f64, f64, usize)> = None;
        for &(score, i) in &hits {
            let c = &self.candidates[i];
            let bonus = 3.0 * (team_agrees(team, c.team.as_deref()) as u8 as f64)
                + 1.0 * (positions_agree(position, c.position.as_deref()) as u8 as f64);
            let rank = (score + bonus, score);
            let better = match best {
                None => true,
                Some((r0, r1, _)) => rank.0 > r0 || (rank.0 == r0 && rank.1 > r1),
            };
            if better {
                best = Some((rank.0, rank.1, i));
            }
        }
        let (_, score, i) = best.expect("non-empty hits");
        let c = &self.candidates[i];
        let agrees = team_agrees(team, c.team.as_deref()) || positions_agree(position, c.position.as_deref());
        if score >= HIGH_THRESHOLD && agrees {
            return self.result(i, score, Confidence::High);
        }
        self.result(i, score, Confidence::Pending)
    }

    /// `match` with the default `fuzzy_limit=8`.
    pub fn find(&self, query_name: &str, team: Option<&str>, position: Option<&str>) -> MatchResult {
        self.match_(query_name, team, position, FUZZY_LIMIT)
    }

    pub fn match_many(&self, queries: &[(&str, Option<&str>, Option<&str>)]) -> Vec<MatchResult> {
        queries.iter().map(|(n, t, p)| self.find(n, *t, *p)).collect()
    }
}

pub fn build_index<I: IntoIterator<Item = Candidate>>(candidates: I) -> PlayerIndex {
    PlayerIndex::new(candidates)
}

/// `match_player(query_name, candidates, team, position)` on a prebuilt index.
pub fn match_player(query_name: &str, index: &PlayerIndex, team: Option<&str>, position: Option<&str>) -> MatchResult {
    index.find(query_name, team, position)
}

/// `match_player` on a raw candidate list (builds an index per call).
pub fn match_player_in(query_name: &str, candidates: &[Candidate], team: Option<&str>, position: Option<&str>) -> MatchResult {
    PlayerIndex::new(candidates.iter().cloned()).find(query_name, team, position)
}

/// `candidates_from(items, key, name, team, position)` for JSON-object items (dicts).
pub fn candidates_from(items: &[serde_json::Value], key: &str, name: &str, team: &str, position: &str) -> Vec<Candidate> {
    items
        .iter()
        .map(|it| {
            let s = |k: &str| it.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
            let k = match it.get(key) {
                Some(serde_json::Value::Number(n)) if n.is_i64() => Key::Int(n.as_i64().unwrap_or(0)),
                Some(serde_json::Value::String(s)) => Key::Str(s.clone()),
                _ => Key::Str(String::new()),
            };
            Candidate { key: k, name: s(name).unwrap_or_default(), team: s(team), position: s(position) }
        })
        .collect()
}
