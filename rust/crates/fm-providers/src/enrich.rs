//! Rust port of `fantasy_manager/providers/enrich.py`: enrich a provider's LeagueContext with NHL
//! data (schedule, season rows, crosswalk ids, birth dates, injuries, stat lines, pedigree, rookie
//! evidence, game logs, preseason lines, deployment, Daily Faceoff lines, MoneyPuck xG) and
//! collect the structured `ctx.sources`. Owner: work package `W4-enrich` (wave 4).
//!
//! Every step is best effort, exactly like Python: a failing source adds a warning to
//! `ctx.warnings` (same text) and the rest still runs.
//!
//! Deviations from Python (API shape only, results identical):
//! * The module-level registries (`preseason_enrich._REGISTRY`, `rookie_enrich._HISTORY /
//!   _SIGNALS`, `xg_enrich._GSAX / _NORMS`, `lines_enrich._TEAM_META`) live in `ctx.enrich`
//!   (`Arc<EnrichState>`, copy-on-write via `Arc::make_mut`). Enrichment starts from whatever
//!   state the context already carries (Python's process-global registries), so pass a fresh
//!   context for a fresh run.
//! * The clock (`time.time()`, `date.today()`) is a [`Clock`] parameter ([`EnrichOptions::clock`])
//!   so parity runs can pin it.
//! * `enrich_context` mutates `ctx` in place and returns nothing (Python returned the same object).
//! * Python's duck-typed injections (`nhl=`, `injuries_fetch=`, `crosswalk=`, `ledger=`,
//!   `lines_client=`, `xg_client=`) are the fields of [`EnrichOptions`].
//! * Player mutation goes through the first occurrence of each cid (`ctx.all_players()` order),
//!   which is the object Python mutates.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};
use fm_core::config::Settings;
use fm_core::models::{FantasyTeam, LeagueContext, Player, SourceSeverity, SourceStatus, Split, StatLine, Status};
use fm_core::normalize::normalize_team;
use fm_core::{Map, PyDateTime};
use fm_harness::ledger::{Ledger, DB_NAME};
use fm_http::cache::ParamValue;
use fm_http::{cache_key, FetchJson, HttpCache, Params};
use fm_pycompat::fmt::fixed;
use fm_pycompat::text::{py_rstrip, py_slice_str, title};
use fm_sources::dailyfaceoff::{self, DailyFaceoffClient};
use fm_sources::injuries::{fetch_injuries, InjuryReport, INJURIES_URL};
use fm_sources::matching::crosswalk::{candidates_of, nhl_candidates, Crosswalk};
use fm_sources::matching::matcher::{match_player, Candidate, PlayerIndex};
use fm_sources::moneypuck::{self, MoneyPuckClient};
use fm_sources::nhl::{
    cached_nhl_fetch, current_season, games_per_day, prior_season, NhlClient, NhlGameLogEntry, NhlGoalieSeason,
    NhlPlayerLanding, NhlRosterPlayer, NhlSkaterSeason, NHL_TEAMS, WEB_BASE,
};
use regex::Regex;
use serde_json::Value;

use crate::deployment_enrich::{enrich_deployment, DeploymentResult};
use crate::lines_enrich::{enrich_lines, LineSnapshotStore, LinesResult};
use crate::preseason_enrich::{enrich_preseason, PreseasonResult};
use crate::rookie_enrich::{enrich_rookies, RookieOptions, HISTORY_LIMIT};
use crate::xg_enrich::{enrich_xg, XgEnrichResult};

pub const HOUR: f64 = 3600.0;
pub const TTL_NHL_STATS: f64 = 6.0 * HOUR;
/// finished seasons before the prior one
pub const TTL_NHL_HISTORY: f64 = 30.0 * 24.0 * HOUR;
pub const TTL_GAME_LOG: f64 = 12.0 * HOUR;
pub const TTL_SCHEDULE: f64 = 7.0 * 24.0 * HOUR;
/// feeds nhl_goalies: trades / call-ups must show within a day
pub const TTL_ROSTER: f64 = 24.0 * HOUR;
pub const TTL_LANDING: f64 = 30.0 * 24.0 * HOUR;
pub const TTL_INJURIES: f64 = 1.0 * HOUR;
pub const TTL_RSS: f64 = 0.5 * HOUR;

pub const DEEP_FA_LIMIT: usize = 40;
pub const PEDIGREE_LIMIT: usize = 150;
pub const PEDIGREE_MAX_AGE: f64 = 24.0;
pub const PEDIGREE_PROVEN_GP: i64 = 40;
pub const PEDIGREE_FA_LIMIT: usize = 150;
pub const MIN_LEDGER_TEAM_GAMES: i64 = 5;

/// `SEVERITY`: more severe wins when merging provider and feed status.
pub fn severity(s: &str) -> i64 {
    match s {
        "dtd" => 1,
        "out" => 2,
        "ir" => 3,
        "ltir" => 4,
        _ => 0,
    }
}

// --------------------------------------------------------------------------- clock

/// The run's clock: `now` = Python `time.time()` (epoch seconds), `today` = `date.today()`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clock {
    pub now: f64,
    pub today: NaiveDate,
}

impl Clock {
    /// The real clock (local date).
    pub fn system() -> Clock {
        Clock::at(Utc::now())
    }

    /// A pinned clock: `today` is the local date at `now`.
    pub fn at(now: DateTime<Utc>) -> Clock {
        Clock { now: now.timestamp() as f64 + now.timestamp_subsec_micros() as f64 / 1e6, today: now.with_timezone(&Local).date_naive() }
    }

    /// `now` as an aware UTC datetime.
    pub fn utc(&self) -> DateTime<Utc> {
        let secs = self.now.floor();
        let nanos = ((self.now - secs) * 1e9).round().clamp(0.0, 999_999_999.0) as u32;
        DateTime::<Utc>::from_timestamp(secs as i64, nanos).unwrap_or_else(Utc::now)
    }
}

// --------------------------------------------------------------------------- error helpers

/// Python `type(e).__name__` for the errors enrichment can see.
pub fn exc_name(e: &anyhow::Error) -> &'static str {
    if e.downcast_ref::<fm_http::CacheMiss>().is_some() {
        return "CacheMiss";
    }
    if e.downcast_ref::<fm_http::HttpError>().is_some() {
        return "HttpError";
    }
    if e.downcast_ref::<dailyfaceoff::DfoError>().is_some() {
        return "DfoError";
    }
    if e.downcast_ref::<fm_sources::llm::LLMError>().is_some() {
        return "LLMError";
    }
    if e.downcast_ref::<fm_core::ProviderError>().is_some() {
        return "ProviderError";
    }
    if e.downcast_ref::<serde_json::Error>().is_some() {
        return "JSONDecodeError";
    }
    if e.downcast_ref::<reqwest::Error>().is_some() {
        return "ConnectionError";
    }
    if e.downcast_ref::<rusqlite::Error>().is_some() {
        return "OperationalError";
    }
    if e.downcast_ref::<std::io::Error>().is_some() {
        return "OSError";
    }
    "Exception"
}

/// `_short(e)`: "ErrorClass: message[:140]".
pub fn short(e: &anyhow::Error) -> String {
    let s = e.to_string();
    let msg = if s.is_empty() { exc_name(e).to_string() } else { s };
    format!("{}: {}", exc_name(e), py_slice_str(&msg, None, Some(140)))
}

// --------------------------------------------------------------------------- TTL routing

fn param_str<'a>(params: &'a Params, key: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
        ParamValue::Str(s) => Some(s.as_str()),
        _ => None,
    })
}

fn stats_season(params: &Params) -> Option<i64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"seasonId=(\d{8})").expect("static regex"));
    re.captures(param_str(params, "cayenneExp").unwrap_or("")).and_then(|c| c[1].parse().ok())
}

/// `ttl_for(url, params, today)`: (source label, TTL seconds). NHL stats reports for seasons
/// before the prior one never change, so they are cached for 30 days.
pub fn ttl_for(url: &str, params: &Params, today: NaiveDate) -> (&'static str, f64) {
    if url.starts_with(INJURIES_URL) {
        return ("injuries", TTL_INJURIES);
    }
    if url.contains("rss") {
        return ("news", TTL_RSS);
    }
    if url.contains("/game-log/") {
        return ("NHL game logs", TTL_GAME_LOG);
    }
    if url.contains("/club-schedule-season/") || url.contains("/schedule/") {
        return ("NHL schedule", TTL_SCHEDULE);
    }
    if url.contains("/roster/") {
        return ("NHL rosters", TTL_ROSTER);
    }
    if url.ends_with("/landing") {
        return ("NHL player pages", TTL_LANDING);
    }
    if url.contains("api.nhle.com/stats") {
        if let Some(s) = stats_season(params) {
            if s < prior_season(current_season(today)) {
                return ("NHL stats (history)", TTL_NHL_HISTORY);
            }
        }
        return ("NHL stats", TTL_NHL_STATS);
    }
    ("NHL", TTL_NHL_STATS)
}

/// `SOURCE_NAMES`: display names of the tracked sources.
pub fn source_name(label: &str) -> String {
    match label {
        "NHL stats" => "NHL stats",
        "NHL stats (history)" => "NHL stats (past seasons)",
        "NHL schedule" => "NHL schedules",
        "NHL rosters" => "NHL rosters",
        "NHL player pages" => "NHL player pages",
        "NHL game logs" => "NHL game logs",
        "injuries" => "Injuries",
        "news" => "News feeds",
        "NHL" => "NHL (other)",
        other => other,
    }
    .to_string()
}

/// `VALUATION_SOURCES`: the sources valuation reads.
pub const VALUATION_SOURCES: [&str; 10] = [
    "NHL stats",
    "NHL schedules",
    "NHL rosters",
    "Injuries",
    "Daily Faceoff lines",
    "Daily Faceoff goalies",
    "MoneyPuck",
    "Deployment ledger",
    "Fantrax",
    "ESPN",
];

/// `_utc(epoch)`: `datetime.fromtimestamp(epoch, tz=utc)` (microseconds rounded half-even).
pub fn utc(epoch: Option<f64>) -> Option<PyDateTime> {
    let e = epoch?;
    let mut secs = e.floor();
    let mut us = ((e - secs) * 1e6).round_ties_even();
    if us >= 1e6 {
        secs += 1.0;
        us -= 1e6;
    }
    let dt = DateTime::<Utc>::from_timestamp(secs as i64, (us as u32) * 1000)?;
    Some(PyDateTime::from_utc(dt))
}

/// `_age(seconds)`: "just now", "12m ago", "5h ago", "3d ago".
pub fn age(seconds: f64) -> String {
    if seconds < 90.0 {
        return "just now".into();
    }
    if seconds < 90.0 * 60.0 {
        return format!("{}m ago", fixed(seconds / 60.0, 0, false));
    }
    if seconds < 36.0 * HOUR {
        return format!("{}h ago", fixed(seconds / HOUR, 0, false));
    }
    format!("{}d ago", fixed(seconds / (24.0 * HOUR), 0, false))
}

// --------------------------------------------------------------------------- SourceTracker

/// One tracked source label's counters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackEntry {
    /// successful fetches
    pub requests: i64,
    /// epoch of the oldest data served
    pub oldest: Option<f64>,
    pub ttl: Option<f64>,
    /// failed fetches
    pub failed: i64,
    /// last failure
    pub error: Option<String>,
    /// newest cached copy of a failed request (epoch)
    pub last_good: Option<f64>,
}

/// `SourceTracker`: routes fetches through the HttpCache with per-source TTLs and records data
/// freshness. Shared (`Arc`) by the fetch callables it hands out.
pub struct SourceTracker {
    pub cache: Arc<HttpCache>,
    /// `date.today()` for [`ttl_for`]
    pub today: NaiveDate,
    sources: Mutex<BTreeMap<String, TrackEntry>>,
}

impl SourceTracker {
    pub fn new(cache: Arc<HttpCache>, today: NaiveDate) -> Arc<SourceTracker> {
        Arc::new(SourceTracker { cache, today, sources: Mutex::new(BTreeMap::new()) })
    }

