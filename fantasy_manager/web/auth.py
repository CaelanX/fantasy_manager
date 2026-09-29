"""Dashboard access control: one shared password, a signed session cookie, and a loopback-only
fallback when no password is set.

Three policies, chosen by ``auth_from_settings`` (``FM_WEB_PASSWORD`` / ``FM_WEB_ALLOW_INSECURE``):

* **password** (``FM_WEB_PASSWORD`` set): every route except ``/login``, ``/logout``, ``/healthz``
  and ``/static/*`` needs a valid ``fm_session`` cookie, which ``POST /login`` sets after a
  constant-time password check. The cookie is ``<issued>.<nonce>.<HMAC-SHA256>``: signed with
  ``FM_WEB_SECRET`` or a random key kept in ``<FM_DATA_DIR>/web_secret`` (mode 0600), bound to a
  fingerprint of the password (changing the password logs everyone out) and valid for
  ``FM_WEB_SESSION_DAYS`` days. HttpOnly, SameSite=Lax, and Secure when the request came over
  HTTPS (directly or via ``X-Forwarded-Proto`` from a trusted proxy). Failed logins are limited to
  5 per 15 minutes per client IP (in memory) and logged without the password.
* **local only** (no password, ``FM_WEB_ALLOW_INSECURE`` unset): the dashboard works exactly as
  before for loopback clients and answers 403 to anyone else, so a forgotten password behind a
  reverse proxy fails closed.
* **open** (no password, ``FM_WEB_ALLOW_INSECURE=1``): no checks (trusted LAN only).

``create_app(auth=None)`` (the test default) installs nothing. The module stays importable without
FastAPI; ``install`` imports it lazily.
"""
# No `from __future__ import annotations`: the route handlers in install() annotate `request:
# Request` with a locally imported class, which FastAPI must see as a real object, not a string.
import base64
import hashlib
import hmac
import ipaddress
import logging
import os
import secrets
import threading
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable
from urllib.parse import parse_qsl, quote, urlsplit, urlunsplit

log = logging.getLogger(__name__)

COOKIE_NAME = "fm_session"
SECRET_FILE = "web_secret"
DEFAULT_SESSION_DAYS = 30
MAX_FAILURES = 5
FAILURE_WINDOW = 15 * 60.0
DEFAULT_TRUSTED_PROXIES = ("127.0.0.1", "::1")
PUBLIC_PATHS = frozenset({"/login", "/logout", "/healthz"})
PUBLIC_PREFIXES = ("/static/",)
MIN_PASSWORD_LENGTH = 12
_TOKEN_VERSION = "v1"


class InsecureBindError(ValueError):
    """Refused to listen on a non-loopback interface without a dashboard password."""


# --------------------------------------------------------------------------- hosts and proxies

def is_loopback_host(host: str | None) -> bool:
    """True for 127.0.0.0/8, ::1 and "localhost"; False for 0.0.0.0, LAN/public IPs and names."""
    if not host:
        return False
    h = host.strip().strip("[]").lower()
    if h == "localhost":
        return True
    try:
        return ipaddress.ip_address(h).is_loopback
    except ValueError:
        return False


def check_bind(host: str, password_set: bool, allow_insecure: bool) -> None:
    """Raise InsecureBindError when binding ``host`` would expose an unprotected dashboard."""
    if password_set or allow_insecure or is_loopback_host(host):
        return
    raise InsecureBindError(
        f"refusing to serve the dashboard on {host} without a password: anyone who can reach this "
        "machine could read your league data and change settings. Set FM_WEB_PASSWORD in .env "
        "(see docs/hosting.md), bind to 127.0.0.1, or set FM_WEB_ALLOW_INSECURE=1 on a trusted "
        "home network only.")


def parse_proxies(raw: str | None) -> frozenset[str]:
    items = [p.strip() for p in (raw or "").split(",") if p.strip()]
    return frozenset(items or DEFAULT_TRUSTED_PROXIES)


def _peer(request: Any) -> str:
    client = getattr(request, "client", None)
    return (getattr(client, "host", None) or "") if client else ""


def _first_header(request: Any, name: str) -> str | None:
    v = request.headers.get(name)
    return v.split(",")[0].strip() if v else None


