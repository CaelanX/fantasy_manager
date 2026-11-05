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
