//! Rust port of `fantasy_manager/report/news_match.py`: attach news items to league players.
//! Owner: work package `W2-nhl-matching` (wave 2).
//!
//! * Items with `player_name` (RotoWire "Player: headline") go through the matcher; only
//!   `exact`/`high` confidence matches are kept.
//! * Items without one (ESPN headlines) are scanned for players' normalized full names appearing
//!   as whole-word substrings of the normalized headline + blurb. Names shared by several players
//!   in the pool are skipped as ambiguous.
//!
//! `NICKNAMES` / `SUFFIXES` are copied from `matching/normalize.py` (fm-core keeps them private).

use std::collections::HashMap;

use fm_core::normalize::{normalize_name, strip_accents};
use fm_core::{Map, Player};
use fm_pycompat::text::{py_len, py_slice, split_ws};
use indexmap::{IndexMap, IndexSet};

use crate::matching::matcher::{Candidate, Key, PlayerIndex};
use crate::news::NewsItem;

const SUFFIXES: [&str; 5] = ["jr", "sr", "ii", "iii", "iv"];
const NICKNAMES: [(&str, &str); 9] = [
    ("alex", "alexander"),
    ("mitch", "mitchell"),
    ("matt", "matthew"),
    ("nick", "nicholas"),
    ("mike", "michael"),
    ("chris", "christopher"),
    ("zach", "zachary"),
    ("josh", "joshua"),
    ("will", "william"),
];

/// `_SHORT_FORMS[full]` = short forms in `NICKNAMES` order.
fn short_forms(full: &str) -> Vec<&'static str> {
    NICKNAMES.iter().filter(|(_, f)| *f == full).map(|(s, _)| *s).collect()
}

/// Accent/punctuation-insensitive token string for substring scans (no nickname rewriting).
pub fn normalize_text(text: Option<&str>) -> String {
    let Some(text) = text.filter(|t| !t.is_empty()) else { return String::new() };
    let lowered = strip_accents(text).to_lowercase();
    let mut s = String::with_capacity(lowered.len());
    for ch in lowered.chars() {
        match ch {
            '\'' | '\u{2019}' | '\u{2018}' | '`' | '\u{b4}' | '.' => {}
            c if c.is_ascii_lowercase() || c.is_ascii_digit() => s.push(c),
            _ => s.push(' '),
        }
    }
    split_ws(&s).join(" ")
}

/// Full-name variants to look for in free text (accents stripped, with/without suffix, canonical
/// and short first-name forms). Single-token names produce no keys.
pub fn name_keys(name: &str) -> IndexSet<String> {
    let mut keys: IndexSet<String> = IndexSet::new();
    let raw_s = normalize_text(Some(name));
    let raw: Vec<&str> = raw_s.split(' ').filter(|t| !t.is_empty()).collect();
    let canon_s = normalize_name(Some(name));
    let canon: Vec<&str> = canon_s.split(' ').filter(|t| !t.is_empty()).collect();
    let nosuf: Vec<&str> = raw.iter().copied().filter(|t| !SUFFIXES.contains(t)).collect();
    for toks in [&raw, &nosuf, &canon] {
        if toks.len() >= 2 {
            keys.insert(toks.join(" "));
            for short in short_forms(toks[0]) {
                let mut v = vec![short];
                v.extend_from_slice(&toks[1..]);
                keys.insert(v.join(" "));
            }
        }
    }
    keys
}

fn candidate(p: &Player) -> Candidate {
    let pos = p.positions.join("/");
    Candidate {
        key: Key::Str(p.cid.clone()),
        name: p.name.clone(),
        team: p.team.clone(),
        position: if pos.is_empty() { None } else { Some(pos) },
    }
}

/// Sort key for "newest first": `n.published or datetime.min(UTC)`.
fn published_key(n: &NewsItem) -> Option<chrono::NaiveDateTime> {
    n.published.map(|p| p.to_utc().map(|u| u.naive_utc()).unwrap_or(p.local))
}

/// Stable newest-first sort (Python `sorted(..., reverse=True)` keeps the order of equal keys).
fn sort_newest_first(items: &mut [NewsItem]) {
    items.sort_by(|a, b| published_key(b).cmp(&published_key(a)));
}