    /// label -> counters, sorted by label.
    pub fn sources(&self) -> BTreeMap<String, TrackEntry> {
        self.sources.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn with_entry(&self, label: &str, ttl: Option<f64>, f: impl FnOnce(&mut TrackEntry)) {
        let mut g = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        let s = g.entry(label.to_string()).or_insert_with(|| TrackEntry { ttl, ..Default::default() });
        if ttl.is_some() {
            s.ttl = ttl;
        }
        f(s);
    }

    fn fetched_at(&self, url: &str, params: &Params) -> Option<f64> {
        self.cache.lookup(&cache_key("GET", url, params, &[])).map(|r| r.3)
    }

    fn record(&self, url: &str, params: &Params, label: &str, ttl: f64) {
        let fetched = self.fetched_at(url, params);
        self.with_entry(label, Some(ttl), |s| {
            s.requests += 1;
            if let Some(f) = fetched {
                s.oldest = Some(match s.oldest {
                    None => f,
                    Some(o) => o.min(f),
                });
            }
        });
    }

    fn record_failure(&self, url: &str, params: &Params, label: &str, ttl: f64, e: &anyhow::Error) {
        let fetched = self.fetched_at(url, params);
        let err = short(e);
        self.with_entry(label, Some(ttl), |s| {
            s.failed += 1;
            s.error = Some(err);
            if let Some(f) = fetched {
                s.last_good = Some(match s.last_good {
                    None => f,
                    Some(o) => o.max(f),
                });
            }
        });
    }

    pub fn fetch_json(&self, url: &str, params: &Params) -> Result<Value> {
        let (label, ttl) = ttl_for(url, params, self.today);
        match self.cache.get_json(url, params, &[], &[], ttl) {
            Ok(v) => {
                self.record(url, params, label, ttl);
                Ok(v)
            }
            Err(e) => {
                self.record_failure(url, params, label, ttl, &e);
                Err(e)
            }
        }
    }

    pub fn fetch_text(&self, url: &str, params: &Params) -> Result<String> {
        let (label, ttl) = ttl_for(url, params, self.today);
        match self.cache.get_text(url, params, &[], &[], ttl) {
            Ok(v) => {
                self.record(url, params, label, ttl);
                Ok(v)
            }
            Err(e) => {
                self.record_failure(url, params, label, ttl, &e);
                Err(e)
            }
        }
    }

    /// One SourceStatus per tracked source: fail when every request failed, warn when some did;
    /// `fetched_at` is the oldest data served (or the newest cached copy of a failure).
    pub fn statuses(&self, now: f64) -> Vec<SourceStatus> {
        let mut out = Vec::new();
        for (label, s) in self.sources() {
            let name = source_name(&label);
            let (good, bad) = (s.requests, s.failed);
            let t = if good != 0 { s.oldest } else { s.last_good };
            let err = s.error.clone().unwrap_or_else(|| "None".into());
            let (sev, detail) = if bad == 0 {
                (SourceSeverity::Ok, None)
            } else if good != 0 {
                (SourceSeverity::Warn, Some(format!("{bad} of {} requests failed ({err})", good + bad)))
            } else {
                (SourceSeverity::Fail, Some(format!("unavailable ({err})")))
            };
            let feeds = VALUATION_SOURCES.contains(&name.as_str());
            out.push(SourceStatus {
                name,
                ok: sev != SourceSeverity::Fail,
                severity: sev,
                fetched_at: utc(t),
                age_seconds: t.map(|t| (now - t).max(0.0)),
                ttl_seconds: s.ttl,
                detail,
                requests: Some(good + bad),
                feeds_valuation: feeds,
                stale: None,
            });
        }
        out
    }

    /// "NHL rosters: 32 requests, data 20h ago" per label, sorted by label.
    pub fn notes(&self, now: f64) -> Vec<String> {
        self.sources()
            .into_iter()
            .map(|(label, s)| {
                let fresh = match s.oldest {
                    Some(o) if o != 0.0 => format!(", data {}", age(now - o)),
                    _ => String::new(),
                };
                format!("{label}: {} requests{fresh}", s.requests)
            })
            .collect()
    }
}

/// `make_fetch_json(cache)`: a tracked fetch_json (its own tracker).
pub fn make_fetch_json(cache: Arc<HttpCache>, today: NaiveDate) -> Box<FetchJson<'static>> {
    let t = SourceTracker::new(cache, today);
    Box::new(move |url, params, _ttl| t.fetch_json(url, params))
}

/// `make_fetch_text(cache)`: a tracked fetch_text (its own tracker).
pub fn make_fetch_text(cache: Arc<HttpCache>, today: NaiveDate) -> Box<fm_http::FetchText<'static>> {
    let t = SourceTracker::new(cache, today);
    Box::new(move |url, params, _ttl| t.fetch_text(url, params))
}

// --------------------------------------------------------------------------- pure helpers

/// More severe of the two (healthy < dtd < out < ir < ltir); suspension is kept unless the other
/// source reports an injury at least as serious as `out`.
pub fn merge_status(provider: &str, feed: &str) -> String {
    let suspended = provider == "suspended" || feed == "suspended";
    let a = if provider == "suspended" { "healthy" } else { provider };
    let b = if feed == "suspended" { "healthy" } else { feed };
    let worst = if severity(b) > severity(a) { b } else { a };
    if suspended && severity(worst) < severity("out") {
        return "suspended".into();
    }
    worst.to_string()
}

fn truthy(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|x| !x.is_empty())
}

/// `_injury_note(r)`.
pub fn injury_note(r: &InjuryReport) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let comment = truthy(&r.comment);
    if let Some(t) = truthy(&r.injury_type) {
        if comment.is_none_or(|c| !c.to_lowercase().contains(&t.to_lowercase())) {
            parts.push(format!("{t}:"));
        }
    }
    if let Some(c) = comment {
        parts.push(c.to_string());
    }
    if let Some(d) = r.return_date {
        parts.push(format!("(est. return {})", d.format("%Y-%m-%d")));
    }
    let joined = parts.join(" ");
    let mut note = fm_pycompat::text::py_strip(&joined).to_string();
    if note.chars().count() > 220 {
        note = format!("{}...", py_rstrip(py_slice_str(&note, None, Some(217))));
    }
    (!note.is_empty()).then_some(note)
}

fn pos_str(p: &Player) -> Option<String> {
    let pos: Vec<&str> = p.positions.iter().map(String::as_str).filter(|x| *x != "F").collect();
    let pos = if pos.is_empty() { p.positions.iter().map(String::as_str).collect() } else { pos };
    (!pos.is_empty()).then(|| pos.join("/"))
}

fn teams_agree(a: Option<&str>, b: Option<&str>) -> bool {
    match (normalize_team(a), normalize_team(b)) {
        (Some(ta), Some(tb)) => ta.is_empty() || tb.is_empty() || ta == tb,
        _ => true,
    }
}

/// Apply injury reports to players (ESPN id first, else name + team). Returns matches.
pub fn merge_injuries(players: &mut [&mut Player], reports: &[InjuryReport]) -> usize {
    if reports.is_empty() {
        return 0;
    }
    let mut by_espn: HashMap<String, &InjuryReport> = HashMap::new();
    for r in reports {
        if let Some(id) = r.espn_player_id {
            by_espn.insert(id.to_string(), r);
        }
    }
    let index = PlayerIndex::new(
        reports
            .iter()
            .enumerate()
            .map(|(i, r)| Candidate::new(i as i64, &r.player_name, r.team.as_deref(), r.position.as_deref())),
    );
    let mut applied = 0;
    for p in players.iter_mut() {
        let mut rep = if p.cid.starts_with("espn:") {
            by_espn.get(p.ids.get("espn").map(String::as_str).unwrap_or("")).copied()
        } else {
            None
        };
        if rep.is_none() {
            let m = match_player(&p.name, &index, p.team.as_deref(), pos_str(p).as_deref());
            if !m.matched() {
                continue;
            }
            let Some(i) = m.key.as_ref().and_then(|k| k.as_i64()) else { continue };
            let cand = &reports[i as usize];
            if !teams_agree(p.team.as_deref(), cand.team.as_deref()) {
                continue;
            }
            rep = Some(cand);
        }
        let rep = rep.expect("set above");
        applied += 1;
        p.status = Status::parse(&merge_status(p.status.as_str(), rep.status.as_str())).unwrap_or_default();
        if let Some(note) = injury_note(rep) {
            p.status_note = Some(note);
        }
    }
    applied
}

/// `Split` from its literal.
pub fn split_of(s: &str) -> Option<Split> {
    Some(match s {
        "season" => Split::Season,
        "prior" => Split::Prior,
        "prior2" => Split::Prior2,
        "prior3" => Split::Prior3,
        "last7" => Split::Last7,
        "last15" => Split::Last15,
        "last30" => Split::Last30,
        "projected" => Split::Projected,
        _ => return None,
    })
}

/// Canonical StatLine from an NHL season row's `stats` (None if no games).
pub fn statline_from_nhl(row_stats: &Map<String, f64>, split: Split) -> Option<StatLine> {
    let mut stats: Map<String, f64> =
        row_stats.iter().filter(|(k, _)| fm_core::models::is_canonical_stat(k)).map(|(k, v)| (k.clone(), *v)).collect();
    if let (Some(&ppp), Some(&ppg)) = (stats.get("PPP"), stats.get("PPG")) {
        stats.entry("PPA".into()).or_insert(ppp - ppg);
    }
    if let (Some(&shp), Some(&shg)) = (stats.get("SHP"), stats.get("SHG")) {
        stats.entry("SHA".into()).or_insert(shp - shg);
    }
    let gp = stats.get("GP").copied().unwrap_or(0.0).trunc() as i64;
    if gp <= 0 {
        return None;
    }
    Some(StatLine { split, gp, stats })
}

/// NHL season rows by player id (`{r.player_id: r for r in [*skaters, *goalies]}`: a later row wins).
pub type NhlRows<'a> = HashMap<i64, &'a Map<String, f64>>;

pub fn rows_by_id<'a>(sk: &'a [NhlSkaterSeason], gl: &'a [NhlGoalieSeason]) -> NhlRows<'a> {
    let mut m: NhlRows = HashMap::new();
    for r in sk {
        m.insert(r.player_id, &r.stats);
    }
    for r in gl {
        m.insert(r.player_id, &r.stats);
    }
    m
}

/// Add missing `season` / `prior` / `prior2` / `prior3` lines (seasons N, N-1, N-2, N-3) from NHL
/// rows keyed by NHL id. Returns {split: filled}.
pub fn fill_stat_lines(
    players: &mut [&mut Player],
    current: &NhlRows,
    prior: &NhlRows,
    prior2: Option<&NhlRows>,
    prior3: Option<&NhlRows>,
) -> Map<String, i64> {
    let empty: NhlRows = HashMap::new();
    let sources: [(&str, Split, &NhlRows); 4] = [
        ("season", Split::Season, current),
        ("prior", Split::Prior, prior),
        ("prior2", Split::Prior2, prior2.unwrap_or(&empty)),
        ("prior3", Split::Prior3, prior3.unwrap_or(&empty)),
    ];
    let mut filled: Map<String, i64> = sources.iter().map(|(s, _, _)| (s.to_string(), 0)).collect();
    for p in players.iter_mut() {
        let Some(nid) = p.nhl_id() else { continue };
        for (name, split, rows) in &sources {
            if p.lines.contains_key(*name) {
                continue;
            }
            let Some(stats) = rows.get(&nid) else { continue };
            if let Some(line) = statline_from_nhl(stats, *split) {
                p.lines.insert(name.to_string(), line);
                *filled.get_mut(*name).expect("initialised") += 1;
            }
        }
    }
    filled
}

/// Aggregate game-log entries into date-window StatLines (last7 / last15 / last30) ending at
/// `as_of` (inclusive).
pub fn recent_lines(entries: &[NhlGameLogEntry], as_of: NaiveDate) -> Map<String, StatLine> {
    let mut out: Map<String, StatLine> = Map::new();
    for (name, split, days) in [("last7", Split::Last7, 7), ("last15", Split::Last15, 15), ("last30", Split::Last30, 30)] {
        let start = as_of - Duration::days(days);
        let sel: Vec<&NhlGameLogEntry> = entries.iter().filter(|e| start < e.date && e.date <= as_of).collect();
        if sel.is_empty() {
            continue;
        }
        let mut tot: Map<String, f64> = Map::new();
        let mut toi = 0.0;
        for e in &sel {
            toi += e.toi_seconds.unwrap_or(0.0);
            for (k, v) in &e.stats {
                if fm_core::models::is_canonical_stat(k) && k != "SVPCT" && k != "GAA" {
                    *tot.entry(k.clone()).or_insert(0.0) += *v;
                }
            }
        }
        tot.insert("GP".into(), sel.len() as f64);
        if sel.iter().any(|e| e.is_goalie) {
            let sa = tot.get("SA").copied().unwrap_or(0.0);
            let ga = tot.get("GA").copied().unwrap_or(0.0);
            tot.insert("SVPCT".into(), if sa != 0.0 { (sa - ga) / sa } else { 0.0 });
            tot.insert("GAA".into(), if toi != 0.0 { ga * 3600.0 / toi } else { ga / sel.len() as f64 });
        }
        out.insert(name.to_string(), StatLine { split, gp: sel.len() as i64, stats: tot });
    }
    out
}

// --------------------------------------------------------------------------- player access

