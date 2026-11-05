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


## Hand-offs (from W5-provider-pipeline)
- CLI: `LoadError::Unavailable` -> exit 2; `Failed` -> "Error: ..." exit 1. Hidden `--from-snapshot` uses `pipeline::from_snapshot`. Multiple leagues: `load_all_many`.
- Web refresh: `load_base` + `install_params(settings)`; `finish` (advise) belongs to W10.
- `FM_FETCH_WORKERS` (default 12; 1 = sequential). Live cold enrich ESPN: >=99.6s sequential -> 20.7s concurrent; offline pipeline ~1.9s -> ~0.9s/league.


## Hand-offs (from W6-lineup-waivers)
- Callers (W7 trades, W8 plan/claims, matchup, playoffs, CLI, web) pass `now: &PyDateTime` and a `ScheduleMemo` into `recommend_lineup`/`daily_plan`/waivers/`goalie_week`. Use `lineup::dp`/`dp_idx` for Python `_dp` and `lineup_lock::lock` for `_lock`.
- goalie_week parity goldens: set `FM_W6_GOLDEN_DIR` = C:\Users\caela\.claude\jobs\e681cc9d\fixtures\_w6_golden (generator: rust/parity/dump_goalie_week.py). W11: fold into dump_fixtures as `<league>/analysis/goalie_week.json`.
- Timing: ESPN matchup waivers Python ~9-11s -> Rust 0.05-0.12s; Fantrax ~4s -> 0.03-0.05s.


## Hand-offs (from W6-harness-metrics-refit)
- W8-outlook-playoffs-calib: after porting `harness::matchup_calib`, re-point `fm_harness::metrics::matchup_calibration` and `health::matchup_warning` (both return None now).
- CLI/web pass `now`/`today` and `RefitOptions` to capture/full_report/extend_league/calendar/TrackBook::load/run_refit.
- Tests that mutate process-global params must take the same serial guard as metrics_refit.


## Cleanup (W11), from W7-injuries-flags-alerts-matchup
- Make `valuation::adjust::games_remaining` and `schedule_grid::group_of` pub and drop local copies in injuries.rs; also local copies of `PreseasonLine.summary`/`pts_per_game`.
- `cli/alerts.json` merge/cap is W10's job (engine parts already at parity).


## Hand-offs (from W7-trades)
- W8: implement `fm_engine::recommend::trades::OutlookView` (`recommended()`, `direction(team_id)`) for `Outlook`; outlook `_lineup` can call `trades::greedy_lineup_value`.
- W10 CLI/web: `recommend_trades(ctx, values, &TradeOpts{..}, future_only, sweet_spot)`; `exploit_opportunities(ctx, values, &opts, EXPLOIT_N, EXPLOIT_PER_TEAM)`; trade-eval = `evaluate_offer` + `offer_summary` with `find_players`/`find_team`/`find_pick`; trade blocks `trade_block_wants(ctx, &extras.trade_blocks)`.
- Timing (release, 16 threads): recommend_trades ESPN 21.5s -> 46ms, Fantrax 19.7s -> 103ms; need trades 10.9s -> 29ms / 8.6s -> 70ms. `cargo bench -p fm-engine --bench trades`.


## From W8-outlook-playoffs-calib
- W11: make `trades::greedy` `greedy_assignment` (+ `SlotSpec`, `Held`) pub/re-exported; drop `outlook.rs::greedy_starters` copy.
- Matchup-calibration golden: `FM_PARITY_FIXTURES/_w8_golden/matchup_calib.json` from rust/parity/dump_matchup_calib.py (synthetic rows; fixture harness.db has none).
- `fm_report::health::data_health(ctx, now)` exists (web footer / digest).


## Hand-offs (from W8-plan-claims-advise)
- CLI/web: `plan_claims(..., memo, Some(data_dir))`. `cli/advise.json` = `advise(lineup, waivers, flags, injuries, alerts)` then `merge_normalized` with trades (wants/offered blocks), cap 15.
- `fm plan`: `build_week_plan(ctx, values, &PlanOpts{..}, now, memo).to_json()` + meta; `archive_plan` takes `now`. `today_checklist(plan, day, CHECKLIST_LIMIT)`.
- Set-order field to add to ARCHITECTURE 4.3: `days[*].goalies`.
- W11: `ScheduleMemo::fresh()`/`data_dir()` so `plan_claims` can drop its `data_dir` arg.
- Timing: advise+trades ESPN 13.8s->0.11s, Fantrax 9.1s->0.15s; claims 13.1s->0.12s / 2.3s->0.11s; plan 7.0s->0.37s / 2.8s->0.32s.


## W12-cutover (added 2026-11-04, user decision: Rust replaces Python on master)
- Python preserved on branch `legacy-python` (8bbfa2f). After W11 passes, a cutover package (opus: layout decisions) must:
  - remove `fantasy_manager/`, Python `tests/`, `pyproject.toml`; decide final layout (Cargo workspace at repo root vs keep `rust/`); move the data the Rust code embeds/loads (nhle_factors.json, fitted_params.json, web/templates, web/static, tests/fixtures used by Rust tests) under the Rust tree and fix `include_str!` / `CARGO_MANIFEST_DIR` paths.
  - parity dump scripts need Python: keep `rust/parity/*.py`, documented as run from a `legacy-python` checkout; fixtures stay checked in.
  - port Dockerfile, docker-compose.yml, .dockerignore, `.github/workflows/hosted.yml`, `deploy/` scripts to the Rust binary; update README (setup: rustup GNU toolchain + MinGW on Windows), docs/*.md commands, .env.example, `.claude/launch.json`.

## Hand-offs (from W9-report-pregame)
- `fm harness daily` (W10-cli-ops): call `fm_report::pregame::write_daily_snapshot(league, settings, ctx, values, &now)` after each league load.
- `run_pregame(PregameOpts{loader, dfo, injuries_fetch, notifier, planner, sender})`; `default_loader` + full `run_pregame` flow untested: W11 must run `fm pregame` once on real data.
- W11 check: ESPN injury refresh via `HttpCache::get_json` passes no headers/cookies; confirm cache key matches Python's.
- Report golden: `FM_PARITY_FIXTURES/_w9_golden/report.json` (rust/parity/dump_report.py).

