//! The load pipeline every command and the web app run: provider.load -> dynasty mode -> NHL
//! enrichment -> valuation -> dynasty values. Rust port of `cli.py` (`_cache`, `_load_context`,
//! `_resolve_mode`, `_valuate`, `_dynasty`, `Loaded`, `_load_all`), `cli_backtest.py`
//! (`_load_league_full`, without the optional recommendations) and `web/app.py` (`BaseLoad`,
//! `load_base`, `_news`, `_dynasty`). Owner: work package `W5-provider-pipeline`.
//!
//! Deviations (API shape only, same results):
//! * Python's user-facing exits (`_fail`, `typer.Exit(2)`) become [`LoadError`]: the CLI prints
//!   `Unavailable` in yellow and exits 2, `Failed` as `Error: ...` with exit 1. Any other error
//!   propagates unchanged (Python let those raise).
//! * Settings and the clock are parameters ([`LoadOptions::clock`]; FM_NOW is the binary's job).
//! * The valuation params (a process-global, loaded lazily from `get_settings().fm_data_dir` in
//!   Python) are installed from `Settings.fm_data_dir` by [`install_params`] (default in
//!   [`LoadOptions`]); [`ParamsSource::Keep`] leaves whatever is installed (tests, snapshots).
//! * `enrich_context` cannot raise in Rust (each step is best effort); a panic inside it is
//!   caught and reported as Python's "NHL enrichment failed: ..." warning. Same for dynasty.
//! * [`load_all_many`] loads several leagues at once: provider loads (league APIs) and
//!   valuation run concurrently; enrichment runs league by league in the given order, because
//!   leagues share the crosswalk / news / line stores and the second league reuses the first
//!   one's cached NHL responses, exactly as the sequential Python loop does.
//! * [`from_snapshot`] (the hidden `--from-snapshot` flag): snapshot context + tape-backed
//!   [`SnapshotProvider`], params from the snapshot, pinned clock.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime};
use fm_core::config::Settings;
use fm_core::models::{DynastyMode, LeagueContext, ScoringKind};
use fm_core::prefs::dynasty_mode_info;
use fm_core::provider::LeagueProvider;
use fm_core::snapshot::Snapshot;
use fm_core::{DynastyValue, Map, PlayerValue, ProviderError};
use fm_engine::scoring::from_config;
use fm_engine::valuation::dynasty::apply_dynasty;
use fm_engine::valuation::params;
use fm_engine::valuation::schedule::ScheduleMemo;
use fm_engine::valuation::valuate::fit_and_valuate;
use fm_http::{CacheMiss, HttpCache, Tape};
use fm_sources::news::NewsItem;

use crate::enrich::{enrich_context, make_fetch_text, Clock, EnrichOptions};
use crate::registry::get_provider_at;
use crate::snapshot::{snapshot_file, tape_next_to, SnapshotProvider};

// --------------------------------------------------------------------------- errors / options

/// A user-facing load failure (Python `_fail` / `typer.Exit(2)` in `_load_context`).
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// `ProviderUnavailable`: printed in yellow, exit code 2.
    #[error("{0}")]
    Unavailable(String),
    /// `ProviderError` / offline cache miss: `Error: <msg>`, exit code 1.
    #[error("{0}")]
    Failed(String),
}

/// Where the valuation params come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ParamsSource {
    /// packaged params + the harness override under `<fm_data_dir>/harness/params`
    #[default]
    DataDir,
    /// leave the installed params alone (already installed by the caller)
    Keep,
}

/// `_load_all(league, deep, cache, mode)` keyword arguments plus the Rust-only clock.
#[derive(Clone, Default)]
pub struct LoadOptions {
    pub deep: bool,
    /// `--mode`: dynasty mode for this run only (never saved)
    pub mode: Option<DynastyMode>,
    /// reuse an open cache (None: open `<fm_data_dir>/cache.db`)
    pub cache: Option<Arc<HttpCache>>,
    /// pinned clock (None: the system clock); also pins the providers' `date.today()`
    pub clock: Option<Clock>,
    pub params: ParamsSource,
}

impl LoadOptions {
    fn clock(&self) -> Clock {
        self.clock.unwrap_or_else(Clock::system)
    }
}