/// Mutable access to `ctx.all_players()`: the first occurrence of each cid (rostered players in
/// team / slot order, then free agents) - the objects Python mutates.
pub fn players_mut<'a>(teams: &'a mut [FantasyTeam], free_agents: &'a mut [Player]) -> Vec<&'a mut Player> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<&'a mut Player> = Vec::new();
    for t in teams.iter_mut() {
        for s in t.slots.iter_mut() {
            if let Some(p) = s.player.as_mut() {
                if seen.insert(p.cid.clone()) {
                    out.push(p);
                }
            }
        }
    }
    for p in free_agents.iter_mut() {
        if seen.insert(p.cid.clone()) {
            out.push(p);
        }
    }
    out
}

/// [`players_mut`] over a whole context.
pub fn ctx_players_mut(ctx: &mut LeagueContext) -> Vec<&mut Player> {
    players_mut(&mut ctx.teams, &mut ctx.free_agents)
}

/// cids of rostered players.
pub fn rostered_cids(ctx: &LeagueContext) -> HashSet<String> {
    ctx.teams.iter().flat_map(|t| t.players_iter().map(|p| p.cid.clone())).collect()
}

// --------------------------------------------------------------------------- options

/// Python `enrich_context`'s keyword arguments.
pub struct EnrichOptions<'a> {
    /// replaces the tracked NHL client (tests)
    pub nhl: Option<NhlClient>,
    /// replaces the injury feed fetcher (tests)
    pub injuries_fetch: Option<Box<FetchJson<'static>>>,
    /// crosswalk to use (default: `<fm_data_dir>/crosswalk.db`, opened and closed here)
    pub crosswalk: Option<&'a Crosswalk>,
    pub teams: Vec<String>,
    pub deep_fa_limit: usize,
    pub pedigree_limit: usize,
    pub deployment: bool,
    pub lines: bool,
    pub xg: bool,
    pub preseason: bool,
    /// a harness Ledger overriding `<fm_data_dir>/harness.db`
    pub ledger: Option<&'a Ledger>,
    pub lines_client: Option<DailyFaceoffClient<'a>>,
    pub xg_client: Option<&'a MoneyPuckClient<'a>>,
    pub rookies: bool,
    pub clock: Clock,
}

impl Default for EnrichOptions<'_> {
    fn default() -> Self {
        EnrichOptions {
            nhl: None,
            injuries_fetch: None,
            crosswalk: None,
            teams: NHL_TEAMS.iter().map(|t| t.to_string()).collect(),
            deep_fa_limit: DEEP_FA_LIMIT,
            pedigree_limit: PEDIGREE_LIMIT,
            deployment: true,
            lines: true,
            xg: true,
            preseason: true,
            ledger: None,
            lines_client: None,
            xg_client: None,
            rookies: true,
            clock: Clock::system(),
        }
    }
}

// --------------------------------------------------------------------------- orchestration

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StepFailure {
    pub attempts: i64,
    pub failed: i64,
    pub detail: Option<String>,
}

/// The results of steps 7b-10 that `collect_sources` reads.
#[derive(Debug, Default)]
pub struct StepResults {
    pub preseason: Option<PreseasonResult>,
    pub deployment: Option<DeploymentInfo>,
    pub deployment_warnings: Vec<String>,
    pub lines: Option<LinesResult>,
    pub xg: Option<XgEnrichResult>,
}

struct Steps {
    failures: Map<String, StepFailure>,
    season: i64,
    prior: i64,
}

impl Steps {
    fn begin(&mut self, name: &str) {
        let key = step_source(name, Some(self.season), Some(self.prior));
        self.failures.entry(key).or_default().attempts += 1;
    }

    fn end<T>(&mut self, name: &str, r: Result<T>, warnings: &mut Vec<String>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                let s = short(&e);
                warnings.push(format!("{name} unavailable ({s})"));
                let f = self.failures.entry(step_source(name, Some(self.season), Some(self.prior))).or_default();
                f.failed += 1;
                if f.detail.is_none() {
                    f.detail = Some(format!("unavailable ({s})"));
                }
                None
            }
        }
    }
}

/// Mutates `ctx`; never fails for a single failing data source (see the module docs).
///
/// `deployment` / `lines` / `xg` switch steps 8-10 off (tests, offline runs). Steps 6c, 7b, 9 and
/// 10 need `cache` (skipped without one unless a client is injected).
pub fn enrich_context(
    ctx: &mut LeagueContext,
    settings: &Settings,
    cache: Option<Arc<HttpCache>>,
    deep: bool,
    opts: EnrichOptions<'_>,
) {
    let EnrichOptions {
        nhl,
        injuries_fetch,
        crosswalk,
        teams,
        deep_fa_limit,
        pedigree_limit,
        deployment,
        lines,
        xg,
        preseason,
        ledger,
        mut lines_client,
        xg_client,
        rookies,
        clock,
    } = opts;
    let tracker = cache.as_ref().map(|c| SourceTracker::new(c.clone(), clock.today));
    let season = current_season(ctx.as_of);
    let prior = prior_season(season);
    let prior2 = prior_season(prior);
    let prior3 = prior_season(prior2);
    let injected_nhl = nhl.is_some();
    let client: NhlClient = match nhl {
        Some(c) => c,
        None => match &tracker {
            Some(t) => {
                let t = t.clone();
                NhlClient::new(Box::new(move |url, params, _ttl| t.fetch_json(url, params)), Some(season), clock.today)
            }
            None => NhlClient::with_default_fetch(Some(season), clock.today),
        },
    };
    let team_refs: Vec<&str> = teams.iter().map(String::as_str).collect();
    let mut steps = Steps { failures: Map::new(), season, prior };
    let mut results = StepResults::default();

    macro_rules! step {
        ($name:expr, $body:expr) => {{
            let name: &str = &$name;
            steps.begin(name);
            let r = $body;
            steps.end(name, r, &mut ctx.warnings)
        }};
    }

    // 1. schedule ---------------------------------------------------------------
    let as_of = ctx.as_of;
    let wk = step!("NHL week schedule", client.week_schedule(Some(as_of)));
    let games = step!("NHL team schedules", client.league_games(Some(season), &team_refs, Some(2)));
    if let Some(games) = games.as_ref().filter(|g| !g.is_empty()) {
        ctx.schedule = games
            .iter()
            .map(|(t, gs)| {
                let mut ds: Vec<NaiveDate> = gs.iter().map(|g| g.date).collect();
                ds.sort();
                (t.clone(), ds)
            })
            .collect();
        ctx.games_per_day = games_per_day(&ctx.schedule);
        ctx.opponents = games
            .iter()
            .map(|(t, gs)| {
                let m: Map<NaiveDate, String> = gs
                    .iter()
                    .map(|g| (g.date, if &g.home == t { g.away.clone() } else { format!("@{}", g.home) }))
                    .collect();
                (t.clone(), m)
            })
            .collect();
        ctx.game_starts = games
            .iter()
            .map(|(t, gs)| (t.clone(), gs.iter().filter_map(|g| g.start_time_utc.map(|s| (g.date, s))).collect()))
            .collect();
    }
    let min_start = ctx.schedule.values().flat_map(|ds| ds.iter().copied()).min();
    ctx.season_start = wk.as_ref().and_then(|w| w.regular_season_start).or(min_start);
    if !ctx.schedule.is_empty() {
        let n: i64 = ctx.games_per_day.values().sum();
        let start = ctx.season_start.map(|d| d.to_string()).unwrap_or_else(|| "None".into());
        ctx.source_notes.push(format!("Schedule {season}: {n} games, regular season starts {start}"));
    }

    // 2. NHL season rows and rosters ---------------------------------------------
    let cur_sk = step!(format!("NHL skater stats {season}"), client.all_skaters(Some(season), 2, true, true)).unwrap_or_default();
    let cur_g = step!(format!("NHL goalie stats {season}"), client.goalie_summary(Some(season), 2)).unwrap_or_default();
    let pr_sk = step!(format!("NHL skater stats {prior}"), client.all_skaters(Some(prior), 2, true, true)).unwrap_or_default();
    let pr_g = step!(format!("NHL goalie stats {prior}"), client.goalie_summary(Some(prior), 2)).unwrap_or_default();
    let mut hist_sk: Vec<(i64, Vec<NhlSkaterSeason>, Vec<NhlGoalieSeason>)> = Vec::new();
    for hs in [prior2, prior3] {
        let sk = step!(format!("NHL skater stats {hs}"), client.all_skaters(Some(hs), 2, true, true)).unwrap_or_default();
        let gl = step!(format!("NHL goalie stats {hs}"), client.goalie_summary(Some(hs), 2)).unwrap_or_default();
        hist_sk.push((hs, sk, gl));
    }
    let mut roster: Vec<NhlRosterPlayer> = Vec::new();
    let mut roster_errors = 0usize;
    let mut last_err: Option<anyhow::Error> = None;
    for t in &team_refs {
        match client.team_roster(t, Some(season)) {
            Ok(r) => roster.extend(r),
            Err(e) => {
                roster_errors += 1;
                last_err = Some(e);
            }
        }
    }
    if roster_errors > 0 {
        let s = short(last_err.as_ref().expect("set with roster_errors"));
        ctx.warnings.push(format!("NHL rosters: {roster_errors}/{} teams unavailable ({s})", teams.len()));
        steps.failures.insert(
            "NHL rosters".into(),
            StepFailure {
                attempts: teams.len() as i64,
                failed: roster_errors as i64,
                detail: Some(format!("{roster_errors} of {} teams unavailable ({s})", teams.len())),
            },
        );
    }
    let mut goalies: Map<String, Map<i64, String>> = Map::new();
    for r in &roster {
        if r.position.as_deref() == Some("G") {
            goalies.entry(r.team.clone()).or_default().insert(r.player_id, r.name());
        }
    }
    if !goalies.is_empty() {
        ctx.nhl_goalies = goalies;
    }

    // 3. crosswalk ------------------------------------------------------------------
    let cands = nhl_candidates(&[
        candidates_of(&pr_sk),
        candidates_of(&pr_g),
        candidates_of(&cur_sk),
        candidates_of(&cur_g),
        candidates_of(&roster),
    ]);
    if !cands.is_empty() {
        let mut own: Option<Crosswalk> = None;
        let res = (|| -> Result<fm_sources::matching::crosswalk::ResolveStats> {
            let xw: &Crosswalk = match crosswalk {
                Some(x) => x,
                None => {
                    own = Some(Crosswalk::new(&settings.fm_data_dir)?);
                    own.as_ref().expect("just set")
                }
            };
            let mut ps = ctx_players_mut(ctx);
            let mut owned: Vec<Player> = ps.iter().map(|p| (**p).clone()).collect();
            let r = xw.resolve(&mut owned, &cands);
            for (p, o) in ps.iter_mut().zip(owned) {
                p.ids = o.ids;
            }
            r
        })();
        match res {
            Ok(res) => {
                let hint = if res.pending > 0 { " (fm sync --review)" } else { "" };
                ctx.source_notes.push(format!("NHL ids: {}{hint}", res.summary()));
            }
            Err(e) => ctx.warnings.push(format!("crosswalk failed ({})", short(&e))),
        }
        if let Some(x) = &own {
            x.close();
        }
    }

    // 4. birth dates / teams from rosters ----------------------------------------------
    let by_roster: HashMap<i64, &NhlRosterPlayer> = roster.iter().map(|r| (r.player_id, r)).collect();
    for p in ctx_players_mut(ctx) {
        let Some(r) = p.nhl_id().and_then(|id| by_roster.get(&id)) else { continue };
        if p.birth_date.is_none() && r.birth_date.is_some() {
            p.birth_date = r.birth_date;
        }
        if p.team.as_deref().is_none_or(str::is_empty) {
            p.team = Some(r.team.clone());
        }
    }

    // 5. injuries ------------------------------------------------------------------------
    let tracked_fetch: Option<Box<FetchJson<'static>>> = tracker.as_ref().map(|t| {
        let t = t.clone();
        Box::new(move |url: &str, params: &Params, _ttl: f64| t.fetch_json(url, params)) as Box<FetchJson<'static>>
    });
    let inj_fetch: Option<&FetchJson> = injuries_fetch.as_deref().or(tracked_fetch.as_deref());
    let reports = step!("Injury feed", fetch_injuries(inj_fetch, TTL_INJURIES));
    if let Some(reports) = reports {
        let n = merge_injuries(&mut ctx_players_mut(ctx), &reports);
        ctx.source_notes.push(format!("Injuries: {} reports, {n} matched to league players", reports.len()));
    }

    // 6. stat lines ---------------------------------------------------------------------
    let cur_rows = rows_by_id(&cur_sk, &cur_g);
    let pr_rows = rows_by_id(&pr_sk, &pr_g);
    let hist_rows: Vec<(i64, NhlRows)> = hist_sk.iter().map(|(hs, sk, gl)| (*hs, rows_by_id(sk, gl))).collect();
    let h2 = &hist_rows[0].1;
    let h3 = &hist_rows[1].1;
    if !cur_rows.is_empty() || !pr_rows.is_empty() || hist_rows.iter().any(|(_, r)| !r.is_empty()) {
        let filled = fill_stat_lines(&mut ctx_players_mut(ctx), &cur_rows, &pr_rows, Some(h2), Some(h3));
        ctx.source_notes.push(format!(
            "NHL stats: {} rows {season}, {} rows {prior}, {} rows {prior2}, {} rows {prior3}; filled {} season / {} prior / {} prior2 / {} prior3 lines",
            cur_rows.len(),
            pr_rows.len(),
            h2.len(),
            h3.len(),
            filled["season"],
            filled["prior"],
            filled["prior2"],
            filled["prior3"]
        ));
    }

    // 6b. pedigree (needs prior lines from step 6) ------------------------------------------
    let mut landings: HashMap<i64, NhlPlayerLanding> = HashMap::new();
    step!("NHL player pages", pedigree(ctx, &client, pedigree_limit, cache.as_deref(), &mut landings, clock.now));

    // 6c. rookie evidence: league history (reuses 6b's landings) + news role signals -----------
    if rookies {
        if let Some(c) = &cache {
            let tr = tracker.clone();
            let news_fetch = move |url: &str, params: Option<&Params>| -> Result<String> {
                let empty = Vec::new();
                tr.as_ref().expect("tracker exists with a cache").fetch_text(url, params.unwrap_or(&empty))
            };
            let llm = rookie_llm(settings);
            let cache_c = c.clone();
            let now = clock.now;
            let is_cached = move |nid: i64| landing_cached(Some(&cache_c), nid, now);
            let ropts = RookieOptions {
                landings: Some(&landings),
                news: None,
                fetch_text: Some(&news_fetch),
                llm: llm.as_ref().map(|l| l as &dyn fm_sources::llm::SupportsComplete),
                store_dir: Some(settings.fm_data_dir.as_path()),
                limit: HISTORY_LIMIT,
                is_cached: Some(&is_cached),
                fetch_missing: true,
                now: clock.utc(),
            };
            step!("Rookie evidence", enrich_rookies(ctx, Some(&client), ropts).map(|_| ()));
        }
    }

    // 7. deep: recent form from game logs ---------------------------------------------------
    if deep {
        deep_game_logs(ctx, &client, season, deep_fa_limit);
    }

    // 7b. preseason lines of unproven skaters (box scores; needs career GP from 6b) ---------------
    if preseason {
        if let Some(c) = &cache {
            results.preseason = step!("NHL preseason box scores", enrich_preseason(ctx, Some(c), None, Some(season), &teams));
        }
    }

    if let Some(t) = &tracker {
        ctx.source_notes.extend(t.notes(clock.now));
    }

    // 8-10. deployment, lines, expected goals (each best effort) -------------------------------
    if deployment {
        let own_dep: Option<NhlClient> = if injected_nhl {
            None
        } else {
            cache.as_ref().map(|c| {
                let today = clock.today;
                NhlClient::new(cached_nhl_fetch(c.clone()), Some(season), ctx.as_of)
                    .with_ttl_for(Box::new(move |u, p| ttl_for(u, p, today).1))
            })
        };
        let dep_nhl: Option<&NhlClient> = if injected_nhl { Some(&client) } else { own_dep.as_ref() };
        let n_warn = ctx.warnings.len();
        results.deployment = step!("Deployment", deployment_step(ctx, settings, dep_nhl, ledger));
        results.deployment_warnings =
            ctx.warnings[n_warn..].iter().filter(|w| w.starts_with("Deployment (")).cloned().collect();
    }
    if lines {
        if cache.is_none() && lines_client.is_none() {
            ctx.source_notes.push("Daily Faceoff lines skipped (no HTTP cache)".into());
        } else {
            results.lines =
                step!("Daily Faceoff lines", lines_step(ctx, settings, cache.as_ref(), crosswalk, lines_client.as_mut(), clock));
        }
    }
    if xg {
        if cache.is_none() && xg_client.is_none() {
            ctx.source_notes.push("MoneyPuck expected goals skipped (no HTTP cache)".into());
        } else {
            results.xg = step!("MoneyPuck expected goals", xg_step(ctx, cache.as_ref(), xg_client, clock));
        }
    }
    // additive and best effort: never let the health summary sink enrichment
    match collect_sources(ctx, settings, cache.as_deref(), tracker.as_deref(), &steps.failures, &results, clock.now) {
        Ok(s) => ctx.sources = s,
        Err(e) => ctx.warnings.push(format!("Data health summary unavailable ({})", short(&e))),
    }
}

