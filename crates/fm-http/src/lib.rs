//! `fantasy_manager/cache.py` + the Fantrax fxpa JSON-file cache + parity tapes.
//! Public API is FIXED by the architect (other wave-2 packages code against it); bodies are
//! work package `W2-http-sources`.
//!
//! Byte-compatibility: Rust and Python share `<FM_DATA_DIR>/cache.db` and
//! `<FM_DATA_DIR>/fantrax_cache/*.json`, so [`cache_key`] / [`fxpa_key`] must produce the exact
//! Python hashes (sha256 of `json.dumps(..., sort_keys=True)` with default separators and
//! ensure_ascii). Golden tests: compute keys in Python for a few URLs/params and assert equality.
//!
//! Blocking I/O (reqwest::blocking): the CLI is synchronous like Python; the web server calls
//! into the pipeline from `spawn_blocking`.

pub mod cache;
pub mod fxpa;
pub mod tape;

pub use cache::{cache_key, CacheMiss, CachedResponse, HttpCache, HttpError, ParamValue, Params, KEY_HEADERS, USER_AGENT};
pub use fxpa::{fxpa_key, FxpaFileCache};
pub use tape::{params_from_json, Tape};

/// `fetch_text(url, params, ttl) -> str` callables the Python source clients take
/// (`MoneyPuckClient(fetch_text)`, `DailyFaceoffClient(fetch_text)`).
pub type FetchText<'a> = dyn Fn(&str, &Params, f64) -> anyhow::Result<String> + Send + Sync + 'a;
/// `fetch_json(url, params, ttl) -> Any` (`NhlClient(fetch_json, season)`).
pub type FetchJson<'a> = dyn Fn(&str, &Params, f64) -> anyhow::Result<serde_json::Value> + Send + Sync + 'a;
