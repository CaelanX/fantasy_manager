//! Rust port of `fantasy_manager/harness/match.py` (renamed: `match` is a keyword): match
//! recommendation episodes to what I actually did.
//!
//! Owner: work package `W2-harness-ledger-stats`.
//!
//! `match_rows` is pure (plain structs in, updates out) so every rule is unit-testable;
//! `match_episodes` loads the ledger rows, runs it and writes the results back. Statuses are
//! recomputed from scratch on every run (the inputs are append-only), so matching is idempotent.
//!
//! Rules (window = first_seen .. window_end; only my own transactions count): waiver (last_seen
//! + 3d, my ADD of an `add` player: `followed` when the drops of that transaction equal the
//! rec's, else `partial`), trade (+7d, a completed trade sharing a give and a get: `followed`
//! when both sets are equal else `partial`; only a matching proposal: `proposed`), lineup (graded
//! on the first lockable day: ESPN the day itself, Fantrax the next Monday using the first roster
//! snapshot after the lock), injury (+2d, IR move and/or add; "Activate X" recs), flags
//! (sell_high / buy_low, +14d) and alerts (free-agent alerts +3d, negative alerts about my own
//! player +7d, everything else informational and never graded). Unmatched episodes turn
//! `expired` once `today` is past `window_end`. Matched episodes become decisions; my transaction
//! groups no episode consumed become `decisions(origin=user_only)`.
//!
//! Deviations from the Python: sets are `BTreeSet`s (only ever compared or emitted sorted, so
//! the order is the same as Python's `sorted(...)`); the case-insensitive title regex of
//! `alert_category` is spelled out with `to_lowercase()` (all its alternatives are ASCII).

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use indexmap::IndexMap;
use serde_json::{json, Value};

use crate::ledger::{json_dumps, now_iso, Ledger, Row};

pub type Detail = serde_json::Map<String, Value>;

pub const ALERT_FA_DAYS: i64 = 3; // a free-agent alert followed by my add within last_seen + 3d
pub const ALERT_MINE_DAYS: i64 = 7; // a negative alert on my player followed by my drop / trade within last_seen + 7d
pub const FA_COUNTERPARTY: &str = "FA";
pub const TERMINAL: [&str; 2] = ["followed", "partial"];

/// `WINDOW_DAYS` lookup (waiver 3, trade 7, injury 2, sell_high 14, buy_low 14); other kinds 3.
fn window_days(kind: &str) -> i64 {
    match kind {
        "waiver" => 3,
        "trade" => 7,
        "injury" => 2,
        "sell_high" | "buy_low" => 14,
        _ => 3,
    }
}

/// `LINEUP_GRACE.get(provider, 2)`.
fn lineup_grace(provider: &str) -> i64 {
    match provider {
        "espn" => 2,
        "fantrax" => 7,
        _ => 2,
    }
}

#[derive(Clone, Debug)]
pub struct EpisodeRow {
    pub episode_id: String,
    pub kind: String,
    pub first_seen: NaiveDate,
    pub last_seen: NaiveDate,
    pub title: String,
    pub adds: BTreeSet<String>,
    pub drops: BTreeSet<String>,
    pub subjects: BTreeSet<String>,
    pub counterparty: Option<String>,
    /// alerts: a negative signal (role loss, demotion, scratch)
    pub negative: bool,
}

impl EpisodeRow {
    pub fn new(episode_id: &str, kind: &str, first_seen: NaiveDate, last_seen: NaiveDate) -> Self {
        EpisodeRow {
            episode_id: episode_id.to_string(),
            kind: kind.to_string(),
            first_seen,
            last_seen,
            title: String::new(),
            adds: BTreeSet::new(),
            drops: BTreeSet::new(),
            subjects: BTreeSet::new(),
            counterparty: None,
            negative: false,
        }
    }
}

/// `_NEGATIVE_ALERT_RE.search(title)`: `^Role loss:|dropped off PP1|out of the lineup|scratched|demot`
/// (case-insensitive).
fn negative_alert_title(title: &str) -> bool {
    let t = title.to_lowercase();
    t.starts_with("role loss:")
        || t.contains("dropped off pp1")
        || t.contains("out of the lineup")
        || t.contains("scratched")
        || t.contains("demot")
}

/// Alerts only: "fa" (a free agent: counterparty "FA" or "free agent" in the title), "mine" (a
/// negative alert about my own player) or "info" (anything else; `match_rows` still turns an
/// "info" alert whose subject is on no fantasy roster that day into "fa").
pub fn alert_category(ep: &EpisodeRow) -> Option<&'static str> {
    if ep.kind != "alert" {
        return None;
    }
    if ep.counterparty.as_deref().unwrap_or("").to_uppercase() == FA_COUNTERPARTY
        || ep.title.to_lowercase().contains("free agent")
    {
        return Some("fa");
    }
    if ep.negative || negative_alert_title(&ep.title) {
        return Some("mine");
    }
    Some("info")
}

