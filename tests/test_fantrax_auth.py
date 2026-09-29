"""Fantrax login / session refresh (no network: fake sessions only)."""
import json
import os
import sys

import pytest
import requests

from fantasy_manager.cache import HttpCache
from fantasy_manager.config import Settings
from fantasy_manager.providers import fantrax_auth as fa
from fantasy_manager.providers.base import ProviderError
from fantasy_manager.providers.fantrax import FXPA_URL, FantraxProvider
from fantasy_manager.providers.fantrax_auth import SESSION_FILE, FantraxAuth, LoginFailed

from .test_fantrax import POINTS, TODAY, FakeResponse, FakeSession, LiveSession

USER, PASSWORD = "user-sentinel-8841", "pw-sentinel-5512"
NOT_LOGGED_IN = {"code": "WARNING_NOT_LOGGED_IN"}


@pytest.fixture(autouse=True)
def _clear_attempts():
    fa._ATTEMPTS.clear()
    yield
    fa._ATTEMPTS.clear()


def settings(tmp_path, creds=True, **kw):
    base = dict(_env_file=None, fantrax_league_id="lg1", fantrax_cookie=None, fantrax_points=POINTS,
                fantrax_dynasty=True, fm_data_dir=tmp_path)
    if creds:
        base.update(fantrax_username=USER, fantrax_password=PASSWORD)
    base.update(kw)
    return Settings(**base)


class Clock:
    def __init__(self, t=1_800_000_000.0):
        self.t = t

    def __call__(self):
        return self.t


class LoginSession(FakeSession):
    """A fresh requests-like session: answers the fxpa `login` message (lgnu=1), then data calls."""

    def __init__(self, log, login_data=None, login_errors=None, login_body=None, still_logged_out=False):
        super().__init__(page_errors={m: NOT_LOGGED_IN for m in ("getFantasyLeagueInfo", "getTeamRosterInfo")}
                         if still_logged_out else None)
        self.log = log
        self.headers = {}
        self.cookies = requests.cookies.RequestsCookieJar()
        self.login_data = {"userInfo": {"userId": "u1"}} if login_data is None else login_data
        self.login_errors = login_errors
        self.login_body = login_body

    def get(self, url, timeout=None):
        self.log.append(("get", url))
        return FakeResponse({})

    def post(self, url, params=None, json=None, timeout=None):
        if params == {"lgnu": "1"}:
            assert url == FXPA_URL
            self.log.append(("login", json))
            if self.login_body is not None:
                return FakeResponse(self.login_body)
            resp = {"data": dict(self.login_data)}
            if self.login_errors:
                resp["errors"] = self.login_errors
            if "userInfo" in self.login_data:
                self.cookies.set("FX_RM", "fresh-remember-me", domain=".fantrax.com", path="/")
            return FakeResponse({"responses": [resp]})
        return super().post(url, params=params, json=json, timeout=timeout)


def make_auth(tmp_path, log=None, clock=None, s=None, **login_kw):
    log = [] if log is None else log
    return FantraxAuth(s or settings(tmp_path), session_factory=lambda: LoginSession(log, **login_kw),
                       clock=clock or Clock()), log


def logins(log):
    return [x for x in log if x[0] == "login"]


# -- request layer: NOT_LOGGED_IN -> one login -> retry ---------------------

def test_not_logged_in_triggers_one_login_and_retry(tmp_path):
    auth, log = make_auth(tmp_path)
    expired = FakeSession(page_errors={m: NOT_LOGGED_IN for m in (
        "getFantasyLeagueInfo", "getTeamRosterInfo", "getStandings", "getPlayerStats", "getLeagueRulesOld",
        "getTradeBlocks", "getPendingTransactions", "getTransactionDetailsHistory")})
    prov = FantraxProvider(settings(tmp_path), HttpCache(tmp_path), session=expired, today=TODAY, auth=auth)
    lc = prov.load()
    assert lc.teams and len(logins(log)) == 1
    assert len(expired.calls) == 1                      # only the first request hit the stale session
    assert prov.warnings.count("Fantrax session refreshed by login") == 1
    msg = logins(log)[0][1]
    assert msg["msgs"] == [{"method": "login", "data": {"u": USER, "p": PASSWORD, "t": "", "v": 3}}]
    assert msg["uiv"] == 3 and msg["refUrl"] == "https://www.fantrax.com/login"
    saved = json.loads((tmp_path / SESSION_FILE).read_text(encoding="utf-8"))
    assert [c["name"] for c in saved["cookies"]] == ["FX_RM"] and saved["source"] == "login"
    assert saved["last_login_ok"] is True
    assert PASSWORD not in json.dumps(saved) and USER not in json.dumps(saved)
    assert auth.load_session().cookies.get("FX_RM") == "fresh-remember-me"


