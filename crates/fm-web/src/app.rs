//! Rust port of `fantasy_manager/web/app.py`: the dashboard server (axum) - every page and API
//! route, the per-league result cache, refresh / dynasty-mode / "Did it" / explain / params
//! rollback endpoints and static files. Templates are the Python ones, rendered by
//! [`crate::templates`]. Owner: work package `W10-web-app`.
//!
//! Shape of the port:
//! * `create_app(loader, ...)` -> [`App::new`] + [`App::router`] (no auth) / [`serve`] (auth,
//!   connect-info, bind check). Handlers run their work (loads, view models, rendering) on the
//!   blocking pool; the routing is a single dispatcher so query parsing / 404 / 405 / the 422
//!   league check mirror FastAPI's.
//! * [`PipelineLoader`] (mode-independent [`fm_providers::pipeline::BaseLoad`] kept per league)
//!   and [`ResultCache`] (TTL, one load at a time per league) as in Python. `finish` runs
//!   `advise` with its default engines (as Python's `finish` did) and reads the injury history.
//! * Performance additions (not in Python, same pages): a fresh cache hit is served without
//!   taking the league lock; with [`AppConfig::background`] both leagues are loaded at startup
//!   (default league first) and a refresher thread reloads a league shortly before its TTL runs
//!   out (stale-while-revalidate: requests keep the old result until the new one is in), for
//!   leagues requested within [`REFRESH_IDLE`]. The explicit Refresh button behaves as before.
//! * The clock is explicit ([`AppConfig::clock`]; `FM_NOW` in [`run`]).
//! * Errors that Python caught as `ProviderError` are any error with `fm_core::ProviderError` or
//!   `fm_providers::pipeline::LoadError` in the chain; others render error.html with
//!   `"Error: <message>"` (Python printed the exception class name).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use axum::Router;
use chrono::{NaiveDate, NaiveDateTime};
use fm_core::config::Settings;
use fm_core::models::{FantasyTeam, LeagueContext, Player, RecKind, Recommendation};
use fm_core::provider::LeagueProvider;
use fm_core::{Map, ProviderError, PyDateTime};
use fm_engine::recommend::advise::{advise, kind_group as advise_kind_group, order_cmp, AdviseOpts, MOVE_GROUPS};
use fm_engine::recommend::injuries::{History, StatusHistory};
use fm_pycompat::pyjson::{dumps, DumpOpts};
use fm_pycompat::text::{capitalize, py_strip};
use fm_sources::llm::SupportsComplete;
use minijinja::{Environment, Value};
use serde_json::{json, Value as J};

use crate::auth::{self, AuthConfig, AuthState};
use crate::templates::{self, escape_html, quote, safe, to_mj, EnvOptions, HealthViewObj, ResObj};
use crate::views::{self, LoadResult};

pub const LEAGUES: [&str; 2] = ["espn", "fantrax"];
pub const LEAGUE_PATTERN: &str = "^(espn|fantrax)$";
pub const CACHE_TTL: f64 = 600.0;
/// background refresh starts this long before a cached result expires
pub const REFRESH_MARGIN: Duration = Duration::from_secs(90);
/// leagues not requested for this long are no longer refreshed in the background
pub const REFRESH_IDLE: Duration = Duration::from_secs(3 * 3600);
pub const ALERT_STATUSES: [&str; 5] = ["dtd", "out", "ir", "ltir", "suspended"];
/// filter key -> (tab label, rec kinds), in display priority order
pub const KIND_FILTERS: [(&str, &str, &[&str]); 6] = [
    ("injury", "Injury", &["injury"]),
    ("lineup", "Lineup", &["lineup"]),
    ("waiver", "Waiver", &["waiver"]),
    ("trade", "Trade", &["trade"]),
    ("flags", "Flags", &["sell_high", "buy_low"]),
    ("alert", "Alerts", &["alert"]),
];
pub const KIND_LABEL_KEYS: [&str; 7] = ["lineup", "waiver", "trade", "sell_high", "buy_low", "injury", "alert"];
pub const OVERVIEW_MOVES: usize = 5;
pub const REASONS_SHOWN: usize = 3;
pub const EXPLAIN_FAILED: &str =
    "Explanation unavailable right now (free model rate-limited); numbers above are the full basis.";
pub const NAME_SUFFIXES: [&str; 7] = ["jr", "jr.", "sr", "sr.", "ii", "iii", "iv"];
pub const NUMBER_WORDS: [&str; 11] = ["No", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten"];
pub const LEAD_VERBS: [&str; 8] = ["move", "add", "start", "trade", "activate", "bench", "sell", "buy"];
const PAGE_PATHS: [&str; 10] =
    ["/", "/plan", "/overview", "/roster", "/recommendations", "/news", "/health", "/schedule", "/matchup", "/trade-eval"];

/// (env var, Settings attribute, requirement note) shown on the setup page.
pub fn setup_vars(league: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    match league {
        "espn" => &[
            ("ESPN_LEAGUE_ID", "espn_league_id", "required - the leagueId=... number in your league URL"),
            ("ESPN_S2", "espn_s2", "private leagues - espn_s2 cookie from a logged-in browser"),
            ("ESPN_SWID", "espn_swid", "private leagues - SWID cookie, including the braces"),
            ("ESPN_YEAR", "espn_year", "optional - season as ESPN labels it (2026-27 is 2027)"),
            ("ESPN_TEAM", "espn_team", "optional - your team id or part of its name"),
        ],
        "fantrax" => &[
            ("FANTRAX_LEAGUE_ID", "fantrax_league_id", "required - the id in your league URL"),
            ("FANTRAX_USERNAME", "fantrax_username", "recommended - your Fantrax login (fm logs in itself)"),
            ("FANTRAX_PASSWORD", "fantrax_password", "recommended - stored only in the local .env"),
            ("FANTRAX_COOKIE", "fantrax_cookie", "alternative to a login - Cookie request header"),
            ("FANTRAX_COOKIE_FILE", "fantrax_cookie_file", "alternative to FANTRAX_COOKIE - path to a cookie file"),
            ("FANTRAX_TEAM", "fantrax_team", "optional - your team id or part of its name"),
            ("FANTRAX_POINTS", "fantrax_points", "recommended - your point values, e.g. G=3,A=2,SOG=0.4"),
        ],
        _ => &[],
    }
}

fn status_label(s: &str) -> Option<&'static str> {
    Some(match s {
        "healthy" => "Healthy",
        "dtd" => "Day-to-day",
        "out" => "Out",
        "ir" => "IR",
        "ltir" => "LTIR",
        "suspended" => "Suspended",
        "unknown" => "Unknown",
        _ => return None,
    })
}

// --------------------------------------------------------------------------- loading

/// One league's dashboard data: the result plus the provider the matchup / plan / trade pages
/// call after the load (Python `views.provider_for`).
#[derive(Clone)]
pub struct Page {
    pub res: Arc<LoadResult>,
    pub provider: Option<Arc<dyn LeagueProvider>>,
}

/// `loader(league) -> LoadResult`.
pub trait Loader: Send + Sync {
    fn load(&self, league: &str, settings: &Settings, now: &PyDateTime) -> anyhow::Result<Page>;
    /// Drop cached intermediate state (`PipelineLoader.invalidate`); None = every league.
    fn invalidate(&self, _league: Option<&str>) {}
    /// Load again from scratch for the background refresher (default: invalidate + load).
    fn reload(&self, league: &str, settings: &Settings, now: &PyDateTime) -> anyhow::Result<Page> {
        self.invalidate(Some(league));
        self.load(league, settings, now)
    }
}

impl<F> Loader for F
where
    F: Fn(&str) -> anyhow::Result<Page> + Send + Sync,
{
    fn load(&self, league: &str, _settings: &Settings, _now: &PyDateTime) -> anyhow::Result<Page> {
        self(league)
    }
}

/// `current_mode()`: (effective dynasty mode, source); never fails.
pub fn current_mode(settings: &Settings) -> (String, String) {
    let (m, s) = fm_core::prefs::dynasty_mode_info(settings);
    (m.as_str().to_string(), s)
}

/// The mode-independent half of a load, shared by every mode (`BaseLoad`).
pub struct Base {
    pub ctx: LeagueContext,
    pub values: Map<String, fm_core::PlayerValue>,
    pub provider: Arc<dyn LeagueProvider>,
    pub news_by_cid: Map<String, Vec<J>>,
    pub loaded_at: NaiveDateTime,
    pub seconds: f64,
    pub cache: Option<Arc<fm_http::HttpCache>>,
}

impl Base {
    pub fn from_pipeline(b: fm_providers::pipeline::BaseLoad) -> Base {
        let news = b
            .news_by_cid
            .into_iter()
            .map(|(k, v)| (k, v.iter().map(|n| serde_json::to_value(n).unwrap_or(J::Null)).collect()))
            .collect();
        Base {
            ctx: b.ctx,
            values: b.values,
            provider: Arc::from(b.provider),
            news_by_cid: news,
            loaded_at: b.loaded_at,
            seconds: b.seconds,
            cache: Some(b.cache),
        }
    }
}

/// `finish(base, mode, source, recalc_only)`: dynasty values + recommendations for one mode.
pub fn finish(base: &Base, mode: &str, source: &str, recalc_only: bool, settings: &Settings, now: &PyDateTime) -> LoadResult {
    let t0 = Instant::now();
    let mut ctx = base.ctx.clone();
    if let Some(m) = fm_core::prefs::normalize_mode(mode) {
        ctx.dynasty_mode = m;
    }
    ctx.dynasty_mode_source = Some(source.to_string());
    let dynv = fm_providers::pipeline::dynasty(&mut ctx, &base.values, Some(base.provider.as_ref()));
    let today = local_naive(now).date();
    let memo = fm_engine::valuation::schedule::ScheduleMemo::new(Some(settings.fm_data_dir.clone()));
    let (recs, status_log) = match StatusHistory::new(&settings.fm_data_dir) {
        Ok(h) => {
            let hist = History::Store(&h);
            let opts = AdviseOpts { dynasty_values: dynv.as_ref(), history: &hist, ..AdviseOpts::default() };
            let recs = advise(&ctx, &base.values, &opts, now, &memo);
            let log = match h.by_player() {
                Ok(l) => l,
                Err(e) => {
                    ctx.warnings.push(format!("Injury history unavailable: {e}"));
                    Map::new()
                }
            };
            (recs, log)
        }
        Err(e) => {
            let opts = AdviseOpts { dynasty_values: dynv.as_ref(), ..AdviseOpts::default() };
            let recs = advise(&ctx, &base.values, &opts, now, &memo);
            ctx.warnings.push(format!("Injury history unavailable: {e}"));
            (recs, Map::new())
        }
    };
    let warnings = ctx.warnings.clone();
    let mut res = LoadResult::new(ctx, base.values.clone(), dynv, recs, now.clone(), today, settings.fm_data_dir.clone());
    res.news_by_cid = base.news_by_cid.clone();
    res.loaded_at = base.loaded_at;
    *res.warnings.lock().unwrap_or_else(|e| e.into_inner()) = warnings;
    res.mode = Some(mode.to_string());
    res.mode_source = Some(source.to_string());
    res.compute_seconds = Some(t0.elapsed().as_secs_f64() + if recalc_only { 0.0 } else { base.seconds });
    res.recalc_only = recalc_only;
    res.status_log = status_log;
    res
}

fn local_naive(now: &PyDateTime) -> NaiveDateTime {
    match now.to_utc() {
        Some(u) => u.with_timezone(&chrono::Local).naive_local(),
        None => now.date().and_hms_opt(0, 0, 0).unwrap_or_default(),
    }
}

/// The real pipeline with the mode-independent base kept per league for the TTL.
pub struct PipelineLoader {
    pub ttl: Duration,
    bases: Mutex<HashMap<String, (Instant, Arc<Base>)>>,
}

impl PipelineLoader {
    pub fn new(ttl: Duration) -> Self {
        PipelineLoader { ttl, bases: Mutex::new(HashMap::new()) }
    }