#[derive(Clone, Debug)]
pub struct TxRow {
    pub tx_id: String,
    pub action: String,
    pub cid: String,
    pub team_id: String,
    pub day: NaiveDate,
    pub group_id: Option<String>,
    pub counterparty_id: Option<String>,
}

impl TxRow {
    pub fn new(tx_id: &str, action: &str, cid: &str, team_id: &str, day: NaiveDate) -> Self {
        TxRow {
            tx_id: tx_id.to_string(),
            action: action.to_string(),
            cid: cid.to_string(),
            team_id: team_id.to_string(),
            day,
            group_id: None,
            counterparty_id: None,
        }
    }

    /// `group_id or tx_id`.
    pub fn group(&self) -> &str {
        match self.group_id.as_deref() {
            Some(g) if !g.is_empty() => g,
            _ => &self.tx_id,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LineupRow {
    pub team_id: String,
    pub day: NaiveDate,
    pub cid: String,
    pub slot: String,
    pub starting: bool,
}

/// One entry of `MatchResult.episodes` (Python: a dict with these keys).
#[derive(Clone, Debug, PartialEq)]
pub struct EpisodeUpdate {
    pub episode_id: String,
    pub status: String,
    pub acted_on: Option<String>,
    pub window_end: String,
    pub match_json: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct MatchResult {
    pub episodes: Vec<EpisodeUpdate>,
    /// `decisions` table rows (decision_id, league, day, origin, kind, episode_id, group_id,
    /// adds_json, drops_json, created_at), ready for `Ledger::upsert`.
    pub decisions: Vec<Row>,
}

#[derive(Clone, Debug, Default)]
pub struct MatchSummary {
    pub league: String,
    pub episodes: usize,
    pub by_status: IndexMap<String, i64>,
    pub by_kind_status: IndexMap<String, IndexMap<String, i64>>,
    pub decisions: IndexMap<String, i64>,
}

impl MatchSummary {
    pub fn line(&self) -> String {
        fn joined(m: &IndexMap<String, i64>) -> String {
            let mut v: Vec<(&String, &i64)> = m.iter().collect();
            v.sort_by(|a, b| a.0.cmp(b.0));
            if v.is_empty() {
                return "none".to_string();
            }
            v.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ")
        }
        format!(
            "{}: {} episodes ({}); decisions: {}",
            self.league,
            self.episodes,
            joined(&self.by_status),
            joined(&self.decisions)
        )
    }
}

/// ESPN locks per game (the rec's own day); Fantrax lineups lock weekly on Monday.
pub fn first_lockable_day(provider: &str, day: NaiveDate) -> NaiveDate {
    if provider == "fantrax" {
        let wd = day.weekday_index();
        return day + Duration::days((7 - wd) % 7);
    }
    day
}

trait WeekdayIndex {
    fn weekday_index(&self) -> i64;
}
impl WeekdayIndex for NaiveDate {
    /// `date.weekday()`: Monday = 0.
    fn weekday_index(&self) -> i64 {
        use chrono::Datelike;
        self.weekday().num_days_from_monday() as i64
    }
}

pub fn window_end(ep: &EpisodeRow, provider: &str) -> NaiveDate {
    if ep.kind == "alert" {
        let days = if alert_category(ep) == Some("mine") { ALERT_MINE_DAYS } else { ALERT_FA_DAYS };
        return ep.last_seen + Duration::days(days);
    }
    if ep.kind == "lineup" {
        return first_lockable_day(provider, ep.first_seen) + Duration::days(lineup_grace(provider));
    }
    ep.last_seen + Duration::days(window_days(&ep.kind))
}

// --------------------------------------------------------------------------- per-kind rules

type Matched = (String, NaiveDate, BTreeSet<String>, Detail);

fn in_window(t: &TxRow, ep: &EpisodeRow, end: NaiveDate) -> bool {
    ep.first_seen <= t.day && t.day <= end
}

fn sorted_vec<'a, I: IntoIterator<Item = &'a str>>(it: I) -> Value {
    let s: BTreeSet<&str> = it.into_iter().collect();
    Value::Array(s.into_iter().map(|c| Value::String(c.to_string())).collect())
}

fn detail(pairs: Vec<(&str, Value)>) -> Detail {
    let mut d = Detail::new();
    for (k, v) in pairs {
        d.insert(k.to_string(), v);
    }
    d
}

/// The drops of the transaction group of `t0`, else the drops of its day.
fn drops_with<'a>(mine: &[&'a TxRow], t0: &TxRow) -> Vec<&'a TxRow> {
    let same: Vec<&TxRow> =
        mine.iter().copied().filter(|t| t.action == "DROP" && t.group() == t0.group()).collect();
    if !same.is_empty() {
        return same;
    }
    mine.iter().copied().filter(|t| t.action == "DROP" && t.day == t0.day).collect()
}

fn match_waiver(ep: &EpisodeRow, mine: &[&TxRow], end: NaiveDate) -> Option<Matched> {
    let mut adds: Vec<&TxRow> = mine
        .iter()
        .copied()
        .filter(|t| t.action == "ADD" && ep.adds.contains(&t.cid) && in_window(t, ep, end))
        .collect();
    adds.sort_by_key(|t| t.day);
    let t0 = *adds.first()?;
    let same = drops_with(mine, t0);
    let dropped: BTreeSet<String> = same.iter().map(|t| t.cid.clone()).collect();
    let status = if dropped == ep.drops { "followed" } else { "partial" };
    let mut groups: BTreeSet<String> = BTreeSet::new();
    groups.insert(t0.group().to_string());
    groups.extend(same.iter().map(|t| t.group().to_string()));
    let d = detail(vec![
        ("added", sorted_vec(adds.iter().map(|t| t.cid.as_str()))),
        ("dropped", sorted_vec(dropped.iter().map(|s| s.as_str()))),
    ]);
    Some((status.to_string(), t0.day, groups, d))
}

type TradeGroups = IndexMap<String, (BTreeSet<String>, BTreeSet<String>, NaiveDate)>;

/// group -> (players I get, players I give, day) of my trades (or my proposals).
fn trade_groups(
    mine: &[&TxRow],
    my_team: Option<&str>,
    ep: &EpisodeRow,
    end: NaiveDate,
    proposed: bool,
) -> TradeGroups {
    let mut out: TradeGroups = IndexMap::new();
    for t in mine {
        if !in_window(t, ep, end) {
            continue;
        }
        let (mut get, mut give) = (false, false);
        if proposed && t.action == "PROPOSED" {
            get = Some(t.team_id.as_str()) == my_team;
            give = t.counterparty_id.as_deref() == my_team && !get;
        } else if !proposed && (t.action == "TRADE_IN" || t.action == "TRADE_OUT") {
            get = t.action == "TRADE_IN";
            give = t.action == "TRADE_OUT";
        }
        if !(get || give) {
            continue;
        }
        let g = out
            .entry(t.group().to_string())
            .or_insert_with(|| (BTreeSet::new(), BTreeSet::new(), t.day));
        if get {
            g.0.insert(t.cid.clone());
        } else {
            g.1.insert(t.cid.clone());
        }
        if t.day < g.2 {
            g.2 = t.day;
        }
    }
    out
}

fn match_trade(ep: &EpisodeRow, mine: &[&TxRow], my_team: Option<&str>, end: NaiveDate) -> Option<Matched> {
    for proposed in [false, true] {
        // (exact rank, day, status, gid, gets, gives)
        let mut best: Option<(i32, NaiveDate, &str, String, BTreeSet<String>, BTreeSet<String>)> = None;
        for (gid, (gets, gives, day)) in trade_groups(mine, my_team, ep, end, proposed) {
            let has_get = gets.iter().any(|c| ep.adds.contains(c));
            let has_give = gives.iter().any(|c| ep.drops.contains(c));
            if !(has_get && has_give) {
                continue;
            }
            let exact = gets == ep.adds && gives == ep.drops;
            let status = if proposed {
                "proposed"
            } else if exact {
                "followed"
            } else {
                "partial"
            };
            let rank = if exact { 0 } else { 1 };
            let better = match &best {
                None => true,
                Some(b) => (rank, day) < (b.0, b.1),
            };
            if better {
                best = Some((rank, day, status, gid, gets, gives));
            }
        }
        if let Some((_, day, status, gid, gets, gives)) = best {
            let mut groups = BTreeSet::new();
            groups.insert(gid);
            let d = detail(vec![
                ("got", sorted_vec(gets.iter().map(|s| s.as_str()))),
                ("gave", sorted_vec(gives.iter().map(|s| s.as_str()))),
            ]);
            return Some((status.to_string(), day, groups, d));
        }
    }
    None
}

fn match_flag(ep: &EpisodeRow, mine: &[&TxRow], my_team: Option<&str>, end: NaiveDate) -> Option<Matched> {
    let sell = ep.kind == "sell_high";
    let targets = if sell { &ep.drops } else { &ep.adds };
    let action = if sell { "TRADE_OUT" } else { "TRADE_IN" };
    let mut hits: Vec<&TxRow> = mine
        .iter()
        .copied()
        .filter(|t| t.action == action && targets.contains(&t.cid) && in_window(t, ep, end))
        .collect();
    hits.sort_by_key(|t| t.day);
    if let Some(h0) = hits.first() {
        let mut groups = BTreeSet::new();
        groups.insert(h0.group().to_string());
        let d = detail(vec![("traded", sorted_vec(hits.iter().map(|t| t.cid.as_str())))]);
        return Some(("followed".to_string(), h0.day, groups, d));
    }
    let mut all: Vec<&TxRow> = mine.to_vec();
    all.sort_by_key(|t| t.day);
    for t in all {
        if t.action != "PROPOSED" || !targets.contains(&t.cid) || !in_window(t, ep, end) {
            continue;
        }
        if (sell && t.counterparty_id.as_deref() == my_team)
            || (ep.kind == "buy_low" && Some(t.team_id.as_str()) == my_team)
        {
            let mut groups = BTreeSet::new();
            groups.insert(t.group().to_string());
            let d = detail(vec![("offered", Value::Array(vec![Value::String(t.cid.clone())]))]);
            return Some(("proposed".to_string(), t.day, groups, d));
        }
    }
    None
}

/// The first of `days` that has lineup rows, with those rows by cid (a later row of the same
/// cid replaces an earlier one, like the Python dict).
fn lineup_on<'a>(
    lineups: &'a [LineupRow],
    days: impl IntoIterator<Item = NaiveDate>,
) -> Option<(NaiveDate, HashMap<&'a str, &'a LineupRow>)> {
    let mut by_day: HashMap<NaiveDate, HashMap<&str, &LineupRow>> = HashMap::new();
    for r in lineups {
        by_day.entry(r.day).or_default().insert(r.cid.as_str(), r);
    }
    for d in days {
        if let Some(rows) = by_day.remove(&d) {
            if !rows.is_empty() {
                return Some((d, rows));
            }
        }
    }
    None
}

