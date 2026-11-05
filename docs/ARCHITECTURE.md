# fantasy-manager in Rust: architecture and porting spec

> **Status (2026-11, cutover):** the port is complete and replaced the Python app, which now lives
> on the `legacy-python` branch. The workspace moved from `rust/` to the repo root (`Cargo.toml`,
> `crates/`, `parity/`); this file moved to `docs/`, `WORK_PACKAGES.json` and the porting log to
> `docs/port/`. Data the binary embeds now sits in the crates: `crates/fm-engine/data/fitted_params.json`,
> `crates/fm-sources/data/nhle_factors.json`, `crates/fm-web/templates/`, `crates/fm-web/static/`.
> Parity scripts import the Python app from a `legacy-python` checkout (`FM_LEGACY_ROOT`, README
> "Parity tooling"). The rest of this document is the porting spec as written.

Goal: a full Rust port of `fantasy_manager/` (~44k lines Python) that gives **the same numbers and
recommendations on the same inputs** and runs faster, especially the trade search. The Python app
keeps working untouched until the Rust one reaches parity. Translation is done in parallel by many
agents from `WORK_PACKAGES.json`. This document is the contract they share.

## 1. Workspace layout

```
rust/
  Cargo.toml              workspace (shared dependency versions)
  ARCHITECTURE.md         this file
  WORK_PACKAGES.json      the parallel work plan (waves, file ownership, notes)
  parity/
    dump_fixtures.py      Python -> frozen inputs + every stage's outputs (fixtures/)
    diff_fixtures.py      diff two fixture trees (determinism checks)
    shapes.py             print the shape of a fixture JSON
    fixtures/<league>/    espn, fantrax (see section 6)
  crates/
    fm-pycompat   Python-exact sum/round/format/repr/json.dumps/random/str/strftime helpers
    fm-core       shared types (models, values, snapshot, provider trait), settings, prefs, normalize
    fm-http       HTTP GET cache over the SAME cache.db, Fantrax fxpa file cache, parity tapes
    fm-sources    NHL, MoneyPuck, Daily Faceoff, injuries, news, news roles, odds, matching, LLM
    fm-engine     scoring, valuation, recommend engines, analysis, archive files (pure compute)
    fm-backtest   backtest data / models / fit / evaluate / reports
    fm-harness    ledger (harness.db), ingest, matching, outcomes, metrics, deployment, refit, health
    fm-providers  ESPN + Fantrax providers, enrichment, load pipeline, SnapshotProvider
    fm-report     digest, data-health report, notifications, pregame
    fm-web        axum dashboard, auth, view models, minijinja over the Python templates, export
    fm-cli        the `fm` binary (all typer commands incl. harness/backtest)
    fm-parity     fixture loaders + comparator; tests/stage_*.rs and tests/cli_*.rs
```

Crate dependency order (acyclic; each crate depends only on crates above it):

```
fm-pycompat -> fm-core -> fm-http -> fm-sources -> fm-engine -> fm-backtest -> fm-harness
            -> fm-providers -> fm-report -> fm-web -> fm-cli        (fm-parity: dev-deps on all)
```

The Python packages import each other in cycles (valuation <-> analysis <-> recommend,
providers <-> recommend/harness). The port breaks them like this:

* valuation, recommend and analysis share ONE crate (`fm-engine`), so their mutual imports are
  plain module paths.
* Small provider-agnostic helpers that providers need moved down into `fm-core`:
  `schedule_grid.calendar_weeks` -> `fm_core::calendar`, `recommend.base.{RECENT_ADDS_WINDOW,
  count_adds, recent_adds_from}` -> `fm_core::activity`, `preseason_enrich.is_unproven` ->
  `fm_core::unproven`. `providers.fantrax.lineup_lock_for` moved up into
  `fm_engine::recommend::lineup_lock`. `deployment_enrich.role_change_alerts` lives in
  `fm_engine::recommend::flags`. `report/news_match.py` lives in `fm_sources::news_match`.
* `analysis/goalie_plan.py`'s `team_gf_factors / team_ga_factors / second_night_prob / _odds_fn`
  (needed by valuation.schedule) are in `fm_engine::analysis::team_factors`;
  `analysis/outlook.py: future_values` (needed by trades) is in `fm_engine::analysis::outlook_values`.