    fn load_base(&self, league: &str, settings: &Settings, now: &PyDateTime) -> anyhow::Result<Arc<Base>> {
        fm_providers::pipeline::install_params(settings);
        let clock = now.to_utc().map(fm_providers::enrich::Clock::at);
        let b = fm_providers::pipeline::load_base(league, settings, clock)?;
        Ok(Arc::new(Base::from_pipeline(b)))
    }
}

impl Loader for PipelineLoader {
    fn load(&self, league: &str, settings: &Settings, now: &PyDateTime) -> anyhow::Result<Page> {
        let (mode, source) = current_mode(settings);
        let hit = self.bases.lock().unwrap_or_else(|e| e.into_inner()).get(league).cloned();
        if let Some((at, base)) = hit {
            if at.elapsed() < self.ttl {
                let res = finish(&base, &mode, &source, true, settings, now);
                return Ok(Page { res: Arc::new(res), provider: Some(base.provider.clone()) });
            }
        }
        let base = self.load_base(league, settings, now)?;
        self.bases.lock().unwrap_or_else(|e| e.into_inner()).insert(league.to_string(), (Instant::now(), base.clone()));
        let res = finish(&base, &mode, &source, false, settings, now);
        Ok(Page { res: Arc::new(res), provider: Some(base.provider.clone()) })
    }

    fn invalidate(&self, league: Option<&str>) {
        let mut b = self.bases.lock().unwrap_or_else(|e| e.into_inner());
        match league {
            None => b.clear(),
            Some(l) => {
                b.remove(l);
            }
        }
    }
}

/// Per-league result cache with a TTL; one load at a time per league.
pub struct ResultCache {
    pub loader: Arc<dyn Loader>,
    pub ttl: Duration,
    items: Mutex<HashMap<String, (Instant, Page)>>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    access: Mutex<HashMap<String, Instant>>,
}

impl ResultCache {
    pub fn new(loader: Arc<dyn Loader>, ttl: Duration) -> Self {
        ResultCache {
            loader,
            ttl,
            items: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            access: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self, league: &str) -> Arc<Mutex<()>> {
        self.locks.lock().unwrap_or_else(|e| e.into_inner()).entry(league.to_string()).or_default().clone()
    }

    fn fresh(&self, league: &str) -> Option<Page> {
        let items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        items.get(league).filter(|(at, _)| at.elapsed() < self.ttl).map(|(_, p)| p.clone())
    }

    pub fn get(&self, league: &str, settings: &Settings, now: &PyDateTime) -> anyhow::Result<Page> {
        self.access.lock().unwrap_or_else(|e| e.into_inner()).insert(league.to_string(), Instant::now());
        if let Some(p) = self.fresh(league) {
            return Ok(p);
        }
        let l = self.lock(league);
        let _g = l.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = self.fresh(league) {
            return Ok(p);
        }
        let page = self.loader.load(league, settings, now)?;
        self.items.lock().unwrap_or_else(|e| e.into_inner()).insert(league.to_string(), (Instant::now(), page.clone()));
        Ok(page)
    }

    /// Reload a league in place (background refresh): the old result stays served meanwhile.
    pub fn refresh_ahead(&self, league: &str, settings: &Settings, now: &PyDateTime) -> anyhow::Result<()> {
        let l = self.lock(league);
        let _g = l.lock().unwrap_or_else(|e| e.into_inner());
        let page = self.loader.reload(league, settings, now)?;
        self.items.lock().unwrap_or_else(|e| e.into_inner()).insert(league.to_string(), (Instant::now(), page));
        Ok(())
    }

    pub fn age(&self, league: &str) -> Option<f64> {
        self.items.lock().unwrap_or_else(|e| e.into_inner()).get(league).map(|(at, _)| at.elapsed().as_secs_f64())
    }

    pub fn last_access(&self, league: &str) -> Option<Instant> {
        self.access.lock().unwrap_or_else(|e| e.into_inner()).get(league).copied()
    }

    /// The cached result (even if expired) without loading.
    pub fn peek(&self, league: &str) -> Option<Page> {
        self.items.lock().unwrap_or_else(|e| e.into_inner()).get(league).map(|(_, p)| p.clone())
    }

    pub fn clear(&self, league: Option<&str>) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        match league {
            None => items.clear(),
            Some(l) => {
                items.remove(l);
            }
        }
    }
}

// --------------------------------------------------------------------------- view helpers

/// Footer league line (`_moves_footer`): None without budget information.
pub fn moves_footer(ctx: &LeagueContext) -> Option<String> {
    if ctx.moves_limit_per_period.is_none() && ctx.moves_used_this_period.is_none() && ctx.moves_limit_season.is_none() {
        return None;
    }
    catch_unwind(AssertUnwindSafe(|| fm_engine::recommend::base::moves_text(ctx))).ok()
}

/// `ago(when, now)` from the elapsed seconds.
pub fn ago_text(secs: f64) -> String {
    if secs < 60.0 {
        "just now".into()
    } else if secs < 3600.0 {
        format!("{} min ago", fm_pycompat::fmt::fixed((secs / 60.0).floor(), 0, false))
    } else if secs < 86400.0 {
        format!("{} h ago", fm_pycompat::fmt::fixed((secs / 3600.0).floor(), 0, false))
    } else {
        format!("{} d ago", fm_pycompat::fmt::fixed((secs / 86400.0).floor(), 0, false))
    }
}

/// `rec_key(league, r)`: sha1 of league|kind|sorted add cids|sorted drop cids|counterparty.
pub fn rec_key(league: &str, r: &Recommendation) -> String {
    fm_harness::ledger::rec_key(
        league,
        r.kind.as_str(),
        r.add.iter().map(|p| p.cid.as_str()),
        r.drop.iter().map(|p| p.cid.as_str()),
        r.counterparty.as_deref(),
        std::iter::empty::<&str>(),
    )
}

fn kind_str(k: RecKind) -> &'static str {
    k.as_str()
}

fn ranked(recs: &[Recommendation]) -> Vec<&Recommendation> {
    let mut v: Vec<&Recommendation> = recs.iter().collect();
    v.sort_by(|a, b| order_cmp(a, b));
    v
}

fn filter_recs<'a>(recs: &'a [Recommendation], kind: &str) -> Vec<&'a Recommendation> {
    if kind.is_empty() || kind == "all" {
        return ranked(recs);
    }
    let kinds: Vec<&str> = match KIND_FILTERS.iter().find(|(k, _, _)| *k == kind) {
        Some((_, _, ks)) => ks.to_vec(),
        None => vec![kind],
    };
    let mut v: Vec<&Recommendation> = recs.iter().filter(|r| kinds.contains(&kind_str(r.kind))).collect();
    v.sort_by(|a, b| order_cmp(a, b));
    v
}

fn kind_counts(recs: &[Recommendation]) -> J {
    J::Object(KIND_FILTERS.iter().map(|(k, _, _)| (k.to_string(), json!(filter_recs(recs, k).len()))).collect())
}

pub fn normalize_kind(kind: Option<&str>) -> String {
    match kind {
        Some(k) if !k.is_empty() && (KIND_FILTERS.iter().any(|(f, _, _)| *f == k) || KIND_LABEL_KEYS.contains(&k)) => k.to_string(),
        _ => "all".into(),
    }
}

// --------------------------------------------------------------------------- move wording

pub fn last_name(name: &str) -> String {
    let mut parts: Vec<&str> = name.split_whitespace().collect();
    while parts.len() > 1 && NAME_SUFFIXES.contains(&parts[parts.len() - 1].to_lowercase().as_str()) {
        parts.pop();
    }
    parts.last().map(|s| s.to_string()).unwrap_or_else(|| name.to_string())
}

fn short_names(players: &[Player], others: &[Player]) -> Vec<String> {
    let lasts: Vec<String> = players.iter().chain(others).map(|p| last_name(&p.name)).collect();
    players
        .iter()
        .map(|p| {
            let l = last_name(&p.name);
            if lasts.iter().filter(|x| **x == l).count() == 1 {
                l
            } else {
                p.name.clone()
            }
        })
        .collect()
}

/// One imperative "what to do" line per move.
pub fn action_line(r: &Recommendation) -> String {
    let title = py_strip(&r.title).to_string();
    let (add, drop) = (&r.add, &r.drop);
    match r.kind {
        RecKind::Lineup => {
            if title.starts_with("Bench ") && !drop.is_empty() {
                return format!("Bench {}", short_names(drop, &[])[0]);
            }
            if title.starts_with("Activate ") && !add.is_empty() {
                return format!("Activate {} from IR", short_names(add, &[])[0]);
            }
            if !add.is_empty() {
                let ins = short_names(add, drop);
                let outs = short_names(drop, add);
                if !outs.is_empty() && title.ends_with(" has no game)") {
                    return format!("Start {} in {}'s slot today", ins.join(" + "), outs.join(" + "));
                }
                let over = if outs.is_empty() { String::new() } else { format!(" over {}", outs.join(" + ")) };
                return format!("Start {}{over}", ins.join(" + "));
            }
        }
        RecKind::Trade if !add.is_empty() && !drop.is_empty() => {
            return format!("Propose: {} for {}", short_names(drop, add).join(" + "), short_names(add, drop).join(" + "));
        }
        RecKind::Injury => {
            if title.starts_with("Move ") {
                return title.replace(" and add ", ", add ");
            }
            if title.contains(" now ") && !title.starts_with("Activate ") && !title.starts_with("Add ") {
                let (name, rest) = title.split_once(" now ").unwrap_or((&title, ""));
                return format!("Check on {name}: now {rest}");
            }
            if title.starts_with("Activate ") && !drop.is_empty() {
                return format!("{title}, drop {}", drop[0].name);
            }
            return title;
        }
        RecKind::Waiver => return title.replace(" (open roster spot)", ""),
        RecKind::SellHigh if !drop.is_empty() => {
            return format!("Shop {} while his value is high", short_names(drop, &[])[0]);
        }
        RecKind::BuyLow if !add.is_empty() => {
            let cp = py_strip(r.counterparty.as_deref().unwrap_or("")).to_string();
            let cpt = if cp.is_empty() { String::new() } else { format!(" ({cp})") };
            return format!("Ask about {}{cpt} while his value is low", short_names(add, &[])[0]);
        }
        _ => {}
    }
    title
}

/// Context the action line drops from the title; None when it adds nothing.
pub fn action_note(r: &Recommendation) -> Option<String> {
    let title = py_strip(&r.title).to_string();
    let lineup = r.kind == RecKind::Lineup;
    if lineup && title.ends_with("(locks Monday)") {
        return Some("For this week's lineup: Fantrax locks it on Monday.".into());
    }
    if lineup && title.contains("; move ") {
        let rest = py_strip(title.split_once("; ").map(|x| x.1).unwrap_or("")).to_string();
        let dot = if rest.ends_with('.') { "" } else { "." };
        return Some(format!("Then {rest}{dot}"));
    }
    if lineup && (title.contains("(empty slot)") || title.contains("(open starting slot)")) {
        return Some("Fills an empty starting slot.".into());
    }
    if lineup && title.starts_with("Bench ") && title.contains(':') {
        let after = py_strip(title.split_once(':').map(|x| x.1).unwrap_or("")).to_string();
        return Some(capitalize(&after) + ".");
    }
    if r.kind == RecKind::Waiver {
        if let Some(w) = r.reasons.iter().find(|x| x.code == "WAIVER_CLAIM") {
            let dot = if w.text.ends_with('.') { "" } else { "." };
            return Some(format!("{}{dot}", w.text));
        }
        if title.contains("(open roster spot)") {
            return Some("Uses an open roster spot, no drop needed.".into());
        }
    }
    None
}

fn number_word(n: usize, capital: bool) -> String {
    let w = NUMBER_WORDS.get(n).map(|s| s.to_string()).unwrap_or_else(|| n.to_string());
    if capital {
        w
    } else {
        w.to_lowercase()
    }
}

fn status_phrase(status: &str) -> String {
    match status {
        "ir" | "ltir" => format!("is on {}", status_label(status).unwrap_or(status)),
        "dtd" => "is day-to-day".into(),
        "out" => "is out".into(),
        "suspended" => "is suspended".into(),
        s => format!("is {s}"),
    }
}

fn join_markup(items: &[String]) -> String {
    if items.len() <= 2 {
        return items.join(" and ");
    }
    format!("{}, and {}", items[..items.len() - 1].join(", "), items[items.len() - 1])
}

