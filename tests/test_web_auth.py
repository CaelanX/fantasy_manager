"""Dashboard access control (fantasy_manager/web/auth.py): password login, session cookie, rate
limit, loopback-only mode without a password, and the insecure-bind guard."""
import logging
import os
import stat

import pytest

pytest.importorskip("fastapi")
pytest.importorskip("httpx")

from fastapi.testclient import TestClient  # noqa: E402

from fantasy_manager.config import Settings, get_settings  # noqa: E402
from fantasy_manager.providers.base import ProviderError  # noqa: E402
from fantasy_manager.web import auth as web_auth  # noqa: E402
from fantasy_manager.web.app import create_app  # noqa: E402
from tests.test_web import CountingLoader  # noqa: E402

PW = "correct horse battery staple"
REMOTE = ("203.0.113.7", 50000)
PROXY = ("127.0.0.1", 50000)


def make_cfg(**kw):
    kw.setdefault("password", PW)
    kw.setdefault("secret", b"k" * 64)
    return web_auth.AuthConfig(**kw)


@pytest.fixture
def loader():
    return CountingLoader()


@pytest.fixture
def app(loader):
    return create_app(loader, auth=make_cfg())


def client(app, peer=REMOTE):
    return TestClient(app, follow_redirects=False, client=peer)


def login(c, password=PW, nxt="/", headers=None):
    return c.post("/login", data={"password": password, "next": nxt}, headers=headers or {})


# -- login required ---------------------------------------------------------------------------

@pytest.mark.parametrize("path", ["/", "/roster", "/recommendations", "/news", "/health", "/player/a",
                                  "/?league=fantrax"])
def test_pages_redirect_to_login(app, loader, path):
    r = client(app).get(path)
    assert r.status_code == 303
    assert r.headers["location"].startswith("/login?next=%2F")
    assert loader.calls == []


@pytest.mark.parametrize("method,path", [
    ("get", "/api/recs.json"), ("get", "/api/roster.json"), ("get", "/api/health.json"),
    ("get", "/api/mode.json"), ("post", "/explain"), ("post", "/mode"), ("post", "/refresh"),
    ("post", "/params/rollback")])
def test_api_and_actions_need_login(app, loader, method, path):
    r = getattr(client(app), method)(path)
    assert r.status_code == 401 and r.json() == {"detail": "login required"}
    assert loader.calls == []


def test_setup_page_is_behind_login():
    ld = CountingLoader(ProviderError("ESPN_LEAGUE_ID is not set."))
    c = client(create_app(ld, auth=make_cfg()))
    assert c.get("/").status_code == 303
    assert ld.calls == []
    assert login(c).status_code == 303
    r = c.get("/")
    assert r.status_code == 200 and "Setup needed" in r.text


def test_public_paths(app, loader):
    c = client(app)
    assert c.get("/healthz").status_code == 200
    assert c.get("/static/style.css").status_code == 200
    r = c.get("/login?next=/roster")
    assert r.status_code == 200 and 'type="password"' in r.text and 'value="/roster"' in r.text
    assert r.headers["cache-control"] == "no-store"
    assert loader.calls == []


def test_htmx_request_gets_hx_redirect(app):
    r = client(app).get("/roster", headers={"HX-Request": "true"})
    assert r.status_code == 401 and r.headers["hx-redirect"] == "/login?next=%2Froster"


# -- password and rate limit ------------------------------------------------------------------

def test_wrong_password_is_401_and_logged_without_the_password(app, caplog):
    c = client(app)
    with caplog.at_level(logging.WARNING, logger="fantasy_manager.web.auth"):
        r = login(c, "hunter2-guess")
    assert r.status_code == 401 and "Wrong password" in r.text
    assert web_auth.COOKIE_NAME not in r.cookies
    assert "login failed from 203.0.113.7" in caplog.text
    assert "hunter2-guess" not in caplog.text and PW not in caplog.text
    assert c.get("/").status_code == 303


def test_rate_limit_five_failures_per_15_minutes(app):
    now = [1000.0]
    app.state.login_limiter.clock = lambda: now[0]
    c = client(app)
    for _ in range(5):
        assert login(c, "nope").status_code == 401
    r = login(c)                                   # even the right password is refused now
    assert r.status_code == 429 and int(r.headers["retry-after"]) == 900
    assert "Too many failed attempts" in r.text
    assert client(app, ("198.51.100.9", 1)).post("/login", data={"password": PW}).status_code == 303
    now[0] += 901
    assert login(c).status_code == 303


