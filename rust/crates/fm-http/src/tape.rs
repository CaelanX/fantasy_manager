//! Parity tapes: `rust/parity/fixtures/<league>/tape.json.gz` written by `dump_fixtures.py`:
//! `{"schema": 1, "http": {cache_key: {url, params, key_headers, status, final_url, body}},
//!   "fxpa": {fxpa_key: {league, msgs, body}}}`. Replaying a tape through [`crate::HttpCache`] /
//! the Fantrax client lets providers + enrichment run with zero network and no cache.db, and it
//! proves the Rust cache-key derivation (a wrong key = a tape miss).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize)]
pub struct TapeHttp {
    pub url: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub key_headers: HashMap<String, String>,
    pub status: u16,
    pub final_url: String,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TapeFxpa {
    pub league: String,
    pub msgs: Vec<Value>,
    pub body: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TapeData {
    #[serde(default)]
    pub http: HashMap<String, TapeHttp>,
    #[serde(default)]
    pub fxpa: HashMap<String, TapeFxpa>,
}

/// Shared, read-only tape.
#[derive(Clone, Debug, Default)]
pub struct Tape(pub Arc<TapeData>);

impl Tape {
    /// Load a `.json.gz` (or plain `.json`) tape.
    pub fn load(_path: &Path) -> anyhow::Result<Tape> {
        todo!("W2-http-sources")
    }

    pub fn http(&self, key: &str) -> Option<&TapeHttp> {
        self.0.http.get(key)
    }

    pub fn fxpa(&self, key: &str) -> Option<&TapeFxpa> {
        self.0.fxpa.get(key)
    }
}
