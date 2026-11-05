//! The Fantrax fxpa response cache (`providers/fantrax.py` FxpaClient._cached_post):
//! `<data_dir>/fantrax_cache/<fxpa_key>.json` = `{"fetched_at": float, "body": {...}}`.
//! TTL 15 min (2 min when a message is getTeamRosterInfo); offline ignores TTL; corrupt files
//! are ignored. The POST itself (session, relogin) lives in `fm_providers::fantrax`.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// sha256 hex of `json.dumps({"league": league_id, "msgs": msgs}, sort_keys=True)`.
/// `msgs` are fantraxapi `Method.msg_block`s: `{"method": m, "data": {"leagueId": id, k: str(v)...}}`.
pub fn fxpa_key(_league_id: &str, _msgs: &[Value]) -> String {
    todo!("W2-http-sources")
}

pub struct FxpaFileCache {
    pub dir: PathBuf,
    pub offline: bool,
}

impl FxpaFileCache {
    pub fn new(data_dir: &Path, offline: bool) -> Self {
        FxpaFileCache { dir: data_dir.join("fantrax_cache"), offline }
    }

    /// Cached body for `key` if present and fresh (any age when offline).
    pub fn get(&self, _key: &str, _ttl: f64) -> Option<Value> {
        todo!("W2-http-sources")
    }

    /// Store a body (`{"fetched_at": now, "body": body}`, plain `json.dumps`).
    pub fn put(&self, _key: &str, _body: &Value) -> anyhow::Result<()> {
        todo!("W2-http-sources")
    }

    /// `clear_cache(data_dir)`: delete every cached response; returns how many.
    pub fn clear(&self) -> usize {
        todo!("W2-http-sources")
    }
}