fn match_lineup(ep: &EpisodeRow, lineups: &[LineupRow], provider: &str) -> Option<Matched> {
    let lock = first_lockable_day(provider, ep.first_seen);
    // ESPN: the box score of the lock day itself (pulled the next morning). Fantrax: the first
    // snapshot taken after the Monday lock (a Monday-morning snapshot predates the lock).
    let offsets: Vec<i64> = if provider == "fantrax" { (1..7).collect() } else { (0..2).collect() };
    let (day, rows) = lineup_on(lineups, offsets.into_iter().map(|i| lock + Duration::days(i)))?;
    let activation = ep.title.starts_with("Activate");

    let on = |c: &String| -> bool {
        match rows.get(c.as_str()) {
            Some(r) => {
                if activation {
                    r.slot != "IR"
                } else {
                    r.starting
                }
            }
            None => false,
        }
    };
    let in_ok: Option<bool> = if ep.adds.is_empty() { None } else { Some(ep.adds.iter().all(on)) };
    let out_ok: Option<bool> = if ep.drops.is_empty() {
        None
    } else {
        Some(ep.drops.iter().all(|c| !rows.get(c.as_str()).map(|r| r.starting).unwrap_or(false)))
    };
    let parts: Vec<bool> = [in_ok, out_ok].into_iter().flatten().collect();
    let to_v = |o: Option<bool>| o.map(Value::Bool).unwrap_or(Value::Null);
    let mut d = detail(vec![
        ("lock_day", Value::String(lock.to_string())),
        ("graded_day", Value::String(day.to_string())),
        ("in_ok", to_v(in_ok)),
        ("out_ok", to_v(out_ok)),
    ]);
    if !parts.is_empty() && parts.iter().all(|b| *b) {
        return Some(("followed".to_string(), lock, BTreeSet::new(), d));
    }
    if parts.iter().any(|b| *b) {
        return Some(("partial".to_string(), lock, BTreeSet::new(), d));
    }
    d.insert("reason".to_string(), Value::String("lineup not changed".to_string()));
    Some(("expired".to_string(), lock, BTreeSet::new(), d))
}

