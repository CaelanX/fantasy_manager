//! The Fantrax fxpa response cache (`providers/fantrax.py` FxpaClient._cached_post):
//! `<data_dir>/fantrax_cache/<fxpa_key>.json` = `{"fetched_at": float, "body": {...}}`.
//! TTL 15 min (2 min when a message is getTeamRosterInfo); offline ignores TTL; corrupt files
//! are ignored. The POST itself (session, relogin) lives in `fm_providers::fantrax`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cache::{now_secs, py_dumps, sha256_hex};

/// sha256 hex of `json.dumps({"league": league_id, "msgs": msgs}, sort_keys=True)`.
/// `msgs` are fantraxapi `Method.msg_block`s: `{"method": m, "data": {"leagueId": id, k: str(v)...}}`.
pub fn fxpa_key(league_id: &str, msgs: &[Value]) -> String {
    let mut m = serde_json::Map::new();
    m.insert("league".to_string(), Value::String(league_id.to_string()));
    m.insert("msgs".to_string(), Value::Array(msgs.to_vec()));
    sha256_hex(&py_dumps(&Value::Object(m), true))
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
    pub fn get(&self, key: &str, ttl: f64) -> Option<Value> {
        let text = std::fs::read_to_string(self.dir.join(format!("{key}.json"))).ok()?;
        let entry: Value = serde_json::from_str(&text).ok()?;
        let fetched_at = match entry.get("fetched_at")? {
            Value::Number(n) => n.as_f64()?,
            Value::String(s) => s.trim().parse::<f64>().ok()?,
            _ => return None,
        };
        let body = entry.get("body")?;
        if self.offline || now_secs() - fetched_at < ttl {
            Some(body.clone())
        } else {
            None
        }
    }

    /// Store a body (`{"fetched_at": now, "body": body}`, plain `json.dumps`).
    pub fn put(&self, key: &str, body: &Value) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut m = serde_json::Map::new();
        m.insert("fetched_at".to_string(), Value::from(now_secs()));
        m.insert("body".to_string(), body.clone());
        std::fs::write(self.dir.join(format!("{key}.json")), py_dumps(&Value::Object(m), false))?;
        Ok(())
    }

    /// `clear_cache(data_dir)`: delete every cached response; returns how many.
    pub fn clear(&self) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return 0 };
        let mut n = 0;
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "json").unwrap_or(false) && p.is_file() && std::fs::remove_file(&p).is_ok() {
                n += 1;
            }
        }
        n
    }
}
