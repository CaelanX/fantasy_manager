//! `prefs.py`: dashboard / `fm mode` preferences in `<FM_DATA_DIR>/prefs.json`.
//! Implementation: work package `W2-core-runtime`. Signatures mirror Python.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::Settings;
use crate::models::DynastyMode;
use crate::Map;

pub const PREFS_FILE: &str = "prefs.json";

pub fn prefs_path(data_dir: &Path) -> PathBuf {
    data_dir.join(PREFS_FILE)
}

/// `load_prefs`: {} when missing / corrupt.
pub fn load_prefs(_data_dir: &Path) -> Map<String, Value> {
    todo!("W2-core-runtime")
}

pub fn get_pref(_key: &str, _data_dir: &Path) -> Option<Value> {
    todo!("W2-core-runtime")
}

/// Atomic write (`indent=2, sort_keys=True` + newline) via a temp file + rename;
/// `value = None` deletes the key.
pub fn set_pref(_key: &str, _value: Option<Value>, _data_dir: &Path) -> anyhow::Result<()> {
    todo!("W2-core-runtime")
}

/// `normalize_mode`.
pub fn normalize_mode(_raw: &str) -> Option<DynastyMode> {
    todo!("W2-core-runtime")
}

/// `dynasty_mode_info(settings)` -> (mode, source): prefs, then env (only if explicitly set:
/// `settings.fields_set`), then default "balanced".
pub fn dynasty_mode_info(_settings: &Settings) -> (DynastyMode, String) {
    todo!("W2-core-runtime")
}

/// `mode_source_label(source)`.
pub fn mode_source_label(_source: &str) -> String {
    todo!("W2-core-runtime")
}

/// `harness_auto_apply(data_dir)` (default true; "0/false/no/off" strings mean false).
pub fn harness_auto_apply(_data_dir: &Path) -> bool {
    todo!("W2-core-runtime")
}
