//! Rust port of `fantasy_manager/report/health.py`: data health, i.e. which sources failed or went
//! stale this run, in plain English. Owner: work package `W8-outlook-playoffs-calib`.
//!
//! Reads `LeagueContext.sources` (`providers.enrich.collect_sources`); a context without them
//! falls back to classifying `ctx.warnings`.
//!
//! * failed: severity "fail"
//! * stale: older than 3x its cache TTL (36 h for sources without a TTL), or flagged stale by its
//!   producer
//! * warn: partial failures (shown, no alert)
//!
//! Deviations: `now` is an `Option<&PyDateTime>` like Python's `now=None`; Python's identity
//! sets (`id(s)`) are index sets here; the fallback classifier passes the clock to
//! `provider_status` as an epoch (only used for an age that stays None).

use std::collections::HashSet;

use chrono::{Local, TimeZone, Utc};
use fm_core::models::{LeagueContext, SourceSeverity, SourceStatus};
use fm_core::PyDateTime;
use fm_providers::enrich::{provider_status, step_source, VALUATION_SOURCES};
use fm_pycompat::fmt::fixed;
use fm_pycompat::order::{cmp_f64, first_max_by};
use fm_pycompat::text::py_slice;
use fm_sources::nhl::{current_season, prior_season};
use regex::Regex;

pub const HOUR: f64 = 3600.0;
pub const TTL_FACTOR: f64 = 3.0;
pub const NO_TTL_STALE: f64 = 36.0 * HOUR;
pub const WARN_SIGN: &str = "\u{26a0}";

/// `STEP_SOURCES` keys (enrichment step labels that name a source).
const STEP_SOURCE_KEYS: [&str; 7] = [
    "Injury feed",
    "NHL player pages",
    "Rookie evidence",
    "NHL preseason box scores",
    "Deployment",
    "Daily Faceoff lines",
    "MoneyPuck expected goals",
];

// --------------------------------------------------------------------------- formatting

/// "just now", "14 min ago", "5h ago", "yesterday", "3 days ago".
pub fn ago(seconds: Option<f64>) -> String {
    let Some(s) = seconds else { return "at an unknown time".to_string() };
    if s < 90.0 {
        return "just now".to_string();
    }
    if s < 90.0 * 60.0 {
        return format!("{} min ago", fixed(s / 60.0, 0, false));
    }
    if s < 36.0 * HOUR {
        return format!("{}h ago", fixed(s / HOUR, 0, false));
    }
    let days = s / (24.0 * HOUR);
    if days < 2.0 {
        "yesterday".to_string()
    } else {
        format!("{} days ago", fixed(days, 0, false))
    }
}

/// Compact age for one-line footers: "12m", "19h", "3d".
pub fn short_age(seconds: Option<f64>) -> String {
    let Some(s) = seconds else { return "?".to_string() };
    if s < 90.0 * 60.0 {
        return format!("{}m", fixed(s.max(0.0) / 60.0, 0, false));
    }
    if s < 48.0 * HOUR {
        return format!("{}h", fixed(s / HOUR, 0, false));
    }
    format!("{}d", fixed(s / (24.0 * HOUR), 0, false))
}

fn span(seconds: f64) -> String {
    if seconds < 2.0 * HOUR {
        format!("{} min", fixed(seconds / 60.0, 0, false))
    } else if seconds < 48.0 * HOUR {
        format!("{}h", fixed(seconds / HOUR, 0, false))
    } else {
        format!("{} days", fixed(seconds / (24.0 * HOUR), 0, false))
    }
}