// --------------------------------------------------------------------------- steps 8-10

/// `_cached_at(cache, url, headers)`: when `url` was fetched into the HTTP cache (epoch seconds).
pub fn cached_at(cache: &HttpCache, url: &str, headers: &[(String, String)]) -> Option<f64> {
    cache.lookup(&cache_key("GET", url, &Vec::new(), headers)).map(|r| r.3)
}

/// `_open_ledger(settings)`: the harness ledger when `<fm_data_dir>/harness.db` exists (never
/// created here).
pub fn open_ledger(settings: &Settings) -> Result<Option<Ledger>> {
    let dir = &settings.fm_data_dir;
    if dir.as_os_str().is_empty() || !dir.join(DB_NAME).exists() {
        return Ok(None);
    }
    Ok(Some(Ledger::open(dir)?))
}

/// Step 8's info for `ctx.sources`.
#[derive(Clone, Debug, Default)]
pub struct DeploymentInfo {
    pub ledger: bool,
    pub last_game: Option<String>,
    pub pulled_at: Option<String>,
    pub result: Option<DeploymentResult>,
}

fn value_str(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Step 8: TOI / PP deployment per skater, and (ledger with >= 5 team games) goalie starts and
/// back-to-back second-night rates for valuation.
pub fn deployment_step(
    ctx: &mut LeagueContext,
    settings: &Settings,
    nhl: Option<&NhlClient>,
    ledger: Option<&Ledger>,
) -> Result<DeploymentInfo> {
    let owned: Option<Ledger> = if ledger.is_none() { open_ledger(settings)? } else { None };
    let led: Option<&Ledger> = ledger.or(owned.as_ref());
    let mut info = DeploymentInfo { ledger: led.is_some(), ..Default::default() };
    let as_of = ctx.as_of;
    let res = enrich_deployment(ctx, led, Some(as_of), nhl, fm_harness::deployment::DEFAULT_LAST_N, true)?;
    ctx.deployment_details = res.details.clone();
    let counts = match led {
        Some(l) => fm_harness::deployment::team_goalie_counts(l, as_of)?,
        None => Map::new(),
    };
    let ready: Map<&String, &fm_harness::deployment::TeamGoalieCounts> =
        counts.iter().filter(|(_, d)| d.games >= MIN_LEDGER_TEAM_GAMES).collect();
    if let Some(l) = led {
        for t in ready.keys() {
            let r = fm_harness::deployment::b2b_second_night_rate(l, t, as_of, Some(&counts))?;
            ctx.b2b_second_night.insert((*t).clone(), r);
        }
    }
    let mut actual: Vec<(String, (i64, i64))> = Vec::new();
    for p in ctx.all_players() {
        let Some(team) = normalize_team(p.team.as_deref()) else { continue };
        let Some(nid) = p.nhl_id() else { continue };
        if !p.is_goalie() {
            continue;
        }
        let Some(d) = ready.get(&team) else { continue };
        actual.push((p.cid.clone(), (d.starts.get(&nid).copied().unwrap_or(0), d.games)));
    }
    for (cid, v) in actual {
        ctx.goalie_actual_starts.insert(cid, v);
    }
    let mut fresh = String::new();
    if let Some(l) = led {
        let last = l.query("SELECT MAX(game_date) d, MAX(pulled_at) t FROM deployment_pulls", &[])?;
        if let Some(row) = last.first() {
            if let Some(d) = row.get("d").and_then(value_str) {
                fresh = format!("; ledger through {d}");
                info.last_game = Some(d);
                info.pulled_at = row.get("t").and_then(value_str);
            }
        }
    }
    let src = if led.is_some() { "ledger" } else { "no harness ledger" };
    ctx.source_notes.push(format!(
        "Deployment (NHL TOI / PP reports, {src}): {} skaters - {} from the ledger, {} from the season report, {} from last season, {} missing; {} with trends; {} teams with >= {MIN_LEDGER_TEAM_GAMES} ledger games (goalie starts / back-to-backs){fresh}",
        res.skaters,
        res.ledger,
        res.season_report,
        res.prior_season,
        res.missing,
        res.trends,
        ready.len()
    ));
    info.result = Some(res);
    Ok(info)
}

/// Step 9: Daily Faceoff lines / units / starting goalies (and the daily snapshot).
pub fn lines_step(
    ctx: &mut LeagueContext,
    settings: &Settings,
    cache: Option<&Arc<HttpCache>>,
    crosswalk: Option<&Crosswalk>,
    client: Option<&mut DailyFaceoffClient<'_>>,
    clock: Clock,
) -> Result<LinesResult> {
    let own: Option<Crosswalk> = if crosswalk.is_none() {
        match Crosswalk::new(&settings.fm_data_dir) {
            Ok(x) => Some(x),
            Err(e) => {
                // match without persisting
                ctx.warnings.push(format!("Daily Faceoff crosswalk unavailable ({})", short(&e)));
                None
            }
        }
    } else {
        None
    };
    let xw = crosswalk.or(own.as_ref());
    let store = LineSnapshotStore::new(&settings.fm_data_dir);
    let as_of = ctx.as_of;
    let res = enrich_lines(ctx, cache, xw, Some(as_of), Some(&store), client, None, None, true, clock);
    if let Some(x) = &own {
        x.close();
    }
    if let Some(c) = cache {
        let ua = vec![("User-Agent".to_string(), dailyfaceoff::USER_AGENT.to_string())];
        if let Some(t) = cached_at(c, &dailyfaceoff::goalies_url(ctx.as_of), &ua) {
            ctx.source_notes.push(format!(
                "Daily Faceoff (dailyfaceoff.com; lines cached 12h, starting goalies 3h): starting goalies fetched {}",
                age(clock.now - t)
            ));
        }
    }
    Ok(res)
}

/// Step 10: MoneyPuck expected goals (credit line added by `enrich_xg`).
pub fn xg_step(
    ctx: &mut LeagueContext,
    cache: Option<&Arc<HttpCache>>,
    client: Option<&MoneyPuckClient<'_>>,
    clock: Clock,
) -> Result<XgEnrichResult> {
    let res = enrich_xg(ctx, cache, None, client, clock.today);
    if let Some(c) = cache {
        if res.count() > 0 {
            let url = moneypuck::season_url(moneypuck::current_start_year(Some(ctx.as_of)), moneypuck::Kind::Skaters);
            if let Some(t) = cached_at(c, &url, &[]) {
                ctx.source_notes.push(format!(
                    "MoneyPuck: {} skaters with xG; current-season file fetched {}",
                    res.count(),
                    age(clock.now - t)
                ));
            }
        }
    }
    Ok(res)
}

/// `_rookie_llm(settings)`: the free-only LLM client for the news-role pass, or None (no key, or
/// FM_NEWS_ROLES_LLM=0).
pub fn rookie_llm(settings: &Settings) -> Option<fm_sources::llm::LLMClient> {
    use fm_sources::llm::SupportsComplete;
    let flag = std::env::var("FM_NEWS_ROLES_LLM").unwrap_or_else(|_| "1".into());
    if matches!(flag.trim().to_lowercase().as_str(), "0" | "false" | "no" | "off") {
        return None;
    }
    settings.openrouter_api_key.as_ref().filter(|k| !k.expose().is_empty())?;
    let client = fm_sources::llm::LLMClient::new(Some(settings), None, None, None, 30.0);
    client.available().then_some(client)
}

/// Young (<= PEDIGREE_MAX_AGE, or age unknown) or unproven (no prior line with at least
/// PEDIGREE_PROVEN_GP games) players whose draft pedigree has not been loaded yet.
pub fn needs_pedigree(p: &Player, as_of: NaiveDate) -> bool {
    if p.nhl_id().is_none() || p.draft_overall.is_some() || p.career_gp.is_some() {
        return false;
    }
    let age = p.birth_date.map(|b| (as_of - b).num_days() as f64 / 365.25);
    match age {
        None => true,
        Some(a) if a <= PEDIGREE_MAX_AGE => true,
        _ => p.gp("prior") < PEDIGREE_PROVEN_GP,
    }
}

/// Indices (into `ctx.all_players()`) of the players to look up: every rostered one, then the
/// `fa_limit` most-owned free agents.
pub fn pedigree_targets(ctx: &LeagueContext, fa_limit: usize) -> Vec<usize> {
    let rostered = rostered_cids(ctx);
    let players = ctx.all_players();
    let mut todo: Vec<usize> = (0..players.len()).filter(|&i| needs_pedigree(players[i], ctx.as_of)).collect();
    todo.sort_by(|&a, &b| {
        let ka = (!rostered.contains(&players[a].cid), -players[a].pct_owned.unwrap_or(0.0));
        let kb = (!rostered.contains(&players[b].cid), -players[b].pct_owned.unwrap_or(0.0));
        ka.0.cmp(&kb.0).then(fm_pycompat::order::cmp_f64(ka.1, kb.1))
    });
    let n_rostered = todo.iter().filter(|&&i| rostered.contains(&players[i].cid)).count();
    todo.truncate(n_rostered + fa_limit);
    todo
}

pub fn apply_landing(p: &mut Player, landing: &NhlPlayerLanding) {
    p.draft_overall = landing.draft_overall;
    p.draft_round = landing.draft_round;
    p.draft_year = landing.draft_year;
    p.career_gp = landing.career_gp;
    if p.birth_date.is_none() && landing.birth_date.is_some() {
        p.birth_date = landing.birth_date;
    }
}

/// True when the player's landing page is in the HTTP cache and still fresh.
pub fn landing_cached(cache: Option<&HttpCache>, player_id: i64, now: f64) -> bool {
    let Some(cache) = cache else { return false };
    let key = cache_key("GET", &format!("{WEB_BASE}/player/{player_id}/landing"), &Vec::new(), &[]);
    match cache.lookup(&key) {
        Some(row) => cache.offline || now - row.3 < TTL_LANDING,
        None => false,
    }
}

/// `_pedigree`: load pedigree for every target; only uncached pages count toward `limit`
/// network requests per run. `landings` collects the parsed pages by NHL id.
pub fn pedigree(
    ctx: &mut LeagueContext,
    client: &NhlClient,
    limit: usize,
    cache: Option<&HttpCache>,
    landings: &mut HashMap<i64, NhlPlayerLanding>,
    now: f64,
) -> Result<()> {
    let todo = pedigree_targets(ctx, PEDIGREE_FA_LIMIT);
    let (mut done, mut errors, mut drafted, mut fetched, mut skipped) = (0, 0, 0, 0usize, 0);
    {
        let mut players = ctx_players_mut(ctx);
        for &i in &todo {
            let p = &mut players[i];
            let nid = p.nhl_id().expect("targets carry an NHL id");
            if !landing_cached(cache, nid, now) {
                if fetched >= limit {
                    skipped += 1;
                    continue;
                }
                fetched += 1;
            }
            let landing = match client.player_landing(nid) {
                Ok(l) => l,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            };
            apply_landing(p, &landing);
            done += 1;
            drafted += landing.draft_overall.is_some() as i64;
            landings.insert(nid, landing);
        }
    }
    if !todo.is_empty() {
        let mut note = format!("Pedigree: {done}/{} young or unproven players loaded, {drafted} drafted", todo.len());
        if errors > 0 {
            note.push_str(&format!(", {errors} failed"));
        }
        if skipped > 0 {
            note.push_str(&format!(", {skipped} deferred to the next run (limit {limit} requests)"));
        }
        ctx.source_notes.push(note);
    }
    Ok(())
}

/// Step 7: last7 / last15 / last30 lines from per-player game logs.
pub fn deep_game_logs(ctx: &mut LeagueContext, client: &NhlClient, season: i64, fa_limit: usize) {
    if let Some(start) = ctx.season_start {
        if ctx.as_of < start {
            ctx.source_notes.push(format!("Game logs skipped: regular season starts {start}"));
            return;
        }
    }
    let mut fas: Vec<&Player> = ctx.free_agents.iter().collect();
    fas.sort_by(|a, b| fm_pycompat::order::cmp_f64(-a.pct_owned.unwrap_or(0.0), -b.pct_owned.unwrap_or(0.0)));
    fas.truncate(fa_limit);
    let mut todo: Map<String, ()> = Map::new();
    for p in ctx.teams.iter().flat_map(|t| t.players_iter()).chain(fas) {
        todo.insert(p.cid.clone(), ());
    }
    let as_of = ctx.as_of;
    let (mut checked, mut built, mut errors) = (0, 0, 0);
    {
        let mut players = ctx_players_mut(ctx);
        let by_cid: HashMap<String, usize> = players.iter().enumerate().map(|(i, p)| (p.cid.clone(), i)).collect();
        for cid in todo.keys() {
            let Some(&i) = by_cid.get(cid) else { continue };
            let p = &mut players[i];
            let Some(nid) = p.nhl_id() else { continue };
            if ["last7", "last15", "last30"].iter().any(|s| p.lines.contains_key(*s)) {
                continue;
            }
            checked += 1;
            match client.game_log(nid, Some(season), 2) {
                Ok(log) => {
                    let lines = recent_lines(&log, as_of);
                    if !lines.is_empty() {
                        p.lines.extend(lines);
                        built += 1;
                    }
                }
                Err(_) => errors += 1,
            }
        }
    }
    let failed = if errors > 0 { format!(", {errors} failed") } else { String::new() };
    ctx.source_notes.push(format!("Game logs: {checked} players checked, {built} with recent games{failed}"));
}

// --------------------------------------------------------------------------- data health (ctx.sources)

/// `step_source(name, season, prior)`: SourceStatus name of an `enrich_context` step.
pub fn step_source(name: &str, season: Option<i64>, prior: Option<i64>) -> String {
    if name.starts_with("NHL week schedule") || name.starts_with("NHL team schedules") {
        return "NHL schedules".into();
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^NHL (?:skater|goalie) stats (\d{8})$").expect("static regex"));
    if let Some(c) = re.captures(name) {
        let s: i64 = c[1].parse().unwrap_or(0);
        return if Some(s) == season || Some(s) == prior { "NHL stats" } else { "NHL stats (past seasons)" }.into();
    }
    match name {
        "Injury feed" => "Injuries",
        "NHL player pages" => "NHL player pages",
        "Rookie evidence" => "Rookie evidence",
        "NHL preseason box scores" => "NHL preseason",
        "Deployment" => "Deployment ledger",
        "Daily Faceoff lines" => "Daily Faceoff lines",
        "MoneyPuck expected goals" => "MoneyPuck",
        other => other,
    }
    .to_string()
}

fn rank(s: SourceSeverity) -> i64 {
    match s {
        SourceSeverity::Ok => 0,
        SourceSeverity::Warn => 1,
        SourceSeverity::Fail => 2,
    }
}

/// `_status(...)`.
#[allow(clippy::too_many_arguments)]
pub fn status(
    name: &str,
    severity: SourceSeverity,
    fetched: Option<f64>,
    now: f64,
    ttl: Option<f64>,
    detail: Option<String>,
    requests: Option<i64>,
    stale: Option<bool>,
) -> SourceStatus {
    SourceStatus {
        name: name.to_string(),
        ok: severity != SourceSeverity::Fail,
        severity,
        fetched_at: utc(fetched),
        age_seconds: fetched.map(|f| (now - f).max(0.0)),
        ttl_seconds: ttl,
        detail,
        requests,
        feeds_valuation: VALUATION_SOURCES.contains(&name),
        stale,
    }
}

fn re(pat: &'static str, cell: &'static OnceLock<Regex>) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("static regex"))
}