def test_rate_limit_uses_forwarded_ip_only_from_a_trusted_proxy(app):
    app.state.login_limiter.clock = lambda: 0.0
    via_proxy = client(app, PROXY)
    for _ in range(5):
        login(via_proxy, "nope", headers={"X-Forwarded-For": "192.0.2.1"})
    assert login(via_proxy, headers={"X-Forwarded-For": "192.0.2.1"}).status_code == 429
    assert login(via_proxy, headers={"X-Forwarded-For": "192.0.2.2"}).status_code == 303
    # a direct (untrusted) client can't dodge the limit by inventing X-Forwarded-For
    direct = client(app)
    for i in range(5):
        login(direct, "nope", headers={"X-Forwarded-For": f"10.0.0.{i}"})
    assert login(direct, headers={"X-Forwarded-For": "10.9.9.9"}).status_code == 429


def test_limiter_bounds_memory():
    lim = web_auth.LoginLimiter(clock=lambda: 0.0, max_clients=3)
    for i in range(10):
        lim.fail(f"ip{i}")
    assert len(lim._fails) <= 3


# -- session cookie ---------------------------------------------------------------------------

def test_login_sets_cookie_and_pages_work(app, loader):
    c = client(app)
    r = login(c, nxt="/roster?league=espn")
    assert r.status_code == 303 and r.headers["location"] == "/roster?league=espn"
    sc = r.headers["set-cookie"].lower()
    assert "fm_session=" in sc and "httponly" in sc and "samesite=lax" in sc and "secure" not in sc
    assert "max-age=2592000" in sc
    page = c.get("/")
    assert page.status_code == 200 and "Test League" in page.text
    assert 'action="/logout"' in page.text and PW not in page.text
    assert c.get("/api/recs.json").status_code == 200
    assert c.get("/login").status_code == 303                  # already logged in
    assert loader.calls == ["espn"]


def test_cookie_is_secure_behind_https_proxy(app):
    r = login(client(app, PROXY), headers={"X-Forwarded-Proto": "https", "X-Forwarded-For": "192.0.2.5"})
    assert r.status_code == 303 and "secure" in r.headers["set-cookie"].lower()
    # X-Forwarded-Proto from an untrusted peer is ignored
    r = login(client(app), headers={"X-Forwarded-Proto": "https"})
    assert "secure" not in r.headers["set-cookie"].lower()


def test_open_redirects_are_refused(app):
    for bad in ("//evil.example/", "https://evil.example/", "/\\evil", "/login", "javascript:alert(1)"):
        r = login(client(app), nxt=bad)
        assert r.headers["location"] == "/", bad


def test_tampered_expired_and_old_password_tokens_are_rejected():
    cfg = make_cfg(session_seconds=3600)
    tok = cfg.make_token(now=10_000)
    assert cfg.verify_token(tok, now=10_100)
    assert not cfg.verify_token(tok, now=10_000 + 3601)
    assert not cfg.verify_token(tok[:-2] + ("A" if tok[-2] != "A" else "B") + tok[-1], now=10_100)
    issued_later = tok.replace(".10000.", ".99999.", 1)
    assert not cfg.verify_token(issued_later, now=99_999)
    assert not cfg.verify_token("garbage", now=10_100) and not cfg.verify_token(None)
    assert not make_cfg(password="a new password", session_seconds=3600).verify_token(tok, now=10_100)
    assert not make_cfg(secret=b"z" * 64, session_seconds=3600).verify_token(tok, now=10_100)


def test_forged_cookie_is_rejected(app):
    c = client(app)
    c.cookies.set(web_auth.COOKIE_NAME, "v1.9999999999.abc.forged")
    assert c.get("/").status_code == 303


def test_logout_clears_the_cookie(app):
    c = client(app)
    login(c)
    assert c.get("/").status_code == 200
    r = c.post("/logout")
    assert r.status_code == 303 and r.headers["location"] == "/login"
    assert 'fm_session=""' in r.headers["set-cookie"] or "max-age=0" in r.headers["set-cookie"].lower()
    assert c.get("/").status_code == 303


def test_password_compare_is_constant_time_shaped():
    cfg = make_cfg()
    assert cfg.check_password(PW) and not cfg.check_password(PW[:-1]) and not cfg.check_password("")
    assert not web_auth.AuthConfig().check_password("")
    assert PW not in repr(cfg) and "password" in repr(cfg)


# -- no password: local mode unchanged, remote refused ---------------------------------------

def test_no_auth_config_is_todays_behaviour(loader):
    c = TestClient(create_app(loader), follow_redirects=False)
    assert c.get("/").status_code == 200
    assert c.get("/login").status_code == 404
    assert 'action="/logout"' not in c.get("/").text


def test_local_only_policy_serves_loopback_and_refuses_others(loader):
    app = create_app(loader, auth=web_auth.AuthConfig())
    assert client(app, ("127.0.0.1", 1)).get("/").status_code == 200
    assert client(app, ("::1", 1)).get("/api/recs.json").status_code == 200
    r = client(app).get("/")
    assert r.status_code == 403 and "FM_WEB_PASSWORD" in r.text
    # behind a reverse proxy the forwarded client counts, so a forgotten password fails closed
    assert client(app, PROXY).get("/", headers={"X-Forwarded-For": "203.0.113.9"}).status_code == 403
    assert client(app).get("/healthz").status_code == 200
    assert client(app, ("127.0.0.1", 1)).get("/login").status_code == 404