def test_login_failure_during_refresh_is_provider_error(tmp_path):
    auth, log = make_auth(tmp_path, login_data={}, login_errors=[{"code": "INVALID_CREDENTIALS", "text": "x"}])
    prov = FantraxProvider(settings(tmp_path), HttpCache(tmp_path), today=TODAY, auth=auth,
                           session=FakeSession(page_errors={"getFantasyLeagueInfo": NOT_LOGGED_IN}))
    with pytest.raises(LoginFailed) as e:
        prov.load()
    text = str(e.value)
    assert "rejected FANTRAX_USERNAME / FANTRAX_PASSWORD" in text and "FANTRAX_COOKIE" in text
    assert PASSWORD not in text and USER not in text
    assert len(logins(log)) == 1 and auth.status()["last_login_ok"] is False


def test_still_logged_out_after_login(tmp_path):
    auth, log = make_auth(tmp_path, still_logged_out=True)
    prov = FantraxProvider(settings(tmp_path), HttpCache(tmp_path), today=TODAY, auth=auth,
                           session=FakeSession(page_errors={"getFantasyLeagueInfo": NOT_LOGGED_IN}))
    with pytest.raises(ProviderError, match="accepted the login .* still says you are not logged in"):
        prov.load()
    assert len(logins(log)) == 1


def test_no_credentials_keeps_cookie_advice(tmp_path):
    auth, log = make_auth(tmp_path, s=settings(tmp_path, creds=False, fantrax_cookie="FX_RM=x"))
    prov = FantraxProvider(settings(tmp_path, creds=False, fantrax_cookie="FX_RM=x"), HttpCache(tmp_path),
                           today=TODAY, auth=auth,
                           session=FakeSession(page_errors={"getFantasyLeagueInfo": NOT_LOGGED_IN}))
    with pytest.raises(ProviderError, match="FANTRAX_USERNAME and FANTRAX_PASSWORD.*FANTRAX_COOKIE"):
        prov.load()
    assert not logins(log)


# -- login results / rate limit ---------------------------------------------

@pytest.mark.parametrize("kw,match", [
    (dict(login_data={}, login_errors=[{"code": "BAD_INTERACTION"}]), "reCAPTCHA"),
    (dict(login_data={"tfa": True}), "two-factor"),
    (dict(login_data={"passwordExpired": True}), "expired"),
    (dict(login_data={}), "endpoint may have changed"),
    (dict(login_body={"pageError": {"code": "STALE_CLIENT"}}), "STALE_CLIENT"),
])
def test_login_failure_reasons(tmp_path, kw, match):
    auth, _ = make_auth(tmp_path, **kw)
    with pytest.raises(LoginFailed, match=match) as e:
        auth.refresh()
    assert "copy the whole Cookie request header" in str(e.value)
    assert not (tmp_path / SESSION_FILE).is_file() or not auth.status()["saved_session"]


def test_login_rate_limited_per_process_and_file(tmp_path):
    clock = Clock()
    auth, log = make_auth(tmp_path, clock=clock)
    auth.refresh()
    clock.t += 60
    with pytest.raises(LoginFailed, match="less than 10 minutes ago"):
        auth.refresh()
    fa._ATTEMPTS.clear()                               # a new process still sees the file timestamp
    other, log2 = make_auth(tmp_path, clock=clock)
    with pytest.raises(LoginFailed, match="not trying again for 9 min"):
        other.login()
    (tmp_path / SESSION_FILE).unlink()                 # ... and the process remembers without the file
    auth2, _ = make_auth(tmp_path, clock=clock)
    fa._ATTEMPTS[str(auth2.path.resolve())] = clock.t
    with pytest.raises(LoginFailed):
        auth2.login()
    clock.t += fa.LOGIN_MIN_INTERVAL + 1
    auth.refresh()
    assert len(logins(log)) == 2 and not logins(log2)