/// `cli.Loaded`: everything a command needs.
pub struct Loaded {
    pub lc: LeagueContext,
    pub values: Map<String, PlayerValue>,
    /// `dyn`: dynasty values (None outside dynasty leagues or when they failed)
    pub dyn_values: Option<Map<String, DynastyValue>>,
    pub provider: Box<dyn LeagueProvider>,
    pub cache: Arc<HttpCache>,
    pub errors: Vec<String>,
    /// the clock the load ran with (the snapshot's for [`from_snapshot`])
    pub clock: Clock,
}

// --------------------------------------------------------------------------- pieces

/// `_cache()`: the HTTP cache over `<fm_data_dir>/cache.db` (offline per FM_OFFLINE).
pub fn open_cache(settings: &Settings) -> Result<Arc<HttpCache>> {
    Ok(Arc::new(HttpCache::open(&settings.fm_data_dir, settings.fm_offline)?))
}

/// Install the valuation params the way Python's lazy `load_params()` builds them, from
/// `settings.fm_data_dir` (not the env fallback of `params::params_dir(None)`).
pub fn install_params(settings: &Settings) -> Arc<params::Params> {
    let packaged = params::load_packaged(None);
    let active = if params::override_enabled() {
        params::read_active(Some(&params::params_dir(Some(&settings.fm_data_dir))))
    } else {
        None
    };
    let merged = match &active {
        Some(a) => params::deep_merge(&packaged, &a["params"]),
        None => packaged,
    };
    params::install_with_active(merged, active)
}

/// `_resolve_mode(lc, override)`: `--mode` > prefs.json > FANTRAX_MODE > default, recording the
/// source on the context.
pub fn resolve_mode(lc: &mut LeagueContext, settings: &Settings, mode: Option<DynastyMode>) {
    if let Some(m) = mode {
        lc.dynasty_mode = m;
        lc.dynasty_mode_source = Some("option".into());
        return;
    }
    // Python catches a broken prefs file here; the Rust reader never fails (bad prefs = absent)
    let (m, source) = dynasty_mode_info(settings);
    lc.dynasty_mode = m;
    lc.dynasty_mode_source = Some(source);
}

/// `_mode_line(lc)`: "Dynasty mode: contend (from dashboard/prefs)".
pub fn mode_line(lc: &LeagueContext) -> String {
    let src = match lc.dynasty_mode_source.as_deref() {
        Some(s) if !s.is_empty() => format!(" (from {})", fm_core::prefs::mode_source_label(s)),
        _ => String::new(),
    };
    format!("Dynasty mode: {}{src}", lc.dynasty_mode.as_str())
}

fn find_in_chain<T: std::error::Error + Send + Sync + 'static>(e: &anyhow::Error) -> Option<&T> {
    e.downcast_ref::<T>().or_else(|| e.chain().find_map(|c| c.downcast_ref::<T>()))
}

/// Map provider / cache errors to Python's user-facing messages (`_load_context`).
fn user_facing(e: anyhow::Error) -> anyhow::Error {
    if let Some(pe) = find_in_chain::<ProviderError>(&e) {
        return match pe {
            ProviderError::Unavailable(m) => LoadError::Unavailable(m.clone()).into(),
            other => LoadError::Failed(other.to_string()).into(),
        };
    }
    if let Some(cm) = find_in_chain::<CacheMiss>(&e) {
        return LoadError::Failed(format!("offline mode (FM_OFFLINE=1) and data not cached: {cm}")).into();
    }
    e
}

/// `_load_context(league, cache, mode)`: provider.load() with user-facing errors
/// ([`LoadError`]) and the effective dynasty mode applied. Returns (ctx, provider).
pub fn load_context(
    league: &str,
    settings: &Settings,
    cache: &Arc<HttpCache>,
    mode: Option<DynastyMode>,
    clock: Option<Clock>,
) -> Result<(LeagueContext, Box<dyn LeagueProvider>)> {
    let provider = get_provider_at(league, settings, cache.clone(), clock.map(|c| c.today)).map_err(user_facing)?;
    load_context_with(provider, settings, mode)
}

/// [`load_context`] over an already built provider (tests, tape replay).
pub fn load_context_with(
    mut provider: Box<dyn LeagueProvider>,
    settings: &Settings,
    mode: Option<DynastyMode>,
) -> Result<(LeagueContext, Box<dyn LeagueProvider>)> {
    let mut lc = provider.load().map_err(user_facing)?;
    lc.warnings.extend(provider.warnings().iter().cloned());
    resolve_mode(&mut lc, settings, mode);
    Ok((lc, provider))
}

