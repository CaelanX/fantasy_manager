//! Rust port of `fantasy_manager/harness/params_store.py`: versioned valuation-parameter
//! overrides for the harness (`vNNNN.json` + `active.json` under `<fm_data_dir>/harness/params/`).
//! Owner: work package `W3-harness-deploy-data`.
//!
//! File format is byte-compatible with Python: `json.dump(indent=2, sort_keys=True,
//! default=str)` + newline, written atomically through a temp file in the same directory.
//! Records are `serde_json` maps (`Rec`); every write is mirrored into the ledger's
//! `param_versions` table when a ledger is given.
//!
//! Deviations: `apply(as_of=None)` reads the local date; errors Python raises as `ValueError`
//! are `anyhow` errors with the same messages.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::NaiveDate;
use fm_engine::valuation::params as vparams;
use fm_pycompat::pyjson::{dumps, DumpOpts};
use serde_json::{json, Value};

use crate::ledger::{now_iso, Ledger, Row};

pub const PACKAGED: &str = "packaged";
pub const STATUSES: [&str; 5] = ["proposed", "active", "shadow", "rolled_back", "retired"];

/// One version record (a JSON object).
pub type Rec = serde_json::Map<String, Value>;

/// `^v(\d{4,})$`
fn version_number(s: &str) -> Option<u64> {
    let digits = s.strip_prefix('v')?;
    if digits.len() >= 4 && digits.chars().all(|c| c.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

/// `_write_json`: atomic, indent=2, sort_keys, trailing newline.
pub fn write_json(path: &Path, data: &Value) -> Result<()> {
    let dir = path.parent().ok_or_else(|| anyhow!("no parent dir for {}", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("x");
    let mut tmp = tempfile::Builder::new().prefix(&format!(".{stem}-")).suffix(".tmp").tempfile_in(dir)?;
    let opts = DumpOpts { indent: Some(2), sort_keys: true, ..Default::default() };
    tmp.write_all(dumps(data, &opts).as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| anyhow!("replacing {}: {}", path.display(), e.error))?;
    Ok(())
}

/// `params_hash` of the packaged params deep-merged with `override_` ({} -> packaged hash).
pub fn merged_hash(override_: &Value) -> String {
    vparams::params_hash(Some(&vparams::deep_merge(&vparams::load_packaged(None), override_)))
}

fn s_of<'a>(rec: &'a Rec, k: &str) -> Option<&'a str> {
    rec.get(k).and_then(|v| v.as_str())
}

/// The versions directory (default `valuation.params.params_dir()`) plus an optional ledger to
/// mirror into.
pub struct ParamsStore<'a> {
    pub dir: PathBuf,
    pub ledger: Option<&'a Ledger>,
}

impl<'a> ParamsStore<'a> {
    pub fn new(pdir: Option<&Path>, ledger: Option<&'a Ledger>, data_dir: Option<&Path>) -> Self {
        let dir = match pdir {
            Some(p) => p.to_path_buf(),
            None => {
                let base: Option<PathBuf> = data_dir.map(|d| d.to_path_buf()).or_else(|| ledger.map(|l| l.data_dir.clone()));
                match std::env::var(vparams::PARAMS_DIR_ENV).ok().filter(|v| !v.is_empty()) {
                    Some(env) => PathBuf::from(env),
                    None => vparams::params_dir(base.as_deref()),
                }
            }
        };
        ParamsStore { dir, ledger }
    }

    // -- reading --------------------------------------------------------------
    pub fn path(&self, version: &str) -> PathBuf {
        self.dir.join(format!("{version}.json"))
    }

    pub fn get(&self, version: &str) -> Option<Rec> {
        if version == PACKAGED || version_number(version).is_none() {
            return None;
        }
        let text = std::fs::read_to_string(self.path(version)).ok()?;
        match serde_json::from_str::<Value>(&text).ok()? {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    /// Every readable version record, oldest first.
    pub fn versions(&self) -> Vec<Rec> {
        let mut names: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with('v') && n.ends_with(".json") {
                    names.push(n);
                }
            }
        }
        names.sort();
        let mut out = Vec::new();
        for n in names {
            let stem = &n[..n.len() - 5];
            if version_number(stem).is_some() {
                if let Some(rec) = self.get(stem) {
                    out.push(rec);
                }
            }
        }
        out
    }

    pub fn history(&self) -> Vec<Rec> {
        self.versions()
    }

    /// The pointer's version, or `packaged`.
    pub fn active_name(&self) -> String {
        let ptr = std::fs::read_to_string(self.dir.join(vparams::ACTIVE_FILE))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        match ptr.as_ref().and_then(|p| p.get("version")).and_then(|v| v.as_str()) {
            Some(v) if self.get(v).is_some() => v.to_string(),
            _ => PACKAGED.to_string(),
        }
    }

    pub fn active(&self) -> Option<Rec> {
        let name = self.active_name();
        if name == PACKAGED {
            None
        } else {
            self.get(&name)
        }
    }

    /// The active version's override params ({} for packaged).
    pub fn active_params(&self) -> Value {
        match self.active().and_then(|r| r.get("params").cloned()) {
            Some(p @ Value::Object(_)) => p,
            _ => json!({}),
        }
    }

    pub fn next_version(&self) -> String {
        let mut max = 0u64;
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if let Some(stem) = n.strip_suffix(".json") {
                    if let Some(k) = version_number(stem) {
                        max = max.max(k);
                    }
                }
            }
        }
        format!("v{:04}", max + 1)
    }

    // -- writing --------------------------------------------------------------
    fn save(&self, rec: &Rec) -> Result<()> {
        let version = s_of(rec, "version").ok_or_else(|| anyhow!("KeyError: 'version'"))?.to_string();
        write_json(&self.path(&version), &Value::Object(rec.clone()))?;
        if self.ledger.is_some() {
            self.mirror(std::slice::from_ref(rec))?;
        }
        Ok(())
    }

    fn log(rec: &mut Rec, event: &str, by: &str, note: Option<&str>) {
        let entry = json!({"at": now_iso(), "event": event, "by": by, "note": note});
        match rec.get_mut("changelog") {
            Some(Value::Array(a)) => a.push(entry),
            _ => {
                rec.insert("changelog".into(), Value::Array(vec![entry]));
            }
        }
    }

    fn point(&self, version: Option<&str>) -> Result<()> {
        write_json(&self.dir.join(vparams::ACTIVE_FILE), &json!({"version": version, "updated": now_iso()}))?;
        vparams::reload();
        Ok(())
    }

    /// Write a new `proposed` version (`params`: the full cumulative override).
    pub fn propose(
        &self,
        params: &Value,
        changed_keys: &[String],
        metrics: Option<&Value>,
        note: Option<&str>,
        parent: Option<&str>,
        by: &str,
        extra: Option<&Rec>,
    ) -> Result<Rec> {
        let version = self.next_version();
        let mut keys: Vec<String> = changed_keys.to_vec();
        keys.sort();
        let parent = parent.filter(|p| !p.is_empty()).map(String::from).unwrap_or_else(|| self.active_name());
        let mut rec = Rec::new();
        rec.insert("version".into(), version.into());
        rec.insert("parent".into(), parent.into());
        rec.insert("created".into(), now_iso().into());
        rec.insert("params".into(), params.clone());
        rec.insert("hash".into(), merged_hash(params).into());
        rec.insert("changed_keys".into(), json!(keys));
        rec.insert("metrics".into(), metrics.cloned().unwrap_or_else(|| json!({})));
        rec.insert("status".into(), "proposed".into());
        rec.insert("applied_by".into(), Value::Null);
        rec.insert("applied_at".into(), Value::Null);
        rec.insert("changelog".into(), json!([]));
        if let Some(x) = extra {
            for (k, v) in x {
                rec.insert(k.clone(), v.clone());
            }
        }
        Self::log(&mut rec, "proposed", by, note);
        self.save(&rec)?;
        Ok(rec)
    }

    /// Activate `version`: the previously active version becomes its `shadow` (scored weekly
    /// against it), an older shadow is retired, and the pointer moves.
    pub fn apply(&self, version: &str, by: &str, note: Option<&str>, as_of: Option<NaiveDate>) -> Result<Rec> {
        let mut rec = self.get(version).ok_or_else(|| anyhow!("unknown params version {}", py_repr(version)))?;
        let prev = self.active_name();
        if prev == version {
            return Ok(rec);
        }
        for mut other in self.versions() {
            let ov = s_of(&other, "version").unwrap_or("").to_string();
            if ov == version {
                continue;
            }
            if s_of(&other, "status") == Some("shadow") {
                other.insert("status".into(), "retired".into());
                Self::log(&mut other, "retired", by, Some(&format!("superseded by {version}")));
                self.save(&other)?;
            }
            if ov == prev {
                other.insert("status".into(), "shadow".into());
                Self::log(&mut other, "shadowed", by, Some(&format!("replaced by {version}")));
                self.save(&other)?;
            }
        }
        rec.insert("status".into(), "active".into());
        rec.insert("applied_by".into(), by.into());
        rec.insert("applied_at".into(), now_iso().into());
        let since = as_of.unwrap_or_else(|| chrono::Local::now().date_naive());
        rec.insert("shadow".into(), json!({"version": prev, "since": since.format("%Y-%m-%d").to_string(), "weeks": {}}));
        Self::log(&mut rec, "applied", by, note);
        self.save(&rec)?;
        self.point(Some(version))?;
        Ok(rec)
    }

    /// Roll the active version back to `to` (default its parent; `packaged` for none). Returns
    /// `{"from", "to", "hash"}`; the rolled-back version is marked `rolled_back`.
    pub fn rollback(&self, to: Option<&str>, by: &str, note: Option<&str>) -> Result<Value> {
        let to = to.filter(|t| !t.is_empty());
        let cur = self.active();
        if cur.is_none() && (to.is_none() || to == Some(PACKAGED)) {
            bail!("nothing to roll back: the packaged params are active");
        }
        let target: String = match to {
            Some(t) => t.to_string(),
            None => cur
                .as_ref()
                .and_then(|c| s_of(c, "parent"))
                .filter(|p| !p.is_empty())
                .unwrap_or(PACKAGED)
                .to_string(),
        };
        if target != PACKAGED && self.get(&target).is_none() {
            bail!("unknown params version {}", py_repr(&target));
        }
        let cur_version = cur.as_ref().and_then(|c| s_of(c, "version")).map(String::from);
        if cur_version.as_deref() == Some(target.as_str()) {
            bail!("{target} is already active");
        }
        if let Some(mut c) = cur {
            c.insert("status".into(), "rolled_back".into());
            let n = note.map(String::from).unwrap_or_else(|| format!("rolled back to {target}"));
            Self::log(&mut c, "rolled_back", by, Some(&n));
            self.save(&c)?;
        }
        for mut other in self.versions() {
            if s_of(&other, "status") == Some("shadow") {
                other.insert("status".into(), "retired".into());
                Self::log(&mut other, "retired", by, Some("rollback"));
                self.save(&other)?;
            }
        }
        if target == PACKAGED {
            self.point(None)?;
        } else {
            let mut rec = self.get(&target).context("target vanished")?;
            rec.insert("status".into(), "active".into());
            rec.insert("applied_by".into(), by.into());
            rec.insert("applied_at".into(), now_iso().into());
            rec.insert("shadow".into(), Value::Null);
            let from = cur_version.clone().unwrap_or_else(|| PACKAGED.to_string());
            let n = note.map(String::from).unwrap_or_else(|| format!("rollback from {from}"));
            Self::log(&mut rec, "reactivated", by, Some(&n));
            self.save(&rec)?;
            self.point(Some(&target))?;
        }
        Ok(json!({
            "from": cur_version.unwrap_or_else(|| PACKAGED.to_string()),
            "to": target,
            "hash": vparams::params_hash(None),
        }))
    }

    /// Save a modified record (e.g. its shadow-scoring weeks).
    pub fn update(&self, rec: &Rec) -> Result<()> {
        self.save(rec)
    }

    // -- ledger mirror --------------------------------------------------------
    pub fn ledger_row(rec: &Rec) -> Row {
        let falsy = |v: Option<&Value>| match v {
            None | Some(Value::Null) => true,
            Some(Value::Object(o)) => o.is_empty(),
            Some(Value::Array(a)) => a.is_empty(),
            _ => false,
        };
        let metrics = if falsy(rec.get("metrics")) { json!({}) } else { rec["metrics"].clone() };
        let changelog = if falsy(rec.get("changelog")) { json!([]) } else { rec["changelog"].clone() };
        let get = |k: &str| rec.get(k).cloned().unwrap_or(Value::Null);
        let mut r = Row::new();
        r.insert("version".into(), rec["version"].clone());
        r.insert("parent".into(), get("parent"));
        r.insert("params_hash".into(), get("hash"));
        r.insert("status".into(), get("status"));
        r.insert("created_at".into(), get("created"));
        r.insert("metrics_json".into(), dumps(&metrics, &DumpOpts { sort_keys: true, ..Default::default() }).into());
        r.insert("changelog".into(), dumps(&changelog, &DumpOpts::default()).into());
        r
    }

    fn mirror(&self, recs: &[Rec]) -> Result<()> {
        if let Some(l) = self.ledger {
            let rows: Vec<Row> = recs.iter().map(Self::ledger_row).collect();
            l.upsert("param_versions", &rows, &["version"])?;
        }
        Ok(())
    }

    /// Rebuild the ledger's `param_versions` rows from the version files.
    pub fn sync_ledger(&self, ledger: Option<&Ledger>) -> Result<usize> {
        let Some(led) = ledger.or(self.ledger) else { return Ok(0) };
        let recs = self.versions();
        led.execute("DELETE FROM param_versions", &[])?;
        led.commit()?;
        if !recs.is_empty() {
            let rows: Vec<Row> = recs.iter().map(Self::ledger_row).collect();
            led.upsert("param_versions", &rows, &["version"])?;
        }
        Ok(recs.len())
    }
}

/// Python `repr(str)` for the error messages (single quotes unless the text has one).
fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

// ---- functional API -------------------------------------------------------------------------

pub fn store<'a>(ledger: Option<&'a Ledger>, pdir: Option<&Path>) -> ParamsStore<'a> {
    ParamsStore::new(pdir, ledger, None)
}

pub fn history(ledger: Option<&Ledger>, pdir: Option<&Path>) -> Vec<Rec> {
    ParamsStore::new(pdir, ledger, None).versions()
}