def test_allow_insecure_serves_everyone(loader):
    app = create_app(loader, auth=web_auth.AuthConfig(allow_insecure=True))
    assert client(app).get("/").status_code == 200


# -- insecure bind guard ----------------------------------------------------------------------

@pytest.mark.parametrize("host", ["127.0.0.1", "localhost", "::1", "127.0.1.1"])
def test_loopback_hosts_are_always_allowed(host):
    web_auth.check_bind(host, password_set=False, allow_insecure=False)


@pytest.mark.parametrize("host", ["0.0.0.0", "::", "192.168.1.20", "myhost.example"])
def test_non_loopback_needs_password_or_opt_in(host):
    with pytest.raises(web_auth.InsecureBindError, match="FM_WEB_PASSWORD"):
        web_auth.check_bind(host, password_set=False, allow_insecure=False)
    web_auth.check_bind(host, password_set=True, allow_insecure=False)
    web_auth.check_bind(host, password_set=False, allow_insecure=True)


@pytest.fixture
def web_env(monkeypatch, tmp_path):
    monkeypatch.chdir(tmp_path)                     # no .env here
    for k in ("FM_WEB_PASSWORD", "FM_WEB_SECRET", "FM_WEB_ALLOW_INSECURE"):
        monkeypatch.delenv(k, raising=False)
    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path / "data"))
    get_settings.cache_clear()
    yield tmp_path
    get_settings.cache_clear()


def test_run_refuses_public_bind_without_password(web_env, monkeypatch):
    import uvicorn

    from fantasy_manager.web import app as mod

    calls = []
    monkeypatch.setattr(uvicorn, "run", lambda *a, **kw: calls.append(kw))
    with pytest.raises(web_auth.InsecureBindError):
        mod.run("0.0.0.0", 9999)
    assert calls == []
    mod.run("127.0.0.1", 9999)
    assert calls and calls[-1]["host"] == "127.0.0.1" and calls[-1]["proxy_headers"] is True
    monkeypatch.setenv("FM_WEB_PASSWORD", PW)
    mod.run("0.0.0.0", 9999)
    assert calls[-1]["host"] == "0.0.0.0"
    assert not (web_env / "data" / "web_secret").exists()   # created lazily, on first login


def test_cli_web_refuses_public_bind(web_env, monkeypatch):
    import uvicorn
    from typer.testing import CliRunner

    from fantasy_manager import cli

    monkeypatch.setattr(uvicorn, "run", lambda *a, **kw: pytest.fail("must not serve"))
    res = CliRunner().invoke(cli.app, ["web", "--host", "0.0.0.0", "--port", "9998"])
    assert res.exit_code == 1 and "FM_WEB_PASSWORD" in res.output


# -- settings and the secret key --------------------------------------------------------------

def test_auth_from_settings_and_secret_file(web_env, monkeypatch):
    monkeypatch.setenv("FM_WEB_PASSWORD", PW)
    s = Settings()
    dumped = s.model_dump_json()
    assert PW not in dumped and "fm_web_password" not in dumped and "fm_web_secret" not in dumped
    cfg = web_auth.auth_from_settings(s)
    assert cfg.mode == "password" and cfg.password == PW
    path = web_env / "data" / "web_secret"
    assert not path.exists()
    key = cfg.key()
    assert path.exists() and len(key) >= 32
    if os.name == "posix":
        assert stat.S_IMODE(path.stat().st_mode) == 0o600
    assert web_auth.auth_from_settings(s).key() == key           # reused across restarts


def test_secret_from_env_writes_no_file(web_env, monkeypatch):
    monkeypatch.setenv("FM_WEB_PASSWORD", PW)
    monkeypatch.setenv("FM_WEB_SECRET", "s" * 40)
    cfg = web_auth.auth_from_settings(Settings())
    assert cfg.key() == b"s" * 40
    assert not (web_env / "data" / "web_secret").exists()


def test_stray_whitespace_in_the_password_is_ignored(web_env, monkeypatch):
    monkeypatch.setenv("FM_WEB_PASSWORD", PW + "\r")
    assert web_auth.auth_from_settings(Settings()).check_password(PW)


def test_policy_modes_from_env(web_env, monkeypatch):
    assert web_auth.auth_from_settings(Settings()).mode == "local-only"
    monkeypatch.setenv("FM_WEB_ALLOW_INSECURE", "1")
    assert web_auth.auth_from_settings(Settings()).mode == "open"


def test_module_app_has_a_policy():
    from fantasy_manager.web import app as mod

    assert isinstance(mod.app.state.auth, web_auth.AuthConfig)

