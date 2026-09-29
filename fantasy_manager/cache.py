"""SQLite-backed HTTP GET cache with per-call TTL and an offline mode.

Cookies are sent on the wire but never stored and never part of the cache key.
"""
from __future__ import annotations

import hashlib
import json
import sqlite3
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping

import httpx

# Request headers that change the response and therefore belong in the key.
KEY_HEADERS = ("x-fantasy-filter",)
USER_AGENT = "fantasy-manager/0.1 (+https://github.com/)"


class CacheMiss(Exception):
    """Raised in offline mode when a request is not in the cache."""


class HttpError(Exception):
    def __init__(self, status_code: int, url: str):
        super().__init__(f"HTTP {status_code} for {url}")
        self.status_code = status_code
        self.url = url


@dataclass
class CachedResponse:
    status_code: int
    text: str
    url: str
    from_cache: bool = False

    def json(self) -> Any:
        return json.loads(self.text)


def _norm_params(params: Mapping[str, Any] | None) -> list[tuple[str, Any]]:
    if not params:
        return []
    items = []
    for k, v in params.items():
        if isinstance(v, (list, tuple)):
            v = [str(x) for x in v]
        elif v is not None:
            v = str(v)
        items.append((str(k), v))
    return sorted(items, key=lambda kv: kv[0])


def cache_key(method: str, url: str, params: Mapping[str, Any] | None = None,
              headers: Mapping[str, str] | None = None) -> str:
    hdrs = {k.lower(): v for k, v in (headers or {}).items()}
    keyed = sorted((h, hdrs[h]) for h in KEY_HEADERS if h in hdrs)
    blob = json.dumps([method.upper(), url, _norm_params(params), keyed], sort_keys=True)
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()


class HttpCache:
    def __init__(self, data_dir: Path | str, offline: bool = False, timeout: float = 30.0):
        self.data_dir = Path(data_dir)
        self.data_dir.mkdir(parents=True, exist_ok=True)
        self.path = self.data_dir / "cache.db"
        self.offline = offline
        self._lock = threading.Lock()
        self._db = sqlite3.connect(self.path, check_same_thread=False)
        self._db.execute(
            "CREATE TABLE IF NOT EXISTS http_cache ("
            " key TEXT PRIMARY KEY, url TEXT NOT NULL, final_url TEXT, status INTEGER NOT NULL,"
            " body TEXT NOT NULL, fetched_at REAL NOT NULL)"
        )
        self._db.commit()
        self._client = httpx.Client(follow_redirects=True, timeout=timeout,
                                    headers={"User-Agent": USER_AGENT})

    # -- storage -----------------------------------------------------------
    def _lookup(self, key: str) -> tuple[str, int, str, float] | None:
        with self._lock:
            row = self._db.execute(
                "SELECT final_url, status, body, fetched_at FROM http_cache WHERE key=?", (key,)
            ).fetchone()
        return row

    def _store(self, key: str, url: str, resp: CachedResponse) -> None:
        with self._lock:
            self._db.execute(
                "INSERT OR REPLACE INTO http_cache(key, url, final_url, status, body, fetched_at)"
                " VALUES (?,?,?,?,?,?)",
                (key, url, resp.url, resp.status_code, resp.text, time.time()),
            )
            self._db.commit()

    def clear(self) -> None:
        with self._lock:
            self._db.execute("DELETE FROM http_cache")
            self._db.commit()

    # -- fetching ----------------------------------------------------------
    def fetch(self, url: str, params: Mapping[str, Any] | None = None,
              headers: Mapping[str, str] | None = None, cookies: Mapping[str, str] | None = None,
              ttl: float = 3600) -> CachedResponse:
        """GET with caching. Only 2xx responses are stored; others are returned as-is."""
        key = cache_key("GET", url, params, headers)
        row = self._lookup(key)
        if row is not None:
            final_url, status, body, fetched_at = row
            if self.offline or (time.time() - fetched_at) < ttl:
                return CachedResponse(status, body, final_url or url, from_cache=True)
        if self.offline:
            raise CacheMiss(f"offline and not cached: {url}")

        send_headers = dict(headers or {})
        if cookies:  # per-request cookies= is deprecated in httpx; send a Cookie header instead
            send_headers["Cookie"] = "; ".join(f"{k}={v}" for k, v in cookies.items() if v is not None)
        r = self._client.get(url, params=_httpx_params(params), headers=send_headers)
        resp = CachedResponse(r.status_code, r.text, str(r.url))
        if 200 <= r.status_code < 300:
            self._store(key, url, resp)
        return resp

    def get_text(self, url: str, params: Mapping[str, Any] | None = None,
                 headers: Mapping[str, str] | None = None, cookies: Mapping[str, str] | None = None,
                 ttl: float = 3600) -> str:
        resp = self.fetch(url, params=params, headers=headers, cookies=cookies, ttl=ttl)
        if not 200 <= resp.status_code < 300:
            raise HttpError(resp.status_code, resp.url)
        return resp.text

    def get_json(self, url: str, params: Mapping[str, Any] | None = None,
                 headers: Mapping[str, str] | None = None, cookies: Mapping[str, str] | None = None,
                 ttl: float = 3600) -> Any:
        return json.loads(self.get_text(url, params=params, headers=headers, cookies=cookies, ttl=ttl))

    def close(self) -> None:
        self._client.close()
        self._db.close()


def _httpx_params(params: Mapping[str, Any] | None) -> list[tuple[str, str]] | None:
    if not params:
        return None
    out: list[tuple[str, str]] = []
    for k, v in params.items():
        if v is None:
            continue
        if isinstance(v, (list, tuple)):
            out.extend((k, str(x)) for x in v)
        else:
            out.append((k, str(v)))
    return out


# -- espn_api integration ---------------------------------------------------

class _RequestsShim:
    """Stands in for the `requests` module inside espn_api.requests.espn_requests."""

    def __init__(self, cache: HttpCache, ttl: float, real_module: Any):
        self.cache = cache
        self.ttl = ttl
        self._real = real_module

    def get(self, url: str, params: Any = None, headers: Any = None, cookies: Any = None,
            **_: Any) -> CachedResponse:
        return self.cache.fetch(url, params=params, headers=headers, cookies=cookies, ttl=self.ttl)

    def __getattr__(self, name: str) -> Any:  # delegate anything else to real requests
        return getattr(self._real, name)


def install_espn_cache(cache: HttpCache, ttl: float = 900) -> None:
    """Route every espn_api HTTP GET through `cache`. Idempotent; re-calling updates cache/ttl."""
    from espn_api.requests import espn_requests

    current = espn_requests.requests
    if isinstance(current, _RequestsShim):
        current.cache = cache
        current.ttl = ttl
        return
    espn_requests.requests = _RequestsShim(cache, ttl, current)