fn match_injury(ep: &EpisodeRow, mine: &[&TxRow], lineups: &[LineupRow], end: NaiveDate) -> Option<Matched> {
    if ep.title.starts_with("Activate") && ep.subjects.is_empty() {
        for t in mine {
            if t.action == "ACTIVATE" && ep.adds.contains(&t.cid) && in_window(t, ep, end) {
                let mut g = BTreeSet::new();
                g.insert(t.group().to_string());
                let d = detail(vec![("activated", Value::Array(vec![Value::String(t.cid.clone())]))]);
                return Some(("followed".to_string(), t.day, g, d));
            }
        }
        let mut sorted: Vec<&LineupRow> = lineups.iter().collect();
        sorted.sort_by_key(|r| r.day);
        for r in sorted {
            if ep.adds.contains(&r.cid) && r.slot != "IR" && ep.first_seen <= r.day && r.day <= end {
                let d = detail(vec![("activated", Value::Array(vec![Value::String(r.cid.clone())]))]);
                return Some(("followed".to_string(), r.day, BTreeSet::new(), d));
            }
        }
        return None;
    }
    let mut groups: BTreeSet<String> = BTreeSet::new();
    let mut days: Vec<NaiveDate> = Vec::new();
    let mut parts: Vec<bool> = Vec::new();
    let mut d = Detail::new();
    if !ep.subjects.is_empty() {
        let ir_tx: Vec<&&TxRow> = mine
            .iter()
            .filter(|t| t.action == "IR" && ep.subjects.contains(&t.cid) && in_window(t, ep, end))
            .collect();
        let ir_slot: Vec<&LineupRow> = lineups
            .iter()
            .filter(|r| {
                ep.subjects.contains(&r.cid) && r.slot == "IR" && ep.first_seen <= r.day && r.day <= end
            })
            .collect();
        let ok = !ir_tx.is_empty() || !ir_slot.is_empty();
        parts.push(ok);
        d.insert("ir".to_string(), Value::Bool(ok));
        groups.extend(ir_tx.iter().map(|t| t.group().to_string()));
        days.extend(ir_tx.iter().map(|t| t.day));
        days.extend(ir_slot.iter().map(|r| r.day));
    }
    if !ep.adds.is_empty() {
        let adds: Vec<&&TxRow> = mine
            .iter()
            .filter(|t| t.action == "ADD" && ep.adds.contains(&t.cid) && in_window(t, ep, end))
            .collect();
        parts.push(!adds.is_empty());
        d.insert("added".to_string(), Value::Bool(!adds.is_empty()));
        groups.extend(adds.iter().map(|t| t.group().to_string()));
        days.extend(adds.iter().map(|t| t.day));
    }
    if parts.is_empty() || !parts.iter().any(|b| *b) {
        return None;
    }
    let status = if parts.iter().all(|b| *b) { "followed" } else { "partial" };
    let day = *days.iter().min()?;
    Some((status.to_string(), day, groups, d))
}

