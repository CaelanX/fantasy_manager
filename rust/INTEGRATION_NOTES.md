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