def test_rate_limit_applies_to_failed_logins(tmp_path):
    clock = Clock()
    auth, log = make_auth(tmp_path, clock=clock, login_data={}, login_errors=[{"code": "INVALID_CREDENTIALS"}])
    with pytest.raises(LoginFailed, match="rejected"):
        auth.refresh()
    with pytest.raises(LoginFailed, match="less than 10 minutes"):
        auth.refresh()
    assert len(logins(log)) == 1


# -- precedence and persistence ---------------------------------------------

def _saved_file(tmp_path, value="saved", **extra):
    (tmp_path / SESSION_FILE).write_text(json.dumps({
        "version": 1, "saved_at": 1_799_999_000.0, "source": "login",
        "cookies": [{"name": "FX_RM", "value": value, "domain": ".fantrax.com", "path": "/", "expires": None}],
        **extra}), encoding="utf-8")


def test_precedence_saved_then_cookie_then_login(tmp_path):
    s = settings(tmp_path, fantrax_cookie="FX_RM=from-env")
    auth, log = make_auth(tmp_path, s=s)
    _saved_file(tmp_path)
    assert auth.ensure_session().cookies.get("FX_RM") == "saved" and auth.source == "saved"
    auth.mark_expired()
    assert auth.ensure_session().cookies.get("FX_RM") == "from-env" and auth.source == "cookie"
    auth2, log2 = make_auth(tmp_path, s=settings(tmp_path))
    assert auth2.ensure_session().cookies.get("FX_RM") == "fresh-remember-me" and auth2.source == "login"
    assert not logins(log) and len(logins(log2)) == 1
    fa._ATTEMPTS.clear()
    (tmp_path / SESSION_FILE).unlink()
    none, _ = make_auth(tmp_path, s=settings(tmp_path, creds=False))
    with pytest.raises(ProviderError, match="FANTRAX_USERNAME and FANTRAX_PASSWORD.*FANTRAX_COOKIE"):
        none.ensure_session()


def test_stale_saved_session_falls_back_to_cookie_without_credentials(tmp_path):
    s = settings(tmp_path, creds=False, fantrax_cookie="FX_RM=from-env")
    auth, log = make_auth(tmp_path, s=s)
    _saved_file(tmp_path)
    auth.ensure_session()
    sess = auth.recover()
    assert sess.cookies.get("FX_RM") == "from-env" and auth.source == "cookie" and not logins(log)
    assert auth.status()["saved_session_expired"] is True
    assert auth.recover() is None                      # cookie also rejected: nothing left to try


def test_session_roundtrip_and_expired_cookies(tmp_path):
    auth, _ = make_auth(tmp_path)
    s = requests.Session()
    s.cookies.set("FX_RM", "rm", domain=".fantrax.com", path="/")
    s.cookies.set("ui", "u", domain="www.fantrax.com", path="/", expires=int(auth.clock()) + 3600)
    s.cookies.set("old", "o", domain=".fantrax.com", path="/", expires=int(auth.clock()) - 10)
    s.cookies.set("other", "x", domain=".example.com", path="/")
    auth.save_session(s)
    loaded = auth.load_session()
    assert {c.name: c.value for c in loaded.cookies} == {"FX_RM": "rm", "ui": "u"}
    assert loaded.headers["User-Agent"].startswith("Mozilla/5.0")
    st = auth.status()
    assert st["saved_session"] and st["saved_session_has_auth_cookie"] and st["saved_session_age_seconds"] == 0
    if sys.platform != "win32":
        assert (os.stat(auth.path).st_mode & 0o777) == 0o600
    assert auth.logout() is True and auth.load_session() is None and auth.logout() is False


def test_ping_bypasses_cache_and_records(tmp_path):
    auth, _ = make_auth(tmp_path, s=settings(tmp_path, creds=False))
    sess = FakeSession()
    prov = FantraxProvider(settings(tmp_path, creds=False), HttpCache(tmp_path), session=sess, today=TODAY,
                           auth=auth)
    assert prov.ping()["ok"] and prov.ping()["league_id"] == "lg1"
    assert len(sess.calls) == 2
    st = auth.status()
    assert st["last_ping_ok"] is True and st["last_ping_at"] == auth.clock()