fn py_isupper(s: &str) -> bool {
    let mut cased = false;
    for c in s.chars() {
        if c.is_lowercase() {
            return false;
        }
        if c.is_uppercase() {
            cased = true;
        }
    }
    cased
}

/// The plain-English lede on the overview (HTML, every name escaped).
pub fn overview_summary(recs: &[Recommendation], alerts: &[(String, &Player)], prospects: usize) -> String {
    let e = escape_html;
    let mut parts: Vec<String> = Vec::new();
    let rk = ranked(recs);
    let is_move = |r: &Recommendation| MOVE_GROUPS.contains(&advise_kind_group(r.kind));
    let moves: Vec<&Recommendation> = rk.iter().copied().filter(|r| is_move(r)).collect();
    let (n, other) = (moves.len(), rk.len() - moves.len());
    let extra = if other > 0 {
        format!(" ({other} alert{}, trade ideas and flags below)", if other != 1 { "s" } else { "" })
    } else {
        String::new()
    };
    if n == 0 {
        parts.push(format!("No moves to make right now{}.", e(&extra)));
    } else {
        parts.push(format!("<b>{}</b> to make{}.", e(&format!("{n} move{}", if n != 1 { "s" } else { "" })), e(&extra)));
        let title = py_strip(&moves[0].title).trim_end_matches('.').to_string();
        let lead = if n > 1 { "The strongest" } else { "It" };
        let first_word = title.split(' ').next().unwrap_or("").to_lowercase();
        if LEAD_VERBS.contains(&first_word.as_str()) {
            let mut cs = title.chars();
            let lowered = match cs.next() {
                Some(c) => c.to_lowercase().collect::<String>() + cs.as_str(),
                None => String::new(),
            };
            parts.push(format!("{lead} is to {}.", e(&lowered)));
        } else {
            parts.push(format!("{lead}: {}.", e(&title)));
        }
    }
    let acted: std::collections::HashSet<&str> = recs
        .iter()
        .filter(|r| is_move(r))
        .flat_map(|r| r.add.iter().chain(&r.drop).chain(&r.subjects).map(|p| p.cid.as_str()))
        .collect();
    let phrase = |slot: &str, p: &Player| {
        let st = p.status.as_str();
        status_phrase(st) + if slot == "IR" && st == "dtd" { " and still in your IR slot" } else { "" }
    };
    let needs: Vec<&(String, &Player)> = alerts.iter().filter(|(_, p)| acted.contains(p.cid.as_str())).collect();
    let idle: Vec<&(String, &Player)> = alerts.iter().filter(|(_, p)| !acted.contains(p.cid.as_str())).collect();
    let k = needs.len();
    if k == 0 && idle.is_empty() {
        parts.push("Nobody on your roster is hurt.".into());
    } else if k == 1 {
        let (slot, p) = needs[0];
        parts.push(format!("One player needs attention: <b>{}</b> {}.", e(&p.name), e(&phrase(slot, p))));
    } else if k > 1 {
        let who: Vec<String> = needs
            .iter()
            .map(|(slot, p)| {
                let st = p.status.as_str();
                let extra = if slot == "IR" && st == "dtd" { ", still in IR slot" } else { "" };
                format!("<b>{}</b> ({}{})", e(&p.name), e(status_label(st).unwrap_or(st)), e(extra))
            })
            .collect();
        parts.push(format!("{} players need attention: {}.", e(&number_word(k, true)), join_markup(&who)));
    }
    if !idle.is_empty() {
        let who: Vec<String> = idle
            .iter()
            .map(|(slot, p)| {
                let st = p.status.as_str();
                let lab = status_label(st).unwrap_or(st);
                let label = if slot == "IR" {
                    "IR slot".to_string()
                } else if py_isupper(lab) {
                    lab.to_string()
                } else {
                    lab.to_lowercase()
                };
                format!("{} ({})", e(&p.name), e(&label))
            })
            .collect();
        parts.push(format!("Hurt, nothing to do right now: {}.", join_markup(&who)));
    }
    if prospects > 0 {
        let t = if prospects == 1 { "one prospect".to_string() } else { format!("{} prospects", number_word(prospects, false)) };
        parts.push(format!("You carry {}.", e(&t)));
    }
    parts.join(" ")
}

/// Bar geometry for a signed VORP.
pub fn vorp_bar(v: Option<f64>, scale: f64) -> J {
    let Some(v) = v else { return J::Null };
    let width = if scale > 0.0 { (v.abs() / scale * 50.0).min(50.0) } else { 0.0 };
    json!({"side": if v >= 0.0 { "pos" } else { "neg" }, "width": fm_pycompat::num::round_to(width, 1)})
}

fn my_team(ctx: &LeagueContext) -> Option<&FantasyTeam> {
    ctx.my_team().ok().or_else(|| ctx.teams.first())
}

fn where_is<'a>(ctx: &'a LeagueContext, cid: &str) -> (Option<&'a FantasyTeam>, Option<String>) {
    for t in &ctx.teams {
        for s in &t.slots {
            if s.player.as_ref().is_some_and(|p| p.cid == cid) {
                return (Some(t), Some(s.slot.clone()));
            }
        }
    }
    (None, None)
}

fn meta(res: &LoadResult) -> J {
    let lc = &res.ctx;
    let w = res.warnings();
    json!({"provider": lc.provider, "league_id": lc.league_id, "league_name": lc.name,
           "as_of": lc.as_of.format("%Y-%m-%d").to_string(),
           "loaded_at": res.loaded_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
           "season_start": lc.season_start.map(|d| d.format("%Y-%m-%d").to_string()),
           "sources": lc.source_notes, "warnings": if w.is_empty() { lc.warnings.clone() } else { w }})
}

fn setting_set(s: &Settings, attr: &str) -> bool {
    let sec = |x: &Option<fm_core::config::Secret>| x.as_ref().is_some_and(|v| !v.expose().is_empty());
    match attr {
        "espn_league_id" => s.espn_league_id.is_some_and(|v| v != 0),
        "espn_s2" => sec(&s.espn_s2),
        "espn_swid" => sec(&s.espn_swid),
        "espn_year" => s.espn_year.is_some_and(|v| v != 0),
        "espn_team" => s.espn_team.as_ref().is_some_and(|v| !v.is_empty()),
        "fantrax_league_id" => s.fantrax_league_id.as_ref().is_some_and(|v| !v.is_empty()),
        "fantrax_username" => sec(&s.fantrax_username),
        "fantrax_password" => sec(&s.fantrax_password),
        "fantrax_cookie" => sec(&s.fantrax_cookie),
        "fantrax_cookie_file" => s.fantrax_cookie_file.is_some(),
        "fantrax_team" => s.fantrax_team.as_ref().is_some_and(|v| !v.is_empty()),
        "fantrax_points" => !s.fantrax_points.is_empty(),
        _ => false,
    }
}

fn setup_rows(league: &str, s: &Settings) -> J {
    J::Array(setup_vars(league).iter().map(|(env, attr, note)| json!({"env": env, "note": note, "set": setting_set(s, attr)})).collect())
}

