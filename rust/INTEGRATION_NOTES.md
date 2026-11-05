# Integration notes (orchestrator-owned)

Requests and hand-offs raised by finished work packages, to include in later briefs.

## For W10-web-app (from W2-web-auth-export)
- Serve with `into_make_service_with_connect_info::<SocketAddr>()` (auth reads the peer IP from `ConnectInfo`; no peer = fail closed).
- Use `fm_web::auth::install(router, Arc<AuthState>)`; supply a `LoginRenderer` closure `(error, next) -> html` that renders `login.html`.
- Set the template global `auth_enabled = cfg.enabled()`.
- Call `check_bind(host, password_set, allow_insecure)` before listening.
- `export_site` takes a `Router` built WITHOUT auth for the static crawl.
- `fm-web/Cargo.toml` already has: tower(util), http-body-util, form_urlencoded, regex, htmlize. Merge, don't overwrite.
- Python `run()` wiring (uvicorn proxy headers) is not ported yet: W10 owns it.

## For W5-provider-pipeline / W9-cli-core (from W2-valuation-base)
- `valuation::params::params_dir()` falls back to env `FM_DATA_DIR` then "data"; Python reads `Settings.fm_data_dir` (which may come from .env). Pass `data_dir` explicitly from `Settings` when installing params.
- Params are a process-global `RwLock<Option<Arc<Params>>>` (`install`/`current`/`reload`), mirroring Python's module state.

## Cleanup (final sweep, W11)
- Swap private json.dumps copies for `fm_pycompat::pyjson::dumps`: fm-http/src/cache.rs (`py_dumps`), fm-engine/src/valuation/params.rs (`params_hash` canonical dumps).
- Replace private `map_position` copy in fm-sources/src/moneypuck.rs with `fm_sources::nhl::map_position`.

## For W4-backtest-models (from W2-harness-ledger-stats)
- fm-backtest/src/evaluate.rs is only half ported. Still TODO there (W4 now owns finishing it, alongside its own files): `preseason_predictions`, `evaluate_preseason`, `inseason_observations`, `evaluate_inseason`. `Prediction`, `row()`, `pools()`, `ResultRow` are in place for them.
- `recommendations` / `render` / `write_report` take an explicit `AppConstants` (K_BASELINE, K_INSEASON, RECENCY_WEIGHTS, PRODUCTION_CURVES); the backtest CLI (W10-cli-ops) fills it from fm_engine::valuation::{blend, dynasty}.
- report.rs has local copies of `season_label` and `REPORT_AGES` (data.rs / fit.rs were stubs): switch to the real ones.

## Cleanup (W11), more
- fm-backtest/src/scoring.rs `PointsScorer` duplicates `fm_engine::scoring::PointsScoring` (now real): re-export instead.
- fm-harness/src/ledger.rs private `json_dumps` -> `fm_pycompat::pyjson::dumps`.
- `write_report` writes LF; Python on Windows writes CRLF (decide; LF is fine unless byte-compat needed).

## For W4-enrich (from W2-news-odds-llm, W2-nhl-matching)
- Map `FM_NEWS_ROLES_LLM=0` to `llm=None`; pass `now` explicitly into `extract_role_signals` / `refine_with_llm` / news store functions (clock is a parameter in the Rust port).
- Install `providers.enrich.ttl_for` into the NHL client with `NhlClient::with_ttl_for` (`cached_nhl_fetch(Arc<HttpCache>)` wires the cache).
- News fetch callback is a 2-arg `FetchText(url, params)` in fm-sources/news.rs (not fm_http::FetchText).
## Cleanup (W11), more
- fm-http: re-export `ParamValue` (and `tape::params_from_json`) at the crate root.

## For W9/W10 CLI (from W3-espn)
- ESPN warnings raised after load (activity, box_scores, trending) are only in `EspnProvider::warnings_now()`; the `trending` command must read them there.
- `espn_scoreboard` / `espn_league_get` have no fixture coverage (tape lacks the mMatchup view): test them live once.


## Follow-ups (from W3-harness-deploy-data)
- fm-harness/src/deployment.rs: B2B constants (0.35, 5, 5.0) copied locally; re-point to fm_engine::valuation::schedule (after W3-valuation-schedule).
- fm-harness/src/ingest.rs: local `archive_dir` / `load_snapshot`; re-point to fm_engine::archive (W5-archive-outcomes owns that; tell it).
- `pull_lineups` picks the ESPN path by `provider.name() == "espn"`.


## For W4-valuate (from W3-valuation-schedule)
- Pass `settings.data_dir` into `ScheduleMemo::new(Some(dir))` / `odds_fn` to get betting odds; `None` = no odds (goals-against fallback).
- `ScheduleMemo` (Send+Sync) replaces Python's `_CTX_MEMO`; opponent table keyed by (as_of, opponents.len(), schedule.len()).
- Not yet parity-checked against the snapshot (needs valuate): TEAMMATE_OUT base shares, ROOKIE_BLEND rates/FPG text. Make values.json parity cover them.
- `rookie_value(p, prior, signals, RookieInputs)`; signals are `&[impl AsSignal]`.


## Follow-ups (from W4-backtest-models)
- fm-backtest/src/models.rs has its own `baseline_rates` / `positional_means` (no-projection path only): re-point `group_means` and `FmCurrent` to `fm_engine::valuation::valuate` once W4-valuate lands (W11).
- `pipeline::app_constants` builds PRODUCTION_CURVES from `params::dynasty_age_curves` + hard-coded G curve: switch to `dynasty::PRODUCTION_CURVES` when it exists (W11).
- CLI (W10-cli-ops) must pass `now`/`today` and `Arc<HttpCache>` to `run_fit`/`run_evaluation`/`refresh_data`/`run_xg_backtest`.
- xG backtest parity test needs `FM_XG_CACHE_DIR` = C:\Users\caela\.claude\jobs\e681cc9d\tmp\xgdata (MoneyPuck 2018-2025 CSVs).


## Perf follow-up (from W4-enrich) -> W5-provider-pipeline / W11
- Enrichment network fan-out is still sequential: independent fetch steps (NHL stats seasons, Daily Faceoff teams, MoneyPuck CSVs, news feeds, injuries) can run concurrently (rayon scope or threads; results merged in Python's order). Biggest live-load win.
- Optional: record `fetched_at` per tape response so freshness notes are checkable in parity.
- Deployment-ledger `fetched_at` treats naive timestamps as chrono::Local (machine timezone).


## For W5-provider-pipeline (from W4-valuate)
- Call `valuate_league_with(ctx, scoring, &ScheduleMemo::new(Some(settings.data_dir)))` (`valuate_league` uses no odds). Categories/roto: `fit_and_valuate` fits first.
- `value_picks` takes `Option<&Map<Cid, DynastyValue>>` (DynastyValue objects, not floats).
- Hot loops elsewhere: build `ProductionCurves` once instead of calling `dynasty::trajectory_factor`/`growth_ratio` repeatedly.


## Hand-offs (from W5-archive-outcomes, W5-recommend-base)
- Archive writers need the clock: `archive_projections`/`archive_recommendations` take `now`; recs also `as_of` (W8 plan code, CLI).
- `fm backtest score` -> `fm_harness::outcomes::score_archive(..)` (W10-cli-ops).
- `DynastyChange<'a>` borrows `&Player` (kept_in/kept_out); `MarketPool<'a>` is immutable + Sync (safe for rayon in the trade search).
- `game_rate` returns `Result<GameRate, CoreError>`; `streaming_targets(ctx, values, week_start, &StreamingOpts)`.
- ESPN calendar needs `provider.espn_league()` populated.