/// `match_news_to_players(news, players)` -> cid -> items about that player (only exact/high
/// confidence matches). Each cid's list is de-duplicated and sorted newest first.
pub fn match_news_to_players(news: &[NewsItem], players: &[&Player]) -> Map<String, Vec<NewsItem>> {
    let mut uniq: IndexMap<&str, &Player> = IndexMap::new();
    for p in players {
        uniq.entry(p.cid.as_str()).or_insert(p);
    }
    if news.is_empty() || uniq.is_empty() {
        return Map::new();
    }
    let index = PlayerIndex::new(uniq.values().map(|p| candidate(p)));

    let mut key_owners: IndexMap<String, IndexSet<&str>> = IndexMap::new();
    for p in uniq.values() {
        for k in name_keys(&p.name) {
            key_owners.entry(k).or_default().insert(p.cid.as_str());
        }
    }
    let unique_keys: HashMap<String, &str> = key_owners
        .iter()
        .filter(|(_, cids)| cids.len() == 1)
        .map(|(k, cids)| (k.clone(), *cids.iter().next().expect("one owner")))
        .collect();
    // group keys by token count so the scan is a handful of n-gram set lookups per item
    let mut lengths: Vec<usize> = unique_keys.keys().map(|k| k.split_whitespace().count()).collect();
    lengths.sort_unstable();
    lengths.dedup();

    let mut out: IndexMap<String, Vec<NewsItem>> = IndexMap::new();
    let mut seen: HashMap<String, IndexSet<String>> = HashMap::new();

    let mut add = |cid: &str, item: &NewsItem| {
        let ident = match item.id.as_deref().filter(|s| !s.is_empty()) {
            Some(i) => i.to_string(),
            None => format!(
                "{}|{}|{}",
                item.source,
                item.headline,
                item.published.map(|p| p.py_str()).unwrap_or_else(|| "None".to_string())
            ),
        };
        if seen.entry(cid.to_string()).or_default().insert(ident) {
            out.entry(cid.to_string()).or_default().push(item.clone());
        }
    };

    for item in news {
        if let Some(pn) = item.player_name.as_deref().filter(|s| !s.is_empty()) {
            let res = index.find(pn, None, None);
            if res.matched() {
                if let Some(Key::Str(cid)) = &res.key {
                    add(cid, item);
                }
            }
            continue;
        }
        let text = format!("{} {}", item.headline, item.blurb);
        let norm = normalize_text(Some(&text));
        let tokens: Vec<&str> = norm.split(' ').filter(|t| !t.is_empty()).collect();
        let mut hits: IndexSet<&str> = IndexSet::new();
        for &n in &lengths {
            if tokens.len() < n {
                continue;
            }
            for i in 0..=(tokens.len() - n) {
                if let Some(cid) = unique_keys.get(&tokens[i..i + n].join(" ")) {
                    hits.insert(cid);
                }
            }
        }
        let mut hits: Vec<&str> = hits.into_iter().collect();
        hits.sort_unstable();
        for cid in hits {
            add(cid, item);
        }
    }

    out.into_iter()
        .map(|(cid, mut items)| {
            sort_newest_first(&mut items);
            (cid, items)
        })
        .collect()
}

/// `player_news_summary(items, limit=3, blurb_chars=200)`: short one-line summaries, newest
/// first: `'2026-09-27 (rotowire): headline -- blurb'`.
pub fn player_news_summary(items: &[NewsItem], limit: usize, blurb_chars: usize) -> Vec<String> {
    let mut sorted: Vec<NewsItem> = items.to_vec();
    sort_newest_first(&mut sorted);
    let mut lines = Vec::new();
    for n in sorted.iter().take(limit) {
        let when = match &n.published {
            Some(p) => p.date().format("%Y-%m-%d").to_string(),
            None => "undated".to_string(),
        };
        let mut blurb = split_ws(&n.blurb).join(" ");
        if py_len(&blurb) > blurb_chars {
            let cut = py_slice(&blurb, None, Some(blurb_chars as isize - 1));
            blurb = format!("{}\u{2026}", cut.trim_end());
        }
        let head = split_ws(&n.headline).join(" ");
        let mut line = format!("{when} ({}): {head}", n.source);
        if !blurb.is_empty() {
            line.push_str(" -- ");
            line.push_str(&blurb);
        }
        lines.push(line);
    }
    lines
}