/// `parse_qsl(query, keep_blank_values=True)`.
pub fn parse_qsl(q: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(q.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

/// `urlencode(pairs)` (quote_plus).
pub fn urlencode(pairs: &[(String, String)]) -> String {
    let qp = |s: &str| quote(s, " ").replace(' ', "+");
    pairs.iter().map(|(k, v)| format!("{}={}", qp(k), qp(v))).collect::<Vec<_>>().join("&")
}

/// A same-site redirect target (`safe_next`).
pub fn safe_next(next_url: Option<&str>, league: &str, extra: &[(&str, String)]) -> String {
    let mut target = next_url.filter(|s| !s.is_empty()).unwrap_or("/").to_string();
    if !target.starts_with('/') || target.starts_with("//") || target.contains('\\') || target.chars().any(|c| (c as u32) < 32 || c as u32 == 127) {
        target = "/".into();
    }
    let no_frag = target.split('#').next().unwrap_or("").to_string();
    let (path, query) = match no_frag.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (no_frag.clone(), String::new()),
    };
    let mut q: Vec<(String, String)> =
        parse_qsl(&query).into_iter().filter(|(k, _)| k != "msg" && !extra.iter().any(|(e, _)| e == k)).collect();
    if !q.iter().any(|(k, _)| k == "league") {
        q.push(("league".into(), league.into()));
    }
    q.extend(extra.iter().map(|(k, v)| (k.to_string(), v.clone())));
    let path = if path.is_empty() { "/".to_string() } else { path };
    let qs = urlencode(&q);
    if qs.is_empty() {
        path
    } else {
        format!("{path}?{qs}")
    }
}

fn mode_flash(msg: Option<&str>, res: Option<&LoadResult>) -> Option<String> {
    let mode = fm_core::prefs::normalize_mode(msg?.strip_prefix("mode-")?)?.as_str();
    let mut text = format!("Recalculating with {mode} mode… done");
    if let Some(r) = res {
        if let Some(cs) = r.compute_seconds {
            if r.mode.as_deref().is_none_or(|m| m == mode) {
                text += &format!(" in {}s", fm_pycompat::fmt::fixed(cs, 1, false));
                if r.recalc_only {
                    text += " (cached league data reused)";
                }
            }
        }
    }
    Some(text + &format!(". Dynasty values and recommendations now use {mode} weights."))
}

fn is_version(v: &str) -> bool {
    v == "packaged" || (v.len() >= 5 && v.starts_with('v') && v[1..].bytes().all(|b| b.is_ascii_digit()))
}

fn rollback_flash(msg: Option<&str>) -> (Option<String>, bool) {
    let Some(msg) = msg.filter(|m| m.starts_with("rollback-")) else { return (None, false) };
    if let Some(rest) = msg.strip_prefix("rollback-ok-") {
        let parts: Vec<&str> = rest.splitn(2, '-').collect();
        if parts.len() == 2 && is_version(parts[0]) && is_version(parts[1]) {
            return (
                Some(format!(
                    "Rolled back the valuation parameters from {} to {}. Every page recalculates with them on its next load.",
                    parts[0], parts[1]
                )),
                false,
            );
        }
    }
    let text = match msg {
        "rollback-unconfirmed" => "Nothing changed: tick the confirmation box to roll the parameters back.",
        "rollback-none" => "Nothing to roll back: the packaged parameters are active.",
        "rollback-unknown" => "Rollback failed: unknown or already active version. Nothing changed.",
        _ => return (None, false),
    };
    (Some(text.into()), true)
}

// --------------------------------------------------------------------------- health

fn params_without_ledger(data_dir: &Path) -> J {
    let st = fm_harness::params_store::ParamsStore::new(None, None, Some(data_dir));
    json!({"version": st.active_name(), "hash": fm_engine::valuation::params::params_hash(None),
           "source": fm_engine::valuation::params::source(None), "versions": st.versions().len()})
}

/// The harness bar + M4 surfaces for /health (`health_report`).
pub fn health_report(data_dir: &Path, league: Option<&str>, today: NaiveDate, now: NaiveDateTime) -> anyhow::Result<J> {
    use fm_harness::ledger::{Ledger, DB_NAME};
    if !data_dir.join(DB_NAME).exists() {
        let mut rep = fm_harness::metrics::empty_report();
        let store = fm_harness::params_store::ParamsStore::new(None, None, Some(data_dir));
        rep["params"] = params_without_ledger(data_dir);
        rep["changelog"] = fm_harness::health::changelog(&store);
        rep["calendar"] = fm_harness::health::calendar(today, data_dir);
        return Ok(rep);
    }
    let led = Ledger::open(data_dir)?;
    let mut rep = fm_harness::health::full_report(&led, None, today, Some(data_dir), now)?;
    if let Some(lg) = league {
        let known = rep.get("leagues").and_then(|l| l.get(lg)).is_some();
        if !known {
            let l = fm_harness::health::extend_league(&led, lg, Some(fm_harness::metrics::league_report(&led, lg)?), now)?;
            if let Some(J::Object(ls)) = rep.get_mut("leagues") {
                ls.insert(lg.to_string(), l);
            }
        }
    }
    Ok(rep)
}

/// `fm harness refit --dry-run` for /health.
pub fn refit_dry_run(data_dir: &Path, today: NaiveDate, force: bool) -> anyhow::Result<J> {
    use fm_harness::ledger::{Ledger, DB_NAME};
    if !data_dir.join(DB_NAME).exists() {
        return Ok(json!({"error": "no harness ledger yet: run `fm harness daily` first"}));
    }
    let led = Ledger::open(data_dir)?;
    let opts = fm_harness::refit::RefitOptions { mode: "dry-run".into(), force, ..Default::default() };
    Ok(fm_harness::refit::run_refit(&led, today, &opts)?.to_dict())
}

/// Roll the active params version back (`params_rollback`).
pub fn params_rollback(data_dir: &Path, version: Option<&str>, today: NaiveDate) -> anyhow::Result<J> {
    use fm_harness::ledger::{Ledger, DB_NAME};
    use fm_harness::params_store::ParamsStore;
    if !data_dir.join(DB_NAME).exists() {
        return ParamsStore::new(None, None, Some(data_dir)).rollback(version, "dashboard", None);
    }
    let led = Ledger::open(data_dir)?;
    let out = ParamsStore::new(None, Some(&led), Some(data_dir)).rollback(version, "dashboard", None)?;
    let mut summary = out.clone();
    if let J::Object(m) = &mut summary {
        m.insert("via".into(), json!("dashboard"));
    }
    let iso = today.format("%Y-%m-%d").to_string();
    if let Err(e) = fm_harness::ingest::record_run(&led, "rollback", None, Some(&iso), Some(&summary), "ok") {
        tracing::error!("recording the rollback run failed: {e}");
    }
    Ok(out)
}

// --------------------------------------------------------------------------- the app

/// LLM client factory for POST /explain (None or unavailable = the "needs a key" note).
pub type LlmFactory = Arc<dyn Fn(&Settings) -> Option<Box<dyn fm_sources::llm::SupportsComplete>> + Send + Sync>;
pub type SettingsLoader = Arc<dyn Fn() -> anyhow::Result<Settings> + Send + Sync>;
pub type ClockFn = Arc<dyn Fn() -> PyDateTime + Send + Sync>;

/// `create_app` arguments.
#[derive(Clone)]
pub struct AppConfig {
    pub default_league: String,
    /// "plan" (the real server) or "overview": what "/" shows
    pub home: String,
    pub cache_ttl: Duration,
    pub settings: Settings,
    /// re-read the settings (Python `get_settings.cache_clear()` on refresh / mode); None = keep
    pub reload_settings: Option<SettingsLoader>,
    pub llm: Option<LlmFactory>,
    pub clock: ClockFn,
    /// None: the templates compiled into the binary (`FM_TEMPLATES_DIR` overrides)
    pub templates_dir: Option<PathBuf>,
    /// None: the static files compiled into the binary (`FM_STATIC_DIR` overrides)
    pub static_dir: Option<PathBuf>,
    pub auth_enabled: bool,
    /// warm both leagues at startup and refresh them before the TTL runs out
    pub background: bool,
}

impl AppConfig {
    pub fn new(settings: Settings) -> Self {
        AppConfig {
            default_league: "espn".into(),
            home: "overview".into(),
            cache_ttl: Duration::from_secs_f64(CACHE_TTL),
            settings,
            reload_settings: None,
            llm: None,
            clock: Arc::new(system_now),
            templates_dir: templates::template_dir(),
            static_dir: crate::assets::static_override(),
            auth_enabled: false,
            background: false,
        }
    }
}

/// The real clock, or `FM_NOW=<ISO datetime>`.
pub fn system_now() -> PyDateTime {
    if let Some(p) = std::env::var("FM_NOW").ok().and_then(|s| PyDateTime::parse(&s)) {
        return p;
    }
    PyDateTime::from_utc(chrono::Utc::now())
}

pub struct App {
    pub cfg: AppConfig,
    pub cache: ResultCache,
    pub loader: Arc<dyn Loader>,
    env: Environment<'static>,
    settings: RwLock<Arc<Settings>>,
    params_source: RwLock<Option<String>>,
}

/// A handler's answer.
pub enum Resp {
    Html(u16, String),
    Json(u16, J),
    Redirect(String),
}

struct Req {
    method: Method,
    path: String,
    query: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn q(&self, k: &str) -> Option<&str> {
        self.query.iter().rev().find(|(x, _)| x == k).map(|(_, v)| v.as_str())
    }

    fn all(&self, k: &str) -> Vec<String> {
        self.query.iter().filter(|(x, _)| x == k).map(|(_, v)| v.clone()).collect()
    }

    /// query params, then form fields (form wins), like `dict(query) | parse_qsl(body)`.
    fn fields(&self) -> HashMap<String, String> {
        let mut m: HashMap<String, String> = HashMap::new();
        for (k, v) in &self.query {
            m.insert(k.clone(), v.clone());
        }
        if !self.body.is_empty() {
            for (k, v) in parse_qsl(&String::from_utf8_lossy(&self.body)) {
                m.insert(k, v);
            }
        }
        m
    }
}

fn league_error(input: &str) -> Resp {
    Resp::Json(
        422,
        json!({"detail": [{"type": "string_pattern_mismatch", "loc": ["query", "league"],
                           "msg": format!("String should match pattern '{LEAGUE_PATTERN}'"), "input": input,
                           "ctx": {"pattern": LEAGUE_PATTERN}}]}),
    )
}

fn detail(code: u16, msg: String) -> Resp {
    Resp::Json(code, json!({"detail": msg}))
}

fn err_text(e: &anyhow::Error) -> String {
    format!("Error: {e}")
}

fn is_provider_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<ProviderError>().is_some() || c.downcast_ref::<fm_providers::pipeline::LoadError>().is_some())
}

fn panic_text(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "panic".into())
}

type Ctx = Vec<(String, Value)>;

macro_rules! cx {
    ($(($k:expr, $v:expr)),* $(,)?) => { vec![$(($k.to_string(), $v)),*] };
}

impl App {
    pub fn new(loader: Arc<dyn Loader>, cfg: AppConfig) -> Arc<App> {
        let env = templates::build_env(&EnvOptions {
            dir: cfg.templates_dir.clone(),
            home: cfg.home.clone(),
            auth_enabled: cfg.auth_enabled,
        });
        let cache = ResultCache::new(loader.clone(), cfg.cache_ttl);
        let settings = RwLock::new(Arc::new(cfg.settings.clone()));
        Arc::new(App { cfg, cache, loader, env, settings, params_source: RwLock::new(None) })
    }

    pub fn settings(&self) -> Arc<Settings> {
        self.settings.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn clear_settings(&self) {
        if let Some(f) = &self.cfg.reload_settings {
            match f() {
                Ok(s) => *self.settings.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(s),
                Err(e) => tracing::warn!("re-reading the settings failed: {e}"),
            }
        }
    }

    fn params_source(&self) -> String {
        if let Some(s) = self.params_source.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return s;
        }
        let s = catch_unwind(|| fm_engine::valuation::params::source(None)).unwrap_or_else(|_| "unavailable".into());
        *self.params_source.write().unwrap_or_else(|e| e.into_inner()) = Some(s.clone());
        s
    }