static LOGIN_RE: OnceLock<Regex> = OnceLock::new();
static FAILED_RE: OnceLock<Regex> = OnceLock::new();
static PARSE_RE: OnceLock<Regex> = OnceLock::new();
static EXTRA_RE: OnceLock<Regex> = OnceLock::new();

const FANTRAX_LABELS: [&str; 8] = [
    "Fantrax",
    "Standings",
    "League rules",
    "Trade blocks",
    "Pending trades",
    "Transactions",
    "Some Fantrax",
    "Free-agent pool",
];

/// `LOGIN_ADVICE`.
pub fn login_advice(provider: &str) -> &'static str {
    match provider {
        "fantrax" => "login expired — refresh FANTRAX_COOKIE (or run `fm auth fantrax --login`)",
        "espn" => "login expired — refresh ESPN_S2 / ESPN_SWID",
        _ => "login expired",
    }
}

/// `provider_label(provider)`: "Fantrax", "ESPN", else the title-cased name ("League").
pub fn provider_label(provider: &str) -> String {
    match provider.to_lowercase().as_str() {
        "fantrax" => "Fantrax".into(),
        "espn" => "ESPN".into(),
        _ => title(if provider.is_empty() { "League" } else { provider }),
    }
}

fn provider_warning(provider: &str, w: &str) -> bool {
    match provider {
        "fantrax" => FANTRAX_LABELS.iter().any(|l| w.starts_with(l)),
        "espn" => w.starts_with("ESPN "),
        _ => false,
    }
}

/// `_clip(text, n)`: whitespace collapsed, at most `n` characters ("..." when cut).
pub fn clip(text: &str, n: usize) -> String {
    let t = fm_pycompat::text::split_ws(text).join(" ");
    if t.chars().count() <= n {
        return t;
    }
    format!("{}...", py_rstrip(py_slice_str(&t, None, Some((n - 3) as isize))))
}

/// (severity, detail) for one provider warning, or None when it is not a data failure.
pub fn classify_provider_warning(provider: &str, w: &str) -> Option<(SourceSeverity, String)> {
    let provider = provider.to_lowercase();
    if !provider_warning(&provider, w) {
        return None;
    }
    if w.contains("session refreshed by login") {
        return Some((SourceSeverity::Ok, "session refreshed by login".into()));
    }
    let login = re(
        r"(?i)not logged in|log ?in\b|cookie|espn_s2|swid|\b401\b|\b403\b|unauthori[sz]ed|forbidden|session (?:expired|invalid)",
        &LOGIN_RE,
    );
    let failed = re(r"(?i)unavailable|failed|could not reach|not reachable|timed? ?out", &FAILED_RE);
    let parse = re(r"(?i)could not be parsed", &PARSE_RE);
    let extra = re(
        r"(?i)activity|box scores|trending|lineup snapshot|pro schedule|claim limits|move limits|% rostered|ownership",
        &EXTRA_RE,
    );
    if login.is_match(w) && (failed.is_match(w) || w.to_lowercase().contains("not logged in")) {
        return Some((SourceSeverity::Fail, login_advice(&provider).to_string()));
    }
    if parse.is_match(w) {
        return Some((SourceSeverity::Warn, clip(w, 120)));
    }
    if failed.is_match(w) {
        let sev = if extra.is_match(w) { SourceSeverity::Warn } else { SourceSeverity::Fail };
        return Some((sev, clip(w, 120)));
    }
    None
}

/// The league provider's SourceStatus from its warnings (the worst warning wins, its detail kept).
pub fn provider_status(provider: &str, warnings: &[String], now: f64) -> SourceStatus {
    let mut sev = SourceSeverity::Ok;
    let mut detail: Option<String> = None;
    for w in warnings {
        let Some((s, d)) = classify_provider_warning(provider, w) else { continue };
        if rank(s) > rank(sev) || (detail.is_none() && s == sev) {
            sev = s;
            detail = Some(d);
        }
    }
    status(&provider_label(provider), sev, None, now, None, Some(detail.unwrap_or_else(|| "league data loaded".into())), None, None)
}

fn cached_min(cache: &HttpCache, urls: impl IntoIterator<Item = String>) -> Option<f64> {
    urls.into_iter().filter_map(|u| cached_at(cache, &u, &[])).fold(None, |acc: Option<f64>, t| Some(acc.map_or(t, |a| a.min(t))))
}

fn lines_sources(as_of: NaiveDate, cache: Option<&HttpCache>, res: &LinesResult, now: f64) -> Vec<SourceStatus> {
    use fm_sources::dailyfaceoff::{goalies_url, team_url, TEAM_SLUGS, TTL_GOALIES, TTL_LINES};
    let warns = &res.warnings;
    let t_lines = cache.and_then(|c| cached_min(c, TEAM_SLUGS.iter().filter_map(|(t, _)| team_url(t).ok())));
    let t_goalies = cache.and_then(|c| cached_at(c, &goalies_url(as_of), &[]));
    let full: Vec<&String> = warns.iter().filter(|w| w.starts_with("Daily Faceoff lines unavailable (")).collect();
    let part: Vec<&String> = warns.iter().filter(|w| w.starts_with("Daily Faceoff lines unavailable for")).collect();
    let teams = res.teams;
    let (sev, detail) = if teams == 0 {
        (
            SourceSeverity::Fail,
            match full.first() {
                Some(w) => w.replacen("Daily Faceoff lines ", "", 1),
                None => "no line combinations found".into(),
            },
        )
    } else if let Some(w) = part.first() {
        (SourceSeverity::Warn, clip(&w.replacen("Daily Faceoff lines ", "", 1), 120))
    } else {
        (SourceSeverity::Ok, format!("{teams} teams"))
    };
    let mut out = vec![status("Daily Faceoff lines", sev, t_lines, now, Some(TTL_LINES), Some(detail), None, None)];
    let g_fail: Vec<&String> = warns.iter().filter(|w| w.starts_with("Daily Faceoff starting goalies unavailable")).collect();
    if let Some(w) = g_fail.first() {
        out.push(status(
            "Daily Faceoff goalies",
            SourceSeverity::Fail,
            t_goalies,
            now,
            Some(TTL_GOALIES),
            Some(w.replacen("Daily Faceoff starting goalies ", "", 1)),
            None,
            None,
        ));
    } else {
        let named = res.starts.iter().filter(|s| s.is_start()).count();
        out.push(status(
            "Daily Faceoff goalies",
            SourceSeverity::Ok,
            t_goalies,
            now,
            Some(TTL_GOALIES),
            Some(format!("{named} starters named for {as_of}")),
            None,
            None,
        ));
    }
    out
}