* `backtest/archive.py` is `fm_engine::archive` (recommend.plan writes through it).
* Everything Python reads off a provider object with `getattr(provider, ...)` is the
  `fm_core::provider::LeagueProvider` trait + `fm_core::snapshot::ProviderExtras`; compute code takes
  `Option<&dyn LeagueProvider>` where Python took `provider=None`.

## 2. Python -> Rust module map (every file)

| Python | Rust | Package |
|---|---|---|
| `models.py` | `fm_core::models` (+ `pydt`, `values`) | architect (done) |
| `config.py` | `fm_core::config` | W1-core-runtime |
| `prefs.py` | `fm_core::prefs` | W1-core-runtime |
| `matching/normalize.py` | `fm_core::normalize` | W1-core-runtime |
| `cache.py` | `fm_http::{cache, fxpa, tape}` | W2-http-sources |
| `providers/base.py`, `providers/__init__.py` | `fm_core::provider` (trait, done), `fm_providers::registry` | W5-provider-pipeline |
| `providers/espn.py` (+ the used part of `espn_api`) | `fm_providers::espn::{mod, api}` | W3-espn |
| `providers/fantrax.py` | `fm_providers::fantrax` | W3-fantrax |
| `providers/fantrax_auth.py` | `fm_providers::fantrax_auth` | W3-fantrax |
| `providers/nhl.py` | `fm_sources::nhl` | W2-nhl-matching |
| `providers/moneypuck.py` | `fm_sources::moneypuck` | W2-http-sources |
| `providers/dailyfaceoff.py` | `fm_sources::dailyfaceoff` | W2-http-sources |
| `providers/injuries.py` | `fm_sources::injuries` | W2-http-sources |
| `providers/news.py` | `fm_sources::news` | W2-news-odds-llm |
| `providers/news_roles.py` | `fm_sources::news_roles` | W2-news-odds-llm |
| `providers/odds.py` | `fm_sources::odds` | W2-news-odds-llm |
| `providers/enrich.py` | `fm_providers::enrich` | W4-enrich |
| `providers/lines_enrich.py` | `fm_providers::lines_enrich` | W4-enrich |
| `providers/xg_enrich.py` | `fm_providers::xg_enrich` | W4-enrich |
| `providers/preseason_enrich.py` | `fm_providers::preseason_enrich` (+ `fm_core::unproven`) | W4-enrich (+W1-core-runtime) |
| `providers/rookie_enrich.py` | `fm_providers::rookie_enrich` | W4-enrich |
| `providers/deployment_enrich.py` | `fm_providers::deployment_enrich` (+ flags) | W4-enrich (+W7) |
| `matching/matcher.py` | `fm_sources::matching::matcher` | W2-nhl-matching |
| `matching/crosswalk.py` | `fm_sources::matching::crosswalk` | W2-nhl-matching |
| `llm/openrouter.py` | `fm_sources::llm::openrouter` | W2-news-odds-llm |
| `llm/context.py` | `fm_sources::llm::context` | W2-news-odds-llm |
| `scoring/__init__.py` | `fm_engine::scoring` | W2-valuation-base |
| `valuation/params.py` | `fm_engine::valuation::params` | W2-valuation-base |
| `valuation/replacement.py` | `fm_engine::valuation::replacement` | W2-valuation-base |
| `valuation/blend.py` | `fm_engine::valuation::blend` | W2-valuation-base |
| `valuation/regression.py` | `fm_engine::valuation::regression` | W2-valuation-base |
| `valuation/adjust.py` | `fm_engine::valuation::adjust` | W2-valuation-base |
| `valuation/schedule.py` | `fm_engine::valuation::schedule` | W3-valuation-schedule |
| `valuation/roles.py` | `fm_engine::valuation::roles` | W3-valuation-schedule |
| `valuation/rookie.py` | `fm_engine::valuation::rookie` | W3-valuation-schedule |
| `valuation/valuate.py` | `fm_engine::valuation::valuate` (PlayerValue type in `fm_core::values`) | W4-valuate |
| `valuation/dynasty.py` | `fm_engine::valuation::dynasty` (DynastyValue in `fm_core::values`) | W4-valuate |
| `valuation/picks.py` | `fm_engine::valuation::picks` (PickAsset in `fm_core::values`) | W4-valuate |
| `recommend/base.py` | `fm_engine::recommend::base` | W5-recommend-base |
| `recommend/strength.py` | `fm_engine::recommend::strength` | W5-recommend-base |
| `recommend/headline.py` | `fm_engine::recommend::headline` | W5-recommend-base |
| `recommend/market.py` | `fm_engine::recommend::market` | W5-recommend-base |
| `recommend/lineup.py` | `fm_engine::recommend::{lineup, lineup_lock}` | W6-lineup-waivers |
| `recommend/waivers.py` | `fm_engine::recommend::waivers` | W6-lineup-waivers |
| `recommend/trades.py` | `fm_engine::recommend::trades` (+ `trades/` submodules) | W7-trades |
| `recommend/injuries.py` | `fm_engine::recommend::injuries` | W7-injuries-flags-alerts-matchup |
| `recommend/flags.py` | `fm_engine::recommend::flags` | W7-injuries-flags-alerts-matchup |
| `recommend/alerts.py` | `fm_engine::recommend::alerts` | W7-injuries-flags-alerts-matchup |
| `recommend/plan.py` | `fm_engine::recommend::plan` | W8-plan-claims-advise |
| `recommend/plan_memory.py` | `fm_engine::recommend::plan_memory` | W8-plan-claims-advise |
| `recommend/claims.py` | `fm_engine::recommend::claims` | W8-plan-claims-advise |
| `recommend/advise.py` | `fm_engine::recommend::advise` | W8-plan-claims-advise |
| `analysis/schedule_grid.py` | `fm_engine::analysis::schedule_grid` (+ `fm_core::calendar`) | W5-recommend-base |
| `analysis/goalie_plan.py` | `fm_engine::analysis::goalie_plan` (+ `team_factors`) | W6-lineup-waivers (+W3) |
| `analysis/matchup.py` | `fm_engine::analysis::matchup` | W7-injuries-flags-alerts-matchup |
| `analysis/outlook.py` | `fm_engine::analysis::outlook` (+ `outlook_values`) | W8-outlook-playoffs-calib (+W7) |
| `analysis/playoffs.py` | `fm_engine::analysis::playoffs` | W8-outlook-playoffs-calib |
| `backtest/archive.py` | `fm_engine::archive` | W5-archive-outcomes |
| `backtest/data.py` | `fm_backtest::data` | W3-harness-deploy-data |
| `backtest/scoring.py` | `fm_backtest::scoring` | W2-harness-ledger-stats |
| `backtest/evaluate.py` | `fm_backtest::evaluate` | W2-harness-ledger-stats |
| `backtest/report.py` | `fm_backtest::report` | W2-harness-ledger-stats |
| `backtest/models.py` | `fm_backtest::models` | W4-backtest-models |
| `backtest/fit.py` | `fm_backtest::fit` | W4-backtest-models |
| `backtest/pipeline.py` | `fm_backtest::pipeline` | W4-backtest-models |
| `backtest/xg_backtest.py` | `fm_backtest::xg_backtest` | W4-backtest-models |
| `harness/ledger.py` | `fm_harness::ledger` | W2-harness-ledger-stats |
| `harness/match.py` | `fm_harness::episode_match` | W2-harness-ledger-stats |
| `harness/ingest.py` | `fm_harness::ingest` | W3-harness-deploy-data |
| `harness/realized.py` | `fm_harness::realized` | W3-harness-deploy-data |
| `harness/deployment.py` | `fm_harness::deployment` | W3-harness-deploy-data |
| `harness/params_store.py` | `fm_harness::params_store` | W3-harness-deploy-data |
| `harness/outcomes.py` | `fm_harness::outcomes` | W5-archive-outcomes |
| `harness/metrics.py` | `fm_harness::metrics` | W6-harness-metrics-refit |
| `harness/health.py` | `fm_harness::health` | W6-harness-metrics-refit |
| `harness/track.py` | `fm_harness::track` | W6-harness-metrics-refit |
| `harness/refit.py` | `fm_harness::refit` | W6-harness-metrics-refit |
| `harness/matchup_calib.py` | `fm_harness::matchup_calib` | W8-outlook-playoffs-calib |
| `harness/__init__.py`, `backtest/__init__.py`, other `__init__.py` | crate `lib.rs` re-exports | owners of lib.rs |
| `report/digest.py` | `fm_report::digest` | W9-report-pregame |
| `report/health.py` | `fm_report::health` | W8-outlook-playoffs-calib |
| `report/notify.py` | `fm_report::notify` | W9-report-pregame |
| `report/news_match.py` | `fm_sources::news_match` | W2-nhl-matching |
| `pregame.py` | `fm_report::pregame` | W9-report-pregame |
| `cli.py` (helpers, settings..sync, mode, auth, web) | `fm_cli::{main, app, common, cmd_league, cmd_admin}` | W9-cli-core |
| `cli.py` (trades..claims, schedule..plan) | `fm_cli::{cmd_recs, cmd_analysis}` | W10-cli-analysis |
| `cli.py` load pipeline (`_load_all`, `Loaded`, ...) | `fm_providers::pipeline` | W5-provider-pipeline |
| `cli_harness.py` | `fm_cli::cmd_harness` | W10-cli-ops |
| `cli_backtest.py` | `fm_cli::cmd_backtest` (+ `_load_league_full` in `fm_providers::pipeline`) | W10-cli-ops |
| `web/app.py` | `fm_web::{app, templates}` | W10-web-app |
| `web/auth.py` | `fm_web::auth` | W2-web-auth-export |
| `web/export.py` | `fm_web::export` | W2-web-auth-export |
| `web/views.py` | `fm_web::views` | W9-web-views |
| `web/health_view.py` | `fm_web::health_view` | W9-web-views |
| `web/charts.py` | `fm_web::charts` | W9-web-views |
| `web/templates/*.html`, `web/static/*` | used in place (not copied) by `fm_web::templates` | W10-web-app |
| `valuation/fitted_params.json`, `valuation/nhle_factors.json` | `include_str!` from the Python package | W2-valuation-base, W2-nhl-matching |