    fn reload_params(&self) {
        let s = self.settings();
        let _ = catch_unwind(AssertUnwindSafe(|| fm_providers::pipeline::install_params(&s)));
        *self.params_source.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn now(&self) -> PyDateTime {
        (self.cfg.clock)()
    }

    /// Cached result; recomputed when the effective mode changed since it was computed.
    pub fn get_result(&self, league: &str) -> anyhow::Result<Page> {
        let s = self.settings();
        let now = self.now();
        let page = self.cache.get(league, &s, &now)?;
        if let Some(m) = &page.res.mode {
            if *m != current_mode(&s).0 {
                self.cache.clear(Some(league));
                return self.cache.get(league, &s, &now);
            }
        }
        Ok(page)
    }

    /// Render a template (Python `templates.TemplateResponse`), with the always-present
    /// variables of `render` in app.py.
    pub fn render_page(&self, req_path: &str, query: &[(String, String)], name: &str, league: &str, status: u16, ctx: Ctx) -> Resp {
        let mut base: Vec<(String, Value)> = Vec::new();
        let switch_path = if PAGE_PATHS.contains(&req_path) { req_path } else { "/" };
        let kept: Vec<(String, String)> = query.iter().filter(|(k, _)| k != "msg").cloned().collect();
        let here = if kept.is_empty() { req_path.to_string() } else { format!("{req_path}?{}", urlencode(&kept)) };
        let s = self.settings();
        let (cur_mode, cur_source) = current_mode(&s);
        let now = self.now();
        let msg = query.iter().rev().find(|(k, _)| k == "msg").map(|(_, v)| v.as_str());
        let mut page: HashMap<String, Value> = ctx.iter().cloned().collect();
        let res_v = page.get("res").cloned().filter(|v| !v.is_none());
        let res: Option<Arc<LoadResult>> = res_v.as_ref().and_then(|v| v.downcast_object_ref::<ResObj>().map(|o| o.res.clone()));
        let put = |b: &mut Vec<(String, Value)>, k: &str, v: Value| b.push((k.to_string(), v));
        put(&mut base, "league", Value::from(league));
        put(&mut base, "path", Value::from(req_path));
        put(&mut base, "switch_path", Value::from(switch_path));
        put(&mut base, "here", Value::from(here));
        put(&mut base, "now", Value::from(naive_iso(local_naive(&now))));
        put(&mut base, "res", Value::from(()));
        put(&mut base, "cache_age", self.cache.age(league).map(Value::from).unwrap_or(Value::from(())));
        put(&mut base, "cur_mode", Value::from(cur_mode.as_str()));
        put(&mut base, "cur_mode_source", Value::from(cur_source.as_str()));
        put(&mut base, "params_source", Value::from(self.params_source()));
        for (k, v) in &ctx {
            if let Some(slot) = base.iter_mut().find(|(bk, _)| bk == k) {
                slot.1 = v.clone();
            } else {
                base.push((k.to_string(), v.clone()));
            }
        }
        let points = res.as_ref().map(|r| r.ctx.scoring.kind.as_str() == "points").unwrap_or(true);
        if !page.contains_key("vlabel") {
            put(&mut base, "vlabel", Value::from(if points { "FPG" } else { "Val/G" }));
        }
        if !page.contains_key("vlabel_title") {
            put(
                &mut base,
                "vlabel_title",
                Value::from(if points {
                    "fantasy points per game"
                } else {
                    "per-game z-score value summed over your categories (fitted on rostered players), not points"
                }),
            );
        }
        let no_mode = page.get("no_mode").is_some_and(|v| v.is_true());
        let show_mode = (league == "fantrax" || res.as_ref().is_some_and(|r| r.ctx.dynasty)) && !no_mode;
        put(&mut base, "show_mode", Value::from(show_mode));
        let (dm, dms) = match res.as_ref().and_then(|r| r.mode.clone().map(|m| (m, r.mode_source.clone()))) {
            Some((m, src)) => (Value::from(m), src.map(Value::from).unwrap_or(Value::from(()))),
            None => (Value::from(cur_mode.as_str()), Value::from(cur_source.as_str())),
        };
        put(&mut base, "data_mode", dm);
        put(&mut base, "data_mode_source", dms);
        let flash_given = page.get("flash").is_some_and(|v| !v.is_none());
        if !flash_given {
            let mut flash = mode_flash(msg, res.as_deref());
            let mut warn = false;
            if flash.is_none() {
                (flash, warn) = rollback_flash(msg);
                put(&mut base, "flash_warn", Value::from(warn));
            }
            let _ = warn;
            put(&mut base, "flash", flash.map(Value::from).unwrap_or(Value::from(())));
        }
        if !page.contains_key("llm_ok") {
            let ok = if matches!(name, "overview.html" | "recommendations.html" | "player.html") { self.llm_available() } else { false };
            put(&mut base, "llm_ok", Value::from(ok));
        }
        put(&mut base, "__fm_data_dir", Value::from(s.fm_data_dir.to_string_lossy().to_string()));
        put(&mut base, "__fm_today", Value::from(local_naive(&now).date().format("%Y-%m-%d").to_string()));
        page.clear();
        // later keys win (dict.update order)
        let mut dedup: Vec<(String, Value)> = Vec::new();
        for (k, v) in base {
            if let Some(slot) = dedup.iter_mut().find(|(dk, _)| *dk == k) {
                slot.1 = v;
            } else {
                dedup.push((k, v));
            }
        }
        let ctx_v = Value::from_iter(dedup);
        match self.env.get_template(name).and_then(|t| t.render(ctx_v)) {
            Ok(html) => Resp::Html(status, html),
            Err(e) => {
                let mut msg = format!("template {name}: {e}");
                let mut src = std::error::Error::source(&e);
                while let Some(s) = src {
                    msg += &format!("; {s}");
                    src = s.source();
                }
                tracing::error!("{msg}");
                Resp::Html(500, format!("<!doctype html><title>Error</title><pre>{}</pre>", escape_html(&msg)))
            }
        }
    }

    fn llm_available(&self) -> bool {
        match &self.cfg.llm {
            Some(f) => f(&self.settings()).is_some_and(|c| c.available()),
            None => fm_sources::llm::openrouter::LLMClient::from_settings(&self.settings()).available(),
        }
    }

    fn render(&self, req: &Req, name: &str, league: &str, status: u16, ctx: Ctx) -> Resp {
        self.render_page(&req.path, &req.query, name, league, status, ctx)
    }

    /// Login page for the auth layer.
    pub fn login_html(&self, error: Option<&str>, next: &str) -> String {
        let ctx = minijinja::context! { error => error, next => next };
        self.env.get_template("login.html").and_then(|t| t.render(ctx)).unwrap_or_else(|_| auth::default_login_page(error, next))
    }

    fn load_or_error(&self, req: &Req, league: &str) -> Result<Page, Resp> {
        match self.get_result(league) {
            Ok(p) => Ok(p),
            Err(e) if is_provider_error(&e) => Err(self.render(
                req,
                "setup.html",
                league,
                200,
                cx![("error", Value::from(e.to_string())), ("rows", to_mj(&setup_rows(league, &self.settings()), None)), ("title", Value::from("Setup needed"))],
            )),
            Err(e) => {
                tracing::error!("loading {league} failed: {e:#}");
                Err(self.render(req, "error.html", league, 500, cx![("error", Value::from(err_text(&e))), ("title", Value::from("Something went wrong"))]))
            }
        }
    }

    fn json_load(&self, league: &str) -> Result<Page, Resp> {
        match self.get_result(league) {
            Ok(p) => Ok(p),
            Err(e) if is_provider_error(&e) => {
                Err(Resp::Json(503, json!({"error": e.to_string(), "setup": setup_rows(league, &self.settings())})))
            }
            Err(e) => {
                tracing::error!("loading {league} failed: {e:#}");
                Err(Resp::Json(500, json!({"error": err_text(&e)})))
            }
        }
    }

    fn league_q(&self, req: &Req) -> Result<String, Resp> {
        match req.q("league") {
            None => Ok(self.cfg.default_league.clone()),
            Some(l) if LEAGUES.contains(&l) => Ok(l.to_string()),
            Some(l) => Err(league_error(l)),
        }
    }

    // ----------------------------------------------------------------------- dispatch

    fn handle(&self, req: Req) -> Resp {
        let get = req.method == Method::GET || req.method == Method::HEAD;
        let post = req.method == Method::POST;
        let path = req.path.clone();
        let r = match path.as_str() {
            "/healthz" if get => Ok(self.healthz()),
            "/" if get => self.home(&req),
            "/plan" if get => self.plan(&req),
            "/overview" if get => self.overview(&req),
            "/roster" if get => self.roster(&req),
            "/recommendations" if get => self.recommendations(&req),
            "/news" if get => self.news(&req),
            "/schedule" if get => self.schedule(&req),
            "/trade-eval" if get => self.trade_eval(&req),
            "/matchup" if get => self.matchup(&req),
            "/health" if get => self.health(&req),
            "/api/recs.json" if get => self.api_recs(&req),
            "/api/claims.json" if get => self.api_claims(&req),
            "/api/roster.json" if get => self.api_roster(&req),
            "/api/health.json" if get => Ok(self.api_health()),
            "/api/mode.json" if get => Ok(self.api_mode()),
            "/explain" if post => Ok(self.explain(&req)),
            "/refresh" if post => self.refresh(&req),
            "/moved" if post => Ok(self.moved(&req)),
            "/mode" if post => Ok(self.set_mode(&req)),
            "/params/rollback" if post => Ok(self.rollback_params(&req)),
            p if get && p.starts_with("/player/") => {
                let cid = p["/player/".len()..].to_string();
                self.player(&req, &cid)
            }
            "/healthz" | "/" | "/plan" | "/overview" | "/roster" | "/recommendations" | "/news" | "/schedule" | "/trade-eval"
            | "/matchup" | "/health" | "/api/recs.json" | "/api/claims.json" | "/api/roster.json" | "/api/health.json"
            | "/api/mode.json" | "/explain" | "/refresh" | "/moved" | "/mode" | "/params/rollback" => {
                Ok(detail(405, "Method Not Allowed".into()))
            }
            _ => Ok(detail(404, "Not Found".into())),
        };
        r.unwrap_or_else(|e| e)
    }

    fn healthz(&self) -> Resp {
        let cached: serde_json::Map<String, J> = LEAGUES.iter().map(|lg| (lg.to_string(), json!(self.cache.age(lg).is_some()))).collect();
        Resp::Json(200, json!({"status": "ok", "cached": cached}))
    }

    fn home(&self, req: &Req) -> Result<Resp, Resp> {
        if self.cfg.home == "plan" {
            self.plan(req)
        } else {
            self.overview(req)
        }
    }

    fn plan(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.load_or_error(req, &league)?;
        let res = page.res.clone();
        let pv = page.provider.clone();
        let out = catch_unwind(AssertUnwindSafe(|| views::plan_page(&res, pv.as_deref())));
        let mut pagev = match out {
            Ok(v) => v,
            Err(p) => {
                let msg = panic_text(p);
                tracing::error!("weekly plan for {league} failed: {msg}");
                return Ok(self.render(
                    req,
                    "error.html",
                    &league,
                    500,
                    cx![("res", ResObj::value(res)), ("title", Value::from("Plan unavailable")), ("error", Value::from(format!("Error: {msg}")))],
                ));
            }
        };
        if req.q("msg") == Some("moved") {
            let label: String = req.q("label").unwrap_or("").chars().take(120).collect();
            let since = req.q("since");
            let (text, warn, again) = fm_engine::recommend::plan_memory::moved_banner(
                &self.settings().fm_data_dir,
                &league,
                since,
                &res.ctx,
                &label,
                req.q("retry") == Some("1"),
            );
            if let J::Object(m) = &mut pagev {
                m.insert("flash".into(), json!(text));
                m.insert("flash_warn".into(), json!(warn));
                m.insert(
                    "moved_retry".into(),
                    if again { json!({"since": since.unwrap_or(""), "label": label}) } else { J::Null },
                );
            }
        }
        let mut ctx: Ctx = cx![("res", ResObj::value(res.clone())), ("title", Value::from("This week")), ("no_mode", Value::from(true))];
        spread(&mut ctx, &pagev, &res);
        Ok(self.render(req, "plan.html", &league, 200, ctx))
    }

    fn overview(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.load_or_error(req, &league)?;
        let res = page.res.clone();
        let ctx = &res.ctx;
        let team = my_team(ctx);
        let kind = normalize_kind(req.q("kind"));
        let shown = filter_recs(&res.recs, &kind);
        let mut alerts: Vec<(String, &Player)> = Vec::new();
        let mut prospects = 0;
        let mut news: Vec<(&Player, &J)> = Vec::new();
        if let Some(t) = team {
            for s in &t.slots {
                let Some(p) = &s.player else { continue };
                if ALERT_STATUSES.contains(&p.status.as_str()) {
                    alerts.push((s.slot.clone(), p));
                }
                if ctx.dynasty && views::is_prospect(p, views::player_age(p, ctx.as_of, res.dynasty.as_ref())) {
                    prospects += 1;
                }
                if let Some(items) = res.news_by_cid.get(&p.cid) {
                    news.extend(items.iter().map(|n| (p, n)));
                }
            }
        }
        let fa_pos = req.q("fa_pos").filter(|p| views::POSITION_FILTERS.contains(p));
        let fa_q: Option<String> = req.q("fa_q").map(|s| py_strip(s).chars().take(60).collect::<String>()).filter(|s| !s.is_empty());
        let fa_sort = req.q("fa_sort").filter(|s| views::FA_SORTS.iter().any(|(k, _)| k == s)).unwrap_or("vorp");
        let active = fa_pos.is_some() || fa_q.is_some() || fa_sort != "vorp";
        let fa_filter = json!({"pos": fa_pos, "q": fa_q, "sort": fa_sort, "active": active});
        fm_pycompat::order::sort_desc_by(&mut news, |a, b| news_key(a.1).cmp(&news_key(b.1)));
        news.truncate(4);
        let news_v: Vec<J> = news.iter().map(|(p, n)| json!([p, n])).collect();
        let shown_v: Vec<Value> = shown.iter().take(OVERVIEW_MOVES).map(|r| rec_value(&res, r)).collect();
        let pv = page.provider.clone();
        let team_key = team.map(|t| t.team_id.clone()).unwrap_or_default();
        let fa_key = format!("free_agents:{fa_pos:?}:{fa_q:?}:{fa_sort}");
        let alerts_v: Vec<J> = alerts.iter().map(|(s, p)| json!([s, p])).collect();
        let ctxv: Ctx = cx![
            ("res", ResObj::value(res.clone())),
            ("team", memo_mj(&res, &format!("team:{team_key}"), || serde_json::to_value(team).unwrap_or(J::Null))),
            ("kind", Value::from(kind.as_str())),
            ("moves", Value::from(shown_v)),
            ("shown_total", Value::from(shown.len())),
            ("counts", to_mj(&kind_counts(&res.recs), None)),
            ("summary", safe(overview_summary(&res.recs, &alerts, prospects))),
            ("alerts", to_mj(&J::Array(alerts_v), Some(&res))),
            ("news", to_mj(&J::Array(news_v), Some(&res))),
            ("total", Value::from(res.recs.len())),
            ("week", memo_mj(&res, &format!("week:{team_key}"), || views::week_panel(&res, team))),
            ("standings", memo_mj(&res, "standings", || J::Array((*views::standings(&res)).clone()))),
            ("free_agents", memo_mj(&res, &fa_key, || J::Array(views::free_agents(&res, 15, fa_pos, fa_q.as_deref(), fa_sort)))),
            ("fa_filter", to_mj(&fa_filter, None)),
            ("league_injuries", memo_mj(&res, "league_injuries", || J::Array(views::league_injuries(&res, 15)))),
            ("matchup", memo_mj(&res, "matchup_strip", || views::matchup_strip(&res, pv.as_deref()))),
            ("outlook", memo_mj(&res, "outlook", || (*views::outlook_section(&res, pv.as_deref())).clone())),
            ("title", Value::from("Overview")),
        ];
        Ok(self.render(req, "overview.html", &league, 200, ctxv))
    }

    fn roster(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.load_or_error(req, &league)?;
        let res = page.res.clone();
        let ctx = &res.ctx;
        let view = match req.q("view") {
            Some(v @ ("fantasy" | "stats")) => v,
            _ => "fantasy",
        };
        let chosen = req.q("team").filter(|t| !t.is_empty()).and_then(|t| ctx.teams.iter().find(|x| x.team_id == t)).or_else(|| my_team(ctx));
        let key = format!("roster:{}", chosen.map(|t| t.team_id.as_str()).unwrap_or(""));
        let bundle = memo_mj(&res, &key, || {
            let mut slots: Vec<&fm_core::models::RosterSlot> = chosen.map(|t| t.slots.iter().filter(|s| s.player.is_some()).collect()).unwrap_or_default();
            slots.sort_by(|a, b| {
                let ra = views::SLOT_ORDER.iter().position(|s| *s == a.slot).unwrap_or(99);
                let rb = views::SLOT_ORDER.iter().position(|s| *s == b.slot).unwrap_or(99);
                let na = &a.player.as_ref().map(|p| p.name.clone()).unwrap_or_default();
                let nb = &b.player.as_ref().map(|p| p.name.clone()).unwrap_or_default();
                ra.cmp(&rb).then_with(|| na.cmp(nb))
            });
            let pv_of = |cid: &str| res.values.get(cid);
            let has_proj = slots.iter().any(|s| s.player.as_ref().and_then(|p| pv_of(&p.cid)).is_some_and(|v| v.proj_week.is_some()));
            let vorps: Vec<f64> = slots.iter().filter_map(|s| s.player.as_ref().and_then(|p| pv_of(&p.cid)).map(|v| v.vorp)).collect();
            let scale = vorps.iter().fold(0.0_f64, |m, v| if v.abs() > m { v.abs() } else { m });
            let week = chosen.map(|t| views::week_panel(&res, Some(t))).unwrap_or(J::Null);
            let mut changes: HashMap<String, J> = HashMap::new();
            if let Some(rows) = week.get("rows").and_then(|r| r.as_array()) {
                for r in rows {
                    if let Some(c) = r.get("cid").and_then(|c| c.as_str()) {
                        changes.insert(c.to_string(), r.clone());
                    }
                }
            }
            let mut rows: Vec<J> = Vec::new();
            for s in &slots {
                let p = s.player.as_ref().expect("filtered");
                let mut row = views::player_row(&res, p, Some(&s.slot), None);
                let ch = changes.get(&p.cid);
                if let J::Object(m) = &mut row {
                    let vorp = m.get("vorp").and_then(|v| v.as_f64());
                    m.insert("starting".into(), json!(s.starting));
                    m.insert("bar".into(), vorp_bar(vorp, scale));
                    m.insert("change".into(), ch.and_then(|c| c.get("change")).cloned().unwrap_or(J::Null));
                    m.insert("change_gain".into(), ch.and_then(|c| c.get("change_gain")).cloned().unwrap_or(J::Null));
                }
                rows.push(row);
            }
            let is_ir = |r: &J| r.get("slot").and_then(|s| s.as_str()) == Some("IR");
            let starting = |r: &J| r.get("starting").and_then(|s| s.as_bool()).unwrap_or(false);
            let groups: Vec<J> = [
                ("Starting", rows.iter().filter(|r| starting(r) && !is_ir(r)).cloned().collect::<Vec<_>>()),
                ("Bench", rows.iter().filter(|r| !starting(r) && !is_ir(r)).cloned().collect()),
                ("Injured reserve", rows.iter().filter(|r| is_ir(r)).cloned().collect()),
            ]
            .into_iter()
            .filter(|(_, g)| !g.is_empty())
            .map(|(l, g)| json!([l, g]))
            .collect();
            let truthy = |r: &J, k: &str| r.get(k).is_some_and(|v| !(v.is_null() || v == &json!(false) || v == &json!(0) || v == &json!("")));
            let empty_starting: Vec<String> =
                chosen.map(|t| t.slots.iter().filter(|s| s.player.is_none() && s.starting).map(|s| s.slot.clone()).collect()).unwrap_or_default();
            let goalie = |r: &J| truthy(r, "goalie");
            json!({
                "team": chosen,
                "slots": slots,
                "empty_starting": empty_starting,
                "picks": views::draft_picks(&res, chosen),
                "groups": groups,
                "any_prospect": rows.iter().any(|r| truthy(r, "prospect")),
                "skaters": rows.iter().filter(|r| !goalie(r)).cloned().collect::<Vec<_>>(),
                "goalies": rows.iter().filter(|r| goalie(r)).cloned().collect::<Vec<_>>(),
                "week": week,
                "has_proj": has_proj,
                "has_dyn": res.dynasty.is_some(),
                "has_pct": rows.iter().any(|r| r.get("pct").is_some_and(|v| !v.is_null())),
            })
        });
        let mut ctxv: Ctx = cx![("res", ResObj::value(res.clone())), ("view", Value::from(view)), ("title", Value::from("Roster"))];
        spread_mj(&mut ctxv, &bundle);
        Ok(self.render(req, "roster.html", &league, 200, ctxv))
    }

    fn recommendations(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.load_or_error(req, &league)?;
        let res = page.res.clone();
        let kind = normalize_kind(req.q("kind"));
        let min_gain = req.q("min_gain").filter(|s| !s.is_empty());
        let floor: Option<f64> = min_gain.and_then(py_float);
        let pos = req.q("pos").filter(|p| views::POSITION_FILTERS.contains(p));
        let mut recs = filter_recs(&res.recs, &kind);
        if let Some(p) = pos {
            recs.retain(|r| views::involves_position(r, p));
        }
        if let Some(f) = floor {
            recs.retain(|r| views::gain_info(r, Some(&res)).get("value").and_then(|v| v.as_f64()).is_some_and(|g| g >= f));
        }
        let mut extra_pairs: Vec<(String, String)> = Vec::new();
        if floor.is_some() {
            if let Some(m) = min_gain {
                extra_pairs.push(("min_gain".into(), m.into()));
            }
        }
        if let Some(p) = pos {
            extra_pairs.push(("pos".into(), p.into()));
        }
        let extra = urlencode(&extra_pairs);
        let claims = if kind == "all" || kind == "waiver" {
            memo_mj(&res, "claims", || views::claims_section(&res, page.provider.as_deref()))
        } else {
            Value::from(())
        };
        let ctxv: Ctx = cx![
            ("res", ResObj::value(res.clone())),
            ("kind", Value::from(kind.as_str())),
            ("recs", Value::from(recs.iter().map(|r| rec_value(&res, r)).collect::<Vec<_>>())),
            ("counts", to_mj(&kind_counts(&res.recs), None)),
            ("total", Value::from(res.recs.len())),
            ("min_gain", floor.map(Value::from).unwrap_or(Value::from(()))),
            ("pos", pos.map(Value::from).unwrap_or(Value::from(()))),
            ("filter_qs", Value::from(if extra.is_empty() { String::new() } else { format!("&{extra}") })),
            ("filtered", Value::from(pos.is_some() || floor.is_some())),
            ("claims", claims),
            ("title", Value::from("Moves")),
        ];
        Ok(self.render(req, "recommendations.html", &league, 200, ctxv))
    }

    fn news(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.load_or_error(req, &league)?;
        let res = page.res.clone();
        let ctx = &res.ctx;
        let mine: std::collections::HashSet<&str> = my_team(ctx).map(|t| t.players_iter().map(|p| p.cid.as_str()).collect()).unwrap_or_default();
        let by_cid: HashMap<&str, &Player> = ctx.all_players().into_iter().map(|p| (p.cid.as_str(), p)).collect();
        let mut order: Vec<String> = Vec::new();
        let mut entries: HashMap<String, (J, Vec<&Player>, bool)> = HashMap::new();
        for (cid, items) in &res.news_by_cid {
            let Some(p) = by_cid.get(cid.as_str()) else { continue };
            for n in items {
                let ident = match n.get("id").and_then(|i| i.as_str()).filter(|s| !s.is_empty()) {
                    Some(id) => id.to_string(),
                    None => format!(
                        "{}|{}|{}",
                        n.get("source").and_then(|x| x.as_str()).unwrap_or(""),
                        n.get("headline").and_then(|x| x.as_str()).unwrap_or(""),
                        n.get("published").map(|x| x.to_string()).unwrap_or_default()
                    ),
                };
                let e = entries.entry(ident.clone()).or_insert_with(|| {
                    order.push(ident.clone());
                    (n.clone(), Vec::new(), false)
                });
                e.1.push(p);
                e.2 = e.2 || mine.contains(cid.as_str());
            }
        }
        let mut ordered: Vec<&(J, Vec<&Player>, bool)> = order.iter().map(|k| &entries[k]).collect();
        fm_pycompat::order::sort_desc_by(&mut ordered, |a, b| news_key(&a.0).cmp(&news_key(&b.0)));
        let ent = |e: &&(J, Vec<&Player>, bool)| json!({"item": e.0, "players": e.1, "mine": e.2});
        let mine_v: Vec<J> = ordered.iter().filter(|e| e.2).take(50).map(ent).collect();
        let other_v: Vec<J> = ordered.iter().filter(|e| !e.2).take(50).map(ent).collect();
        let ctxv: Ctx = cx![
            ("res", ResObj::value(res.clone())),
            ("mine", to_mj(&J::Array(mine_v), Some(&res))),
            ("other", to_mj(&J::Array(other_v), Some(&res))),
            ("title", Value::from("News")),
        ];
        Ok(self.render(req, "news.html", &league, 200, ctxv))
    }

    fn player(&self, req: &Req, cid: &str) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.load_or_error(req, &league)?;
        let res = page.res.clone();
        let ctx = &res.ctx;
        let Some(p) = ctx.all_players().into_iter().find(|x| x.cid == cid) else {
            return Ok(self.render(
                req,
                "error.html",
                &league,
                404,
                cx![
                    ("res", ResObj::value(res.clone())),
                    ("title", Value::from("Player not found")),
                    ("error", Value::from(format!("No player with id {} in this league.", py_str_repr(cid)))),
                ],
            ));
        };
        let (team, slot) = where_is(ctx, cid);
        let weights = (ctx.scoring.kind.as_str() == "points").then_some(&ctx.scoring.weights);
        let (cols, rows) = views::split_table(p, weights);
        let pv = res.values.get(cid);
        let rates = pv.map(|v| serde_json::to_value(&v.rates).unwrap_or(J::Null)).unwrap_or_else(|| json!({}));
        let recs: Vec<Value> = ranked(&res.recs)
            .into_iter()
            .filter(|r| r.add.iter().chain(&r.drop).chain(&r.subjects).any(|x| x.cid == cid))
            .map(|r| rec_value(&res, r))
            .collect();
        let (start, _, preseason) = views::week_start(ctx);
        let schedule: Vec<J> = views::team_games(ctx, p.team.as_deref(), start, 14).iter().map(|g| g.to_value()).collect();
        let news: Vec<J> = res.news_by_cid.get(cid).map(|v| v.iter().take(20).cloned().collect()).unwrap_or_default();
        let dynv = res.dynasty.as_ref().and_then(|d| d.get(cid));
        let log = views::game_log(&res, p);
        let chart = views::game_log_chart(p, &log, pv.filter(|_| weights.is_some()).map(|v| v.fpg));
        let card = req.q("card") == Some("1");
        let ctxv: Ctx = cx![
            ("game_log", to_mj(&J::Array(log), None)),
            ("gl_chart", chart.map(|c| templates::safe_charts(&c)).unwrap_or(Value::from(()))),
            ("card", Value::from(card)),
            ("res", ResObj::value(res.clone())),
            ("p", to_mj(&serde_json::to_value(p).unwrap_or(J::Null), Some(&res))),
            ("pv", templates::pv_value(&res, cid)),
            ("dyn", templates::dyn_value_of(&res, cid)),
            ("team", to_mj(&serde_json::to_value(team).unwrap_or(J::Null), Some(&res))),
            ("slot", slot.clone().map(Value::from).unwrap_or(Value::from(()))),
            ("cols", Value::from(cols)),
            ("rows", to_mj(&J::Array(rows), Some(&res))),
            ("rates", to_mj(&rates, None)),
            ("recs", Value::from(recs)),
            ("row", to_mj(&views::player_row(&res, p, slot.as_deref(), None), Some(&res))),
            ("schedule", to_mj(&J::Array(schedule), None)),
            ("schedule_start", Value::from(start.format("%Y-%m-%d").to_string())),
            ("preseason", Value::from(preseason)),
            ("history", to_mj(&J::Array(views::status_history(&res, cid)), None)),
            ("breakdown", to_mj(&views::dynasty_breakdown(dynv), None)),
            ("weights", weights.map(|w| to_mj(&serde_json::to_value(w).unwrap_or(J::Null), None)).unwrap_or(Value::from(()))),
            ("news", to_mj(&J::Array(news), None)),
            ("title", Value::from(p.name.as_str())),
        ];
        Ok(self.render(req, if card { "player_card.html" } else { "player.html" }, &league, 200, ctxv))
    }