fn season_label(y: i64) -> String {
    let next = (y + 1).to_string();
    format!("{y}-{}", &next[next.len().saturating_sub(2)..])
}

fn xg_source(cache: Option<&HttpCache>, res: &XgEnrichResult, now: f64) -> SourceStatus {
    let (cur_y, prior_y) = res.seasons;
    let t = cache.and_then(|c| cached_at(c, &moneypuck::season_url(cur_y, moneypuck::Kind::Skaters), &[]));
    let missing: Vec<i64> = [cur_y, prior_y].into_iter().filter(|y| !res.rows.contains_key(y)).collect();
    let err = match res.errors.first() {
        Some(e) => e.split_once(": ").map(|(_, r)| r.to_string()).unwrap_or_else(|| e.clone()),
        None => "no rows".into(),
    };
    let (sev, detail) = if missing.len() == 2 {
        (SourceSeverity::Fail, format!("unavailable ({})", clip(&err, 100)))
    } else if missing.contains(&cur_y) {
        (
            SourceSeverity::Warn,
            format!("{} file unavailable ({}); using {} only", season_label(cur_y), clip(&err, 80), season_label(prior_y)),
        )
    } else if !missing.is_empty() {
        (SourceSeverity::Warn, format!("{} file unavailable ({})", season_label(prior_y), clip(&err, 80)))
    } else {
        (SourceSeverity::Ok, format!("{} skaters with xG", res.count()))
    };
    status("MoneyPuck", sev, t, now, Some(moneypuck::TTL_CURRENT), Some(detail), None, None)
}

/// `_parse_local(ts)`: epoch seconds of an ISO timestamp (naive = local time, as the harness
/// ledger writes it).
pub fn parse_local(ts: Option<&str>) -> Option<f64> {
    let ts = ts.filter(|s| !s.is_empty())?;
    let dt = PyDateTime::parse(&ts.replace('Z', "+00:00"))?;
    let utc = match dt.to_utc() {
        Some(u) => u,
        None => Local.from_local_datetime(&dt.local).earliest()?.with_timezone(&Utc),
    };
    Some(utc.timestamp() as f64 + utc.timestamp_subsec_micros() as f64 / 1e6)
}

fn deployment_source(ctx: &LeagueContext, info: &DeploymentInfo, report_warnings: &[String], now: f64) -> SourceStatus {
    let sev = if report_warnings.is_empty() { SourceSeverity::Ok } else { SourceSeverity::Warn };
    let extra = match report_warnings.first() {
        Some(w) => format!("; {}", clip(w, 100)),
        None => String::new(),
    };
    if !info.ledger {
        return status(
            "Deployment ledger",
            sev,
            None,
            now,
            None,
            Some(format!("no harness ledger; NHL season reports only{extra}")),
            None,
            Some(false),
        );
    }
    let last = info.last_game.as_deref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    let cutoff = ctx.as_of - Duration::days(1);
    let mut missed: Vec<NaiveDate> =
        ctx.games_per_day.keys().copied().filter(|d| last.is_none_or(|l| *d > l) && *d < cutoff).collect();
    missed.sort();
    let mut detail = match last {
        Some(l) => format!("through {l}"),
        None => "no games pulled yet".into(),
    };
    if !missed.is_empty() {
        detail.push_str(&format!("; {} game day(s) not pulled since (run `fm harness daily`)", missed.len()));
    }
    status(
        "Deployment ledger",
        sev,
        parse_local(info.pulled_at.as_deref()),
        now,
        None,
        Some(detail + &extra),
        None,
        Some(!missed.is_empty()),
    )
}

fn preseason_source(res: &PreseasonResult, now: f64) -> Option<SourceStatus> {
    let (games, failed) = (res.games, res.failed);
    if games == 0 && failed == 0 {
        return None; // skipped (out of window / no unproven skaters / no games yet)
    }
    if failed > 0 && games == 0 {
        return Some(status(
            "NHL preseason",
            SourceSeverity::Fail,
            None,
            now,
            None,
            Some(format!("all {failed} box scores failed")),
            None,
            None,
        ));
    }
    let sev = if failed > 0 { SourceSeverity::Warn } else { SourceSeverity::Ok };
    Some(status("NHL preseason", sev, None, now, None, Some(format!("{games}/{} box scores read", games + failed)), None, None))
}

/// `_odds_source`: the newest betting-odds archive, listed only when an odds API key is configured.
pub fn odds_source(settings: &Settings, as_of: NaiveDate, now: f64) -> Option<SourceStatus> {
    settings.odds_api_key.as_ref()?;
    if settings.fm_data_dir.as_os_str().is_empty() {
        return None;
    }
    let arch = settings.fm_data_dir.join("archive");
    let iso = as_of.format("%Y-%m-%d").to_string();
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if arch.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&arch) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(stem) = name.strip_suffix(".json") {
                    if let Some(day) = stem.strip_prefix("odds-") {
                        if day <= iso.as_str() {
                            files.push(e.path());
                        }
                    }
                }
            }
        }
    }
    files.sort();
    let Some(path) = files.last() else {
        return Some(status(
            "Odds archive",
            SourceSeverity::Warn,
            None,
            now,
            None,
            Some("no odds archived yet (`fm harness daily`)".into()),
            None,
            None,
        ));
    };
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut fetched = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().and_then(|o| o.get("fetched_at")).and_then(value_str))
        .and_then(|s| parse_local(Some(&s)));
    if fetched.is_none() {
        fetched = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64());
    }
    Some(status("Odds archive", SourceSeverity::Ok, fetched, now, None, Some(format!("latest {}", &stem[5..])), None, None))
}

/// `ctx.sources` for this run: the league provider (from its warnings), every source the tracker
/// saw, failed steps, and the Daily Faceoff / MoneyPuck / deployment / preseason / odds steps.
/// The provider comes first, the rest by name.
pub fn collect_sources(
    ctx: &LeagueContext,
    settings: &Settings,
    cache: Option<&HttpCache>,
    tracker: Option<&SourceTracker>,
    failures: &Map<String, StepFailure>,
    results: &StepResults,
    now: f64,
) -> Result<Vec<SourceStatus>> {
    let mut out: Map<String, SourceStatus> = Map::new();
    let prov = provider_status(&ctx.provider, &ctx.warnings, now);
    if let Some(t) = tracker {
        for st in t.statuses(now) {
            out.insert(st.name.clone(), st);
        }
    }
    for (name, f) in failures {
        if f.failed == 0 {
            continue;
        }
        let sev = if f.failed >= f.attempts { SourceSeverity::Fail } else { SourceSeverity::Warn };
        match out.get(name).cloned() {
            None => {
                out.insert(name.clone(), status(name, sev, None, now, None, f.detail.clone(), None, None));
            }
            Some(cur) if rank(sev) >= rank(cur.severity) => {
                let mut st = cur.clone();
                st.severity = sev;
                st.ok = sev != SourceSeverity::Fail;
                st.detail = f.detail.clone().or(cur.detail);
                out.insert(name.clone(), st);
            }
            Some(_) => {}
        }
    }
    if let Some(res) = &results.lines {
        for st in lines_sources(ctx.as_of, cache, res, now) {
            out.insert(st.name.clone(), st);
        }
    } else if failures.get("Daily Faceoff lines").is_some_and(|f| f.failed > 0) {
        // the step raised: no goalies either
        out.entry("Daily Faceoff goalies".into()).or_insert_with(|| {
            status(
                "Daily Faceoff goalies",
                SourceSeverity::Fail,
                None,
                now,
                None,
                Some("unavailable (Daily Faceoff step failed)".into()),
                None,
                None,
            )
        });
    }
    if let Some(res) = &results.xg {
        out.insert("MoneyPuck".into(), xg_source(cache, res, now));
    }
    if let Some(info) = &results.deployment {
        out.insert("Deployment ledger".into(), deployment_source(ctx, info, &results.deployment_warnings, now));
    }
    if let Some(res) = &results.preseason {
        if let Some(st) = preseason_source(res, now) {
            out.insert(st.name.clone(), st);
        }
    }
    if let Some(odds) = odds_source(settings, ctx.as_of, now) {
        out.insert(odds.name.clone(), odds);
    }
    out.shift_remove(&prov.name);
    let mut rest: Vec<SourceStatus> = out.into_values().collect();
    rest.sort_by_key(|s| fm_pycompat::text::py_lower(&s.name));
    let mut all = vec![prov];
    all.extend(rest);
    Ok(all)
}

/// `Path` of the fm data dir as an `Option` (Python `getattr(settings, "fm_data_dir", None)`).
pub fn data_dir(settings: &Settings) -> Option<&Path> {
    let p = settings.fm_data_dir.as_path();
    (!p.as_os_str().is_empty()).then_some(p)
}

#[cfg(test)]
mod tests {
    //! The intent of `tests/test_enrich.py` and the enrich parts of
    //! `tests/test_integration_sources.py` (fixtures from the Python `tests/fixtures`).
    use super::*;
    use serde_json::json;

    const SKATER_SUMMARY: &str = include_str!("../../../../tests/fixtures/nhl/skater_summary.json");
    const SKATER_REALTIME: &str = include_str!("../../../../tests/fixtures/nhl/skater_realtime.json");
    const SKATER_FACEOFFS: &str = include_str!("../../../../tests/fixtures/nhl/skater_faceoffwins.json");
    const GOALIE_SUMMARY: &str = include_str!("../../../../tests/fixtures/nhl/goalie_summary.json");
    const INJURIES: &str = include_str!("../../../../tests/fixtures/injuries/espn_injuries.json");
    const SCHED_EDM: &str = include_str!("../../../../tests/fixtures/nhl/club_schedule_EDM.json");
    const SCHED_CGY: &str = include_str!("../../../../tests/fixtures/nhl/club_schedule_CGY.json");
    const ROSTER_EDM: &str = include_str!("../../../../tests/fixtures/nhl/roster_EDM.json");
    const WEEK: &str = include_str!("../../../../tests/fixtures/nhl/schedule_2026-10-07.json");
    const GAME_LOG: &str = include_str!("../../../../tests/fixtures/nhl/game_log_8478402.json");
    const GAME_LOG_G: &str = include_str!("../../../../tests/fixtures/nhl/game_log_goalie_8476883.json");

    fn j(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    type Urls = Arc<Mutex<Vec<String>>>;

    /// `FixtureFetch`: NHL + injuries fixtures; current-season (20262027) reports are empty.
    fn fixture_fetch(urls: Urls, landing: bool) -> Box<FetchJson<'static>> {
        Box::new(move |url: &str, params: &Params, _ttl: f64| {
            urls.lock().unwrap().push(url.to_string());
            if landing && url.ends_with("/player/8476883/landing") {
                return Ok(json!({"playerId": 8476883, "birthDate": "1994-07-28",
                    "draftDetails": {"year": 2012, "round": 1, "overallPick": 19},
                    "careerTotals": {"regularSeason": {"gamesPlayed": 600}}}));
            }
            if url.contains("api.nhle.com/stats") {
                if param_str(params, "cayenneExp").unwrap_or("").contains("seasonId=20262027") {
                    return Ok(json!({"data": [], "total": 0}));
                }
                let mut parts = url.rsplit('/');
                let report = parts.next().unwrap();
                let kind = parts.next().unwrap();
                return Ok(j(match (kind, report) {
                    ("skater", "summary") => SKATER_SUMMARY,
                    ("goalie", "summary") => GOALIE_SUMMARY,
                    (_, "realtime") => SKATER_REALTIME,
                    (_, "faceoffwins") => SKATER_FACEOFFS,
                    _ => return Err(anyhow::anyhow!("KeyError: {report}")),
                }));
            }
            if url.contains("injuries") {
                return Ok(j(INJURIES));
            }
            if url.ends_with("/club-schedule-season/EDM/20262027") {
                return Ok(j(SCHED_EDM));
            }
            if url.ends_with("/club-schedule-season/CGY/20262027") {
                return Ok(j(SCHED_CGY));
            }
            if url.ends_with("/roster/EDM/20262027") {
                return Ok(j(ROSTER_EDM));
            }
            if url.contains("/schedule/") {
                return Ok(j(WEEK));
            }
            if url.contains("/game-log/") {
                return Ok(j(GAME_LOG));
            }
            Ok(json!({}))
        })
    }