## 3. Library choices

| Need | Crate | Why |
|---|---|---|
| (de)serialization | `serde`, `serde_json` with **`preserve_order`** + `float_roundtrip` | dict order = insertion order; exact floats |
| ordered maps/sets | `indexmap` (`fm_core::Map`, `fm_core::Set`) | Python dict semantics |
| dates | `chrono` (+ `chrono-tz`) | `NaiveDate`; `fm_core::PyDateTime` keeps naive vs aware |
| HTTP | `reqwest` **blocking**, `native-tls`, no default features | verified on the GNU toolchain; CLI is synchronous like Python |
| SQLite | `rusqlite` (`bundled`) | same `cache.db`, `harness.db`, `crosswalk.db`, `status_history.db` files |
| regex | `regex`; `fancy-regex` only where Python uses lookaround | |
| HTML/RSS | regexes as in Python (DFO `__NEXT_DATA__`, Fantrax rules page); `quick-xml` for RSS (feedparser replacement); `htmlize` for `html.unescape` | the Python code does not use a DOM parser |
| CSV | `csv` | MoneyPuck |
| parallelism | `rayon`, `dashmap` for shared pure memos | trade search, waivers, valuation, refit grid |
| CLI | `clap` (derive) + `comfy-table` | typer + rich tables |
| web | `axum` 0.8 + `tokio` + `tower-http` (static) | FastAPI replacement; blocking pipeline work in `spawn_blocking` |
| templates | `minijinja` 2 (+ `minijinja-contrib`) loading **the Python templates unmodified** | one set of templates for both apps |
| hashing / auth | `sha2`, `sha1`, `hmac`, `base64`, `hex` | cache keys, rec keys, session cookies |
| compression | `flate2` | parity tapes (`.json.gz`) |
| logging | `tracing` (+ `tracing-subscriber` in binaries) | Python `logging` |

