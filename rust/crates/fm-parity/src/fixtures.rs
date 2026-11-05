//! Fixture loading (owner: W1-parity-harness). Status: TODO bodies.

use std::path::PathBuf;

use fm_core::snapshot::Snapshot;
use fm_core::{DynastyValue, Map, PlayerValue};
use serde_json::Value;

/// One league's fixture directory (`rust/parity/fixtures/<league>`).
pub struct Fixtures {
    pub league: String,
    pub dir: PathBuf,
}

impl Fixtures {
    /// Root: env `FM_PARITY_FIXTURES`, else `<workspace>/parity/fixtures`.
    pub fn root() -> PathBuf {
        std::env::var_os("FM_PARITY_FIXTURES")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures"))
    }

    pub fn league(name: &str) -> Fixtures {
        Fixtures { league: name.to_string(), dir: Self::root().join(name) }
    }

    /// Both leagues ("espn", "fantrax").
    pub fn all() -> Vec<Fixtures> {
        vec![Self::league("espn"), Self::league("fantrax")]
    }

    /// `snapshot.json` with registries attached to `ctx.enrich`.
    pub fn snapshot(&self) -> Snapshot {
        todo!("W1-parity-harness")
    }

    /// `values.json` as typed values (players re-attached from the snapshot ctx by cid).
    pub fn values(&self, _snap: &Snapshot) -> Map<String, PlayerValue> {
        todo!("W1-parity-harness")
    }

    /// `dynasty.json` (None when not a dynasty league).
    pub fn dynasty(&self, _snap: &Snapshot) -> Option<Map<String, DynastyValue>> {
        todo!("W1-parity-harness")
    }

    /// Raw JSON of any fixture file relative to the league dir (e.g. "engines/trades.json").
    pub fn json(&self, _rel: &str) -> Value {
        todo!("W1-parity-harness")
    }

    /// `cli/<name>.json` -> its "output" value.
    pub fn cli_output(&self, _name: &str) -> Value {
        todo!("W1-parity-harness")
    }

    /// `tape.json.gz` path if present (gitignored: tests needing it skip when absent).
    pub fn tape_path(&self) -> Option<PathBuf> {
        let p = self.dir.join("tape.json.gz");
        p.exists().then_some(p)
    }

    /// `fixtures/_datadir` copied into a fresh temp dir (for CLI / plan-memory / status-history
    /// tests); the TempDir must be kept alive by the caller.
    pub fn temp_datadir() -> anyhow::Result<(PathBuf, Box<dyn std::any::Any>)> {
        todo!("W1-parity-harness")
    }
}