/// Whether any of `cids` is on a fantasy roster on `day` per the lineup rows of every team (the
/// latest lineup day on or before `day`, at most 3 days back); None without rows.
fn rostered_on(lineups: &[LineupRow], cids: &BTreeSet<String>, day: NaiveDate) -> Option<bool> {
    let lo = day - Duration::days(3);
    let last = lineups.iter().map(|r| r.day).filter(|d| lo <= *d && *d <= day).max()?;
    Some(lineups.iter().any(|r| r.day == last && cids.contains(&r.cid)))
}

fn match_alert(ep: &EpisodeRow, mine: &[&TxRow], end: NaiveDate, category: &str) -> Option<Matched> {
    let targets: BTreeSet<String> = ep.subjects.union(&ep.adds).cloned().collect();
    if category == "fa" {
        let mut adds: Vec<&TxRow> = mine
            .iter()
            .copied()
            .filter(|t| t.action == "ADD" && targets.contains(&t.cid) && in_window(t, ep, end))
            .collect();
        adds.sort_by_key(|t| t.day);
        let t0 = *adds.first()?;
        let same = drops_with(mine, t0);
        let mut groups = BTreeSet::new();
        groups.insert(t0.group().to_string());
        groups.extend(same.iter().map(|t| t.group().to_string()));
        let d = detail(vec![
            ("alert", Value::String("fa".to_string())),
            ("added", sorted_vec(adds.iter().filter(|t| t.day == t0.day).map(|t| t.cid.as_str()))),
            ("dropped", sorted_vec(same.iter().map(|t| t.cid.as_str()))),
        ]);
        return Some(("followed".to_string(), t0.day, groups, d));
    }
    if category == "mine" {
        let mut outs: Vec<&TxRow> = mine
            .iter()
            .copied()
            .filter(|t| {
                (t.action == "DROP" || t.action == "TRADE_OUT")
                    && targets.contains(&t.cid)
                    && in_window(t, ep, end)
            })
            .collect();
        outs.sort_by_key(|t| t.day);
        let t0 = *outs.first()?;
        let mut back: Vec<&TxRow> = mine
            .iter()
            .copied()
            .filter(|t| (t.action == "ADD" || t.action == "TRADE_IN") && t.group() == t0.group())
            .collect();
        if back.is_empty() && t0.action == "DROP" {
            back = mine.iter().copied().filter(|t| t.action == "ADD" && t.day == t0.day).collect();
        }
        let mut groups = BTreeSet::new();
        groups.insert(t0.group().to_string());
        groups.extend(back.iter().map(|t| t.group().to_string()));
        let d = detail(vec![
            ("alert", Value::String("mine".to_string())),
            ("via", Value::String(if t0.action == "TRADE_OUT" { "trade" } else { "drop" }.to_string())),
            ("dropped", sorted_vec(outs.iter().filter(|t| t.day == t0.day).map(|t| t.cid.as_str()))),
            ("added", sorted_vec(back.iter().map(|t| t.cid.as_str()))),
        ]);
        return Some(("followed".to_string(), t0.day, groups, d));
    }
    None
}