Avoid crates that need cmake/nasm or aws-lc/ring (rustls) - not installed / not verified.

## 4. Conventions every translator follows

### 4.1 Shape of the port
* Keep Python names: modules, functions (`snake_case` as in Python; a leading `_` becomes
  `pub(crate) fn name` without the underscore unless it collides), constants (`SCREAMING_CASE`),
  struct fields = Python field names. A reader must be able to diff a Rust function against its
  Python original side by side. Port the logic line by line first; optimize only where the package
  notes say so, and never change results.
* pydantic models / dataclasses -> `#[derive(Clone, Debug, Serialize, Deserialize)]` structs with
  fields in declaration order (serde serializes in struct order = pydantic `model_dump` order).
  `@property` -> method. `model_copy(update=...)` -> clone + assign. `Field(exclude=True)` ->
  `#[serde(skip_serializing)]` (keep `Deserialize` so fixtures load).
* `Literal[...]` -> enum with `as_str()`; `X | None` -> `Option<X>`; `int` -> `i64`; `float` -> `f64`.
* `Mapping[str, Any]` arguments become concrete types: `values: &Map<Cid, PlayerValue>`,
  `dynasty_values: Option<&Map<Cid, DynastyValue>>`, `provider: Option<&dyn LeagueProvider>`.
