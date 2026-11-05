//! `prefs.py`: dashboard / `fm mode` preferences in `<FM_DATA_DIR>/prefs.json`.
//! Reads never fail (missing / unreadable / corrupt file = empty); writes are atomic.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use serde_json::Value;

use crate::config::Settings;
use crate::models::DynastyMode;
use crate::Map;

pub const PREFS_FILE: &str = "prefs.json";
pub const DYNASTY_MODE_KEY: &str = "dynasty_mode";
pub const HARNESS_AUTO_APPLY_KEY: &str = "harness_auto_apply";
pub const DEFAULT_DYNASTY_MODE: DynastyMode = DynastyMode::Balanced;

static LOCK: Mutex<()> = Mutex::new(());

pub fn prefs_path(data_dir: &Path) -> PathBuf {
    data_dir.join(PREFS_FILE)
}

/// `load_prefs`: {} when missing / corrupt / not a JSON object.
pub fn load_prefs(data_dir: &Path) -> Map<String, Value> {
    let Ok(raw) = std::fs::read_to_string(prefs_path(data_dir)) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(o)) => o.into_iter().collect(),
        _ => Map::new(),
    }
}

pub fn get_pref(key: &str, data_dir: &Path) -> Option<Value> {
    load_prefs(data_dir).get(key).cloned()
}

/// `json.dump(data, indent=2, sort_keys=True)` (ensure_ascii) + newline.
fn dump_sorted(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                // Python sorts str keys by code point; Rust's String order is by UTF-8 bytes, which
                // is the same order.
                keys.sort();
                Value::Object(keys.into_iter().map(|k| (k.clone(), sorted(&o[k]))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    let text = serde_json::to_string_pretty(&sorted(v)).expect("Value serializes");
    let mut out = String::with_capacity(text.len() + 1);
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut buf = [0u16; 2];
            for unit in ch.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out.push('\n');
    out
}

/// Atomic write (`indent=2, sort_keys=True` + newline) via a temp file + rename;
/// `value = None` deletes the key.
pub fn set_pref(key: &str, value: Option<Value>, data_dir: &Path) -> anyhow::Result<()> {
    let path = prefs_path(data_dir);
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut data = load_prefs(data_dir);
    match value {
        None => {
            data.shift_remove(key);
        }
        Some(v) => {
            data.insert(key.to_string(), v);
        }
    }
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let obj: serde_json::Map<String, Value> = data.into_iter().collect();
    let text = dump_sorted(&Value::Object(obj));
    let mut tmp = tempfile::Builder::new().prefix(".prefs-").suffix(".tmp").tempfile_in(data_dir)?;
    tmp.write_all(text.as_bytes())?;
    tmp.flush()?;
    tmp.persist(&path).map_err(|e| e.error).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// `normalize_mode`: 'Rebuild ' -> rebuild; None for anything that is not a known mode.
pub fn normalize_mode(raw: &str) -> Option<DynastyMode> {
    DynastyMode::parse(raw)
}

/// `dynasty_mode_info(settings)` -> (mode, source): prefs, then env (only if explicitly set:
/// `settings.fields_set`), then default "balanced".
pub fn dynasty_mode_info(settings: &Settings) -> (DynastyMode, String) {
    let dir: &Path = if settings.fm_data_dir.as_os_str().is_empty() { Path::new(".") } else { &settings.fm_data_dir };
    if let Some(Value::String(s)) = get_pref(DYNASTY_MODE_KEY, dir) {
        if let Some(m) = normalize_mode(&s) {
            return (m, "prefs".to_string());
        }
    }
    if settings.fields_set.contains("fantrax_mode") {
        return (settings.fantrax_mode, "env".to_string());
    }
    (DEFAULT_DYNASTY_MODE, "default".to_string())
}

/// `effective_dynasty_mode(settings)`.
pub fn effective_dynasty_mode(settings: &Settings) -> DynastyMode {
    dynasty_mode_info(settings).0
}

/// `mode_source_label(source)`.
pub fn mode_source_label(source: &str) -> String {
    match source {
        "prefs" => "dashboard/prefs".to_string(),
        "env" => "FANTRAX_MODE".to_string(),
        "default" => "default".to_string(),
        "option" => "--mode, this run only".to_string(),
        "" => "default".to_string(),
        other => other.to_string(),
    }
}

/// `harness_auto_apply(data_dir)` (default true; "0/false/no/off" strings mean false).
pub fn harness_auto_apply(data_dir: &Path) -> bool {
    match get_pref(HARNESS_AUTO_APPLY_KEY, data_dir) {
        None => true,
        Some(Value::String(s)) => !matches!(s.trim().to_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Some(v) => py_truthy(&v),
    }
}

fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