/// Details without full URLs (host only) and collapsed whitespace.
fn clean(text: Option<&str>) -> String {
    let re = Regex::new(r"https?://([^/\s)]+)[^\s)]*").expect("static regex");
    let t = re.replace_all(text.unwrap_or(""), "$1");
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

// --------------------------------------------------------------------------- classification

/// Age in seconds: from `fetched_at` when `now` is given, else the enrichment-time age.
pub fn age_of(s: &SourceStatus, now: Option<&PyDateTime>) -> Option<f64> {
    if let (Some(now), Some(f)) = (now, s.fetched_at.as_ref()) {
        let n = match now.to_utc() {
            Some(u) => u,
            None => Local.from_local_datetime(&now.local).earliest()?.with_timezone(&Utc),
        };
        let fu = match f.to_utc() {
            Some(u) => u,
            None => Utc.from_utc_datetime(&f.local),
        };
        let us = (n - fu).num_microseconds().map_or_else(|| (n - fu).num_seconds() as f64, |u| u as f64 / 1e6);
        return Some(us.max(0.0));
    }
    s.age_seconds
}

/// Failed sources are never "stale" (they are failed). `stale` set by the producer wins.
pub fn is_stale(s: &SourceStatus, now: Option<&PyDateTime>) -> bool {
    if s.severity == SourceSeverity::Fail {
        return false;
    }
    if let Some(st) = s.stale {
        return st;
    }
    let Some(age) = age_of(s, now) else { return false };
    let limit = match s.ttl_seconds {
        Some(t) if t != 0.0 => TTL_FACTOR * t,
        _ => NO_TTL_STALE,
    };
    age > limit
}

fn is_login(s: &SourceStatus) -> bool {
    s.detail.as_deref().unwrap_or("").contains("login expired")
}

pub fn problem_line(s: &SourceStatus, stale: bool, now: Option<&PyDateTime>) -> String {
    let age = age_of(s, now);
    let detail = clean(s.detail.as_deref());
    if s.severity == SourceSeverity::Fail {
        let mut text = format!("{}: {}", s.name, if detail.is_empty() { "unavailable" } else { &detail });
        if age.is_some() && !is_login(s) {
            text.push_str(&format!("; last good data {}", ago(age)));
        }
        return text;
    }
    if stale {
        if s.stale == Some(true) {
            // producer's verdict: its detail says why
            return if detail.is_empty() { format!("{}: stale", s.name) } else { format!("{}: behind \u{2014} {}", s.name, detail) };
        }
        let every = match s.ttl_seconds {
            Some(t) if t != 0.0 => format!(", normally refreshed every {}", span(t)),
            _ => String::new(),
        };
        return format!("{}: stale \u{2014} fetched {}{}", s.name, ago(age), every);
    }
    format!("{}: {}", s.name, if detail.is_empty() { "partial data" } else { &detail })
}

pub fn ok_line(s: &SourceStatus, now: Option<&PyDateTime>) -> String {
    let age = age_of(s, now);
    if age.is_some() {
        return format!("{}: fetched {}", s.name, ago(age));
    }
    let d = clean(s.detail.as_deref());
    format!("{}: {}", s.name, if d.is_empty() { "ok" } else { &d })
}

/// Two-to-four word problem label for the phone headline.
fn phrase(s: &SourceStatus, stale: bool, now: Option<&PyDateTime>) -> String {
    if s.severity == SourceSeverity::Fail {
        return if is_login(s) { format!("{} login expired", s.name) } else { format!("{} unavailable", s.name) };
    }
    if stale {
        let age = age_of(s, now);
        return if s.stale == Some(true) { format!("{} behind", s.name) } else { format!("{} stale ({})", s.name, short_age(age)) };
    }
    format!("{} partial", s.name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overall {
    Ok,
    Degraded,
    Failed,
}

impl Overall {
    pub fn as_str(&self) -> &'static str {
        match self {
            Overall::Ok => "ok",
            Overall::Degraded => "degraded",
            Overall::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DataHealth {
    pub overall: Overall,
    pub failed: Vec<SourceStatus>,
    pub stale: Vec<SourceStatus>,
    pub ok_count: usize,
    /// problems first (failed, stale, partial), then fresh sources
    pub lines: Vec<String>,
    /// partial failures (not stale)
    pub warned: Vec<SourceStatus>,
    pub sources: Vec<SourceStatus>,
    /// "⚠ Data problems: Fantrax login expired; ..." (None when ok)
    pub headline: Option<String>,
    /// "All 11 sources fresh (oldest: NHL rosters, 19h)" when ok
    pub footer: Option<String>,
    pub problem_lines: Vec<String>,
    fresh_idx: Vec<usize>,
}

impl DataHealth {
    pub fn total(&self) -> usize {
        self.sources.len()
    }

    pub fn fresh(&self) -> Vec<&SourceStatus> {
        self.fresh_idx.iter().map(|&i| &self.sources[i]).collect()
    }
}

/// Non-ok SourceStatus rows recoverable from `ctx.warnings` alone (the fallback when enrichment
/// never listed its sources): provider login / fetch failures, a crashed enrichment and failed
/// enrichment steps.
pub fn warning_sources(ctx: &LeagueContext) -> Vec<SourceStatus> {
    let season = current_season(ctx.as_of);
    let prior = prior_season(season);
    let step_re = Regex::new(r"^(?P<name>.+?) unavailable \((?P<err>.*)\)$").expect("static regex");
    let mut out: Vec<SourceStatus> = Vec::new();
    let prov = provider_status(&ctx.provider, &ctx.warnings, 0.0);
    if prov.severity != SourceSeverity::Ok {
        out.push(prov);
    }
    for w in &ctx.warnings {
        if w.starts_with("NHL enrichment failed") {
            let tail = w.split_once(':').map_or(w.as_str(), |x| x.1).trim();
            out.push(fail_source("NHL data", true, format!("enrichment failed ({})", py_slice(tail, Some(0), Some(120)))));
            continue;
        }
        let Some(m) = step_re.captures(w) else { continue };
        let label = &m["name"];
        if STEP_SOURCE_KEYS.contains(&label) || step_source(label, None, None) != label {
            let name = step_source(label, Some(season), Some(prior));
            let feeds = VALUATION_SOURCES.contains(&name.as_str());
            out.push(fail_source(&name, feeds, format!("unavailable ({})", py_slice(&m["err"], Some(0), Some(120)))));
        }
    }
    out
}

fn fail_source(name: &str, feeds_valuation: bool, detail: String) -> SourceStatus {
    SourceStatus {
        name: name.to_string(),
        ok: false,
        severity: SourceSeverity::Fail,
        fetched_at: None,
        age_seconds: None,
        ttl_seconds: None,
        detail: Some(detail),
        requests: None,
        feeds_valuation,
        stale: None,
    }
}

/// Classify `ctx.sources` (or, when enrichment never listed them, the non-ok rows recoverable
/// from `ctx.warnings`) into failed / stale / partial / fresh, with plain-English lines. `now`
/// recomputes ages from `fetched_at`; by default the ages measured at enrichment are used.
pub fn data_health(ctx: &LeagueContext, now: Option<&PyDateTime>) -> DataHealth {
    let mut sources: Vec<SourceStatus> = ctx.sources.clone();
    let mut names: HashSet<String> = sources.iter().map(|s| s.name.clone()).collect();
    // enrichment lists every source it touched; without that list, recover what the warnings say
    let extra = if sources.is_empty() { warning_sources(ctx) } else { vec![] };
    for s in extra {
        if names.insert(s.name.clone()) {
            sources.push(s);
        }
    }
    let failed_idx: Vec<usize> = (0..sources.len()).filter(|&i| sources[i].severity == SourceSeverity::Fail).collect();
    let stale_idx: Vec<usize> = (0..sources.len()).filter(|&i| is_stale(&sources[i], now)).collect();
    let warned_idx: Vec<usize> =
        (0..sources.len()).filter(|&i| sources[i].severity == SourceSeverity::Warn && !stale_idx.contains(&i)).collect();
    let bad: HashSet<usize> = failed_idx.iter().chain(&stale_idx).chain(&warned_idx).copied().collect();
    let fresh_idx: Vec<usize> = (0..sources.len()).filter(|i| !bad.contains(i)).collect();
    // most important first: valuation inputs before context-only sources
    let mut failed_idx = failed_idx;
    failed_idx.sort_by_key(|&i| !sources[i].feeds_valuation);
    let mut stale_idx = stale_idx;
    stale_idx.sort_by(|&a, &b| {
        (!sources[a].feeds_valuation)
            .cmp(&!sources[b].feeds_valuation)
            .then(cmp_f64(-age_of(&sources[a], now).unwrap_or(0.0), -age_of(&sources[b], now).unwrap_or(0.0)))
    });
    let pick = |idx: &[usize]| -> Vec<SourceStatus> { idx.iter().map(|&i| sources[i].clone()).collect() };
    let failed = pick(&failed_idx);
    let stale = pick(&stale_idx);
    let warned = pick(&warned_idx);
    let mut problems: Vec<String> = failed.iter().map(|s| problem_line(s, false, now)).collect();
    problems.extend(stale.iter().map(|s| problem_line(s, true, now)));
    problems.extend(warned.iter().map(|s| problem_line(s, false, now)));
    let mut lines = problems.clone();
    lines.extend(fresh_idx.iter().map(|&i| ok_line(&sources[i], now)));
    let overall = if !failed.is_empty() {
        Overall::Failed
    } else if !stale.is_empty() || !warned.is_empty() {
        Overall::Degraded
    } else {
        Overall::Ok
    };
    let (mut headline, mut footer) = (None, None);
    if overall != Overall::Ok {
        let mut phrases: Vec<String> = failed.iter().map(|s| phrase(s, false, now)).collect();
        phrases.extend(stale.iter().map(|s| phrase(s, true, now)));
        phrases.extend(warned.iter().map(|s| phrase(s, false, now)));
        headline = Some(format!("{WARN_SIGN} Data problems: {}", phrases.join("; ")));
    }
    if !fresh_idx.is_empty() {
        let aged: Vec<usize> = fresh_idx.iter().copied().filter(|&i| age_of(&sources[i], now).is_some()).collect();
        let oldest = first_max_by(aged, |&a, &b| cmp_f64(age_of(&sources[a], now).unwrap_or(0.0), age_of(&sources[b], now).unwrap_or(0.0)));
        let tail = oldest.map_or(String::new(), |i| format!(" (oldest: {}, {})", sources[i].name, short_age(age_of(&sources[i], now))));
        let noun = if fresh_idx.len() == 1 { "source" } else { "sources" };
        footer = Some(if overall == Overall::Ok {
            format!("All {} {noun} fresh{tail}", fresh_idx.len())
        } else {
            format!("{} other {noun} fresh{tail}", fresh_idx.len())
        });
    }
    DataHealth {
        overall,
        failed,
        stale,
        ok_count: fresh_idx.len(),
        lines,
        warned,
        sources,
        headline,
        footer,
        problem_lines: problems,
        fresh_idx,
    }
}

/// Alert-worthy: any failed source, or a stale source that feeds valuation.
pub fn should_alert(health: &DataHealth) -> bool {
    !health.failed.is_empty() || health.stale.iter().any(|s| s.feeds_valuation)
}

/// Compact web-footer view of `ctx.sources`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FooterSummary {
    /// ok | warn | fail
    pub level: &'static str,
    pub text: String,
}

/// e.g. "12 fresh · 1 stale · 1 failed — worst: Fantrax (login expired ...)". None without sources.
pub fn footer_summary(ctx: &LeagueContext) -> Option<FooterSummary> {
    if ctx.sources.is_empty() {
        return None;
    }
    let h = data_health(ctx, None);
    if h.sources.is_empty() {
        return None;
    }
    if h.overall == Overall::Ok {
        let text = h.footer.clone().unwrap_or_else(|| format!("All {} sources fresh", h.total()));
        let mut chars = text.chars();
        let lowered = chars.next().map_or(String::new(), |c| c.to_lowercase().collect::<String>() + chars.as_str());
        return Some(FooterSummary { level: "ok", text: lowered });
    }
    let mut counts = vec![format!("{} fresh", h.ok_count)];
    if !h.stale.is_empty() {
        counts.push(format!("{} stale", h.stale.len()));
    }
    if !h.warned.is_empty() {
        counts.push(format!("{} partial", h.warned.len()));
    }
    if !h.failed.is_empty() {
        counts.push(format!("{} failed", h.failed.len()));
    }
    let worst = h.failed.first().or(h.stale.first()).or(h.warned.first())?;
    let stale = h.stale.iter().any(|s| std::ptr::eq(s, worst));
    let line = problem_line(worst, stale, None);
    let why = line.split_once(": ").map_or("", |x| x.1).to_string();
    let level = if !h.failed.is_empty() || should_alert(&h) { "fail" } else { "warn" };
    let tail = if why.is_empty() { String::new() } else { format!(" ({why})") };
    Some(FooterSummary { level, text: format!("{} \u{2014} worst: {}{}", counts.join(" \u{b7} "), worst.name, tail) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn src(name: &str, sev: SourceSeverity, age: Option<f64>, ttl: Option<f64>, detail: Option<&str>, valuation: bool, stale: Option<bool>) -> SourceStatus {
        SourceStatus {
            name: name.to_string(),
            ok: sev != SourceSeverity::Fail,
            severity: sev,
            fetched_at: None,
            age_seconds: age,
            ttl_seconds: ttl,
            detail: detail.map(str::to_string),
            requests: None,
            feeds_valuation: valuation,
            stale,
        }
    }

    fn ok(name: &str, age: Option<f64>, ttl: Option<f64>, valuation: bool) -> SourceStatus {
        src(name, SourceSeverity::Ok, age, ttl, None, valuation, None)
    }

    fn league(sources: Vec<SourceStatus>) -> LeagueContext {
        let v = json!({"provider": "espn", "league_id": "1", "season": 2027, "name": "L",
                       "scoring": {"kind": "points", "weights": {}}, "roster_shape": {}, "teams": [],
                       "free_agents": [], "matchup_period": null, "as_of": "2026-11-04"});
        let mut ctx: LeagueContext = serde_json::from_value(v).unwrap();
        ctx.sources = sources;
        ctx
    }

    #[test]
    fn all_fresh() {
        let mut espn = ok("ESPN", None, None, true);
        espn.detail = Some("league data loaded".into());
        let ctx = league(vec![
            espn,
            ok("NHL rosters", Some(19.0 * HOUR), Some(30.0 * 24.0 * HOUR), true),
            ok("Daily Faceoff goalies", Some(14.0 * 60.0), Some(3.0 * HOUR), true),
        ]);
        let h = data_health(&ctx, None);
        assert_eq!((h.overall, h.ok_count, h.failed.len(), h.stale.len()), (Overall::Ok, 3, 0, 0));
        assert!(h.headline.is_none());
        assert_eq!(h.footer.as_deref(), Some("All 3 sources fresh (oldest: NHL rosters, 19h)"));
        assert!(h.lines.contains(&"Daily Faceoff goalies: fetched 14 min ago".to_string()));
        assert!(h.lines.contains(&"ESPN: league data loaded".to_string()));
        assert!(!should_alert(&h));
    }

    #[test]
    fn stale_rules() {
        let inj = ok("Injuries", Some(5.0 * HOUR), Some(HOUR), true);
        let news = ok("News feeds", Some(1.4 * HOUR), Some(0.5 * HOUR), false);
        let ledger = src("Deployment ledger", SourceSeverity::Ok, Some(40.0 * HOUR), None, None, true, Some(false));
        let odds = ok("Odds archive", Some(40.0 * HOUR), None, false);
        let fresh = ok("NHL stats", Some(2.0 * HOUR), Some(6.0 * HOUR), true);
        let flags: Vec<bool> = [&inj, &news, &ledger, &odds, &fresh].iter().map(|s| is_stale(s, None)).collect();
        assert_eq!(flags, vec![true, false, false, true, false]);
        let h = data_health(&league(vec![fresh.clone(), news, odds]), None);
        assert_eq!(h.overall, Overall::Degraded);
        assert_eq!(h.stale.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["Odds archive"]);
        assert!(!should_alert(&h));
        assert_eq!(h.headline.as_deref(), Some("\u{26a0} Data problems: Odds archive stale (40h)"));
        assert_eq!(h.footer.as_deref(), Some("2 other sources fresh (oldest: NHL stats, 2h)"));
        let h = data_health(&league(vec![fresh, inj]), None);
        assert!(should_alert(&h));
        assert_eq!(h.lines[0], "Injuries: stale \u{2014} fetched 5h ago, normally refreshed every 60 min");
        let behind = src(
            "Deployment ledger",
            SourceSeverity::Ok,
            Some(3.0 * 24.0 * HOUR),
            None,
            Some("through 2026-10-02; 3 game day(s) not pulled since (run `fm harness daily`)"),
            true,
            Some(true),
        );
        let h = data_health(&league(vec![behind]), None);
        assert!(should_alert(&h));
        assert!(h.lines[0].starts_with("Deployment ledger: behind \u{2014} through 2026-10-02"));
    }

    #[test]
    fn failed_and_partial() {
        let ctx = league(vec![
            src("Fantrax", SourceSeverity::Fail, None, None, Some("login expired \u{2014} refresh FANTRAX_COOKIE"), true, None),
            src(
                "MoneyPuck",
                SourceSeverity::Fail,
                Some(40.0 * HOUR),
                Some(12.0 * HOUR),
                Some("unavailable (HttpError: HTTP 503 for https://moneypuck.com/x.csv)"),
                true,
                None,
            ),
            src("NHL rosters", SourceSeverity::Warn, Some(HOUR), Some(30.0 * 24.0 * HOUR), Some("1 of 32 requests failed"), true, None),
            ok("NHL stats", Some(HOUR), Some(6.0 * HOUR), true),
        ]);
        let h = data_health(&ctx, None);
        assert_eq!((h.overall, should_alert(&h), h.ok_count), (Overall::Failed, true, 1));
        assert_eq!(h.failed.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["Fantrax", "MoneyPuck"]);
        assert_eq!(h.warned.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["NHL rosters"]);
        assert_eq!(h.headline.as_deref(), Some("\u{26a0} Data problems: Fantrax login expired; MoneyPuck unavailable; NHL rosters partial"));
        assert_eq!(h.lines[0], "Fantrax: login expired \u{2014} refresh FANTRAX_COOKIE");
        assert_eq!(h.lines[1], "MoneyPuck: unavailable (HttpError: HTTP 503 for moneypuck.com); last good data yesterday");
        assert_eq!(h.lines[2], "NHL rosters: 1 of 32 requests failed");
        assert_eq!(h.lines[3], "NHL stats: fetched 60 min ago");
    }

    #[test]
    fn falls_back_to_warnings_without_sources() {
        let mut ctx = league(vec![]);
        let h = data_health(&ctx, None);
        assert_eq!(h.overall, Overall::Ok);
        assert!(h.footer.is_none());
        ctx.warnings.extend(
            [
                "NHL enrichment failed: boom",
                "ESPN scoring period unavailable: 401 Unauthorized",
                "Injury feed unavailable (RuntimeError: offline)",
                "Dynasty valuation failed: x",
            ]
            .map(String::from),
        );
        let h = data_health(&ctx, None);
        assert_eq!(h.failed.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["ESPN", "NHL data", "Injuries"]);
        assert_eq!(h.lines[0], "ESPN: login expired \u{2014} refresh ESPN_S2 / ESPN_SWID");
    }

    #[test]
    fn footer_summary_text() {
        assert_eq!(footer_summary(&league(vec![])), None);
        let ok1 = footer_summary(&league(vec![ok("NHL stats", Some(HOUR), Some(6.0 * HOUR), false)])).unwrap();
        assert_eq!((ok1.level, ok1.text.as_str()), ("ok", "all 1 source fresh (oldest: NHL stats, 60m)"));
        let bad = footer_summary(&league(vec![
            ok("NHL stats", Some(HOUR), Some(6.0 * HOUR), false),
            ok("Injuries", Some(5.0 * HOUR), Some(HOUR), true),
            src("Fantrax", SourceSeverity::Fail, None, None, Some("login expired \u{2014} refresh FANTRAX_COOKIE"), false, None),
        ]))
        .unwrap();
        assert_eq!(bad.level, "fail");
        assert_eq!(bad.text, "1 fresh \u{b7} 1 stale \u{b7} 1 failed \u{2014} worst: Fantrax (login expired \u{2014} refresh FANTRAX_COOKIE)");
    }

    #[test]
    fn ages() {
        assert_eq!(ago(Some(30.0)), "just now");
        assert_eq!(ago(Some(14.0 * 60.0)), "14 min ago");
        assert_eq!(ago(Some(5.0 * HOUR)), "5h ago");
        assert_eq!(ago(Some(40.0 * HOUR)), "yesterday");
        assert_eq!(ago(Some(72.0 * HOUR)), "3 days ago");
        assert_eq!(ago(None), "at an unknown time");
        assert_eq!(short_age(Some(19.0 * HOUR)), "19h");
        assert_eq!(short_age(Some(72.0 * HOUR)), "3d");
    }
}
