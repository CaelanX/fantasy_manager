//! Fixture loading (owner: W1-parity-harness).
//!
//! Root: env `FM_PARITY_FIXTURES`, else `<workspace>/parity/fixtures`. The gitignored files
//! (`tape.json.gz`, `_datadir/harness.db`, `_datadir/fantrax_cache/`) may be absent: helpers for
//! them return `Option` and tests call [`skip`] (prints a `SKIP:` line) instead of failing. The
//! tracked files (snapshot, values, engines, cli, ...) are required: a missing one panics with
//! the path, since that means a broken checkout, not an optional artifact. Use [`Fixtures::has`] /
//! [`Fixtures::try_json`] to probe first.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use fm_core::snapshot::Snapshot;
use fm_core::{DynastyValue, Map, PickAsset, Player, PlayerValue};
use serde_json::Value;

/// Print a clear SKIP line (tests that need an absent gitignored fixture call this and return).
pub fn skip(test: &str, reason: &str) {
    eprintln!("SKIP: {test}: {reason}");
}

/// One league's fixture directory (`parity/fixtures/<league>`).
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

    /// Whether the league directory has its tracked core (`snapshot.json`).
    pub fn available(&self) -> bool {
        self.dir.join("snapshot.json").exists()
    }

    /// File relative to the league dir exists.
    pub fn has(&self, rel: &str) -> bool {
        self.dir.join(rel).exists()
    }

    /// `snapshot.json` with registries attached to `ctx.enrich`.
    pub fn snapshot(&self) -> Snapshot {
        self.try_snapshot().unwrap_or_else(|e| panic!("{e:#}"))
    }

    pub fn try_snapshot(&self) -> anyhow::Result<Snapshot> {
        Snapshot::load(self.dir.join("snapshot.json"))
    }

    /// cid -> player for every player in the snapshot's context.
    fn players_by_cid(snap: &Snapshot) -> HashMap<&str, &Player> {
        snap.ctx.all_players().into_iter().map(|p| (p.cid.as_str(), p)).collect()
    }

    /// Re-attach `player` (stored as `cid` in the fixture) and deserialize.
    fn attach<T: serde::de::DeserializeOwned>(
        what: &str,
        raw: &Value,
        snap: &Snapshot,
    ) -> Map<String, T> {
        let players = Self::players_by_cid(snap);
        let obj = raw.as_object().unwrap_or_else(|| panic!("{what}: expected a JSON object"));
        let mut out = Map::new();
        for (cid, v) in obj {
            let mut o = v.as_object().unwrap_or_else(|| panic!("{what}.{cid}: not an object")).clone();
            o.shift_remove("cid");
            let p = players
                .get(cid.as_str())
                .unwrap_or_else(|| panic!("{what}.{cid}: player not in snapshot ctx"));
            o.insert("player".into(), serde_json::to_value(p).expect("serialize player"));
            let t: T = serde_json::from_value(Value::Object(o))
                .unwrap_or_else(|e| panic!("{what}.{cid}: {e}"));
            out.insert(cid.clone(), t);
        }
        out
    }

    /// `values.json` as typed values (players re-attached from the snapshot ctx by cid).
    pub fn values(&self, snap: &Snapshot) -> Map<String, PlayerValue> {
        Self::attach(&format!("{}/values.json", self.league), &self.json("values.json"), snap)
    }

    /// `dynasty.json` (None when not a dynasty league: the file holds `null`).
    pub fn dynasty(&self, snap: &Snapshot) -> Option<Map<String, DynastyValue>> {
        let raw = self.json("dynasty.json");
        if raw.is_null() {
            return None;
        }
        Some(Self::attach(&format!("{}/dynasty.json", self.league), &raw, snap))
    }

    /// `picks.json`: holder team id -> pick assets (empty map when there are none).
    pub fn picks(&self) -> Map<String, Vec<PickAsset>> {
        serde_json::from_value(self.json("picks.json"))
            .unwrap_or_else(|e| panic!("{}/picks.json: {e}", self.league))
    }

    /// Raw JSON of any fixture file relative to the league dir (e.g. "engines/trades.json").
    pub fn json(&self, rel: &str) -> Value {
        self.try_json(rel).unwrap_or_else(|e| panic!("{e:#}"))
    }

    /// Like [`Fixtures::json`] but `Err` instead of panicking (missing file, bad JSON).
    pub fn try_json(&self, rel: &str) -> anyhow::Result<Value> {
        let p = self.dir.join(rel);
        let text = std::fs::read_to_string(&p).with_context(|| format!("reading fixture {}", p.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing fixture {}", p.display()))
    }

    /// `engines/<name>.json` (e.g. "trades", "waivers").
    pub fn engine(&self, name: &str) -> Value {
        self.json(&format!("engines/{name}.json"))
    }

    /// `cli/<name>.json` whole: `{"args", "exit_code", "output"}`.
    pub fn cli(&self, name: &str) -> Value {
        self.json(&format!("cli/{name}.json"))
    }

    /// `cli/<name>.json` -> its "output" value.
    pub fn cli_output(&self, name: &str) -> Value {
        match self.cli(name) {
            Value::Object(mut o) => o
                .shift_remove("output")
                .unwrap_or_else(|| panic!("{}/cli/{name}.json has no \"output\"", self.league)),
            _ => panic!("{}/cli/{name}.json is not an object", self.league),
        }
    }

    /// `raw_ctx.json`: `provider.load()` before enrichment.
    pub fn raw_ctx(&self) -> Value {
        self.json("raw_ctx.json")
    }

    /// `tape.json.gz` path if present (gitignored: tests needing it skip when absent).
    pub fn tape_path(&self) -> Option<PathBuf> {
        let p = self.dir.join("tape.json.gz");
        p.exists().then_some(p)
    }

    /// Shared `_datadir` (state the Python dump started from).
    pub fn datadir_path() -> PathBuf {
        Self::root().join("_datadir")
    }

    /// `_datadir/harness.db` if present (gitignored).
    pub fn harness_db() -> Option<PathBuf> {
        let p = Self::datadir_path().join("harness.db");
        p.exists().then_some(p)
    }

    /// `_datadir/fantrax_cache/` if present (gitignored).
    pub fn fantrax_cache_dir() -> Option<PathBuf> {
        let p = Self::datadir_path().join("fantrax_cache");
        p.is_dir().then_some(p)
    }

    /// `fixtures/_datadir` copied into a fresh temp dir (for CLI / plan-memory / status-history
    /// tests); the TempDir must be kept alive by the caller.
    pub fn temp_datadir() -> anyhow::Result<(PathBuf, Box<dyn std::any::Any>)> {
        let src = Self::datadir_path();
        anyhow::ensure!(src.is_dir(), "fixture data dir {} not found", src.display());
        let tmp = tempfile::tempdir().context("creating temp data dir")?;
        copy_dir(&src, tmp.path())?;
        Ok((tmp.path().to_path_buf(), Box::new(tmp)))
    }
}

fn copy_dir(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}