// --------------------------------------------------------------------------- driver

fn decision_kind(actions: &BTreeSet<&str>) -> &'static str {
    let has = |a: &str| actions.contains(a);
    if has("TRADE_IN") || has("TRADE_OUT") {
        return "trade";
    }
    if (has("IR") || has("ACTIVATE")) && !(has("ADD") || has("DROP")) {
        return "injury";
    }
    "waiver"
}

fn str_list(d: &Detail, key: &str) -> BTreeSet<String> {
    d.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

fn json_sorted_set(s: &BTreeSet<String>) -> String {
    json_dumps(&Value::Array(s.iter().map(|c| Value::String(c.clone())).collect()), false)
}

fn row(v: Value) -> Row {
    match v {
        Value::Object(m) => m,
        _ => Row::new(),
    }
}

/// Pure matching over ledger rows (see module docs). `txs` / `lineups` may hold every team's
/// rows; only `my_team`'s are used.
pub fn match_rows(
    episodes: &[EpisodeRow],
    txs: &[TxRow],
    lineups: &[LineupRow],
    my_team: Option<&str>,
    provider: &str,
    today: NaiveDate,
    league: &str,
) -> MatchResult {
    let mine: Vec<&TxRow> = txs
        .iter()
        .filter(|t| {
            Some(t.team_id.as_str()) == my_team
                || (t.action == "PROPOSED" && t.counterparty_id.as_deref() == my_team)
        })
        .collect();
    let my_lineups: Vec<LineupRow> =
        lineups.iter().filter(|r| Some(r.team_id.as_str()) == my_team).cloned().collect();
    let mut res = MatchResult::default();
    let mut consumed: BTreeSet<String> = BTreeSet::new();
    for ep0 in episodes {
        let mut category = alert_category(ep0);
        let mut ep: Cow<'_, EpisodeRow> = Cow::Borrowed(ep0);
        if category == Some("info") {
            let cids: BTreeSet<String> = ep0.subjects.union(&ep0.adds).cloned().collect();
            if rostered_on(lineups, &cids, ep0.first_seen) == Some(false) {
                category = Some("fa"); // on no fantasy roster that day: a free agent
                let mut e = ep0.clone();
                e.counterparty = Some(FA_COUNTERPARTY.to_string());
                ep = Cow::Owned(e);
            }
        }
        let ep: &EpisodeRow = &ep;
        let end = window_end(ep, provider);
        let m = match ep.kind.as_str() {
            "waiver" => match_waiver(ep, &mine, end),
            "trade" => match_trade(ep, &mine, my_team, end),
            "sell_high" | "buy_low" => match_flag(ep, &mine, my_team, end),
            "lineup" => match_lineup(ep, &my_lineups, provider),
            "injury" => match_injury(ep, &mine, &my_lineups, end),
            "alert" => match_alert(ep, &mine, end, category.unwrap_or("info")),
            _ => None,
        };
        let Some((status, day, groups, mut det)) = m else {
            let status = if today > end { "expired" } else { "open" };
            res.episodes.push(EpisodeUpdate {
                episode_id: ep.episode_id.clone(),
                status: status.to_string(),
                acted_on: None,
                window_end: end.to_string(),
                match_json: None,
            });
            continue;
        };
        if status == "proposed" && today > end {
            det.insert("window_passed".to_string(), Value::Bool(true));
        }
        res.episodes.push(EpisodeUpdate {
            episode_id: ep.episode_id.clone(),
            status: status.clone(),
            acted_on: if status != "expired" { Some(day.to_string()) } else { None },
            window_end: end.to_string(),
            match_json: Some(json_dumps(&Value::Object(det.clone()), true)),
        });
        if status == "expired" {
            continue;
        }
        consumed.extend(groups.iter().cloned());
        let (adds, drops) = if ep.kind == "alert" {
            (str_list(&det, "added"), str_list(&det, "dropped"))
        } else {
            (ep.adds.clone(), ep.drops.clone())
        };
        let group_id = groups.iter().cloned().collect::<Vec<_>>().join(",");
        res.decisions.push(row(json!({
            "decision_id": format!("ep:{}", ep.episode_id),
            "league": league,
            "day": day.to_string(),
            "origin": status,
            "kind": ep.kind,
            "episode_id": ep.episode_id,
            "group_id": if group_id.is_empty() { Value::Null } else { Value::String(group_id) },
            "adds_json": json_sorted_set(&adds),
            "drops_json": json_sorted_set(&drops),
            "created_at": now_iso(),
        })));
    }
    let mut groups_tx: IndexMap<&str, Vec<&TxRow>> = IndexMap::new();
    for t in &mine {
        if t.action == "PROPOSED" || Some(t.team_id.as_str()) != my_team {
            continue;
        }
        groups_tx.entry(t.group()).or_default().push(t);
    }
    for (gid, ts) in groups_tx {
        if consumed.contains(gid) {
            continue;
        }
        let actions: BTreeSet<&str> = ts.iter().map(|t| t.action.as_str()).collect();
        let pick = |names: [&str; 2]| -> BTreeSet<String> {
            ts.iter().filter(|t| names.contains(&t.action.as_str())).map(|t| t.cid.clone()).collect()
        };
        let day = ts.iter().map(|t| t.day).min().expect("group has rows");
        res.decisions.push(row(json!({
            "decision_id": format!("tx:{league}:{gid}"),
            "league": league,
            "day": day.to_string(),
            "origin": "user_only",
            "kind": decision_kind(&actions),
            "episode_id": Value::Null,
            "group_id": gid,
            "adds_json": json_sorted_set(&pick(["ADD", "TRADE_IN"])),
            "drops_json": json_sorted_set(&pick(["DROP", "TRADE_OUT"])),
            "created_at": now_iso(),
        })));
    }
    res
}

// --------------------------------------------------------------------------- ledger glue

fn parse_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| format!("invalid ISO date {s:?}"))
}