    fn schedule(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.load_or_error(req, &league)?;
        let res = page.res.clone();
        let week = req.q("week");
        let pagev = memo_mj(&res, &format!("schedule:{week:?}"), || views::schedule_page(&res, week, page.provider.as_deref(), 10));
        let mut ctx: Ctx = cx![("res", ResObj::value(res.clone())), ("title", Value::from("Schedule"))];
        spread_mj(&mut ctx, &pagev);
        Ok(self.render(req, "schedule.html", &league, 200, ctx))
    }

    fn trade_eval(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.load_or_error(req, &league)?;
        let res = page.res.clone();
        let (give, get, gp, tp) = (req.all("give"), req.all("get"), req.all("give_pick"), req.all("get_pick"));
        let evaluate = req.q("switch").is_none_or(|s| s.is_empty());
        let team = req.q("team");
        let pv = page.provider.clone();
        let out = catch_unwind(AssertUnwindSafe(|| views::trade_eval_page(&res, team, &give, &get, &gp, &tp, pv.as_deref(), evaluate)));
        let pagev = match out {
            Ok(v) => v,
            Err(p) => {
                let msg = panic_text(p);
                tracing::error!("trade evaluator for {league} failed: {msg}");
                return Ok(self.render(
                    req,
                    "error.html",
                    &league,
                    500,
                    cx![("res", ResObj::value(res)), ("title", Value::from("Something went wrong")), ("error", Value::from(format!("Error: {msg}")))],
                ));
            }
        };
        let mut ctx: Ctx = cx![("res", ResObj::value(res.clone())), ("title", Value::from("Trade evaluator"))];
        spread(&mut ctx, &pagev, &res);
        Ok(self.render(req, "trade_eval.html", &league, 200, ctx))
    }

