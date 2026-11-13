//! Rust port of `fantasy_manager/providers/rookie_enrich.py`: rookie evidence for the enrich
//! pipeline - league history (NHLe input) and news role signals. Owner: work package `W4-enrich`.
//!
//! [`enrich_rookies`] (step 6c of `providers.enrich`, best effort):
//! 1. League history for the context's unproven skaters with NHL ids: the pedigree step's parsed
//!    landings are reused; only the remaining players are fetched, at most `limit` (200)
//!    uncached pages per run.
//! 2. News role signals (`news_roles.extract_role_signals`) over the RotoWire / ESPN items plus
//!    the 30-day news store, mapped to league players with `news_match` (an item must name exactly
//!    one league player).
//!
//! Deviations: the registries `_HISTORY` / `_SIGNALS` are `ctx.enrich.rookie_history` /
//! `ctx.enrich.rookie_signals` (fm_core's `LeagueSeason` / `RoleSignal`, converted field by field
//! from the fm_sources types); the clock (`now`) is a parameter of the news functions.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use fm_core::models::{LeagueContext, Player};
use fm_core::snapshot::{LeagueSeason, RoleSignal};
use fm_core::unproven::is_unproven;
use fm_core::Map;
use fm_sources::llm::SupportsComplete;
use fm_sources::news::{self, AsOf, FetchText, NewsItem};
use fm_sources::news_match::match_news_to_players;
use fm_sources::news_roles::{self, extract_role_signals, LLM_BATCH};
use fm_sources::nhl::{self, parse_season_totals, NhlClient, NhlPlayerLanding};

use crate::enrich::{exc_name, rostered_cids};

pub const HISTORY_LIMIT: usize = 200;

/// fm_sources `LeagueSeason` -> the registry type.
pub fn to_core_season(s: &nhl::LeagueSeason) -> LeagueSeason {
    LeagueSeason {
        season: s.season,
        league: s.league.clone(),
        team: s.team.clone(),
        gp: s.gp,
        g: s.g,
        a: s.a,
        pts: s.pts,
        age_at_season: s.age_at_season,
    }
}

/// fm_sources `RoleSignal` -> the registry type.
pub fn to_core_signal(s: &news_roles::RoleSignal) -> RoleSignal {
    RoleSignal {
        player_name: s.player_name.clone(),
        cid: s.cid.clone(),
        kind: s.kind.clone(),
        direction: s.direction,
        confidence: s.confidence,
        quote: s.quote.clone(),
        published: s.published,
        source: s.source.clone(),
        item_id: s.item_id.clone(),
        origin: s.origin.clone(),
        ambiguous: s.ambiguous,
    }
}

// --------------------------------------------------------------------------- registry

pub fn register_history(ctx: &mut LeagueContext, nhl_id: i64, rows: &[nhl::LeagueSeason]) {
    Arc::make_mut(&mut ctx.enrich).rookie_history.insert(nhl_id, rows.iter().map(to_core_season).collect());
}

pub fn register_signals(ctx: &mut LeagueContext, cid: &str, signals: Vec<RoleSignal>) {
    Arc::make_mut(&mut ctx.enrich).rookie_signals.insert(cid.to_string(), signals);
}

// --------------------------------------------------------------------------- steps

/// Indices (into `ctx.all_players()`) of the unproven skaters with NHL ids, rostered first, then
/// by ownership.
pub fn targets(ctx: &LeagueContext) -> Vec<usize> {
    let rostered = rostered_cids(ctx);
    let players = ctx.all_players();
    let mut todo: Vec<usize> =
        (0..players.len()).filter(|&i| players[i].nhl_id().is_some() && !players[i].is_goalie() && is_unproven(players[i])).collect();
    todo.sort_by(|&a, &b| {
        let ra = !rostered.contains(&players[a].cid);
        let rb = !rostered.contains(&players[b].cid);
        ra.cmp(&rb).then(fm_pycompat::order::cmp_f64(-players[a].pct_owned.unwrap_or(0.0), -players[b].pct_owned.unwrap_or(0.0)))
    });
    todo
}