fn str_of(r: &Row, k: &str) -> Option<String> {
    r.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
}

pub fn my_team_id(ledger: &Ledger, league: &str) -> Result<Option<String>> {
    let key = format!("my_team:{league}");
    let rows = ledger.query("SELECT value FROM meta WHERE key=?", &[&key])?;
    Ok(rows.first().and_then(|r| str_of(r, "value")))
}

pub fn set_my_team_id(ledger: &Ledger, league: &str, team_id: &str) -> Result<()> {
    let key = format!("my_team:{league}");
    ledger.execute(
        "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        &[&key, &team_id.to_string()],
    )?;
    ledger.commit()
}

/// A negative rookie news signal (`ROLE_NEWS` with value < 0) among an alert's reasons.
fn negative_reasons(raw: Option<&str>) -> bool {
    let Some(raw) = raw.filter(|s| !s.is_empty()) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(raw) else { return false };
    let Some(arr) = v.as_array() else { return false };
    arr.iter().any(|r| {
        r.get("code").and_then(|c| c.as_str()) == Some("ROLE_NEWS")
            && r.get("value").and_then(|x| x.as_f64()).map(|x| x < 0.0).unwrap_or(false)
    })
}

pub fn load_episodes(ledger: &Ledger, league: &str) -> Result<Vec<EpisodeRow>> {
    let l = league.to_string();
    let eps = ledger.query(
        "SELECT episode_id, rec_key, kind, title, first_seen, last_seen FROM rec_episodes \
         WHERE league=? ORDER BY first_seen",
        &[&l],
    )?;
    let players = ledger.query("SELECT as_of, rec_key, side, cid FROM rec_players WHERE league=?", &[&l])?;
    let mut recs: HashMap<(String, String), Row> = HashMap::new();
    for r in ledger.query(
        "SELECT rec_key, as_of, counterparty, reasons_json FROM recs WHERE league=? AND kind='alert'",
        &[&l],
    )? {
        recs.insert((str_of(&r, "rec_key").unwrap_or_default(), str_of(&r, "as_of").unwrap_or_default()), r);
    }
    let mut idx: HashMap<(String, String), HashMap<String, BTreeSet<String>>> = HashMap::new();
    for p in &players {
        idx.entry((str_of(p, "rec_key").unwrap_or_default(), str_of(p, "as_of").unwrap_or_default()))
            .or_default()
            .entry(str_of(p, "side").unwrap_or_default())
            .or_default()
            .insert(str_of(p, "cid").unwrap_or_default());
    }
    let mut out = Vec::new();
    for e in &eps {
        let key = (str_of(e, "rec_key").unwrap_or_default(), str_of(e, "first_seen").unwrap_or_default());
        let sides = idx.get(&key);
        let side = |n: &str| sides.and_then(|s| s.get(n)).cloned().unwrap_or_default();
        let rec = recs.get(&key);
        out.push(EpisodeRow {
            episode_id: str_of(e, "episode_id").unwrap_or_default(),
            kind: str_of(e, "kind").unwrap_or_default(),
            title: str_of(e, "title").unwrap_or_default(),
            first_seen: parse_date(&key.1)?,
            last_seen: parse_date(&str_of(e, "last_seen").unwrap_or_default())?,
            adds: side("add"),
            drops: side("drop"),
            subjects: side("subject"),
            counterparty: rec.and_then(|r| str_of(r, "counterparty")),
            negative: negative_reasons(rec.and_then(|r| str_of(r, "reasons_json")).as_deref()),
        });
    }
    Ok(out)
}

