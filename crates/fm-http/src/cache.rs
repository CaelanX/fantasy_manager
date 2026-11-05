//! `cache.py`: SQLite-backed HTTP GET cache with per-call TTL and an offline mode.
//! Table (unchanged from Python):
//! `http_cache(key TEXT PRIMARY KEY, url TEXT NOT NULL, final_url TEXT, status INTEGER NOT NULL,
//!  body TEXT NOT NULL, fetched_at REAL NOT NULL)`; `INSERT OR REPLACE`; only 2xx stored.
//! Fresh when `now - fetched_at < ttl`; offline returns any row regardless of age, else CacheMiss.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
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

/// `json.dumps(value, sort_keys=...)` with Python's defaults (", " / ": " separators,
/// ensure_ascii): [`fm_pycompat::pyjson::dumps`].
pub(crate) fn py_dumps(v: &Value, sort_keys: bool) -> String {
    fm_pycompat::pyjson::dumps(v, &fm_pycompat::pyjson::DumpOpts { sort_keys, ..Default::default() })
}

pub(crate) fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub(crate) fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// `cache_key(method, url, params, headers)` = sha256 hex of
/// `json.dumps([METHOD, url, sorted [[k, v]] params, sorted [[lower header, value]] for
/// KEY_HEADERS], sort_keys=True)` (default separators, ensure_ascii).
pub fn cache_key(method: &str, url: &str, params: &Params, headers: &[(String, String)]) -> String {
    // _norm_params: sorted by key (stable; a Python dict has unique keys)
    let mut items: Vec<&(String, ParamValue)> = params.iter().collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let norm: Vec<Value> = items
        .into_iter()
        .map(|(k, v)| {
            let val = match v {
                ParamValue::Str(s) => Value::String(s.clone()),
                ParamValue::List(l) => Value::Array(l.iter().map(|x| Value::String(x.clone())).collect()),
                ParamValue::None => Value::Null,
            };
            Value::Array(vec![Value::String(k.clone()), val])
        })
        .collect();
    // hdrs = {k.lower(): v}: a later duplicate wins
    let mut keyed: Vec<Value> = Vec::new();
    for h in KEY_HEADERS {
        if let Some((_, v)) = headers.iter().rev().find(|(k, _)| k.to_lowercase() == *h) {
            keyed.push(Value::Array(vec![Value::String((*h).to_string()), Value::String(v.clone())]));
        }
    }
    let blob = Value::Array(vec![
        Value::String(method.to_uppercase()),
        Value::String(url.to_string()),
        Value::Array(norm),
        Value::Array(keyed),
    ]);
    sha256_hex(&py_dumps(&blob, true))
}

/// `HttpCache`: thread-safe (one connection behind a Mutex, like Python's lock). Optionally
/// backed by a parity [`Tape`] (replay: answers only from the tape).
pub struct HttpCache {
    pub data_dir: PathBuf,
    pub path: PathBuf,
    pub offline: bool,
    _db: Mutex<Option<Connection>>,
    _tape: Option<Tape>,
    client: OnceLock<reqwest::blocking::Client>,
}

