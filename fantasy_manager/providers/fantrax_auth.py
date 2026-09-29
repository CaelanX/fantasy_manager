"""Fantrax login and session persistence.

Login mechanics (verified 2026-09-28 against the fantrax.com web app bundle and live responses):

* The web app logs in with an ordinary fxpa message: ``POST https://www.fantrax.com/fxpa/req?lgnu=1``
  with the standard envelope (``msgs`` / ``uiv`` / ``refUrl`` / ``dt`` / ``at`` / ``tz``) and one
  message ``{"method": "login", "data": {"u": <user or email>, "p": <password>, "t": <reCAPTCHA
  token>, "v": <reCAPTCHA version, 3>}}``. ``responses[0].data.userInfo`` is present on success;
  otherwise ``data.tfa`` (two-factor code needed), ``data.passwordExpired`` or ``responses[0].errors``
  (``INVALID_CREDENTIALS``, ``BAD_INTERACTION`` = reCAPTCHA rejected, ``ACCOUNT_LOCKED``).
* The web app obtains ``t`` from reCAPTCHA v3 in the browser. A script cannot, so ``t`` is sent
  empty; Fantrax may answer ``BAD_INTERACTION``, in which case the manual cookie remains the way in.
* Authentication rides on the long-lived ``FX_RM`` ("remember me") cookie: a request carrying only
  ``FX_RM`` is logged in and one without it gets ``WARNING_NOT_LOGGED_IN``. ``github.com/pmurley/
  go-fantrax`` (``auth_client``) keeps only ``FX_RM`` too; it gets it by driving headless Chrome
  through the /login form rather than by posting credentials itself.

The session is kept in ``<FM_DATA_DIR>/fantrax_session.json`` (cookies plus login bookkeeping).
Credentials and cookie values are never logged, printed or put in error messages.
"""
from __future__ import annotations

import json
import os
import time
from pathlib import Path
from typing import Any, Callable

from ..config import Settings, secret_value
from .base import ProviderError
from .fantrax import BROWSER_UA, FXPA_URL, build_session, load_cookies

SESSION_FILE = "fantrax_session.json"
LOGIN_PAGE = "https://www.fantrax.com/login"
LOGIN_MIN_INTERVAL = 10 * 60  # seconds between login attempts, per process and per session file
AUTH_COOKIE = "FX_RM"
SESSION_VERSION = 1

MANUAL_COOKIE_HINT = (
    "Log in at fantrax.com in a browser, open DevTools > Network, click any 'req?leagueId=' request and "
    "copy the whole Cookie request header into FANTRAX_COOKIE (or the file named by FANTRAX_COOKIE_FILE). "
    "See 'Fantrax setup' in the README.")
NO_CREDENTIALS_MSG = (
    "No Fantrax login is configured. Set FANTRAX_USERNAME and FANTRAX_PASSWORD in .env (recommended: "
    "fm logs in itself and keeps the session in FM_DATA_DIR), or set FANTRAX_COOKIE (or FANTRAX_COOKIE_FILE) "
    "to a logged-in browser cookie. " + MANUAL_COOKIE_HINT)

# Last login attempt per session file in this process (in addition to the timestamp in the file).
_ATTEMPTS: dict[str, float] = {}


class LoginFailed(ProviderError):
    """Programmatic Fantrax login did not produce a session."""


def _fmt_age(seconds: float) -> str:
    seconds = max(0.0, seconds)
    if seconds < 90:
        return f"{seconds:.0f}s"
    if seconds < 90 * 60:
        return f"{seconds / 60:.0f} min"
    if seconds < 48 * 3600:
        return f"{seconds / 3600:.1f} h"
    return f"{seconds / 86400:.1f} days"