    fn pl(cid: &str, name: &str, team: &str, pos: &[&str], status: &str) -> Value {
        json!({"cid": cid, "name": name, "name_norm": name.to_lowercase(), "ids": {"espn": cid.split(':').nth(1).unwrap()},
               "team": team, "positions": pos, "status": status})
    }

    fn make_ctx(as_of: &str) -> LeagueContext {
        let mut mcd = pl("espn:1", "Connor McDavid", "EDM", &["C", "F"], "healthy");
        mcd["lines"] = json!({"projected": {"split": "projected", "gp": 80, "stats": {"G": 40.0, "GP": 80.0}}});
        let mine = vec![
            mcd,
            pl("espn:2", "Leon Draisaitl", "EDM", &["C", "F"], "healthy"),
            pl("espn:3", "Troy Terry", "ANA", &["RW", "F"], "healthy"),
            pl("espn:4", "Charlie McAvoy", "BOS", &["D"], "dtd"),
            pl("espn:5", "Matthew Poitras", "BOS", &["C", "F"], "out"),
            pl("espn:6", "A.J. Greer", "ANA", &["LW", "F"], "ir"),
            pl("espn:7", "Andrei Vasilevskiy", "TBL", &["G"], "healthy"),
        ];
        let fas = vec![
            pl("espn:8", "Seamus Casey", "BOS", &["D"], "healthy"),
            pl("espn:9", "Nikita Kucherov", "TBL", &["RW", "F"], "healthy"),
        ];
        let slots: Vec<Value> = mine.into_iter().map(|p| json!({"slot": "BN", "player": p, "starting": false})).collect();
        serde_json::from_value(json!({
            "provider": "test", "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {"G": 1.0}}, "roster_shape": {"C": 2, "BN": 5},
            "teams": [{"team_id": "1", "name": "me", "owner_is_me": true, "slots": slots}],
            "free_agents": fas, "matchup_period": 1, "as_of": as_of,
        }))
        .unwrap()
    }

    fn opts<'a>(nhl: NhlClient, inj: Box<FetchJson<'static>>, xw: &'a Crosswalk) -> EnrichOptions<'a> {
        EnrichOptions {
            nhl: Some(nhl),
            injuries_fetch: Some(inj),
            crosswalk: Some(xw),
            teams: vec!["EDM".into(), "CGY".into()],
            clock: Clock::at(Utc::now()),
            ..Default::default()
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 28).unwrap()
    }

