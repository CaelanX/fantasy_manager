//! `cache.py`: SQLite-backed HTTP GET cache with per-call TTL and an offline mode.
//! Table (unchanged from Python):
//! `http_cache(key TEXT PRIMARY KEY, url TEXT NOT NULL, final_url TEXT, status INTEGER NOT NULL,
//!  body TEXT NOT NULL, fetched_at REAL NOT NULL)`; `INSERT OR REPLACE`; only 2xx stored.
//! Fresh when `now - fetched_at < ttl`; offline returns any row regardless of age, else CacheMiss.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;
use thiserror::Error;

use crate::tape::Tape;

/// Request headers that change the response and therefore belong in the key.
pub const KEY_HEADERS: &[&str] = &["x-fantasy-filter"];
pub const USER_AGENT: &str = "fantasy-manager/0.1 (+https://github.com/)";

/// Query params in insertion order. A Python value that is a list becomes several entries with
/// the same key ([`ParamValue::List`]); `None` stays in the key (JSON null) but is not sent.
pub type Params = Vec<(String, ParamValue)>;

#[derive(Clone, Debug, PartialEq)]
pub enum ParamValue {
    /// `str(v)` of the Python value (Python's str: True -> "True", ints, float repr)
    Str(String),
    List(Vec<String>),
    None,
}

impl From<&str> for ParamValue {
    fn from(s: &str) -> Self {
        ParamValue::Str(s.to_string())
    }
}
impl From<String> for ParamValue {
    fn from(s: String) -> Self {
        ParamValue::Str(s)
    }
}
impl From<i64> for ParamValue {
    fn from(n: i64) -> Self {
        ParamValue::Str(n.to_string())
    }
}

/// Raised in offline mode when a request is not in the cache (`cache.CacheMiss`).
#[derive(Debug, Error)]
#[error("{0}")]
pub struct CacheMiss(pub String);

/// `cache.HttpError`: non-2xx from `get_text` / `get_json`.
#[derive(Debug, Error)]
#[error("HTTP {status_code} for {url}")]
pub struct HttpError {
    pub status_code: u16,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct CachedResponse {
    pub status_code: u16,
    pub text: String,
    pub url: String,
    pub from_cache: bool,
}

impl CachedResponse {
    pub fn json(&self) -> anyhow::Result<Value> {
        Ok(serde_json::from_str(&self.text)?)
    }
}

/// `cache_key(method, url, params, headers)` = sha256 hex of
/// `json.dumps([METHOD, url, sorted [[k, v]] params, sorted [[lower header, value]] for
/// KEY_HEADERS], sort_keys=True)` (default separators, ensure_ascii).
pub fn cache_key(_method: &str, _url: &str, _params: &Params, _headers: &[(String, String)]) -> String {
    todo!("W2-http-sources")
}

/// `HttpCache`: thread-safe (one connection behind a Mutex, like Python's lock). Optionally
/// backed by a parity [`Tape`] (replay: answers only from the tape; record: appends).
pub struct HttpCache {
    pub data_dir: PathBuf,
    pub path: PathBuf,
    pub offline: bool,
    _db: Mutex<Option<rusqlite::Connection>>,
    _tape: Option<Tape>,
}

impl HttpCache {
    /// `HttpCache(data_dir, offline=False, timeout=30.0)`: opens / creates `<data_dir>/cache.db`.
    pub fn open(_data_dir: &Path, _offline: bool) -> anyhow::Result<HttpCache> {
        todo!("W2-http-sources")
    }

    /// A cache that answers only from a parity tape (no SQLite, no network): every request
    /// must be on the tape, else CacheMiss.
    pub fn from_tape(_tape: Tape) -> HttpCache {
        todo!("W2-http-sources")
    }

    /// `fetch(url, params, headers, cookies, ttl)`: GET with caching; only 2xx stored; non-2xx
    /// returned as-is. Cookies are sent as one `Cookie:` header, never stored or keyed.
    pub fn fetch(
        &self,
        _url: &str,
        _params: &Params,
        _headers: &[(String, String)],
        _cookies: &[(String, String)],
        _ttl: f64,
    ) -> anyhow::Result<CachedResponse> {
        todo!("W2-http-sources")
    }

    /// `get_text`: fetch + HttpError on non-2xx.
    pub fn get_text(
        &self,
        _url: &str,
        _params: &Params,
        _headers: &[(String, String)],
        _cookies: &[(String, String)],
        _ttl: f64,
    ) -> anyhow::Result<String> {
        todo!("W2-http-sources")
    }

    /// `get_json`.
    pub fn get_json(
        &self,
        _url: &str,
        _params: &Params,
        _headers: &[(String, String)],
        _cookies: &[(String, String)],
        _ttl: f64,
    ) -> anyhow::Result<Value> {
        todo!("W2-http-sources")
    }

    /// The private `_lookup(key)` providers.enrich uses: (final_url, status, body, fetched_at).
    pub fn lookup(&self, _key: &str) -> Option<(Option<String>, u16, String, f64)> {
        todo!("W2-http-sources")
    }

    /// `clear(url_contains)`: delete rows (`instr(url, ?) > 0` when given); returns how many.
    pub fn clear(&self, _url_contains: Option<&str>) -> anyhow::Result<usize> {
        todo!("W2-http-sources")
    }
}