class FantraxAuth:
    """Finds, creates, saves and refreshes the Fantrax cookie session.

    ``ensure_session()`` precedence: saved session file (unless known expired) > FANTRAX_COOKIE /
    FANTRAX_COOKIE_FILE > fresh login with FANTRAX_USERNAME / FANTRAX_PASSWORD.
    ``source`` tells which one the current session came from ("saved", "cookie" or "login").
    """

    def __init__(self, settings: Settings, data_dir: Path | str | None = None,
                 session_factory: Callable[[], Any] | None = None, clock: Callable[[], float] = time.time):
        self.settings = settings
        self.data_dir = Path(data_dir if data_dir is not None else settings.fm_data_dir)
        self.session_factory = session_factory or (lambda: build_session({}))
        self.clock = clock
        self.source: str | None = None

    # -- configuration ----------------------------------------------------
    @property
    def path(self) -> Path:
        return self.data_dir / SESSION_FILE

    @property
    def has_credentials(self) -> bool:
        return bool(secret_value(self.settings.fantrax_username) and secret_value(self.settings.fantrax_password))

    @property
    def has_cookie(self) -> bool:
        return bool(secret_value(self.settings.fantrax_cookie) or self.settings.fantrax_cookie_file)

    # -- session file -----------------------------------------------------
    def _read_state(self) -> dict[str, Any]:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}
        return data if isinstance(data, dict) else {}

    def _write_state(self, state: dict[str, Any]) -> None:
        """Atomic write, readable by the owner only where the OS allows it (best effort on Windows)."""
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_name(self.path.name + ".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump({"version": SESSION_VERSION, **state}, f)
        try:
            os.chmod(tmp, 0o600)
        except OSError:
            pass
        os.replace(tmp, self.path)

    def _update_state(self, **changes: Any) -> None:
        state = self._read_state()
        state.update(changes)
        try:
            self._write_state(state)
        except OSError:
            pass  # bookkeeping only; the in-process limit still applies

    def save_session(self, session: Any, source: str = "login") -> None:
        cookies = []
        for c in getattr(session, "cookies", []) or []:
            if "fantrax" not in str(getattr(c, "domain", "") or ".fantrax.com").lower():
                continue
            cookies.append({"name": c.name, "value": c.value, "domain": c.domain or ".fantrax.com",
                            "path": c.path or "/", "expires": c.expires})
        state = self._read_state()
        state.update({"cookies": cookies, "saved_at": self.clock(), "source": source, "expired": False})
        self._write_state(state)

    def _saved_cookies(self, state: dict[str, Any]) -> dict[str, str]:
        now = self.clock()
        out: dict[str, str] = {}
        for c in state.get("cookies") or []:
            if not isinstance(c, dict) or not c.get("name"):
                continue
            exp = c.get("expires")
            if isinstance(exp, (int, float)) and 0 < exp < now:
                continue
            out[str(c["name"])] = str(c.get("value", ""))
        return out

    def load_session(self) -> Any | None:
        """The saved session, or None if there is none or it is known to be expired."""
        state = self._read_state()
        if not state or state.get("expired"):
            return None
        cookies = self._saved_cookies(state)
        if not cookies:
            return None
        session = build_session({})
        for c in state.get("cookies") or []:
            if isinstance(c, dict) and c.get("name") in cookies:
                session.cookies.set(c["name"], cookies[c["name"]], domain=c.get("domain") or ".fantrax.com",
                                    path=c.get("path") or "/")
        return session

    def mark_expired(self) -> None:
        if self.path.is_file():
            self._update_state(expired=True, expired_at=self.clock())

    def logout(self) -> bool:
        """Delete the saved session file. True if one existed."""
        _ATTEMPTS.pop(str(self.path.resolve()), None)
        try:
            self.path.unlink()
            return True
        except FileNotFoundError:
            return False

    def record_ping(self, ok: bool, error: str | None = None) -> None:
        self._update_state(last_ping_at=self.clock(), last_ping_ok=ok, last_ping_error=error)

    # -- session selection ------------------------------------------------
    def ensure_session(self, allow_login: bool = True) -> Any:
        saved = self.load_session()
        if saved is not None:
            self.source = "saved"
            return saved
        cookies = load_cookies(self.settings)
        if cookies:
            self.source = "cookie"
            return build_session(cookies)
        if self.has_credentials and allow_login:
            return self.refresh()
        raise ProviderError(NO_CREDENTIALS_MSG)

    def recover(self) -> Any | None:
        """After Fantrax said WARNING_NOT_LOGGED_IN: a new session from the next source, or None.

        A saved session is marked expired. With credentials, logs in (raises LoginFailed on
        failure); otherwise falls back from a stale saved session to FANTRAX_COOKIE if one is set."""
        if self.source == "saved":
            self.mark_expired()
        if self.has_credentials:
            return self.refresh()
        if self.source == "saved":
            cookies = load_cookies(self.settings)
            if cookies:
                self.source = "cookie"
                return build_session(cookies)
        return None

    def refresh(self) -> Any:
        """Force a fresh login and save the session."""
        session = self.login()
        self.save_session(session, source="login")
        self.source = "login"
        return session

    # -- login ------------------------------------------------------------
    def seconds_until_login_allowed(self) -> float:
        key = str(self.path.resolve())
        last = max(float(self._read_state().get("last_login_attempt") or 0), _ATTEMPTS.get(key, 0.0))
        return max(0.0, last + LOGIN_MIN_INTERVAL - self.clock())

    def _fail(self, reason: str, code: str | None = None) -> LoginFailed:
        self._update_state(last_login_ok=False, last_login_error=code or reason)
        return LoginFailed(f"Fantrax login failed: {reason} Until this is fixed, use a browser cookie: "
                           + MANUAL_COOKIE_HINT)

    def login(self) -> Any:
        """Log in with FANTRAX_USERNAME / FANTRAX_PASSWORD; returns a logged-in requests.Session."""
        import requests

        if not self.has_credentials:
            raise ProviderError("FANTRAX_USERNAME and FANTRAX_PASSWORD are not both set. " + NO_CREDENTIALS_MSG)
        wait = self.seconds_until_login_allowed()
        if wait > 0:
            raise LoginFailed(f"A Fantrax login was attempted less than {LOGIN_MIN_INTERVAL // 60} minutes ago; "
                              f"not trying again for {_fmt_age(wait)} so a broken login cannot spam Fantrax. "
                              "If logins keep failing, use a browser cookie instead: " + MANUAL_COOKIE_HINT)
        now = self.clock()
        _ATTEMPTS[str(self.path.resolve())] = now
        self._update_state(last_login_attempt=now)

        session = self.session_factory()
        session.headers.update({"User-Agent": BROWSER_UA, "Referer": LOGIN_PAGE})
        try:
            session.get(LOGIN_PAGE, timeout=30)  # like a browser: picks up Cloudflare cookies first
        except requests.RequestException:
            pass
        payload = {"msgs": [{"method": "login", "data": {
                       "u": secret_value(self.settings.fantrax_username),
                       "p": secret_value(self.settings.fantrax_password),
                       "t": "", "v": 3}}],
                   "uiv": 3, "refUrl": LOGIN_PAGE, "dt": 0, "at": 0, "tz": "UTC"}
        try:
            resp = session.post(FXPA_URL, params={"lgnu": "1"}, json=payload, timeout=30)
        except requests.RequestException as e:
            raise self._fail(f"could not reach Fantrax ({type(e).__name__}).") from None
        status = getattr(resp, "status_code", 200)
        try:
            body = resp.json()
        except ValueError:
            raise self._fail(f"Fantrax returned a non-JSON response (HTTP {status}); the login endpoint "
                             "may have changed.", code=f"HTTP_{status}") from None
        if not isinstance(body, dict):
            raise self._fail("unexpected response shape; the login endpoint may have changed.", code="SHAPE")
        if isinstance(body.get("pageError"), dict):
            code = str(body["pageError"].get("code") or "PAGE_ERROR")
            raise self._fail(f"Fantrax answered {code}; the login endpoint may have changed.", code=code)
        responses = body.get("responses")
        first = responses[0] if isinstance(responses, list) and responses and isinstance(responses[0], dict) else {}
        data = first.get("data") if isinstance(first.get("data"), dict) else {}
        errors = [e for e in (first.get("errors") or data.get("errors") or []) if isinstance(e, dict)]
        user = data.get("userInfo")
        if isinstance(user, dict) and user:
            self._update_state(last_login_ok=True, last_login_error=None, last_login_at=self.clock())
            return session
        if data.get("tfa"):
            raise self._fail("the account uses two-factor authentication, which automated login cannot "
                             "complete.", code="TFA")
        if data.get("passwordExpired"):
            raise self._fail("Fantrax says the password has expired; change it at fantrax.com and update "
                             "FANTRAX_PASSWORD.", code="PASSWORD_EXPIRED")
        code = str(errors[0].get("code") or "") if errors else ""
        reasons = {
            "INVALID_CREDENTIALS": "Fantrax rejected FANTRAX_USERNAME / FANTRAX_PASSWORD; check them in .env.",
            "BAD_INTERACTION": "Fantrax's reCAPTCHA check blocked the automated login.",
            "ACCOUNT_LOCKED": "Fantrax says the account is locked; log in at fantrax.com to unlock it.",
        }
        if code:
            raise self._fail(reasons.get(code, f"Fantrax answered {code}."), code=code)
        raise self._fail("Fantrax returned no user info; the login endpoint may have changed.", code="NO_USER")

    # -- status -----------------------------------------------------------
    def status(self) -> dict[str, Any]:
        """Presence-only summary for `fm auth fantrax --status` (no cookie or credential values)."""
        state = self._read_state()
        now = self.clock()
        saved_at = state.get("saved_at")
        cookies = self._saved_cookies(state) if state else {}

        def ts(key: str) -> float | None:
            v = state.get(key)
            return float(v) if isinstance(v, (int, float)) else None

        wait = self.seconds_until_login_allowed()
        return {
            "session_file": str(self.path),
            "saved_session": bool(state.get("cookies")),
            "saved_session_source": state.get("source"),
            "saved_session_age_seconds": (now - saved_at) if isinstance(saved_at, (int, float)) else None,
            "saved_session_expired": bool(state.get("expired")) or (bool(state.get("cookies")) and not cookies),
            "saved_session_has_auth_cookie": AUTH_COOKIE in cookies,
            "credentials": {
                "FANTRAX_USERNAME": bool(secret_value(self.settings.fantrax_username)),
                "FANTRAX_PASSWORD": bool(secret_value(self.settings.fantrax_password)),
                "FANTRAX_COOKIE": bool(secret_value(self.settings.fantrax_cookie)),
                "FANTRAX_COOKIE_FILE": bool(self.settings.fantrax_cookie_file),
            },
            "last_login_attempt": ts("last_login_attempt"),
            "last_login_ok": state.get("last_login_ok"),
            "last_login_error": state.get("last_login_error"),
            "last_ping_at": ts("last_ping_at"),
            "last_ping_ok": state.get("last_ping_ok"),
            "login_allowed_in_seconds": wait,
        }


def fmt_age(seconds: float | None) -> str:
    return "?" if seconds is None else _fmt_age(seconds)