    fn matchup(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.load_or_error(req, &league)?;
        let res = page.res.clone();
        let pv = page.provider.clone();
        let out = catch_unwind(AssertUnwindSafe(|| views::matchup_page(&res, pv.as_deref())));
        let pagev = match out {
            Ok(Ok(v)) => v,
            other => {
                let msg = match other {
                    Ok(Err(m)) => m,
                    Err(p) => format!("Error: {}", panic_text(p)),
                    Ok(Ok(_)) => String::new(),
                };
                tracing::error!("matchup preview for {league} failed: {msg}");
                return Ok(self.render(
                    req,
                    "error.html",
                    &league,
                    500,
                    cx![("res", ResObj::value(res)), ("title", Value::from("Matchup unavailable")), ("error", Value::from(msg))],
                ));
            }
        };
        let mut ctx: Ctx = cx![("res", ResObj::value(res.clone())), ("title", Value::from("Matchup"))];
        spread(&mut ctx, &pagev, &res);
        Ok(self.render(req, "matchup.html", &league, 200, ctx))
    }

    fn health(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let forced = matches!(req.q("force").unwrap_or("").to_lowercase().as_str(), "1" | "on" | "true" | "yes");
        let s = self.settings();
        let now = self.now();
        let lnow = local_naive(&now);
        let report = match health_report(&s.fm_data_dir, Some(&league), lnow.date(), lnow) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("health report failed: {e:#}");
                return Ok(self.render(
                    req,
                    "error.html",
                    &league,
                    500,
                    cx![("error", Value::from(err_text(&e))), ("title", Value::from("Something went wrong"))],
                ));
            }
        };
        let proposal = if req.q("refit") == Some("dry-run") {
            match refit_dry_run(&s.fm_data_dir, lnow.date(), forced) {
                Ok(p) => p,
                Err(e) => json!({"error": err_text(&e)}),
            }
        } else {
            J::Null
        };
        let l = report.get("leagues").and_then(|ls| ls.get(&league)).cloned().unwrap_or(J::Null);
        let charts = crate::health_view::health_charts(l.get("series"), &league);
        let rows = crate::health_view::refit_rows(if proposal.is_null() { None } else { Some(&proposal) });
        let ctxv: Ctx = cx![
            ("report", to_mj(&report, None)),
            ("L", to_mj(&l, None)),
            ("no_mode", Value::from(true)),
            ("title", Value::from("Model health")),
            ("charts", templates::safe_charts(&charts)),
            ("hv", Value::from_object(HealthViewObj)),
            ("proposal", to_mj(&proposal, None)),
            ("proposal_rows", to_mj(&J::Array(rows), None)),
            ("forced", Value::from(forced)),
        ];
        Ok(self.render(req, "health.html", &league, 200, ctxv))
    }

    fn api_recs(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.json_load(&league)?;
        let res = &page.res;
        let recs = filter_recs(&res.recs, &normalize_kind(req.q("kind")));
        Ok(Resp::Json(200, json!({"meta": meta(res), "recommendations": recs})))
    }

    fn api_claims(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.json_load(&league)?;
        let res = &page.res;
        let cp = views::claim_plan(res, page.provider.as_deref());
        Ok(Resp::Json(200, json!({"meta": meta(res), "claims": cp.as_ref().as_ref().map(|c| serde_json::to_value(c).unwrap_or(J::Null))})))
    }

    fn api_roster(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        let page = self.json_load(&league)?;
        let res = &page.res;
        let ctx = &res.ctx;
        let chosen = req.q("team").filter(|t| !t.is_empty()).and_then(|t| ctx.teams.iter().find(|x| x.team_id == t)).or_else(|| my_team(ctx));
        let mut slots = Vec::new();
        for s in chosen.map(|t| t.slots.as_slice()).unwrap_or(&[]) {
            let Some(p) = &s.player else { continue };
            let mut pj = serde_json::to_value(p).unwrap_or(J::Null);
            if let J::Object(m) = &mut pj {
                m.shift_remove("lines");
            }
            let strip_player = |v: J| match v {
                J::Object(mut m) => {
                    m.shift_remove("player");
                    J::Object(m)
                }
                x => x,
            };
            let value = res.values.get(&p.cid).map(|v| strip_player(serde_json::to_value(v).unwrap_or(J::Null))).unwrap_or(J::Null);
            let dynasty =
                res.dynasty.as_ref().and_then(|d| d.get(&p.cid)).map(|v| strip_player(serde_json::to_value(v).unwrap_or(J::Null))).unwrap_or(J::Null);
            slots.push(json!({"slot": s.slot, "starting": s.starting, "player": pj, "value": value, "dynasty": dynasty}));
        }
        let team = chosen.map(|t| json!({"team_id": t.team_id, "name": t.name, "record": t.record})).unwrap_or(J::Null);
        Ok(Resp::Json(200, json!({"meta": meta(res), "team": team, "slots": slots})))
    }

    fn api_health(&self) -> Resp {
        let s = self.settings();
        let lnow = local_naive(&self.now());
        match health_report(&s.fm_data_dir, None, lnow.date(), lnow) {
            Ok(r) => Resp::Json(200, r),
            Err(e) => Resp::Json(500, json!({"error": err_text(&e)})),
        }
    }

    fn api_mode(&self) -> Resp {
        let s = self.settings();
        let (mode, source) = current_mode(&s);
        let mut computed = serde_json::Map::new();
        for lg in LEAGUES {
            let v = match self.cache.peek(lg) {
                None => J::Null,
                Some(p) => {
                    let r = &p.res;
                    json!({"mode": r.mode.clone().unwrap_or_else(|| r.ctx.dynasty_mode.as_str().to_string()),
                           "dynasty": r.ctx.dynasty, "stale": r.mode.as_ref().is_some_and(|m| *m != mode),
                           "compute_seconds": r.compute_seconds, "recalc_only": r.recalc_only})
                }
            };
            computed.insert(lg.to_string(), v);
        }
        Resp::Json(
            200,
            json!({"mode": mode, "source": source, "source_label": fm_core::prefs::mode_source_label(&source),
                   "modes": ["contend", "balanced", "rebuild"], "computed": computed}),
        )
    }

    fn form_league(&self, f: &HashMap<String, String>) -> Result<String, Resp> {
        let league = f.get("league").filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| self.cfg.default_league.clone());
        if !LEAGUES.contains(&league.as_str()) {
            return Err(detail(422, format!("league must be one of {}", LEAGUES.join(", "))));
        }
        Ok(league)
    }

    fn explain(&self, req: &Req) -> Resp {
        let f = req.fields();
        let league = match self.form_league(&f) {
            Ok(l) => l,
            Err(r) => return r,
        };
        let key = f.get("rec_key").map(|s| py_strip(s).to_lowercase()).unwrap_or_default();
        let target = safe_next(f.get("next").map(String::as_str), &league, &[]);
        let Ok(page) = self.get_result(&league) else { return Resp::Redirect(target) };
        let res = page.res.clone();
        let Some(rec) = res.recs.iter().find(|r| rec_key(&league, r) == key) else { return Resp::Redirect(target) };
        let s = self.settings();
        let client: Option<Box<dyn fm_sources::llm::SupportsComplete>> = match &self.cfg.llm {
            Some(f) => f(&s),
            None => Some(Box::new(fm_sources::llm::openrouter::LLMClient::from_settings(&s))),
        };
        let set = |v: J| {
            res.explanations.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), v);
        };
        let Some(client) = client.filter(|c| c.available()) else {
            set(json!({"error": "Explanations need an OpenRouter key (set OPENROUTER_API_KEY)."}));
            return Resp::Redirect(format!("{target}#rec-{key}"));
        };
        let mut one = vec![rec.clone()];
        one[0].narrative = None;
        let news: Map<String, Vec<fm_sources::news::NewsItem>> = res
            .news_by_cid
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().filter_map(|n| serde_json::from_value(n.clone()).ok()).collect()))
            .collect();
        let c: &dyn fm_sources::llm::SupportsComplete = client.as_ref();
        let r = catch_unwind(AssertUnwindSafe(|| {
            fm_sources::llm::openrouter::narrate(&mut one, Some(&res.ctx), fm_sources::llm::context::NewsInput::ByCid(&news), Some(c), 1)
        }));
        if let Err(p) = r {
            tracing::warn!("explain failed: {}", panic_text(p));
        }
        match one[0].narrative.clone().filter(|n| !n.is_empty()) {
            Some(text) => set(json!({"text": text, "model": c.model()})),
            None => set(json!({"error": EXPLAIN_FAILED})),
        }
        Resp::Redirect(format!("{target}#rec-{key}"))
    }

    fn clear_league_data(&self, league: &str) {
        let s = self.settings();
        if s.fm_offline {
            return;
        }
        let cache = match fm_providers::pipeline::open_cache(&s) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("could not clear the {league} league cache: {e}");
                return;
            }
        };
        if let Err(e) = fm_providers::registry::clear_league_cache(league, &s, Some(&cache)) {
            tracing::warn!("could not clear the {league} league cache: {e}");
            return;
        }
        if let Err(e) = fm_providers::registry::clear_status_cache(&cache) {
            tracing::warn!("could not clear the goalie / injury cache: {e}");
        }
    }

    fn drop_league(&self, league: Option<&str>) {
        self.cache.clear(league);
        self.loader.invalidate(league);
    }

    fn refresh(&self, req: &Req) -> Result<Resp, Resp> {
        let league = self.league_q(req)?;
        self.clear_settings();
        self.reload_params();
        self.clear_league_data(&league);
        self.drop_league(Some(&league));
        Ok(Resp::Redirect(safe_next(Some(req.q("next").unwrap_or("/")), &league, &[])))
    }

    fn moved(&self, req: &Req) -> Resp {
        let f = req.fields();
        let league = match self.form_league(&f) {
            Ok(l) => l,
            Err(r) => return r,
        };
        let mut since = f.get("since").cloned().unwrap_or_default();
        if since.is_empty() {
            since = fm_engine::recommend::plan_memory::latest_snapshot(&self.settings().fm_data_dir, &league)
                .and_then(|s| s.get("created").and_then(|c| c.as_str()).map(str::to_string))
                .unwrap_or_default();
        }
        self.clear_settings();
        self.clear_league_data(&league);
        self.drop_league(Some(&league));
        let mut extra: Vec<(&str, String)> = vec![("msg", "moved".into()), ("since", since.chars().take(40).collect())];
        if let Some(l) = f.get("label").filter(|l| !l.is_empty()) {
            extra.push(("label", l.chars().take(120).collect()));
        }
        if f.get("retry").map(String::as_str) == Some("1") {
            extra.push(("retry", "1".into()));
        }
        Resp::Redirect(safe_next(Some("/plan"), &league, &extra))
    }

    fn set_mode(&self, req: &Req) -> Resp {
        let f = req.fields();
        let Some(mode) = f.get("mode").and_then(|m| fm_core::prefs::normalize_mode(m)) else {
            return detail(422, "mode must be one of contend, balanced, rebuild".into());
        };
        let league = match self.form_league(&f) {
            Ok(l) => l,
            Err(r) => return r,
        };
        self.clear_settings();
        let s = self.settings();
        if let Err(e) = fm_core::prefs::set_pref(fm_core::prefs::DYNASTY_MODE_KEY, Some(json!(mode.as_str())), &s.fm_data_dir) {
            tracing::error!("saving the dynasty mode failed: {e}");
            return detail(500, format!("could not save the mode: {e}"));
        }
        self.cache.clear(Some(&league));
        tracing::info!("dynasty mode set to {} from the dashboard", mode.as_str());
        Resp::Redirect(safe_next(f.get("next").map(String::as_str), &league, &[("msg", format!("mode-{}", mode.as_str()))]))
    }

    fn rollback_params(&self, req: &Req) -> Resp {
        let f = req.fields();
        let league = match self.form_league(&f) {
            Ok(l) => l,
            Err(r) => return r,
        };
        let nxt = f.get("next").filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| format!("/health?league={league}"));
        let confirm = f.get("confirm").map(|s| py_strip(s).to_lowercase()).unwrap_or_default();
        if !["1", "on", "yes", "true"].contains(&confirm.as_str()) {
            return Resp::Redirect(safe_next(Some(&nxt), &league, &[("msg", "rollback-unconfirmed".into())]));
        }
        let version = f.get("version").map(|s| py_strip(s).to_string()).filter(|s| !s.is_empty());
        if let Some(v) = &version {
            if !is_version(v) {
                return Resp::Redirect(safe_next(Some(&nxt), &league, &[("msg", "rollback-unknown".into())]));
            }
        }
        let s = self.settings();
        let today = local_naive(&self.now()).date();
        let out = match params_rollback(&s.fm_data_dir, version.as_deref(), today) {
            Ok(o) => o,
            Err(e) if e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some()) => {
                tracing::error!("params rollback failed: {e:#}");
                return detail(500, format!("could not roll back: {e}"));
            }
            Err(e) => {
                let code = if e.to_string().contains("nothing to roll back") { "rollback-none" } else { "rollback-unknown" };
                return Resp::Redirect(safe_next(Some(&nxt), &league, &[("msg", code.into())]));
            }
        };
        let from = out.get("from").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let to = out.get("to").and_then(|x| x.as_str()).unwrap_or("").to_string();
        tracing::info!("params rolled back {from} -> {to} from the dashboard");
        self.reload_params();
        self.drop_league(None);
        Resp::Redirect(safe_next(Some(&nxt), &league, &[("msg", format!("rollback-ok-{from}-{to}"))]))
    }

    // ----------------------------------------------------------------------- background

    /// Warm both leagues (default first) and keep them fresh; returns immediately.
    pub fn start_background(self: &Arc<Self>) {
        let app = self.clone();
        std::thread::Builder::new()
            .name("fm-web-warm".into())
            .spawn(move || {
                let mut order: Vec<&str> = vec![app.cfg.default_league.as_str()];
                order.extend(LEAGUES.iter().copied().filter(|l| *l != app.cfg.default_league));
                for lg in order {
                    let t0 = Instant::now();
                    match app.get_result(lg) {
                        Ok(_) => tracing::info!("warmed {lg} in {:.1}s", t0.elapsed().as_secs_f64()),
                        Err(e) => tracing::warn!("warming {lg} failed: {e}"),
                    }
                }
                loop {
                    std::thread::sleep(Duration::from_secs(20));
                    app.refresh_due();
                }
            })
            .ok();
    }

    /// Reload every league whose result expires within [`REFRESH_MARGIN`] and that was requested
    /// within [`REFRESH_IDLE`].
    pub fn refresh_due(&self) {
        let ttl = self.cache.ttl;
        for lg in LEAGUES {
            let Some(age) = self.cache.age(lg) else { continue };
            let recent = self.cache.last_access(lg).is_some_and(|t| t.elapsed() < REFRESH_IDLE);
            if !recent || Duration::from_secs_f64(age) + REFRESH_MARGIN < ttl {
                continue;
            }
            let s = self.settings();
            let now = self.now();
            let t0 = Instant::now();
            match self.cache.refresh_ahead(lg, &s, &now) {
                Ok(()) => tracing::info!("background refresh of {lg} in {:.1}s", t0.elapsed().as_secs_f64()),
                Err(e) => tracing::warn!("background refresh of {lg} failed: {e}"),
            }
        }
    }

    // ----------------------------------------------------------------------- axum

    /// The routes without access control (tests, `export_site`).
    pub fn router(self: &Arc<Self>) -> Router {
        let r = Router::new();
        let r = match &self.cfg.static_dir {
            Some(dir) => r.nest_service("/static", tower_http::services::ServeDir::new(dir)),
            None => r.route("/static/{*path}", axum::routing::get(embedded_static)),
        };
        r.fallback(dispatch)
            .with_state(self.clone())
    }

    /// The routes behind `auth::install` with a login page rendered from login.html.
    pub fn router_with_auth(self: &Arc<Self>, cfg: AuthConfig) -> Router {
        let app = self.clone();
        let renderer: auth::LoginRenderer = Arc::new(move |error, next| app.login_html(error, next));
        let st = AuthState::with(cfg, renderer, auth::monotonic_clock());
        auth::install(self.router(), st)
    }
}