impl HttpCache {
    /// `HttpCache(data_dir, offline=False, timeout=30.0)`: opens / creates `<data_dir>/cache.db`.
    pub fn open(data_dir: &Path, offline: bool) -> anyhow::Result<HttpCache> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join("cache.db");
        let db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS http_cache (\
             key TEXT PRIMARY KEY, url TEXT NOT NULL, final_url TEXT, status INTEGER NOT NULL,\
             body TEXT NOT NULL, fetched_at REAL NOT NULL)",
        )?;
        Ok(HttpCache {
            data_dir: data_dir.to_path_buf(),
            path,
            offline,
            _db: Mutex::new(Some(db)),
            _tape: None,
            client: OnceLock::new(),
        })
    }

    /// A cache that answers only from a parity tape (no SQLite, no network): every request
    /// must be on the tape, else CacheMiss.
    pub fn from_tape(tape: Tape) -> HttpCache {
        HttpCache {
            data_dir: PathBuf::new(),
            path: PathBuf::new(),
            offline: true,
            _db: Mutex::new(None),
            _tape: Some(tape),
            client: OnceLock::new(),
        }
    }

    fn http_client(&self) -> anyhow::Result<reqwest::blocking::Client> {
        if let Some(c) = self.client.get() {
            return Ok(c.clone());
        }
        let c = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(USER_AGENT)
            .build()?;
        Ok(self.client.get_or_init(|| c).clone())
    }

    /// `_store`: INSERT OR REPLACE (the caller decides what is worth storing).
    pub fn store(&self, key: &str, url: &str, resp: &CachedResponse) -> anyhow::Result<()> {
        let guard = self._db.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(db) = guard.as_ref() {
            db.execute(
                "INSERT OR REPLACE INTO http_cache(key, url, final_url, status, body, fetched_at) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![key, url, resp.url, resp.status_code as i64, resp.text, now_secs()],
            )?;
        }
        Ok(())
    }

    /// `fetch(url, params, headers, cookies, ttl)`: GET with caching; only 2xx stored; non-2xx
    /// returned as-is. Cookies are sent as one `Cookie:` header, never stored or keyed.
    pub fn fetch(
        &self,
        url: &str,
        params: &Params,
        headers: &[(String, String)],
        cookies: &[(String, String)],
        ttl: f64,
    ) -> anyhow::Result<CachedResponse> {
        let key = cache_key("GET", url, params, headers);
        if let Some(tape) = &self._tape {
            return match tape.http(&key) {
                Some(t) => Ok(CachedResponse {
                    status_code: t.status,
                    text: t.body.clone(),
                    url: if t.final_url.is_empty() { url.to_string() } else { t.final_url.clone() },
                    from_cache: true,
                }),
                None => Err(CacheMiss(format!("offline and not cached: {url}")).into()),
            };
        }
        if let Some((final_url, status, body, fetched_at)) = self.lookup(&key) {
            if self.offline || (now_secs() - fetched_at) < ttl {
                let u = match final_url {
                    Some(f) if !f.is_empty() => f,
                    _ => url.to_string(),
                };
                return Ok(CachedResponse { status_code: status, text: body, url: u, from_cache: true });
            }
        }
        if self.offline {
            return Err(CacheMiss(format!("offline and not cached: {url}")).into());
        }

        let client = self.http_client()?;
        let mut req = client.get(url);
        let query: Vec<(&str, &str)> = params
            .iter()
            .flat_map(|(k, v)| -> Vec<(&str, &str)> {
                match v {
                    ParamValue::Str(s) => vec![(k.as_str(), s.as_str())],
                    ParamValue::List(l) => l.iter().map(|x| (k.as_str(), x.as_str())).collect(),
                    ParamValue::None => vec![],
                }
            })
            .collect();
        if !query.is_empty() {
            req = req.query(&query);
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if !cookies.is_empty() {
            let c = cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
            req = req.header("Cookie", c);
        }
        let r = req.send()?;
        let status = r.status().as_u16();
        let final_url = r.url().to_string();
        let text = r.text()?;
        let resp = CachedResponse { status_code: status, text, url: final_url, from_cache: false };
        if (200..300).contains(&status) {
            self.store(&key, url, &resp)?;
        }
        Ok(resp)
    }

    /// `get_text`: fetch + HttpError on non-2xx.
    pub fn get_text(
        &self,
        url: &str,
        params: &Params,
        headers: &[(String, String)],
        cookies: &[(String, String)],
        ttl: f64,
    ) -> anyhow::Result<String> {
        let resp = self.fetch(url, params, headers, cookies, ttl)?;
        if !(200..300).contains(&resp.status_code) {
            return Err(HttpError { status_code: resp.status_code, url: resp.url }.into());
        }
        Ok(resp.text)
    }

    /// `get_json`.
    pub fn get_json(
        &self,
        url: &str,
        params: &Params,
        headers: &[(String, String)],
        cookies: &[(String, String)],
        ttl: f64,
    ) -> anyhow::Result<Value> {
        let text = self.get_text(url, params, headers, cookies, ttl)?;
        Ok(serde_json::from_str(&text)?)
    }

    /// The private `_lookup(key)` providers.enrich uses: (final_url, status, body, fetched_at).
    pub fn lookup(&self, key: &str) -> Option<(Option<String>, u16, String, f64)> {
        if let Some(tape) = &self._tape {
            return tape.http(key).map(|t| (Some(t.final_url.clone()), t.status, t.body.clone(), now_secs()));
        }
        let guard = self._db.lock().unwrap_or_else(|e| e.into_inner());
        let db = guard.as_ref()?;
        db.query_row(
            "SELECT final_url, status, body, fetched_at FROM http_cache WHERE key=?1",
            params![key],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, i64>(1)? as u16,
                    r.get::<_, String>(2)?,
                    r.get::<_, f64>(3)?,
                ))
            },
        )
        .optional()
        .ok()
        .flatten()
    }

    /// `clear(url_contains)`: delete rows (`instr(url, ?) > 0` when given); returns how many.
    pub fn clear(&self, url_contains: Option<&str>) -> anyhow::Result<usize> {
        let guard = self._db.lock().unwrap_or_else(|e| e.into_inner());
        let Some(db) = guard.as_ref() else { return Ok(0) };
        let n = match url_contains {
            None => db.execute("DELETE FROM http_cache", [])?,
            Some(s) => db.execute("DELETE FROM http_cache WHERE instr(url, ?1) > 0", params![s])?,
        };
        Ok(n)
    }
}