fn panic_text(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

/// `enrich_context(lc, settings, cache, deep=deep)` inside Python's best-effort guard; warnings
/// go to `warnings` (`lc.warnings` for the CLI and web, a separate list for the backtest).
fn enrich_guarded(lc: &mut LeagueContext, settings: &Settings, cache: &Arc<HttpCache>, deep: bool, clock: Clock) -> Option<String> {
    let r = catch_unwind(AssertUnwindSafe(|| {
        enrich_context(lc, settings, Some(cache.clone()), deep, EnrichOptions { clock, ..Default::default() })
    }));
    r.err().map(|p| format!("NHL enrichment failed: {}", panic_text(p)))
}

/// Valuation (categories / roto fitted on the rostered pool first), with odds from the data dir.
fn valuate_only(lc: &mut LeagueContext, settings: &Settings) -> Map<String, PlayerValue> {
    let mut scoring = from_config(&lc.scoring);
    fit_and_valuate(lc, &mut scoring, &ScheduleMemo::new(Some(settings.fm_data_dir.clone())))
}

/// `_valuate(lc)`: value every player. Non-points systems get a source note explaining that
/// FPG/VORP are per-game z-score sums.
pub fn valuate(lc: &mut LeagueContext, settings: &Settings) -> Map<String, PlayerValue> {
    if lc.scoring.kind != ScoringKind::Points {
        let cats = if lc.scoring.categories.is_empty() { "none configured".to_string() } else { lc.scoring.categories.join(", ") };
        lc.source_notes.push(format!(
            "{} league: FPG/VORP columns are per-game z-score value summed over categories ({cats}), not fantasy points",
            lc.scoring.kind.as_str()
        ));
    }
    valuate_only(lc, settings)
}

/// `_dynasty(lc, values, provider)` (same as web `_dynasty`): None outside dynasty leagues;
/// the provider's league-reported ages fill in missing birth dates; a failure is a warning.
pub fn dynasty(
    lc: &mut LeagueContext,
    values: &Map<String, PlayerValue>,
    provider: Option<&dyn LeagueProvider>,
) -> Option<Map<String, DynastyValue>> {
    if !lc.dynasty {
        return None;
    }
    let ages = provider.map(|p| p.ages()).filter(|a| !a.is_empty());
    let ctx: &LeagueContext = lc;
    match catch_unwind(AssertUnwindSafe(|| apply_dynasty(values, ctx, ages, None))) {
        Ok(d) => Some(d),
        Err(p) => {
            lc.warnings.push(format!("Dynasty valuation failed: {}", panic_text(p)));
            None
        }
    }
}

// --------------------------------------------------------------------------- _load_all

fn prepare(settings: &Settings, opts: &LoadOptions) -> Result<Arc<HttpCache>> {
    if opts.params == ParamsSource::DataDir {
        install_params(settings);
    }
    match &opts.cache {
        Some(c) => Ok(c.clone()),
        None => open_cache(settings),
    }
}

fn finish_load(
    mut lc: LeagueContext,
    provider: Box<dyn LeagueProvider>,
    settings: &Settings,
    cache: Arc<HttpCache>,
    clock: Clock,
) -> Loaded {
    let values = valuate(&mut lc, settings);
    let dyn_values = dynasty(&mut lc, &values, Some(provider.as_ref()));
    Loaded { lc, values, dyn_values, provider, cache, errors: Vec::new(), clock }
}

/// `_load_all(league, deep, cache, mode)`: provider.load -> enrich (NHL ids, schedule, injuries,
/// stats) -> valuate -> dynasty. `opts.mode` overrides the dynasty mode for this run only.
pub fn load_all(league: &str, settings: &Settings, opts: &LoadOptions) -> Result<Loaded> {
    let cache = prepare(settings, opts)?;
    let provider = get_provider_at(league, settings, cache.clone(), opts.clock.map(|c| c.today)).map_err(user_facing)?;
    run_load(provider, settings, cache, opts)
}

/// [`load_all`] over an already built provider (tests, tape replay); `opts.cache` (or the data
/// dir's cache) serves the enrichment requests.
pub fn load_all_with(provider: Box<dyn LeagueProvider>, settings: &Settings, opts: &LoadOptions) -> Result<Loaded> {
    let cache = prepare(settings, opts)?;
    run_load(provider, settings, cache, opts)
}

fn run_load(provider: Box<dyn LeagueProvider>, settings: &Settings, cache: Arc<HttpCache>, opts: &LoadOptions) -> Result<Loaded> {
    let t0 = Instant::now();
    let clock = opts.clock();
    let (mut lc, provider) = load_context_with(provider, settings, opts.mode)?;
    let t_load = t0.elapsed();
    if let Some(w) = enrich_guarded(&mut lc, settings, &cache, opts.deep, clock) {
        lc.warnings.push(w);
    }
    let t_enrich = t0.elapsed();
    let out = finish_load(lc, provider, settings, cache, clock);
    tracing::debug!(league = out.lc.provider.as_str(), load = ?t_load, enrich = ?(t_enrich - t_load), valuate = ?(t0.elapsed() - t_enrich), "load_all");
    Ok(out)
}

/// `_load(league, deep, cache, mode)`: (ctx, values, dynasty values).
pub fn load(
    league: &str,
    settings: &Settings,
    opts: &LoadOptions,
) -> Result<(LeagueContext, Map<String, PlayerValue>, Option<Map<String, DynastyValue>>)> {
    let d = load_all(league, settings, opts)?;
    Ok((d.lc, d.values, d.dyn_values))
}

/// `_load_all` for several leagues at once (results in `leagues` order, one per league). The
/// league API loads run concurrently, then enrichment league by league in order (shared stores,
/// shared NHL cache), then valuation concurrently. Same results as calling [`load_all`] for each
/// league in turn with one shared cache.
pub fn load_all_many(leagues: &[&str], settings: &Settings, opts: &LoadOptions) -> Vec<Result<Loaded>> {
    let cache = match prepare(settings, opts) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{e:#}");
            return leagues.iter().map(|_| Err(anyhow::anyhow!("{msg}"))).collect();
        }
    };
    let clock = opts.clock();
    let loaded: Vec<Result<(LeagueContext, Box<dyn LeagueProvider>)>> = std::thread::scope(|s| {
        let hs: Vec<_> = leagues
            .iter()
            .map(|lg| {
                let cache = &cache;
                s.spawn(move || load_context(lg, settings, cache, opts.mode, opts.clock))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap_or_else(|p| Err(anyhow::anyhow!(panic_text(p))))).collect()
    });
    let enriched: Vec<Result<(LeagueContext, Box<dyn LeagueProvider>)>> = loaded
        .into_iter()
        .map(|r| {
            r.map(|(mut lc, p)| {
                if let Some(w) = enrich_guarded(&mut lc, settings, &cache, opts.deep, clock) {
                    lc.warnings.push(w);
                }
                (lc, p)
            })
        })
        .collect();
    std::thread::scope(|s| {
        let hs: Vec<_> = enriched
            .into_iter()
            .map(|r| {
                let cache = cache.clone();
                s.spawn(move || r.map(|(lc, p)| finish_load(lc, p, settings, cache, clock)))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap_or_else(|p| Err(anyhow::anyhow!(panic_text(p))))).collect()
    })
}

/// `_load_schedule_ctx(league, need_values=False)`: provider.load -> NHL enrichment only.
pub fn load_schedule_ctx(league: &str, settings: &Settings, opts: &LoadOptions) -> Result<(LeagueContext, Box<dyn LeagueProvider>, Arc<HttpCache>)> {
    let cache = match &opts.cache {
        Some(c) => c.clone(),
        None => open_cache(settings)?,
    };
    let (mut lc, provider) = load_context(league, settings, &cache, opts.mode, opts.clock)?;
    if let Some(w) = enrich_guarded(&mut lc, settings, &cache, false, opts.clock()) {
        lc.warnings.push(w);
    }
    Ok((lc, provider, cache))
}

// --------------------------------------------------------------------------- backtest

/// `cli_backtest._load_league_full` without the optional recommendations (the caller runs
/// `advise` on top): provider.load -> enrich -> fitted valuation. Provider and enrichment
/// warnings go to `warnings`, not the context; no dynasty-mode resolution (as in Python).
pub struct FullLoad {
    pub ctx: LeagueContext,
    pub values: Map<String, PlayerValue>,
    pub warnings: Vec<String>,
    pub provider: Box<dyn LeagueProvider>,
}

pub fn load_league_full(league: &str, settings: &Settings, cache: &Arc<HttpCache>, clock: Option<Clock>) -> Result<FullLoad> {
    let mut provider = get_provider_at(league, settings, cache.clone(), clock.map(|c| c.today))?;
    let mut ctx = provider.load()?;
    let mut warnings: Vec<String> = provider.warnings().to_vec();
    if let Some(w) = enrich_guarded(&mut ctx, settings, cache, false, clock.unwrap_or_else(Clock::system)) {
        warnings.push(w);
    }
    let values = valuate_only(&mut ctx, settings);
    Ok(FullLoad { ctx, values, warnings, provider })
}

/// The `with_recs` dynasty step of `_load_league_full`: failures become
/// "dynasty valuation failed: ..." in `warnings`.
pub fn league_full_dynasty(full: &mut FullLoad) -> Option<Map<String, DynastyValue>> {
    if !full.ctx.dynasty {
        return None;
    }
    let ages = Some(full.provider.ages()).filter(|a| !a.is_empty());
    match catch_unwind(AssertUnwindSafe(|| apply_dynasty(&full.values, &full.ctx, ages, None))) {
        Ok(d) => Some(d),
        Err(p) => {
            full.warnings.push(format!("dynasty valuation failed: {}", panic_text(p)));
            None
        }
    }
}

// --------------------------------------------------------------------------- web

/// `web.app.BaseLoad`: the mode-independent (expensive) half of a dashboard load.
pub struct BaseLoad {
    pub ctx: LeagueContext,
    pub values: Map<String, PlayerValue>,
    pub provider: Box<dyn LeagueProvider>,
    pub news_by_cid: Map<String, Vec<NewsItem>>,
    /// naive local `datetime.now()`
    pub loaded_at: NaiveDateTime,
    pub seconds: f64,
    pub cache: Arc<HttpCache>,
}

/// `web.app._news(ctx, cache)`: RotoWire + ESPN news matched to the league's players.
pub fn news(ctx: &LeagueContext, cache: &Arc<HttpCache>, settings: &Settings, clock: Clock) -> Map<String, Vec<NewsItem>> {
    let ft = make_fetch_text(cache.clone(), clock.today);
    let fetch = move |url: &str, params: Option<&fm_http::Params>| -> Result<String> {
        let empty = Vec::new();
        ft(url, params.unwrap_or(&empty), 0.0)
    };
    let (items, failed) = fm_sources::news::fetch_news(Some(&fetch), Some(&settings.fm_data_dir), None, clock.utc());
    for (label, e) in &failed {
        tracing::warn!("news source {label} failed: {e}");
    }
    fm_sources::news_match::match_news_to_players(&items, &ctx.all_players())
}

/// `web.app.load_base(league)`: provider load -> NHL enrichment -> valuation -> news (no
/// dynasty, no recommendations, no dynasty-mode resolution: `finish` applies the mode).
pub fn load_base(league: &str, settings: &Settings, clock: Option<Clock>) -> Result<BaseLoad> {
    let t0 = Instant::now();
    let cache = open_cache(settings)?;
    let clk = clock.unwrap_or_else(Clock::system);
    let mut provider = get_provider_at(league, settings, cache.clone(), clock.map(|c| c.today))?;
    let mut ctx = match provider.load() {
        Ok(c) => c,
        Err(e) => {
            if let Some(cm) = find_in_chain::<CacheMiss>(&e) {
                if find_in_chain::<ProviderError>(&e).is_none() {
                    return Err(ProviderError::Message(format!(
                        "Offline mode (FM_OFFLINE=1) and league data is not cached: {cm}"
                    ))
                    .into());
                }
            }
            return Err(e);
        }
    };
    ctx.warnings.extend(provider.warnings().iter().cloned());
    if let Some(w) = enrich_guarded(&mut ctx, settings, &cache, false, clk) {
        ctx.warnings.push(w);
    }
    let values = valuate_only(&mut ctx, settings);
    let news_by_cid = news(&ctx, &cache, settings, clk);
    Ok(BaseLoad {
        ctx,
        values,
        provider,
        news_by_cid,
        loaded_at: Local::now().naive_local(),
        seconds: t0.elapsed().as_secs_f64(),
        cache,
    })
}

// --------------------------------------------------------------------------- snapshots

/// `from_snapshot(path)`: a [`Loaded`] from a parity snapshot (`fixtures/<league>` or its
/// `snapshot.json`): the snapshot's enriched context (dynasty mode as dumped), params installed
/// from the snapshot, values / dynasty recomputed, a [`SnapshotProvider`] answering remote calls
/// from `tape.json.gz` when present, and the snapshot's pinned clock. `data_dir` (odds archive,
/// provider state) defaults to the fixtures' shared `_datadir` next to the league directory.
pub fn from_snapshot(path: &Path, data_dir: Option<&Path>) -> Result<Loaded> {
    let file = snapshot_file(path);
    let snap = Snapshot::load(&file)?;
    let data_dir: PathBuf = match data_dir {
        Some(d) => d.to_path_buf(),
        None => file
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("_datadir"))
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| file.parent().map(Path::to_path_buf).unwrap_or_default()),
    };
    params::install_with_active(snap.params.merged.clone(), Some(snap.params.active.clone()));
    let tape = match tape_next_to(&file) {
        Some(t) => Some(Tape::load(&t).with_context(|| format!("loading {}", t.display()))?),
        None => None,
    };
    let cache = Arc::new(HttpCache::from_tape(tape.clone().unwrap_or_default()));
    let now = snap.now.to_utc().context("snapshot clock must be aware")?;
    let clock = Clock { now: now.timestamp() as f64 + now.timestamp_subsec_micros() as f64 / 1e6, today: snap.today };
    let provider: Box<dyn LeagueProvider> = Box::new(SnapshotProvider::new(&snap, tape, &data_dir));
    let mut lc = snap.ctx;
    let settings = Settings { fm_data_dir: data_dir, ..Settings::default() };
    // the snapshot was dumped after `_valuate`: no second source note
    let values = valuate_only(&mut lc, &settings);
    let dyn_values = dynasty(&mut lc, &values, Some(provider.as_ref()));
    Ok(Loaded { lc, values, dyn_values, provider, cache, errors: Vec::new(), clock })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_resolution_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings { fm_data_dir: dir.path().to_path_buf(), ..Settings::default() };
        let scoring = fm_core::models::ScoringConfig {
            kind: ScoringKind::Points,
            weights: Map::new(),
            categories: vec![],
            goalie_weights: Map::new(),
        };
        let as_of = chrono::NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let mut lc =
            crate::espn::new_context("1".into(), 2027, "L".into(), scoring, Map::new(), vec![], vec![], None, as_of);
        resolve_mode(&mut lc, &s, None);
        assert_eq!((lc.dynasty_mode, lc.dynasty_mode_source.as_deref()), (DynastyMode::Balanced, Some("default")));
        s.fantrax_mode = DynastyMode::Rebuild;
        s.fields_set.insert("fantrax_mode".into());
        resolve_mode(&mut lc, &s, None);
        assert_eq!((lc.dynasty_mode, lc.dynasty_mode_source.as_deref()), (DynastyMode::Rebuild, Some("env")));
        fm_core::prefs::set_pref("dynasty_mode", Some(serde_json::json!("contend")), dir.path()).unwrap();
        resolve_mode(&mut lc, &s, None);
        assert_eq!((lc.dynasty_mode, lc.dynasty_mode_source.as_deref()), (DynastyMode::Contend, Some("prefs")));
        resolve_mode(&mut lc, &s, Some(DynastyMode::Balanced));
        assert_eq!((lc.dynasty_mode, lc.dynasty_mode_source.as_deref()), (DynastyMode::Balanced, Some("option")));
        assert_eq!(mode_line(&lc), "Dynasty mode: balanced (from --mode, this run only)");
    }

    #[test]
    fn user_facing_errors() {
        let e = user_facing(ProviderError::Message("no team".into()).into());
        assert!(matches!(e.downcast_ref::<LoadError>(), Some(LoadError::Failed(m)) if m == "no team"));
        let e = user_facing(ProviderError::Unavailable("later".into()).into());
        assert!(matches!(e.downcast_ref::<LoadError>(), Some(LoadError::Unavailable(m)) if m == "later"));
        let e = user_facing(anyhow::Error::from(CacheMiss("offline and not cached: u".into())).context("loading"));
        assert_eq!(e.to_string(), "offline mode (FM_OFFLINE=1) and data not cached: offline and not cached: u");
        let e = user_facing(anyhow::anyhow!("boom"));
        assert!(e.downcast_ref::<LoadError>().is_none());
    }
}