/// `load_histories` counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryCounts {
    pub targets: i64,
    pub reused: i64,
    pub fetched: i64,
    pub cached: i64,
    pub failed: i64,
    pub deferred: i64,
}

/// Register league history for the unproven skaters (see the module docs).
pub fn load_histories(
    ctx: &mut LeagueContext,
    client: Option<&NhlClient>,
    landings: Option<&HashMap<i64, NhlPlayerLanding>>,
    limit: usize,
    is_cached: Option<&dyn Fn(i64) -> bool>,
    fetch_missing: bool,
) -> HistoryCounts {
    let mut res = HistoryCounts::default();
    let ids: Vec<i64> = {
        let players = ctx.all_players();
        targets(ctx).into_iter().map(|i| players[i].nhl_id().expect("targets carry an NHL id")).collect()
    };
    for nid in ids {
        res.targets += 1;
        if let Some(landing) = landings.and_then(|l| l.get(&nid)) {
            let rows = parse_season_totals(&landing.season_totals, landing.birth_date, 2);
            register_history(ctx, nid, &rows);
            res.reused += 1;
            continue;
        }
        if ctx.enrich.rookie_history.contains_key(&nid) {
            res.cached += 1;
            continue;
        }
        let Some(client) = client.filter(|_| fetch_missing) else { continue };
        let cached = is_cached.is_some_and(|f| f(nid));
        if !cached {
            if res.fetched >= limit as i64 {
                res.deferred += 1;
                continue;
            }
            res.fetched += 1;
        } else {
            res.cached += 1;
        }
        match client.player_history(nid, None) {
            Ok(rows) => register_history(ctx, nid, &rows),
            Err(_) => res.failed += 1,
        }
    }
    res
}

/// Map signals to league players (cid) through their news items; returns players with signals.
pub fn attach_signals(ctx: &mut LeagueContext, news: &[NewsItem], signals: &[news_roles::RoleSignal]) -> usize {
    let by_cid = match_news_to_players(news, &ctx.all_players());
    let mut item_owners: HashMap<String, Vec<String>> = HashMap::new();
    for (cid, items) in &by_cid {
        for it in items {
            item_owners.entry(news_roles::item_key(it)).or_default().push(cid.clone());
        }
    }
    let mut out: Map<String, Vec<RoleSignal>> = Map::new();
    for s in signals {
        let owners = item_owners.get(s.item_id.as_deref().unwrap_or("")).map(Vec::as_slice).unwrap_or(&[]);
        if owners.len() != 1 {
            // unnamed or naming several league players: skip
            continue;
        }
        let mut sig = to_core_signal(s);
        sig.cid = Some(owners[0].clone());
        out.entry(owners[0].clone()).or_default().push(sig);
    }
    let n = out.len();
    for (cid, sigs) in out {
        register_signals(ctx, &cid, sigs);
    }
    n
}

/// RotoWire + ESPN items through `fetch_text` plus the 30-day news store (a failing source
/// becomes a warning).
pub fn fetch_news(
    fetch_text: &FetchText,
    warnings: Option<&mut Vec<String>>,
    store_dir: Option<&Path>,
    as_of: Option<AsOf>,
    now: DateTime<Utc>,
) -> Vec<NewsItem> {
    let (items, failed) = news::fetch_news(Some(fetch_text), store_dir, as_of, now);
    if let Some(w) = warnings {
        for (label, e) in &failed {
            w.push(format!("Rookie role signals: {label} unavailable ({})", exc_name(e)));
        }
    }
    items
}