# -- secrets never serialized ------------------------------------------------

def test_settings_never_serialize_secrets(tmp_path):
    s = settings(tmp_path, fantrax_cookie="FX_RM=cookie-sentinel-7",
                 openrouter_api_key="key-sentinel", espn_s2="s2-sentinel")
    for text in (repr(s), str(s), s.model_dump_json(), json.dumps(s.model_dump(mode="json"), default=str)):
        for secret in (USER, PASSWORD, "cookie-sentinel-7", "key-sentinel", "s2-sentinel"):
            assert secret not in text
    assert s.fantrax_password.get_secret_value() == PASSWORD


def _cli_env(monkeypatch, tmp_path, **extra):
    from fantasy_manager import cli

    monkeypatch.chdir(tmp_path)
    env = {"FM_DATA_DIR": str(tmp_path), "FANTRAX_LEAGUE_ID": "lg1", "FANTRAX_COOKIE": "FX_RM=cookie-sentinel-7",
           "FANTRAX_POINTS": POINTS, "FANTRAX_USERNAME": USER, "FANTRAX_PASSWORD": PASSWORD, **extra}
    for k, v in env.items():
        monkeypatch.setenv(k, v)
    cli.get_settings.cache_clear()
    return cli


def test_cli_settings_json_and_auth_status_hide_secrets(tmp_path, monkeypatch):
    from typer.testing import CliRunner

    from fantasy_manager.providers import fantrax as fmod

    orig_init = fmod.FantraxProvider.__init__

    def init(self, settings, cache=None, session=None, **kw):
        orig_init(self, settings, cache, session=LiveSession(), today=TODAY)
    monkeypatch.setattr(fmod.FantraxProvider, "__init__", init)
    cli = _cli_env(monkeypatch, tmp_path)
    runner = CliRunner()
    try:
        outputs = []
        res = runner.invoke(cli.app, ["--league", "fantrax", "--json", "settings"])
        assert res.exit_code == 0, res.output
        json.loads(res.output)
        outputs.append(res.output)
        _saved_file(tmp_path, value="saved-cookie-sentinel")
        res = runner.invoke(cli.app, ["auth", "fantrax", "--status", "--json"])
        assert res.exit_code == 0, res.output
        st = json.loads(res.output)["status"]
        assert st["saved_session"] and st["credentials"] == {"FANTRAX_USERNAME": True, "FANTRAX_PASSWORD": True,
                                                             "FANTRAX_COOKIE": True, "FANTRAX_COOKIE_FILE": False}
        outputs.append(res.output)
        res = runner.invoke(cli.app, ["auth", "fantrax"])
        assert res.exit_code == 0 and "Saved session: yes" in res.output and "FANTRAX_PASSWORD present" in res.output
        outputs.append(res.output)
        res = runner.invoke(cli.app, ["auth", "fantrax", "--logout"])
        assert res.exit_code == 0 and "deleted" in res.output and not (tmp_path / SESSION_FILE).exists()
        for out in outputs:
            for secret in (USER, PASSWORD, "cookie-sentinel-7", "saved-cookie-sentinel"):
                assert secret not in out
    finally:
        cli.get_settings.cache_clear()


def test_cli_login_uses_fake_transport(tmp_path, monkeypatch):
    from typer.testing import CliRunner

    log = []
    monkeypatch.setattr(fa, "build_session", lambda cookies: LoginSession(log) if not cookies else requests.Session())
    cli = _cli_env(monkeypatch, tmp_path)
    try:
        res = CliRunner().invoke(cli.app, ["auth", "fantrax", "--login"])
        assert res.exit_code == 0, res.output
        assert "Logged in to Fantrax" in res.output and len(logins(log)) == 1
        res = CliRunner().invoke(cli.app, ["auth", "fantrax", "--login"])
        assert res.exit_code == 1 and "less than 10 minutes" in res.output
        assert PASSWORD not in res.output and USER not in res.output
    finally:
        cli.get_settings.cache_clear()