def client_ip(request: Any, trusted: frozenset[str]) -> str:
    """The client's IP. ``X-Forwarded-For`` is used only when the direct peer is a trusted proxy
    (so a direct client can't spoof it to dodge the rate limit); its last entry is the address
    the proxy saw. Under ``uvicorn --proxy-headers`` the peer is already the real client."""
    peer = _peer(request)
    if peer in trusted:
        xff = request.headers.get("x-forwarded-for")
        if xff:
            hops = [h.strip() for h in xff.split(",") if h.strip()]
            if hops:
                return hops[-1]
    return peer or "unknown"


def is_https(request: Any, trusted: frozenset[str]) -> bool:
    if request.url.scheme == "https":
        return True
    return _peer(request) in trusted and (_first_header(request, "x-forwarded-proto") or "").lower() == "https"


# --------------------------------------------------------------------------- secret key

def load_or_create_secret(data_dir: Path) -> bytes:
    """The signing key from ``<data_dir>/web_secret``, created (random, 0600) on first use."""
    path = Path(data_dir) / SECRET_FILE
    try:
        text = path.read_text(encoding="utf-8").strip()
        if len(text) >= 32:
            return text.encode("utf-8")
    except FileNotFoundError:
        pass
    path.parent.mkdir(parents=True, exist_ok=True)
    value = secrets.token_hex(32)
    try:
        fd = os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:  # another worker created it first (or it was too short: replace)
        text = path.read_text(encoding="utf-8").strip()
        if len(text) >= 32:
            return text.encode("utf-8")
        fd = os.open(str(path), os.O_WRONLY | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as fh:
        fh.write(value + "\n")
    try:
        os.chmod(path, 0o600)
    except OSError:
        pass
    log.info("created the dashboard session key at %s", path)
    return value.encode("utf-8")


# --------------------------------------------------------------------------- policy

@dataclass
class AuthConfig:
    """``password`` None = no login; then ``allow_insecure`` decides between local-only and open."""
    password: str | None = None
    secret: bytes | Callable[[], bytes] | None = None
    allow_insecure: bool = False
    session_seconds: int = DEFAULT_SESSION_DAYS * 86400
    trusted_proxies: frozenset[str] = frozenset(DEFAULT_TRUSTED_PROXIES)
    max_failures: int = MAX_FAILURES
    failure_window: float = FAILURE_WINDOW
    _key: bytes | None = field(default=None, init=False, repr=False)
    _lock: threading.Lock = field(default_factory=threading.Lock, init=False, repr=False)

    def __repr__(self) -> str:  # never show the password or key
        mode = "password" if self.password else ("open" if self.allow_insecure else "local-only")
        return f"AuthConfig(mode={mode!r})"

    @property
    def mode(self) -> str:
        return "password" if self.password else ("open" if self.allow_insecure else "local-only")

    def key(self) -> bytes:
        """The signing key (resolved lazily, so building the app writes nothing)."""
        if self._key is None:
            with self._lock:
                if self._key is None:
                    s = self.secret
                    if callable(s):
                        s = s()
                    if not s:
                        log.warning("no dashboard session key available; using a temporary one "
                                    "(everyone is logged out on restart)")
                        s = secrets.token_bytes(32)
                    self._key = s if isinstance(s, bytes) else str(s).encode("utf-8")
        return self._key

    # -- password + session tokens
    def check_password(self, attempt: str) -> bool:
        """Constant-time compare (digests first, so the length doesn't leak either)."""
        if not self.password:
            return False
        a = hashlib.sha256(attempt.encode("utf-8")).digest()
        b = hashlib.sha256(self.password.encode("utf-8")).digest()
        return hmac.compare_digest(a, b)

    def _pw_tag(self) -> str:
        return hmac.new(self.key(), b"pw:" + (self.password or "").encode("utf-8"), hashlib.sha256).hexdigest()[:16]

    def _sign(self, body: str) -> str:
        mac = hmac.new(self.key(), f"{body}|{self._pw_tag()}".encode("utf-8"), hashlib.sha256).digest()
        return base64.urlsafe_b64encode(mac).rstrip(b"=").decode("ascii")

    def make_token(self, now: float | None = None) -> str:
        issued = int(time.time() if now is None else now)
        body = f"{_TOKEN_VERSION}.{issued}.{secrets.token_urlsafe(12)}"
        return f"{body}.{self._sign(body)}"

    def verify_token(self, token: str | None, now: float | None = None) -> bool:
        if not token or not self.password:
            return False
        parts = token.split(".")
        if len(parts) != 4 or parts[0] != _TOKEN_VERSION:
            return False
        body, sig = ".".join(parts[:3]), parts[3]
        if not hmac.compare_digest(sig.encode("ascii", "replace"), self._sign(body).encode("ascii")):
            return False
        try:
            issued = int(parts[1])
        except ValueError:
            return False
        now = time.time() if now is None else now
        return issued - 60 <= now < issued + self.session_seconds


def auth_from_settings(settings: Any = None) -> AuthConfig:
    """The policy for the real server (``fm web`` and ``uvicorn fantasy_manager.web.app:app``)."""
    if settings is None:
        from ..config import Settings

        settings = Settings()
    from ..config import secret_value

    # Stray whitespace (a CR from a Windows-edited .env) would make the password impossible to type.
    password = secret_value(getattr(settings, "fm_web_password", None)).strip() or None
    secret_env = secret_value(getattr(settings, "fm_web_secret", None)).strip()
    data_dir = Path(getattr(settings, "fm_data_dir", "./data"))

    def secret() -> bytes:
        if secret_env:
            return secret_env.encode("utf-8")
        try:
            return load_or_create_secret(data_dir)
        except OSError as e:
            log.warning("could not read or create %s (%s)", data_dir / SECRET_FILE, type(e).__name__)
            return b""

    days = int(getattr(settings, "fm_web_session_days", DEFAULT_SESSION_DAYS) or DEFAULT_SESSION_DAYS)
    cfg = AuthConfig(password=password, secret=secret,
                     allow_insecure=bool(getattr(settings, "fm_web_allow_insecure", False)),
                     session_seconds=max(1, days) * 86400,
                     trusted_proxies=parse_proxies(getattr(settings, "fm_web_trusted_proxies", None)))
    if password and len(password) < MIN_PASSWORD_LENGTH:
        log.warning("FM_WEB_PASSWORD is shorter than %d characters; use a long random passphrase "
                    "for an internet-facing dashboard", MIN_PASSWORD_LENGTH)
    return cfg


# --------------------------------------------------------------------------- rate limit

class LoginLimiter:
    """Failed login attempts per client IP in a sliding window (in memory, per process)."""

    def __init__(self, max_failures: int = MAX_FAILURES, window: float = FAILURE_WINDOW,
                 clock: Callable[[], float] = time.monotonic, max_clients: int = 10_000):
        self.max_failures, self.window, self.clock, self.max_clients = max_failures, window, clock, max_clients
        self._fails: dict[str, deque[float]] = {}
        self._lock = threading.Lock()

    def _prune(self, ip: str, now: float) -> deque[float]:
        q = self._fails.get(ip)
        if q is None:
            return deque()
        while q and q[0] <= now - self.window:
            q.popleft()
        if not q:
            self._fails.pop(ip, None)
        return q

    def retry_after(self, ip: str) -> int:
        """Seconds until ``ip`` may try again (0 = allowed now)."""
        with self._lock:
            now = self.clock()
            q = self._prune(ip, now)
            if len(q) < self.max_failures:
                return 0
            return max(1, int(q[0] + self.window - now + 0.999))

    def fail(self, ip: str) -> int:
        """Record a failure; returns the failures in the current window."""
        with self._lock:
            now = self.clock()
            if len(self._fails) >= self.max_clients:
                for other in list(self._fails):
                    self._prune(other, now)
                if len(self._fails) >= self.max_clients:  # still full: drop the oldest client
                    self._fails.pop(next(iter(self._fails)))
            q = self._prune(ip, now)
            q.append(now)
            self._fails[ip] = q
            return len(q)

    def reset(self, ip: str) -> None:
        with self._lock:
            self._fails.pop(ip, None)


# --------------------------------------------------------------------------- FastAPI wiring

def safe_login_next(target: str | None) -> str:
    """A same-site path to return to after login ("/" for anything else, never /login itself)."""
    t = target or "/"
    if (not t.startswith("/") or t.startswith("//") or "\\" in t
            or any(ord(c) < 32 or ord(c) == 127 for c in t)):
        return "/"
    parts = urlsplit(t)
    if parts.scheme or parts.netloc or parts.path in ("/login", "/logout"):
        return "/"
    return urlunsplit(("", "", parts.path or "/", parts.query, ""))


def is_public(path: str) -> bool:
    return path in PUBLIC_PATHS or path.startswith(PUBLIC_PREFIXES)


def install(app: Any, cfg: AuthConfig, templates: Any, *, clock: Callable[[], float] = time.monotonic) -> None:
    """Add the access-control middleware (and, with a password, the /login + /logout routes)."""
    from fastapi import Request
    from fastapi.responses import JSONResponse, RedirectResponse, Response

    app.state.auth = cfg
    templates.env.globals["auth_enabled"] = bool(cfg.password)
    if cfg.mode == "open":
        log.warning("dashboard running WITHOUT a password (FM_WEB_ALLOW_INSECURE=1)")
        return
    limiter = LoginLimiter(cfg.max_failures, cfg.failure_window, clock)
    app.state.login_limiter = limiter

    def authed(request: Request) -> bool:
        return cfg.verify_token(request.cookies.get(COOKIE_NAME))

    def here(request: Request) -> str:
        q = request.url.query
        return request.url.path + (f"?{q}" if q else "")

    @app.middleware("http")
    async def access_control(request: Request, call_next):
        path = request.url.path
        if cfg.mode == "local-only":
            ip = client_ip(request, cfg.trusted_proxies)
            if is_loopback_host(ip) or path == "/healthz":
                return await call_next(request)
            log.warning("refused dashboard request from %s: no FM_WEB_PASSWORD set", ip)
            return Response("This dashboard only answers on this machine until FM_WEB_PASSWORD is set "
                            "(see docs/hosting.md).\n", status_code=403, media_type="text/plain")
        if is_public(path) or authed(request):
            return await call_next(request)
        login = "/login?next=" + quote(here(request), safe="")
        if request.headers.get("hx-request"):
            return Response(status_code=401, headers={"HX-Redirect": login})
        if request.method in ("GET", "HEAD") and not path.startswith("/api/"):
            return RedirectResponse(login, status_code=303)
        return JSONResponse({"detail": "login required"}, status_code=401,
                            headers={"WWW-Authenticate": 'Cookie realm="fantasy-manager"'})

    if not cfg.password:
        return

    def login_page(request: Request, status_code: int = 200, error: str | None = None,
                   nxt: str = "/", headers: dict[str, str] | None = None):
        resp = templates.TemplateResponse(request, "login.html", {"error": error, "next": nxt},
                                          status_code=status_code, headers=headers)
        resp.headers["Cache-Control"] = "no-store"
        return resp

    @app.get("/login")
    def login_form(request: Request, next: str = "/"):
        nxt = safe_login_next(next)
        if authed(request):
            return RedirectResponse(nxt, status_code=303)
        return login_page(request, nxt=nxt)

    @app.post("/login")
    async def login_submit(request: Request):
        body = (await request.body())[:8192]
        fields = dict(parse_qsl(body.decode("utf-8", "replace"), keep_blank_values=True))
        nxt = safe_login_next(fields.get("next"))
        ip = client_ip(request, cfg.trusted_proxies)
        wait = limiter.retry_after(ip)
        if wait:
            log.warning("dashboard login from %s blocked: too many failed attempts (retry in %ds)", ip, wait)
            mins = max(1, (wait + 59) // 60)
            return login_page(request, 429, f"Too many failed attempts. Try again in {mins} minute"
                              f"{'s' if mins != 1 else ''}.", nxt, headers={"Retry-After": str(wait)})
        if not cfg.check_password(fields.get("password", "")):
            n = limiter.fail(ip)
            log.warning("dashboard login failed from %s (%d of %d allowed in %d min)", ip, n,
                        cfg.max_failures, int(cfg.failure_window // 60))
            return login_page(request, 401, "Wrong password.", nxt)
        limiter.reset(ip)
        log.info("dashboard login from %s", ip)
        resp = RedirectResponse(nxt, status_code=303)
        resp.set_cookie(COOKIE_NAME, cfg.make_token(), max_age=cfg.session_seconds, path="/",
                        httponly=True, samesite="lax", secure=is_https(request, cfg.trusted_proxies))
        return resp

    @app.post("/logout")
    def logout(request: Request):
        resp = RedirectResponse("/login", status_code=303)
        resp.delete_cookie(COOKIE_NAME, path="/", httponly=True, samesite="lax",
                           secure=is_https(request, cfg.trusted_proxies))
        return resp


__all__ = ["AuthConfig", "COOKIE_NAME", "InsecureBindError", "LoginLimiter", "auth_from_settings",
           "check_bind", "client_ip", "install", "is_https", "is_loopback_host", "load_or_create_secret",
           "safe_login_next"]