    fn run_enrich(ctx: &mut LeagueContext, deep: bool, landing: bool, pedigree_limit: usize) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let xw = Crosswalk::new(dir.path()).unwrap();
        let urls: Urls = Arc::new(Mutex::new(vec![]));
        let client = NhlClient::new(fixture_fetch(urls.clone(), landing), Some(20262027), today());
        let settings = Settings { fm_data_dir: dir.path().to_path_buf(), ..Settings::default() };
        let mut o = opts(client, fixture_fetch(Arc::new(Mutex::new(vec![])), false), &xw);
        o.pedigree_limit = pedigree_limit;
        enrich_context(ctx, &settings, None, deep, o);
        let out = urls.lock().unwrap().clone();
        out
    }

    fn by_name<'a>(ctx: &'a LeagueContext, name: &str) -> &'a Player {
        ctx.all_players().into_iter().find(|p| p.name == name).unwrap()
    }

    #[test]
    fn merge_status_severity() {
        for (p, f, want) in [
            ("healthy", "out", "out"),
            ("out", "dtd", "out"),
            ("out", "ir", "ir"),
            ("ir", "ltir", "ltir"),
            ("ltir", "dtd", "ltir"),
            ("unknown", "dtd", "dtd"),
            ("healthy", "unknown", "healthy"),
            ("healthy", "suspended", "suspended"),
            ("dtd", "suspended", "suspended"),
            ("suspended", "healthy", "suspended"),
            ("ir", "suspended", "ir"),
            ("suspended", "out", "out"),
        ] {
            assert_eq!(merge_status(p, f), want, "{p} + {f}");
        }
    }

    #[test]
    fn enrich_ids_birthdates_and_stats() {
        let mut ctx = make_ctx("2026-09-28");
        run_enrich(&mut ctx, false, false, PEDIGREE_LIMIT);
        let mcd = by_name(&ctx, "Connor McDavid");
        assert_eq!(mcd.nhl_id(), Some(8478402));
        let row = &j(SKATER_SUMMARY)["data"][0];
        let prior = &mcd.lines["prior"];
        assert_eq!(prior.gp, row["gamesPlayed"].as_i64().unwrap());
        assert_eq!(prior.stats["PTS"], row["points"].as_f64().unwrap());
        assert_eq!(prior.stats["PPA"], row["ppPoints"].as_f64().unwrap() - row["ppGoals"].as_f64().unwrap());
        assert!(!prior.stats.contains_key("OTG"));
        assert!(!mcd.lines.contains_key("season"));
        assert!(mcd.lines.contains_key("projected"));
        assert_eq!(by_name(&ctx, "Leon Draisaitl").birth_date, NaiveDate::from_ymd_opt(1995, 10, 27));
        let vas = by_name(&ctx, "Andrei Vasilevskiy");
        assert_eq!(vas.nhl_id(), Some(8476883));
        assert!(vas.lines["prior"].stats["GS"] > 0.0);
        assert!((vas.lines["prior"].stats["SVPCT"] - 0.91233).abs() < 1e-6);
        assert!(by_name(&ctx, "Nikita Kucherov").lines["prior"].stats.contains_key("HIT"));
        assert!(ctx.source_notes.iter().any(|n| n.starts_with("NHL ids:")));
    }

    #[test]
    fn enrich_injury_merge() {
        let mut ctx = make_ctx("2026-09-28");
        run_enrich(&mut ctx, false, false, PEDIGREE_LIMIT);
        let terry = by_name(&ctx, "Troy Terry");
        assert_eq!(terry.status, Status::Out);
        assert!(terry.status_note.as_deref().unwrap().contains("hip surgery"));
        assert_eq!(by_name(&ctx, "Charlie McAvoy").status, Status::Suspended);
        assert_eq!(by_name(&ctx, "Matthew Poitras").status, Status::Ir);
        let greer = by_name(&ctx, "A.J. Greer");
        assert_eq!(greer.status, Status::Ir);
        assert!(greer.status_note.as_deref().unwrap().contains("Upper Body"));
        assert_eq!(by_name(&ctx, "Seamus Casey").status, Status::Healthy);
        assert_eq!(by_name(&ctx, "Connor McDavid").status, Status::Healthy);
    }

    #[test]
    fn enrich_injury_matches_espn_id() {
        let mut ctx = make_ctx("2026-09-28");
        {
            let p = &mut ctx.free_agents[1];
            p.ids.insert("espn".into(), "3942905".into());
            p.cid = "espn:3942905".into();
        }
        run_enrich(&mut ctx, false, false, PEDIGREE_LIMIT);
        assert_eq!(ctx.free_agents[1].status, Status::Out);
    }

    #[test]
    fn enrich_schedule() {
        let mut ctx = make_ctx("2026-09-28");
        run_enrich(&mut ctx, false, false, PEDIGREE_LIMIT);
        let d = |m, dd| NaiveDate::from_ymd_opt(2026, m, dd).unwrap();
        assert_eq!(ctx.season_start, Some(d(9, 29)));
        let mut teams: Vec<&String> = ctx.schedule.keys().collect();
        teams.sort();
        assert_eq!(teams, vec!["CGY", "EDM"]);
        assert_eq!(ctx.schedule["EDM"][0], d(9, 29));
        assert!(!ctx.games_per_day.is_empty() && ctx.games_per_day.values().all(|n| *n >= 1));
        assert_eq!(ctx.opponents["EDM"][&d(9, 29)], "VAN");
        let a: HashSet<&NaiveDate> = ctx.opponents["EDM"].keys().collect();
        let b: HashSet<&NaiveDate> = ctx.schedule["EDM"].iter().collect();
        assert_eq!(a, b);
        assert!(ctx.opponents["CGY"].values().all(|o| o.starts_with('@') || o.len() == 3));
    }

    fn broken() -> Box<FetchJson<'static>> {
        Box::new(|_: &str, _: &Params, _: f64| Err(anyhow::anyhow!("no network")))
    }

    #[test]
    fn enrich_degrades_gracefully() {
        let mut ctx = make_ctx("2026-09-28");
        let dir = tempfile::tempdir().unwrap();
        let xw = Crosswalk::new(dir.path()).unwrap();
        let settings = Settings { fm_data_dir: dir.path().to_path_buf(), ..Settings::default() };
        let mut o = opts(NhlClient::new(broken(), Some(20262027), today()), broken(), &xw);
        o.teams = vec!["EDM".into()];
        enrich_context(&mut ctx, &settings, None, false, o);
        assert!(ctx.warnings.iter().any(|w| w.contains("Injury feed")));
        assert!(ctx.warnings.iter().any(|w| w.contains("NHL rosters")));
        assert!(ctx.schedule.is_empty() && ctx.season_start.is_none());
        assert_eq!(ctx.all_players()[0].nhl_id(), None);
        // the failed steps show up in ctx.sources (provider first)
        assert_eq!(ctx.sources[0].name, "Test");
        assert!(ctx.sources.iter().any(|s| s.name == "Injuries" && s.severity == SourceSeverity::Fail));
        assert!(ctx.sources.iter().any(|s| s.name == "NHL rosters" && s.severity == SourceSeverity::Fail));
        // test_enrich_network_steps_need_a_cache
        let notes = ctx.source_notes.join(" ");
        assert!(notes.contains("Daily Faceoff lines skipped (no HTTP cache)"));
        assert!(notes.contains("MoneyPuck expected goals skipped (no HTTP cache)"));
        assert!(notes.contains("no harness ledger") && !dir.path().join("harness.db").exists());
    }

    #[test]
    fn enrich_new_steps_off() {
        let mut ctx = make_ctx("2026-09-28");
        let dir = tempfile::tempdir().unwrap();
        let xw = Crosswalk::new(dir.path()).unwrap();
        let settings = Settings { fm_data_dir: dir.path().to_path_buf(), ..Settings::default() };
        let mut o = opts(NhlClient::new(broken(), Some(20262027), today()), broken(), &xw);
        o.deployment = false;
        o.lines = false;
        o.xg = false;
        enrich_context(&mut ctx, &settings, None, false, o);
        let notes = ctx.source_notes.join(" ");
        assert!(!notes.contains("Deployment") && !notes.contains("Daily Faceoff") && !notes.contains("MoneyPuck"));
        assert!(!dir.path().join("harness.db").exists());
    }

    #[test]
    fn deep_skipped_before_season() {
        let mut ctx = make_ctx("2026-09-28");
        let urls = run_enrich(&mut ctx, true, false, PEDIGREE_LIMIT);
        assert!(!urls.iter().any(|u| u.contains("/game-log/")));
        assert!(ctx.source_notes.iter().any(|n| n.contains("Game logs skipped")));
    }

    #[test]
    fn deep_fetches_game_logs_in_season() {
        let mut ctx = make_ctx("2026-10-20");
        let urls = run_enrich(&mut ctx, true, false, PEDIGREE_LIMIT);
        assert!(urls.iter().any(|u| u.contains("/game-log/")));
        assert!(ctx.all_players().iter().all(|p| !p.lines.contains_key("last7")));
        assert!(ctx.source_notes.iter().any(|n| n.starts_with("Game logs:")));
    }

    fn log_client(body: &'static str) -> NhlClient {
        NhlClient::new(Box::new(move |_: &str, _: &Params, _: f64| Ok(j(body))), Some(20252026), today())
    }

    #[test]
    fn recent_lines_windows() {
        let log = log_client(GAME_LOG).game_log(8478402, Some(20252026), 2).unwrap();
        let lines = recent_lines(&log, NaiveDate::from_ymd_opt(2026, 4, 16).unwrap());
        assert_eq!((lines["last7"].gp, lines["last15"].gp, lines["last30"].gp), (3, 4, 4));
        let a: f64 = log.iter().map(|e| e.stats["A"]).sum();
        assert_eq!(lines["last15"].stats["A"], a);
        assert_eq!(lines["last7"].split, Split::Last7);
        assert!(recent_lines(&log, NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()).is_empty());
    }

    #[test]
    fn recent_lines_goalie_rates() {
        let log = log_client(GAME_LOG_G).game_log(8476883, None, 2).unwrap();
        let last = log.iter().map(|e| e.date).max().unwrap();
        let line = &recent_lines(&log, last)["last30"];
        let (sa, ga) = (line.stats["SA"], line.stats["GA"]);
        assert!((line.stats["SVPCT"] - (sa - ga) / sa).abs() < 1e-12);
        assert!((line.per_game()["SVPCT"] - (sa - ga) / sa).abs() < 1e-12);
    }

    #[test]
    fn statline_from_nhl_requires_games() {
        let stats: Map<String, f64> = [("GP".to_string(), 0.0), ("G".to_string(), 0.0)].into_iter().collect();
        assert!(statline_from_nhl(&stats, Split::Season).is_none());
    }

    #[test]
    fn ttl_routing_and_tracker() {
        let t = today();
        let e: Params = vec![];
        assert_eq!(ttl_for("https://api.nhle.com/stats/rest/en/skater/summary", &e, t).1, TTL_NHL_STATS);
        assert_eq!(ttl_for("https://api-web.nhle.com/v1/player/1/game-log/20262027/2", &e, t).1, TTL_GAME_LOG);
        assert_eq!(ttl_for("https://api-web.nhle.com/v1/club-schedule-season/EDM/20262027", &e, t).1, TTL_SCHEDULE);
        assert_eq!(ttl_for("https://api-web.nhle.com/v1/schedule/2026-09-28", &e, t).1, TTL_SCHEDULE);
        assert_eq!(ttl_for("https://api-web.nhle.com/v1/roster/EDM/20262027", &e, t).1, TTL_ROSTER);
        assert_eq!(ttl_for(INJURIES_URL, &e, t).1, TTL_INJURIES);
        assert_eq!(ttl_for("https://www.rotowire.com/rss/news.php?sport=NHL", &e, t).1, TTL_RSS);
        assert_eq!(ttl_for("https://api-web.nhle.com/v1/player/1/landing", &e, t), ("NHL player pages", TTL_LANDING));

        // the tracker counts requests per label (a tape-backed cache answers)
        let url = "https://api-web.nhle.com/v1/roster/EDM/20262027";
        let mut data = fm_http::tape::TapeData::default();
        data.http.insert(
            cache_key("GET", url, &e, &[]),
            fm_http::tape::TapeHttp {
                url: url.into(),
                params: Value::Null,
                key_headers: Default::default(),
                status: 200,
                final_url: url.into(),
                body: "{\"ok\": true}".into(),
            },
        );
        let cache = Arc::new(HttpCache::from_tape(fm_http::Tape(Arc::new(data))));
        let tr = SourceTracker::new(cache, t);
        assert_eq!(tr.fetch_json(url, &e).unwrap(), json!({"ok": true}));
        assert!(tr.fetch_json("https://api-web.nhle.com/v1/roster/CGY/20262027", &e).is_err());
        let s = &tr.sources()["NHL rosters"];
        assert_eq!((s.requests, s.failed, s.ttl), (1, 1, Some(TTL_ROSTER)));
        let st = &tr.statuses(1e10)[0];
        assert_eq!(st.severity, SourceSeverity::Warn);
        assert!(st.detail.as_deref().unwrap().starts_with("1 of 2 requests failed (CacheMiss: offline and not cached"));
        assert_eq!(age(7200.0), "2h ago");
        assert_eq!(age(60.0), "just now");
        assert_eq!(age(3.0 * 24.0 * HOUR), "3d ago");
        assert_eq!(age(600.0), "10m ago");
    }

    #[test]
    fn enrich_pedigree_from_player_pages() {
        let mut ctx = make_ctx("2026-09-28");
        ctx.teams[0].slots[0].player.as_mut().unwrap().birth_date = NaiveDate::from_ymd_opt(1997, 1, 13);
        let urls = run_enrich(&mut ctx, false, true, PEDIGREE_LIMIT);
        let vas = by_name(&ctx, "Andrei Vasilevskiy");
        assert_eq!((vas.draft_overall, vas.draft_round, vas.draft_year, vas.career_gp), (Some(19), Some(1), Some(2012), Some(600)));
        assert_eq!(vas.birth_date, NaiveDate::from_ymd_opt(1994, 7, 28));
        let mcd = by_name(&ctx, "Connor McDavid");
        assert!(mcd.draft_overall.is_none() && !urls.iter().any(|u| u.ends_with("/8478402/landing")));
        assert!(ctx.source_notes.iter().any(|n| n.starts_with("Pedigree:")));
    }

    #[test]
    fn pedigree_request_cap_and_targets() {
        let mut ctx = make_ctx("2026-09-28");
        let urls = run_enrich(&mut ctx, false, true, 0);
        assert!(!urls.iter().any(|u| u.ends_with("/landing")));
        assert!(ctx.source_notes.iter().any(|n| n.contains("deferred to the next run")));
        let mk = |v: Value| -> Player { serde_json::from_value(v).unwrap() };
        let mut young = mk(json!({"cid": "x:1", "name": "Kid", "name_norm": "kid", "ids": {"nhl": "1"}, "team": "EDM",
                                  "positions": ["C"], "birth_date": "2006-01-01"}));
        let vet = mk(json!({"cid": "x:2", "name": "Vet", "name_norm": "vet", "ids": {"nhl": "2"}, "team": "EDM",
                            "positions": ["C"], "birth_date": "1990-01-01",
                            "lines": {"prior": {"split": "prior", "gp": 70, "stats": {"GP": 70.0}}}}));
        assert!(needs_pedigree(&young, ctx.as_of) && !needs_pedigree(&vet, ctx.as_of));
        young.draft_overall = Some(3);
        assert!(!needs_pedigree(&young, ctx.as_of));
    }

    #[test]
    fn enrich_fills_prior2_prior3_with_one_pull_per_season() {
        let mut ctx = make_ctx("2026-09-28");
        let dir = tempfile::tempdir().unwrap();
        let xw = Crosswalk::new(dir.path()).unwrap();
        let seasons: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let inner = fixture_fetch(Arc::new(Mutex::new(vec![])), false);
        let s2 = seasons.clone();
        let counting: Box<FetchJson<'static>> = Box::new(move |url: &str, params: &Params, ttl: f64| {
            if url.contains("api.nhle.com/stats") {
                let cay = param_str(params, "cayenneExp").unwrap_or("");
                s2.lock().unwrap().push(cay.split("seasonId=").nth(1).unwrap()[..8].to_string());
            }
            inner(url, params, ttl)
        });
        let settings = Settings { fm_data_dir: dir.path().to_path_buf(), ..Settings::default() };
        let o = opts(NhlClient::new(counting, Some(20262027), today()), fixture_fetch(Arc::new(Mutex::new(vec![])), false), &xw);
        enrich_context(&mut ctx, &settings, None, false, o);
        let mcd = by_name(&ctx, "Connor McDavid");
        assert!(["prior", "prior2", "prior3"].iter().all(|s| mcd.lines.contains_key(*s)));
        assert_eq!(mcd.lines["prior2"].split, Split::Prior2);
        assert_eq!(mcd.lines["prior3"].gp, mcd.lines["prior"].gp);
        let s = seasons.lock().unwrap();
        let count = |x: &str| s.iter().filter(|v| *v == x).count();
        let set: HashSet<&String> = s.iter().collect();
        assert_eq!(set.len(), 4);
        assert_eq!((count("20242025"), count("20232024")), (4, 4));
        assert!(ctx.source_notes.iter().any(|n| n.contains("prior2") && n.contains("prior3")));
    }

    #[test]
    fn history_seasons_cached_for_30_days() {
        let url = "https://api.nhle.com/stats/rest/en/skater/summary";
        let p = |s: &str| -> Params { vec![("cayenneExp".into(), ParamValue::Str(format!("seasonId={s} and gameTypeId=2")))] };
        let t = today();
        assert_eq!(ttl_for(url, &p("20232024"), t), ("NHL stats (history)", TTL_NHL_HISTORY));
        assert_eq!(ttl_for(url, &p("20242025"), t).1, 30.0 * 24.0 * 3600.0);
        assert_eq!(ttl_for(url, &p("20252026"), t), ("NHL stats", TTL_NHL_STATS));
        assert_eq!(ttl_for(url, &vec![], t).1, TTL_NHL_STATS);
    }

    #[test]
    fn fill_stat_lines_prior2_prior3_only_when_missing() {
        let mut p: Player = serde_json::from_value(json!({"cid": "t:7", "name": "X", "name_norm": "x", "ids": {"nhl": "7"},
            "team": null, "positions": ["C"], "lines": {"prior2": {"split": "prior2", "gp": 3, "stats": {"GP": 3.0}}}}))
        .unwrap();
        let stats: Map<String, f64> = [("GP", 50.0), ("G", 10.0), ("PTS", 20.0)].iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let rows: NhlRows = [(7i64, &stats)].into_iter().collect();
        let empty: NhlRows = HashMap::new();
        let filled = fill_stat_lines(&mut [&mut p], &empty, &empty, Some(&rows), Some(&rows));
        assert_eq!(filled.values().copied().collect::<Vec<_>>(), vec![0, 0, 0, 1]);
        assert_eq!((p.lines["prior2"].gp, p.lines["prior3"].gp, p.lines["prior3"].split), (3, 50, Split::Prior3));
    }

    #[test]
    fn provider_warnings_classify() {
        let w = |s: &str| s.to_string();
        let st = provider_status("fantrax", &[w("Fantrax: not logged in (cookie expired)")], 0.0);
        assert_eq!((st.name.as_str(), st.severity), ("Fantrax", SourceSeverity::Fail));
        assert!(st.detail.unwrap().starts_with("login expired"));
        let st = provider_status("espn", &[w("ESPN activity feed unavailable (timeout)")], 0.0);
        assert_eq!(st.severity, SourceSeverity::Warn);
        let st = provider_status("espn", &[w("FANTRAX_POINTS set")], 0.0);
        assert_eq!((st.severity, st.detail.as_deref()), (SourceSeverity::Ok, Some("league data loaded")));
        assert_eq!(provider_label("yahoo"), "Yahoo");
        assert_eq!(step_source("NHL skater stats 20232024", Some(20262027), Some(20252026)), "NHL stats (past seasons)");
        assert_eq!(step_source("NHL goalie stats 20252026", Some(20262027), Some(20252026)), "NHL stats");
        assert_eq!(clip("a  b   c", 120), "a b c");
        assert_eq!(clip(&"x".repeat(130), 120), format!("{}...", "x".repeat(117)));
    }

    #[test]
    fn goalie_ledger_actual_starts_and_back_to_backs() {
        // test_enrich_new_steps_on_with_fakes, ledger part: goalie starts from a harness ledger
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::open(dir.path().join("led")).unwrap();
        let mut rows: Vec<fm_harness::ledger::Row> = vec![];
        for i in 0..6 {
            let d = (NaiveDate::from_ymd_opt(2026, 10, 8).unwrap() + Duration::days(2 * i)).format("%Y-%m-%d").to_string();
            let me = i < 4;
            for (id, started) in [(8479973, me), (8475717, !me)] {
                let v = json!({"nhl_id": id, "game_date": d, "team": "EDM", "started": started as i64, "back_to_back": 0});
                rows.push(v.as_object().unwrap().clone());
            }
        }
        led.upsert("goalie_starts", &rows, &["nhl_id", "game_date"]).unwrap();
        let mk = |cid: &str, name: &str, pos: &str, nhl: i64| {
            json!({"cid": cid, "name": name, "name_norm": name.to_lowercase(), "ids": {"nhl": nhl.to_string()}, "team": "EDM", "positions": [pos]})
        };
        let slots: Vec<Value> =
            [mk("c1", "Connor McDavid", "C", 8478402), mk("g1", "Stuart Skinner", "G", 8479973), mk("g2", "Calvin Pickard", "G", 8475717)]
                .into_iter()
                .map(|p| json!({"slot": "BN", "player": p, "starting": false}))
                .collect();
        let mut ctx: LeagueContext = serde_json::from_value(json!({
            "provider": "test", "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {"G": 3.0}}, "roster_shape": {"C": 2},
            "teams": [{"team_id": "1", "name": "me", "owner_is_me": true, "slots": slots}],
            "free_agents": [], "matchup_period": 1, "as_of": "2026-10-20",
        }))
        .unwrap();
        let dead = NhlClient::new(broken(), Some(20262027), today());
        let settings = Settings { fm_data_dir: dir.path().to_path_buf(), ..Settings::default() };
        let info = deployment_step(&mut ctx, &settings, Some(&dead), Some(&led)).unwrap();
        assert!(info.ledger);
        assert_eq!(ctx.goalie_actual_starts.get("g1"), Some(&(4, 6)));
        assert_eq!(ctx.goalie_actual_starts.get("g2"), Some(&(2, 6)));
        assert_eq!(ctx.b2b_second_night.get("EDM"), Some(&(0.35, 0)));
        assert!(ctx.warnings.iter().any(|w| w.contains("Deployment (current season report) unavailable")));
        assert!(ctx.source_notes.iter().any(|n| n.starts_with("Deployment (NHL TOI / PP reports, ledger)")));
    }

    #[test]
    fn utc_rounds_microseconds() {
        let t = utc(Some(1_793_000_000.123_456_7)).unwrap();
        assert!(t.to_pydantic().ends_with(".123457Z"));
        assert_eq!(parse_local(Some("2026-10-27T05:53:59Z")), Some(1_793_080_439.0));
    }
}