/// `enrich_rookies` keyword arguments.
pub struct RookieOptions<'a> {
    pub landings: Option<&'a HashMap<i64, NhlPlayerLanding>>,
    pub news: Option<Vec<NewsItem>>,
    pub fetch_text: Option<&'a FetchText<'a>>,
    pub llm: Option<&'a dyn SupportsComplete>,
    /// most uncached news items sent to `llm` per call ([`LLM_BATCH`], or
    /// `news_roles::LLM_LOCAL_BATCH` for a local server)
    pub llm_batch: usize,
    pub store_dir: Option<&'a Path>,
    pub limit: usize,
    pub is_cached: Option<&'a dyn Fn(i64) -> bool>,
    pub fetch_missing: bool,
    pub now: DateTime<Utc>,
}

impl Default for RookieOptions<'_> {
    fn default() -> Self {
        RookieOptions {
            landings: None,
            news: None,
            fetch_text: None,
            llm: None,
            llm_batch: LLM_BATCH,
            store_dir: None,
            limit: HISTORY_LIMIT,
            is_cached: None,
            fetch_missing: true,
            now: Utc::now(),
        }
    }
}

/// `enrich_rookies` counts (Python's result dict).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RookieResult {
    pub history: HistoryCounts,
    pub signal_players: i64,
    pub news: i64,
    pub signals: i64,
    pub llm: i64,
}

/// Steps 1-2 of the module docs; adds one source note. Returns counts.
pub fn enrich_rookies(ctx: &mut LeagueContext, client: Option<&NhlClient>, opts: RookieOptions<'_>) -> anyhow::Result<RookieResult> {
    let hist = load_histories(ctx, client, opts.landings, opts.limit, opts.is_cached, opts.fetch_missing);
    let as_of = Some(AsOf::Date(ctx.as_of));
    let news: Vec<NewsItem> = match (&opts.news, opts.fetch_text) {
        (None, Some(ft)) => fetch_news(ft, Some(&mut ctx.warnings), opts.store_dir, as_of, opts.now),
        _ => news::store_news(opts.store_dir, opts.news.as_deref().unwrap_or(&[]), as_of, opts.now),
    };
    let mut res = RookieResult { history: hist, ..Default::default() };
    let mut signals: Vec<news_roles::RoleSignal> = Vec::new();
    if !news.is_empty() {
        signals = extract_role_signals(&news, opts.llm, opts.store_dir, opts.llm_batch, opts.now);
        res.signal_players = attach_signals(ctx, &news, &signals) as i64;
    }
    res.news = news.len() as i64;
    res.signals = signals.len() as i64;
    res.llm = signals.iter().filter(|s| s.origin == "llm").count() as i64;
    let with_hist = {
        let players = ctx.all_players();
        targets(ctx).into_iter().filter(|&i| !ctx.enrich.history_for(players[i]).is_empty()).count()
    };
    let h = &res.history;
    let deferred = if h.deferred > 0 { format!(", {} deferred (limit {})", h.deferred, opts.limit) } else { String::new() };
    let failed = if h.failed > 0 { format!(", {} failed", h.failed) } else { String::new() };
    let llm_note = if res.llm > 0 { format!(", {} from the LLM pass", res.llm) } else { String::new() };
    ctx.source_notes.push(format!(
        "Rookie evidence: league history for {with_hist}/{} unproven skaters (NHL player pages, {} reused from the pedigree step{failed}{deferred}); {} news role signals from {} items for {} league players{llm_note} (approximate NHLe factors)",
        h.targets, h.reused, res.signals, res.news, res.signal_players
    ));
    Ok(res)
}

/// `rookie_enrich.history_for(p)` on a context.
pub fn history_for<'a>(ctx: &'a LeagueContext, p: &Player) -> &'a [LeagueSeason] {
    ctx.enrich.history_for(p)
}

/// `rookie_enrich.signals_for(p)` on a context.
pub fn signals_for<'a>(ctx: &'a LeagueContext, p: &Player) -> &'a [RoleSignal] {
    ctx.enrich.signals_for(p)
}