* Identity (`is`, `id()`, memo keyed by `id(ctx)`) -> indices or explicit memo structs passed down.

### 4.2 Errors
Library code returns `anyhow::Result<T>`. Errors that Python catches **by type** are typed
(`thiserror`) so the catch site can `downcast_ref`: `fm_core::ProviderError`,
`fm_http::{CacheMiss, HttpError}`, `fm_sources::llm::LLMError`, `fm_core::CoreError::Lookup`.
"Best effort" Python blocks (`except Exception: warn and continue`) stay best effort: catch, push
the same warning text, continue. Never `unwrap()` on data from the network or files.

### 4.3 Numeric and ordering conventions (the parity rules)
Use `fm_pycompat` wherever Python used the builtin:

| Python | Rust |
|---|---|
| `sum(floats)` (3.12 compensated Neumaier!) | `fm_pycompat::num::py_sum` - a plain `+=` loop in Python stays a plain loop |
| `round(x)` | `num::py_round` / `py_round_i64` (ties to even) |
| `round(x, n)` | `num::round_to` (exact decimal, ties to even) |
| `x ** y` on floats | `powf` (never `powi`/`sqrt` substitutions) |
| `int(x)` | `x.trunc() as i64`; `//`, `%` -> `num::floordiv`, `num::pymod` |
| `max(xs, key=)` / `min` | `order::first_max_by` / `first_min_by` (first extreme wins; Rust's `max_by` returns the last) |
| `sorted(..., reverse=True)` | `order::sort_desc_by` (stable descending, not sort+reverse) |
| `sort` / `sorted` | `sort_by` (stable) with the same key tuple; f64 keys via `order::cmp_f64` |
| `bisect`, `itertools.combinations` | `order::bisect_left/right`, `order::combinations` |
| `f"{x:.2f}"`, `{x:+.1f}`, `{x:.0%}`, `{x:g}`, `str(x)` | `fmt::fixed`, `fmt::percent`, `fmt::general`, `fmt::repr_f64` |
| `json.dumps` | `pyjson::dumps` (ensure_ascii, Python separators, float repr) |
| `random.Random(seed)` | `random::PyRandom` (MT19937, gauss with cached `gauss_next`) |
| `len(s)`, `s[:n]` | `text::py_len`, `text::py_slice` (code points) |
| `strftime` / `strptime` / `fromisoformat` | `time::*`, `fm_core::PyDateTime` |

Dicts are `fm_core::Map` (IndexMap) whenever iteration order can reach an output; build them in
the same insertion order as Python. `HashMap` only for lookups that are never iterated.

**Set iteration.** Python `set`/`frozenset` of strings iterates in hash-randomized order. Where
that order reaches an output, Python itself is non-deterministic across processes; the port uses a
canonical order (sorted by cid / name, or F/D/G) and the parity tests use tolerances there. Known
cases (found by dumping fixtures with two `PYTHONHASHSEED`s, see 6.4): `recommend.plan` day lines
and the order of players fed to the seeded win-probability Monte Carlo (`win_*` fields move by up to
~0.01), `fm lineup` daily `no_slot` lists, `PlayerValue.rates` key order (blend set unions),
`trades.find_needs` long-term needs order, `schedule.redistribute_shares` float sum order (last
bit), `llm.openrouter._mask` phrase order.

### 4.4 Global state -> explicit state
* Enrichment registries (`preseason_enrich._REGISTRY`, `rookie_enrich._HISTORY/_SIGNALS`,
  `xg_enrich._GSAX/_NORMS`, `lines_enrich._TEAM_META`) -> `fm_core::snapshot::EnrichState`, stored
  in `LeagueContext.enrich: Arc<EnrichState>` (serde-skipped). Enrichment returns it; valuation and
  recommend read it with the same accessor names (`preseason_line`, `history_for`, ...).
* Valuation params (`valuation.params` module cache) -> a process-global
  `RwLock<Arc<Params>>` in `fm_engine::valuation::params` with `install()` (tests, snapshots) and
  `reload()` (web refresh) - the one deliberate global, mirroring Python.
* Memos keyed by `id(...)` (`schedule._CTX_MEMO`, `CategoriesScoring._pool_key`, trade engine
  caches) -> explicit memo structs owned by the caller; must be `Sync` when used under rayon.
* `get_settings()` (lru_cache) -> `Settings::load()` once in the binary, passed by reference.

### 4.5 Clock
Python reads `date.today()` / `datetime.now()` / `time.time()` in many places. Rust code takes
the time as a parameter (`today: NaiveDate`, `now: PyDateTime` or an epoch `f64`) at the API
boundary where Python had a `now=None` / `as_of=` parameter, and otherwise reads it once at the
top (CLI command / web request / pipeline) from `fm_core::config` helpers. The binaries honour a
Rust-only override `FM_NOW=<ISO datetime>` (and `--from-snapshot` uses the snapshot's `now`) so
outputs are reproducible. Naive local times (Fantrax `strptime`, ESPN `fromtimestamp`, digest
timestamps) use `chrono::Local`; the plan's zone logic (`FM_TZ` / `TZ`) is ported as written.

### 4.6 JSON output compatibility
* `--json` output must be **byte-identical where practical**: build a `serde_json::Value` with the
  same key order as the Python dict literal (or serialize the struct whose field order mirrors the
  pydantic model) and print `pyjson::dumps(&v, &DumpOpts::cli())` + `\n` (`json.dumps(indent=2,
  default=str)` + `typer.echo`). Floats print with Python `repr` (`1.0`, `1e-05`). `default=str`
  on a raw `datetime` gives `"2026-11-04 10:00:00"` (space) while `model_dump(mode="json")` gives
  ISO with `T` - check which one each payload used.
* Known unavoidable differences, accepted: NaN/Infinity cannot live in `serde_json::Value` (emitted
  as `null`); `archive.code_hash` (sha1 of Python sources) becomes `"rust:<version>"`.
* Files Python and Rust both read/write (`cache.db`, `fantrax_cache/`, `harness.db`,
  `crosswalk.db`, `status_history.db`, `prefs.json`, `plans/`, `lines/`, `pregame/`, `archive/`,
  `harness/params/`, `fantrax_session.json`, `news_items.json`, `web_secret`) keep the exact Python
  schema/format, so either implementation can run against the same data dir during migration.
  `rec_key`, `episode_id`, cache keys, `plan_memory._signature`, `params_hash` are byte-exact.

### 4.7 Performance
* Hot path: `recommend/trades.py` (~7.8k candidate evaluations, each re-solving lineups). Plan in
  W7-trades: precompute the engine single-threaded, `par_iter` over teams and candidates collecting
  by index, refine in parallel, merge in Python order, then the same stable sorts; integer bitset
  memo keys instead of frozensets; shared pure memos in `DashMap`.
* Also parallel: waivers (per free agent), `daily_plan` (per day), valuation (per player),
  advise (per engine), refit knob grid, enrichment network fan-out. Every parallel section must
  reduce in the original order - parity first.
* Release profile: `opt-level=3`, thin LTO. Dev builds optimize dependencies (`opt-level=2`).

### 4.8 Logging and warnings
Python `log.warning(...)` -> `tracing::warn!`. Warnings that reach output (`ctx.warnings`,
`lc.source_notes`, CLI `errors`) are data, appended in the same order with the same text.

## 5. Shared types (written, frozen)

`fm_core` is complete and verified against the fixtures (`cargo test -p fm-core`): `models`
(`Player`, `StatLine`, `FantasyTeam`, `LeagueContext`, `Recommendation`, `Reason`, `ActivityItem`,
`LineupDay`, `SourceStatus`, enums), `values` (`PlayerValue`, `DynastyValue`, `PickAsset`,
`RookieEstimate`, `RookiePrior`, `Horizon`), `snapshot` (`EnrichState` + registry payloads,
`ProviderExtras`, `LeagueRules`, `TradeBlockInfo`, `EspnLeagueMeta`, `Snapshot` loader), `provider`
(`LeagueProvider`), `pydt` (`PyDateTime`), `config::Settings` (struct). `fm_http`'s public API and
`fm_sources::news::NewsItem` / `news_match` signatures are also fixed because same-wave packages
code against them. Changing any of these needs the orchestrator; adding helper methods is fine.

## 6. Parity harness

### 6.1 Fixtures
`python parity/dump_fixtures.py` (venv python; offline from `data/cache.db` by default,
`--live` to refresh) writes per league (`espn`, `fantrax`) into `parity/fixtures/<league>/`:

| File | Content | Rust consumer |
|---|---|---|
| `snapshot.json` | enriched `LeagueContext` (+`deployment_details`), registries, merged params (+hash, source), provider extras, `trade_wants/offered`, pinned clock (`now`, `today`, `local_now`, UTC offset), env flags | `fm_core::Snapshot::load` - everything pure stages need, no network |
| `raw_ctx.json` | `provider.load()` before enrichment (mode resolved) | provider parity (W3) |
| `values.json` | `valuate_league` per cid (`player` replaced by `cid`; excluded fields included) | W4-valuate |
| `dynasty.json`, `picks.json` | `apply_dynasty`, pick assets by holder | W4-valuate |
| `engines/<engine>.json` | each advise engine called directly on the snapshot, `advise_merged`, `trades_with_blocks`, `need_trades` | W6-W8 |
| `cli/<cmd>.json` | `{"args", "exit_code", "output"}` for 21 `fm --json` commands with the loader patched to return a copy of the snapshot | W9/W10 CLI |
| `tape.json.gz` (gitignored, ~5 MB) | every HTTP GET / Fantrax POST answered during the run, keyed by the Python cache key | W2-http, W3 providers, W4-enrich |
| `summary.json` | counts + any engine/CLI errors | |
| `../_datadir/` (shared) | the data-dir state the run started from: prefs, plans/, lines/, status_history.db, crosswalk.db, news json (+ gitignored harness.db, fantrax_cache/) | CLI / plan-memory / status-history / ledger tests copy it to a temp dir |

The dump pins the clock (`--now`, default: start time) by swapping `datetime`, `date` and `time`
inside every `fantasy_manager` module, sets `PYTHONHASHSEED=0` (re-executes itself), disables the
news-role LLM (`FM_NEWS_ROLES_LLM=0`), runs on a temp copy of the data dir (the real `cache.db` is
opened read-only), and imports `fantasy_manager` from this worktree (asserted). All stage outputs
are therefore a pure function of `snapshot.json` (+ the data-dir state files for plan memory /
status history). Fixture size ~25 MB without tapes; compact JSON - pretty-print with
`python -m json.tool` or inspect with `python parity/shapes.py <file> [depth] [path]`.

### 6.2 Rust side
* `fm_parity` (W1-parity-harness): `Fixtures` loaders (re-attaching `player` objects to values),
  `compare(expected, actual, tolerance)` with per-path tolerances, unordered paths, ignored paths
  and string normalizers; `assert_parity!`; `parity-diff` binary.
* Stage tests live in `crates/fm-parity/tests/stage_*.rs` (pure stages, from `snapshot.json`) and
  `tests/cli_*.rs` (run the Rust CLI with the hidden `--from-snapshot parity/fixtures/<league>`
  flag, which loads the snapshot, uses a tape-backed `SnapshotProvider` and pins the clock). Each is
  owned by the package implementing the stage (see `parity_tests` in WORK_PACKAGES.json).
* Default tolerance: numbers rel 1e-9 / abs 1e-9, strings exact. Documented exceptions only: the
  set-order fields of 4.3 (win probabilities abs 0.02, unordered lists/maps), meta source notes with
  ages (normalized), timestamps of the run.

### 6.3 Regenerating, and the gitignored parts
Fixtures are tied to the worktree HEAD recorded in `snapshot.json`. Regenerate after any Python
change (`dump_fixtures.py`, ~3 min offline) and re-run `cargo test -p fm-parity`. Tracked in git:
~19 MB of compact JSON + the small `_datadir` files. Gitignored (size): `*/tape.json.gz` (~5 MB
each), `_datadir/harness.db` (13 MB), `_datadir/fantrax_cache/`. A fresh agent worktree only has
the tracked part; tests that need a tape or harness.db must skip with a clear message when absent.
To give agents the full set, run the dump once into a stable shared directory
(`dump_fixtures.py --out C:\code\fantasy_manager\rust_parity_fixtures`) and set
`FM_PARITY_FIXTURES` to it (the loaders honour it). The CLI commands in the dump run
`injuries --no-record` and `plan --no-archive`, so no command changes the state a later one reads
except plan memory (`plans/`), which only `plan` itself reads.

Packages that need more Python-side outputs (harness grading, archive files, digest, web view
models / HTML) add their own `parity/dump_<area>.py` that imports `dump_fixtures` helpers
(`prepare_env`, `freeze_clock`, `jsonable`, `write_json`) and writes under
`fixtures/<league>/<area>/`; `dump_fixtures.py` itself is owned by the architect / W11.

### 6.4 Determinism check
`dump_fixtures.py --now <T>` twice with different `PYTHONHASHSEED` and
`python parity/diff_fixtures.py A B` lists every output that depends on set order. Result at
the time of writing: only the fields listed in 4.3 differ; everything else is byte-identical.

## 7. Working on a package

1. Read your entry in `WORK_PACKAGES.json`, this file, the Python sources, and the stubs of your
   files (each names its owner). Work in your own git worktree; edit **only** your `rust_files`.
   Need something from a file you do not own? Ask the orchestrator; never patch it locally.
2. Shared environment: PowerShell needs
   `$env:PATH = "$env:USERPROFILE\.cargo\bin;C:\Users\caela\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin;$env:PATH"`.
   Python reference: `C:/code/fantasy_manager/.venv/Scripts/python` with this worktree first on
   `sys.path` (`sys.path.insert(0, r"<worktree>")`) - use it freely to generate goldens.
3. Port the Python tests listed under `python_tests_to_port` (intent, not line by line) into your
   test file; add the parity tests listed under `parity_tests`.
4. Done = `cargo build --workspace` clean (no new warnings in your files), `cargo test -p <your
   crate>` and your parity tests green, no `todo!()` left in your files, a short note of any
   deliberate deviation in the module header.

## 8. Decisions and risks

* **Templates are shared, not copied.** minijinja differs from Jinja2 in Python-isms (method
  calls, `'%.2f'|format`, `None`/`True`/float printing, a list `.append` in a `{% set %}`); W10
  bridges them in Rust (unknown-method callback, custom filters, a load-time source rewrite) without
  editing the `.html` files.
* **espn_api is re-implemented** (only the parts `espn.py` uses); request URLs/params/headers must
  match so the shared cache and the tapes hit.
* **numpy lstsq** in Fantrax scoring-weight fitting: results are rounded to 2 dp and tiny weights
  dropped, so a plain least-squares solve is expected to match; verified against fixtures.
* **Non-deterministic Python outputs** (4.3) get canonical Rust order + tolerances.
* **Waves are deep (11)** because the Python dependency chain is deep (pycompat -> valuation ->
  valuate -> recommend base -> lineup -> trades -> plan -> CLI/web). Start a package as soon as its
  `depends_on` are merged rather than waiting for the whole wave; side branches (providers,
  harness, backtest, web auth) fill the gaps.