pub fn load_txs(ledger: &Ledger, league: &str) -> Result<Vec<TxRow>> {
    let l = league.to_string();
    ledger
        .query("SELECT * FROM transactions WHERE league=?", &[&l])?
        .iter()
        .map(|r| {
            Ok(TxRow {
                tx_id: str_of(r, "tx_id").unwrap_or_default(),
                action: str_of(r, "action").unwrap_or_default(),
                cid: str_of(r, "cid").unwrap_or_default(),
                team_id: str_of(r, "team_id").unwrap_or_default(),
                day: parse_date(&str_of(r, "day").unwrap_or_default())?,
                group_id: str_of(r, "group_id"),
                counterparty_id: str_of(r, "counterparty_id"),
            })
        })
        .collect()
}

/// My lineup rows (every team's with `all_teams`: free-agent alerts check who was rostered).
pub fn load_lineups(ledger: &Ledger, league: &str, team_id: Option<&str>, all_teams: bool) -> Result<Vec<LineupRow>> {
    let Some(team_id) = team_id else { return Ok(Vec::new()) };
    let (l, t) = (league.to_string(), team_id.to_string());
    let rows = if all_teams {
        ledger.query("SELECT * FROM lineup_days WHERE league=?", &[&l])?
    } else {
        ledger.query("SELECT * FROM lineup_days WHERE league=? AND team_id=?", &[&l, &t])?
    };
    rows.iter()
        .map(|r| {
            Ok(LineupRow {
                team_id: str_of(r, "team_id").unwrap_or_default(),
                day: parse_date(&str_of(r, "day").unwrap_or_default())?,
                cid: str_of(r, "cid").unwrap_or_default(),
                slot: str_of(r, "slot").unwrap_or_default(),
                starting: match r.get("starting") {
                    Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
                    Some(Value::Bool(b)) => *b,
                    _ => false,
                },
            })
        })
        .collect()
}

/// Match every episode of `league` and rewrite its decisions. `provider` defaults to `league`
/// (the ledger keys leagues by provider name); `today` defaults to the local date.
pub fn match_episodes(
    ledger: &Ledger,
    league: &str,
    today: Option<NaiveDate>,
    provider: Option<&str>,
) -> Result<MatchSummary> {
    let today = today.unwrap_or_else(|| chrono::Local::now().date_naive());
    let team = my_team_id(ledger, league)?;
    let eps = load_episodes(ledger, league)?;
    let res = match_rows(
        &eps,
        &load_txs(ledger, league)?,
        &load_lineups(ledger, league, team.as_deref(), true)?,
        team.as_deref(),
        provider.unwrap_or(league),
        today,
        league,
    );
    let now = now_iso();
    for u in &res.episodes {
        ledger.execute(
            "UPDATE rec_episodes SET status=?, acted_on=?, window_end=?, match_json=?, updated_at=? \
             WHERE episode_id=?",
            &[&u.status, &u.acted_on, &u.window_end, &u.match_json, &now, &u.episode_id],
        )?;
    }
    let l = league.to_string();
    ledger.execute("DELETE FROM decisions WHERE league=?", &[&l])?;
    ledger.commit()?;
    ledger.upsert("decisions", &res.decisions, &["decision_id"])?;
    let mut summary = MatchSummary { league: league.to_string(), episodes: res.episodes.len(), ..Default::default() };
    let kinds: HashMap<&str, &str> = eps.iter().map(|e| (e.episode_id.as_str(), e.kind.as_str())).collect();
    for u in &res.episodes {
        *summary.by_status.entry(u.status.clone()).or_insert(0) += 1;
    }
    for u in &res.episodes {
        let kind = kinds.get(u.episode_id.as_str()).copied().unwrap_or_default().to_string();
        *summary.by_kind_status.entry(kind).or_default().entry(u.status.clone()).or_insert(0) += 1;
    }
    for d in &res.decisions {
        if let Some(o) = d.get("origin").and_then(|v| v.as_str()) {
            *summary.decisions.entry(o.to_string()).or_insert(0) += 1;
        }
    }
    Ok(summary)
}