/// Python `float(s)` for the min-gain filter (None where Python raised ValueError).
fn py_float(s: &str) -> Option<f64> {
    let t = py_strip(s).replace('_', "");
    match t.to_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => return Some(f64::INFINITY),
        "-inf" | "-infinity" => return Some(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => return Some(f64::NAN),
        _ => {}
    }
    t.parse::<f64>().ok().filter(|_| !t.is_empty())
}

fn py_str_repr(s: &str) -> String {
    templates::py_repr(&Value::from(s))
}

fn naive_iso(d: NaiveDateTime) -> String {
    if d.and_utc().timestamp_subsec_micros() == 0 {
        d.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        d.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    }
}

/// Sort key of a news item: `published` as naive wall time (`datetime.min` when missing).
fn news_key(n: &J) -> NaiveDateTime {
    n.get("published")
        .and_then(|p| p.as_str())
        .and_then(|s| fm_pycompat::time::fromisoformat(&s.replace(' ', "T")))
        .map(|(d, _)| d)
        .unwrap_or(NaiveDateTime::MIN)
}

/// A view model converted to a template value once per LoadResult (the views are pure
/// functions of the result; Python memoised most of them on `res.memo` too).
fn memo_mj(res: &Arc<LoadResult>, key: &str, f: impl FnOnce() -> J) -> Value {
    let v: Arc<Value> = res.memo.get_or(&format!("w10:mj:{key}"), || to_mj(&f(), Some(res)));
    (*v).clone()
}

fn rec_value(res: &Arc<LoadResult>, r: &Recommendation) -> Value {
    let idx = res.recs.iter().position(|x| std::ptr::eq(x, r)).unwrap_or(0);
    templates::rec_value_at(res, idx)
}

/// `**page` for an already converted view model.
fn spread_mj(ctx: &mut Ctx, page: &Value) {
    if let Ok(keys) = page.try_iter() {
        for k in keys {
            if let (Some(name), Ok(v)) = (k.as_str(), page.get_item(&k)) {
                ctx.push((name.to_string(), v));
            }
        }
    }
}

/// `**page`: the view-model dict's keys as template variables.
fn spread(ctx: &mut Ctx, page: &J, res: &Arc<LoadResult>) {
    if let J::Object(m) = page {
        for (k, v) in m {
            ctx.push((k.clone(), to_mj(v, Some(res))));
        }
    }
}

/// `/static/<path>` from the files compiled into the binary.
async fn embedded_static(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    match crate::assets::static_file(&path) {
        Some(bytes) => Response::builder()
            .header(header::CONTENT_TYPE, crate::assets::content_type(&path))
            .body(Body::from(Bytes::from_static(bytes)))
            .unwrap(),
        None => Response::builder().status(StatusCode::NOT_FOUND).body(Body::from("Not Found")).unwrap(),
    }
}

async fn dispatch(State(app): State<Arc<App>>, method: Method, uri: Uri, body: Bytes) -> Response {
    let path = percent_decode(uri.path());
    let query = uri.query().map(parse_qsl).unwrap_or_default();
    let req = Req { method: method.clone(), path, query, body: body.to_vec() };
    let head = method == Method::HEAD;
    let out = tokio::task::spawn_blocking(move || app.handle(req)).await;
    let resp = match out {
        Ok(r) => r,
        Err(e) => Resp::Html(500, format!("Internal Server Error: {e}")),
    };
    let mut r = to_response(resp);
    if head {
        *r.body_mut() = Body::empty();
    }
    r
}

fn percent_decode(p: &str) -> String {
    let bytes: Vec<u8> = p.bytes().collect();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 0 && i + 2 <= bytes.len() - 1 {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// FastAPI's JSON body: `json.dumps(ensure_ascii=False, separators=(",", ":"))`.
pub fn json_body(v: &J) -> String {
    dumps(v, &DumpOpts { indent: None, ensure_ascii: false, sort_keys: false, separators: Some((",", ":")) })
}

fn to_response(r: Resp) -> Response {
    let (status, ctype, body, loc) = match r {
        Resp::Html(s, b) => (s, "text/html; charset=utf-8", b, None),
        Resp::Json(s, v) => (s, "application/json", json_body(&v), None),
        Resp::Redirect(l) => (303, "", String::new(), Some(l)),
    };
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if !ctype.is_empty() {
        resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    }
    if let Some(l) = loc {
        if let Ok(v) = HeaderValue::from_str(&l) {
            resp.headers_mut().insert(header::LOCATION, v);
        }
    }
    resp
}

// --------------------------------------------------------------------------- serving

/// `run(host, port, league)`: the dashboard with the real pipeline, access control from the
/// settings, "/" = the weekly plan, both leagues warmed at startup and kept fresh. Blocks.
pub fn run(host: &str, port: u16, league: &str, settings: &Settings) -> anyhow::Result<()> {
    let policy = auth::auth_from_settings(settings);
    let password_set = policy.password.as_ref().is_some_and(|p| !p.is_empty());
    auth::check_bind(host, password_set, policy.allow_insecure)
        .map_err(|e| anyhow::anyhow!(e.0))?;
    let mut cfg = AppConfig::new(settings.clone());
    cfg.default_league = league.to_string();
    cfg.home = "plan".into();
    cfg.auth_enabled = policy.enabled();
    cfg.background = true;
    cfg.reload_settings = Some(Arc::new(|| Settings::load(chrono::Local::now().date_naive())));
    fm_providers::pipeline::install_params(settings);
    let loader: Arc<dyn Loader> = Arc::new(PipelineLoader::new(cfg.cache_ttl));
    let app = App::new(loader, cfg);
    let router = app.router_with_auth(policy);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let addr = format!("{host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        println!(
            "Fantasy Manager dashboard: http://{host}:{port}/?league={league}{}  (Ctrl+C to stop)",
            if password_set { "  (password required)" } else { "" }
        );
        app.start_background();
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok(())
    })
}

/// `fm web-export`: crawl the dashboard (real pipeline, no auth, "/" = plan) into `out`.
pub fn export(out: &Path, limit: usize, settings: &Settings) -> anyhow::Result<crate::export::ExportResult> {
    let mut cfg = AppConfig::new(settings.clone());
    cfg.home = "plan".into();
    fm_providers::pipeline::install_params(settings);
    let loader: Arc<dyn Loader> = Arc::new(PipelineLoader::new(cfg.cache_ttl));
    let app = App::new(loader, cfg);
    let router = app.router();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(crate::export::export_site(router, out, &LEAGUES, limit))
}
