//! Rust port of `fantasy_manager/valuation/params.py`: valuation constants fitted by
//! `fm backtest fit` (packaged `fitted_params.json`) plus the harness override layer.
//! Owner: work package `W2-valuation-base` (wave 2).
//!
//! * The "merged params" are a `serde_json::Value` (object); the process-global cache is
//!   `RwLock<Option<Arc<Params>>>` (the one deliberate global, mirroring Python's `_merged`). It is
//!   loaded lazily (packaged file deep-merged with the active harness version unless
//!   `FM_PARAMS_OVERRIDE=0`), replaced by [`install`] (tests, snapshots) and rebuilt by [`reload`].
//! * Every accessor takes `params: Option<&Value>` like Python's `params=` argument: `Some` computes
//!   from that mapping, `None` reads the memoized result of the global (computed once per load).
//! * `params_hash` uses a private canonical `json.dumps(sort_keys, separators=(",",":"))`
//!   (ensure_ascii, Python float repr) so it does not depend on `fm_pycompat::pyjson`.
//! * Deviation: `json.loads` accepts `NaN`/`Infinity` literals, `serde_json` does not; such a file is
//!   treated as unreadable (-> `{}` / ignored), which only matters for hand-corrupted files.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use fm_core::values::Horizon;
use fm_core::Map;
use fm_pycompat::fmt::repr_f64;
use fm_pycompat::num::py_sum;
use serde_json::Value;
use sha1::{Digest, Sha1};

/// The packaged `crates/fm-engine/data/fitted_params.json` (`fm backtest fit` prints the copy command).
const PACKAGED_JSON: &str = include_str!("../../data/fitted_params.json");

// ---- hard-coded fallbacks (fit of 2026-09-28, ESPN scoring, target seasons 2011-12..2025-26)
pub const FALLBACK_K_BASELINE: [(&str, f64); 3] = [("F", 8.0), ("D", 14.0), ("G", 120.0)];
pub const FALLBACK_K_INSEASON: [(&str, f64); 2] = [("skater", 25.0), ("goalie", 40.0)];
pub const FALLBACK_RECENCY: [(&str, f64); 4] = [("season", 0.85), ("last30", 0.15), ("last15", 0.0), ("last7", 0.0)];
pub const FALLBACK_AGE_GROUPS: [&str; 2] = ["F", "D"];
pub const FALLBACK_AGE_YOY_F: [(i64, f64); 23] = [
    (18, 1.111), (19, 1.101), (20, 1.070), (21, 1.054), (22, 1.037), (23, 1.019), (24, 1.002), (25, 0.988),
    (26, 0.981), (27, 0.970), (28, 0.954), (29, 0.939), (30, 0.941), (31, 0.936), (32, 0.940), (33, 0.910),
    (34, 0.908), (35, 0.896), (36, 0.889), (37, 0.881), (38, 0.858), (39, 0.856), (40, 1.0),
];
pub const FALLBACK_AGE_YOY_D: [(i64, f64); 23] = [
    (18, 1.032), (19, 1.057), (20, 1.056), (21, 1.039), (22, 1.017), (23, 0.997), (24, 0.995), (25, 0.994),
    (26, 0.993), (27, 0.983), (28, 0.980), (29, 0.977), (30, 0.979), (31, 0.963), (32, 0.958), (33, 0.943),
    (34, 0.952), (35, 0.917), (36, 0.913), (37, 0.893), (38, 0.875), (39, 1.0), (40, 1.0),
];
pub const FALLBACK_DYNASTY_CURVE_F: [(f64, f64); 10] = [
    (19.0, 0.762), (21.0, 0.897), (23.0, 0.980), (25.0, 1.000), (27.0, 0.969), (29.0, 0.897),
    (31.0, 0.792), (33.0, 0.697), (35.0, 0.576), (37.0, 0.459),
];
pub const FALLBACK_DYNASTY_CURVE_D: [(f64, f64); 10] = [
    (19.0, 0.848), (21.0, 0.946), (23.0, 1.000), (25.0, 0.992), (27.0, 0.979), (29.0, 0.943),
    (31.0, 0.901), (33.0, 0.831), (35.0, 0.746), (37.0, 0.625),
];
pub const SKATER_GROUPS: [&str; 2] = ["F", "D"];

pub const FALLBACK_PROJECTION_WEIGHT: f64 = 0.6;
pub const FALLBACK_K_PROJECTION: [(&str, f64); 2] = [("skater", 20.0), ("goalie", 12.0)];
/// status -> (week multiplier, season multiplier)
pub const FALLBACK_AVAILABILITY: [(&str, (f64, f64)); 7] = [
    ("healthy", (1.0, 1.0)),
    ("dtd", (0.75, 0.75)),
    ("out", (0.0, 0.6)),
    ("ir", (0.0, 0.4)),
    ("ltir", (0.0, 0.1)),
    ("suspended", (0.0, 0.5)),
    ("unknown", (1.0, 1.0)),
];
pub const FALLBACK_START_SHARE_PRIOR: f64 = 0.5;
pub const FALLBACK_START_SHARE_K: f64 = 10.0;
pub const FALLBACK_OFFNIGHT_BONUS: f64 = 0.05;
/// Backtest mean absolute error of a preseason per-game projection, in ESPN points per game, by
/// group (`preseason.mae_at_fitted_k_multi` of the 2026-09-28 fit).
pub const FALLBACK_PROJECTION_MAE: [(&str, f64); 3] = [("F", 0.23), ("D", 0.20981), ("G", 0.57728)];
/// The points weights the backtest errors above were measured in and the recommendation
/// thresholds were tuned in: the ESPN preset of `fm-backtest/src/scoring.rs` (`PRESETS["espn"]`),
/// which is also the ESPN league's own scoring.
pub const ESPN_REFERENCE_WEIGHTS: [(&str, f64); 14] = [
    ("G", 2.0), ("A", 1.0), ("PPP", 0.5), ("SHP", 0.5), ("SOG", 0.1), ("HIT", 0.1), ("BLK", 0.5), ("DEF", 0.1),
    ("HAT", 1.0), ("W", 2.0), ("SV", 0.2), ("GA", -1.5), ("SO", 2.0), ("OTL", 1.0),
];

/// Days of the real NHL schedule, from today, over which a roster's daily lineups are solved to
/// find the share of his team's games each player would start (`recommend::started`). Four weeks
/// is the first step suggested for gap B1 in `docs/knowledge/09-engine-gap-analysis.md`; it must
/// stay at most 32 (one bit per day in a `u32`).
pub const START_WINDOW_DAYS: i64 = 28;

pub const OVERRIDE_ENV: &str = "FM_PARAMS_OVERRIDE";
pub const PARAMS_DIR_ENV: &str = "FM_PARAMS_DIR";
pub const ACTIVE_FILE: &str = "active.json";

/// `{group: {age: factor}}`
pub type AgeYoy = Map<String, Map<i64, f64>>;
/// `{group: ((age, level), ...)}`
pub type DynastyCurves = Map<String, Vec<(f64, f64)>>;
/// `{status: (week multiplier, season multiplier)}`
pub type AvailabilityTable = Map<String, (f64, f64)>;

fn fallback_map<const N: usize>(a: &[(&str, f64); N]) -> Map<String, f64> {
    a.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

pub fn fallback_k_baseline() -> Map<String, f64> {
    fallback_map(&FALLBACK_K_BASELINE)
}
pub fn fallback_k_inseason() -> Map<String, f64> {
    fallback_map(&FALLBACK_K_INSEASON)
}
pub fn fallback_recency() -> Map<String, f64> {
    fallback_map(&FALLBACK_RECENCY)
}
pub fn fallback_k_projection() -> Map<String, f64> {
    fallback_map(&FALLBACK_K_PROJECTION)
}
pub fn fallback_projection_mae() -> Map<String, f64> {
    fallback_map(&FALLBACK_PROJECTION_MAE)
}
pub fn espn_reference_weights() -> Map<String, f64> {
    fallback_map(&ESPN_REFERENCE_WEIGHTS)
}
pub fn fallback_availability() -> AvailabilityTable {
    FALLBACK_AVAILABILITY.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}
fn fallback_age_yoy(g: &str) -> Map<i64, f64> {
    match g {
        "F" => FALLBACK_AGE_YOY_F.iter().copied().collect(),
        "D" => FALLBACK_AGE_YOY_D.iter().copied().collect(),
        _ => Map::new(),
    }
}
fn fallback_dynasty_curve(g: &str) -> Vec<(f64, f64)> {
    match g {
        "F" => FALLBACK_DYNASTY_CURVE_F.to_vec(),
        _ => FALLBACK_DYNASTY_CURVE_D.to_vec(),
    }
}

// ------------------------------------------------------------------------------ value helpers

/// Python `float(v)` for a JSON value: numbers, bools, numeric strings; None otherwise.
fn pyfloat(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return None;
            }
            t.replace('_', "").parse::<f64>().ok()
        }
        _ => None,
    }
}

/// `_num`: a finite, non-negative float or None.
fn num(v: Option<&Value>) -> Option<f64> {
    let f = pyfloat(v?)?;
    if f.is_finite() && f >= 0.0 {
        Some(f)
    } else {
        None
    }
}

/// `_section(*keys, params=)`: nested lookup; None when a level is not a mapping / key missing.
fn section<'a>(params: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let mut d = params;
    for k in keys {
        d = d.as_object()?.get(*k)?;
    }
    Some(d)
}

/// Python truthiness of `x or {}` for the sections used with `or {}`.
fn truthy_obj(v: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    v.and_then(|x| x.as_object()).filter(|m| !m.is_empty())
}

// ------------------------------------------------------------------------------ derived values

fn k_baseline_all(p: &Value) -> Map<String, f64> {
    let raw = truthy_obj(section(p, &["preseason", "k_multi"]));
    let mut out = fallback_k_baseline();
    for (g, slot) in out.iter_mut() {
        if let Some(v) = raw.and_then(|r| num(r.get(g.as_str()))) {
            *slot = v;
        }
    }
    out
}

fn projection_mae_all(p: &Value) -> Map<String, f64> {
    let raw = truthy_obj(section(p, &["preseason", "mae_at_fitted_k_multi"]));
    let mut out = fallback_projection_mae();
    for (g, slot) in out.iter_mut() {
        if let Some(v) = raw.and_then(|r| num(r.get(g.as_str()))) {
            *slot = v;
        }
    }
    out
}

fn k_pair(raw: Option<&Value>, fallback: Map<String, f64>) -> Map<String, f64> {
    let mut out = fallback;
    if let Some(raw) = raw.and_then(|r| r.as_object()) {
        for (key, src) in [("skater", "k_skater"), ("goalie", "k_goalie")] {
            if let Some(v) = num(raw.get(src)) {
                out.insert(key.to_string(), v);
            }
        }
    }
    out
}

fn k_inseason_all(p: &Value) -> Map<String, f64> {
    k_pair(section(p, &["inseason"]), fallback_k_inseason())
}

/// `inseason.k_skater_volume`: {stat: k} for the volume stats (`blend::VOLUME_STATS`) only;
/// empty (every stat uses `k_skater`) when absent.
fn k_inseason_volume_all(p: &Value) -> Map<String, f64> {
    let mut out = Map::new();
    if let Some(raw) = section(p, &["inseason", "k_skater_volume"]).and_then(|r| r.as_object()) {
        for stat in super::blend::VOLUME_STATS {
            if let Some(v) = num(raw.get(stat)) {
                out.insert(stat.to_string(), v);
            }
        }
    }
    out
}

fn recency_all(p: &Value) -> Map<String, f64> {
    if let Some(raw) = section(p, &["inseason", "recency_weights"]).and_then(|r| r.as_object()) {
        let vals: Vec<Option<f64>> = FALLBACK_RECENCY.iter().map(|(k, _)| num(raw.get(*k))).collect();
        if vals.iter().all(|v| v.is_some()) {
            let vs: Vec<f64> = vals.iter().map(|v| v.unwrap()).collect();
            if (py_sum(vs.iter().copied()) - 1.0).abs() < 1e-6 {
                return FALLBACK_RECENCY.iter().zip(vs).map(|((k, _), v)| (k.to_string(), v)).collect();
            }
        }
    }
    fallback_recency()
}

fn projection_weight_of(p: &Value) -> f64 {
    match num(section(p, &["projection", "weight"])) {
        Some(v) if v <= 1.0 => v,
        _ => FALLBACK_PROJECTION_WEIGHT,
    }
}

fn k_projection_all(p: &Value) -> Map<String, f64> {
    k_pair(section(p, &["projection"]), fallback_k_projection())
}

fn availability_all(p: &Value) -> AvailabilityTable {
    let mut out = fallback_availability();
    if let Some(raw) = section(p, &["availability"]).and_then(|r| r.as_object()) {
        for (status, v) in raw {
            let (week, season) = out.get(status.as_str()).copied().unwrap_or((1.0, 1.0));
            let (w, s) = match v {
                Value::Object(m) => (num(m.get("week")), num(m.get("season"))),
                Value::Array(a) if a.len() == 2 => (num(a.first()), num(a.get(1))),
                _ => continue,
            };
            out.insert(
                status.clone(),
                (
                    match w {
                        Some(w) if w <= 1.0 => w,
                        _ => week,
                    },
                    match s {
                        Some(s) if s <= 1.0 => s,
                        _ => season,
                    },
                ),
            );
        }
    }
    out
}

fn start_share_prior_of(p: &Value) -> f64 {
    match num(section(p, &["schedule", "start_share_prior"])) {
        Some(v) if v <= 1.0 => v,
        _ => FALLBACK_START_SHARE_PRIOR,
    }
}

fn start_share_k_of(p: &Value) -> f64 {
    num(section(p, &["schedule", "start_share_k"])).unwrap_or(FALLBACK_START_SHARE_K)
}

fn offnight_bonus_of(p: &Value) -> f64 {
    match num(section(p, &["schedule", "offnight_bonus"])) {
        Some(v) if v <= 1.0 => v,
        _ => FALLBACK_OFFNIGHT_BONUS,
    }
}

fn age_groups_of(p: &Value) -> Vec<String> {
    if let Some(Value::Array(a)) = section(p, &["fm_fitted", "age_groups"]) {
        if a.iter().all(|g| g.is_string()) {
            return a.iter().map(|g| g.as_str().unwrap().to_string()).collect();
        }
    }
    FALLBACK_AGE_GROUPS.iter().map(|s| s.to_string()).collect()
}

/// Python `int(str)` on a JSON object key.
fn py_int_key(a: &str) -> Option<i64> {
    let t = a.trim().replace('_', "");
    t.parse::<i64>().ok()
}

fn age_yoy_of(p: &Value) -> AgeYoy {
    let raw = section(p, &["fm_fitted", "age_yoy"]);
    let mut out: AgeYoy = Map::new();
    for g in age_groups_of(p) {
        let curve = raw.and_then(|r| r.as_object()).and_then(|r| r.get(&g)).and_then(|c| c.as_object());
        let mut parsed: Map<i64, Option<f64>> = Map::new();
        if let Some(curve) = curve {
            for (a, r) in curve {
                let v = num(Some(r));
                if let Some(age) = py_int_key(a) {
                    parsed.insert(age, v);
                }
            }
        }
        let filtered: Map<i64, f64> =
            parsed.into_iter().filter_map(|(a, v)| v.filter(|v| *v > 0.0).map(|v| (a, v))).collect();
        let chosen = if filtered.is_empty() { fallback_age_yoy(&g) } else { filtered };
        out.insert(g, chosen);
    }
    out.into_iter().filter(|(_, c)| !c.is_empty()).collect()
}

fn dynasty_curves_of(p: &Value) -> DynastyCurves {
    let raw = section(p, &["dynasty_age_curves"]);
    let mut out = Map::new();
    for g in SKATER_GROUPS {
        let pts = raw.and_then(|r| r.as_object()).and_then(|r| r.get(g)).and_then(|v| v.as_array());
        let mut parsed: Vec<(f64, f64)> = Vec::new();
        if let Some(pts) = pts {
            for pt in pts {
                let Some(arr) = pt.as_array() else { continue };
                let (Some(a), Some(lv)) = (arr.first().and_then(pyfloat), arr.get(1).and_then(pyfloat)) else {
                    continue;
                };
                if lv > 0.0 {
                    parsed.push((a, lv));
                }
            }
        }
        if parsed.len() >= 2 {
            parsed.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal).then(
                x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal),
            ));
            out.insert(g.to_string(), parsed);
        } else {
            out.insert(g.to_string(), fallback_dynasty_curve(g));
        }
    }
    out
}

/// Memoized accessor results of one loaded params dict (Python's `_memo`).
#[derive(Clone, Debug)]
pub struct Derived {
    pub k_baseline: Map<String, f64>,
    pub k_inseason: Map<String, f64>,
    pub k_inseason_volume: Map<String, f64>,
    pub recency_weights: Map<String, f64>,
    pub projection_weight: f64,
    pub k_projection: Map<String, f64>,
    pub availability: AvailabilityTable,
    pub start_share_prior: f64,
    pub start_share_k: f64,
    pub offnight_bonus: f64,
    pub age_groups: Vec<String>,
    pub age_yoy: AgeYoy,
    pub dynasty_age_curves: DynastyCurves,
    pub projection_mae: Map<String, f64>,
}

impl Derived {
    pub fn from_value(p: &Value) -> Derived {
        Derived {
            k_baseline: k_baseline_all(p),
            k_inseason: k_inseason_all(p),
            k_inseason_volume: k_inseason_volume_all(p),
            recency_weights: recency_all(p),
            projection_weight: projection_weight_of(p),
            k_projection: k_projection_all(p),
            availability: availability_all(p),
            start_share_prior: start_share_prior_of(p),
            start_share_k: start_share_k_of(p),
            offnight_bonus: offnight_bonus_of(p),
            age_groups: age_groups_of(p),
            age_yoy: age_yoy_of(p),
            dynasty_age_curves: dynasty_curves_of(p),
            projection_mae: projection_mae_all(p),
        }
    }
}

/// One loaded parameter set: the merged dict, the active harness version record and the memos.
#[derive(Clone, Debug)]
pub struct Params {
    pub merged: Value,
    pub active: Option<Value>,
    pub derived: Derived,
}

impl Params {
    pub fn new(merged: Value, active: Option<Value>) -> Params {
        let derived = Derived::from_value(&merged);
        Params { merged, active, derived }
    }
}

// ------------------------------------------------------------------------------ loading

static GLOBAL: RwLock<Option<Arc<Params>>> = RwLock::new(None);
static PACKAGED: OnceLock<Value> = OnceLock::new();

fn empty_obj() -> Value {
    Value::Object(serde_json::Map::new())
}

fn read_json_file(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&text).ok()
}

/// The packaged `fitted_params.json` ({} when missing or unreadable). With `path` that file is read
/// from disk instead; `None` uses the copy embedded at build time.
pub fn load_packaged(path: Option<&str>) -> Value {
    match path {
        Some(p) => match read_json_file(Path::new(p)) {
            Some(v) if v.is_object() => v,
            _ => empty_obj(),
        },
        None => PACKAGED
            .get_or_init(|| match serde_json::from_str::<Value>(PACKAGED_JSON) {
                Ok(v) if v.is_object() => v,
                _ => empty_obj(),
            })
            .clone(),
    }
}

/// `"0" / "false" / "off" / "no"` disables the override layer.
pub fn override_enabled() -> bool {
    let v = std::env::var(OVERRIDE_ENV).unwrap_or_else(|_| "1".to_string());
    !matches!(v.trim().to_lowercase().as_str(), "0" | "false" | "off" | "no")
}

/// `<fm_data_dir>/harness/params` (`FM_PARAMS_DIR` wins when set).
pub fn params_dir(data_dir: Option<&Path>) -> PathBuf {
    if let Some(d) = data_dir {
        return d.join("harness").join("params");
    }
    if let Ok(env) = std::env::var(PARAMS_DIR_ENV) {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    // Python: get_settings().fm_data_dir, falling back to $FM_DATA_DIR, then "data". Settings are
    // not available as a global here, so read the same environment variable.
    let base = std::env::var("FM_DATA_DIR").unwrap_or_else(|_| "data".to_string());
    PathBuf::from(base).join("harness").join("params")
}

/// `base` with `over` merged in: nested dicts merge key by key, anything else replaces.
pub fn deep_merge(base: &Value, over: &Value) -> Value {
    let mut out = match base {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    if let Value::Object(o) = over {
        for (k, v) in o {
            let merged = match (v, out.get(k)) {
                (Value::Object(_), Some(cur @ Value::Object(_))) => deep_merge(cur, v),
                _ => v.clone(),
            };
            out.insert(k.clone(), merged);
        }
    }
    Value::Object(out)
}

/// The active version record (`active.json` -> `vNNNN.json`), or None when there is no pointer, it
/// points nowhere or either file is unreadable / malformed.
pub fn read_active(pdir: Option<&Path>) -> Option<Value> {
    let d = match pdir {
        Some(p) => p.to_path_buf(),
        None => params_dir(None),
    };
    let ptr = read_json_file(&d.join(ACTIVE_FILE))?;
    let version = ptr.as_object()?.get("version")?.as_str()?.to_string();
    if !version.starts_with('v') || version.len() < 2 || !version[1..].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut rec = read_json_file(&d.join(format!("{version}.json")))?;
    {
        let obj = rec.as_object_mut()?;
        if !obj.get("params").map(|p| p.is_object()).unwrap_or(false) {
            return None;
        }
        obj.entry("version".to_string()).or_insert(Value::String(version));
    }
    Some(rec)
}

fn build_from_disk() -> Params {
    let packaged = load_packaged(None);
    let active = if override_enabled() { read_active(None) } else { None };
    let merged = match &active {
        Some(a) => deep_merge(&packaged, &a["params"]),
        None => packaged,
    };
    Params::new(merged, active)
}

/// The current parameter set (loaded lazily).
pub fn current() -> Arc<Params> {
    if let Some(p) = GLOBAL.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return p.clone();
    }
    let mut w = GLOBAL.write().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = w.as_ref() {
        return p.clone();
    }
    let p = Arc::new(build_from_disk());
    *w = Some(p.clone());
    p
}

/// `load_params()` / `load_params(path)`: the effective params (a clone); with `path` only that
/// file is read (no override layer).
pub fn load_params(path: Option<&str>) -> Value {
    match path {
        Some(_) => load_packaged(path),
        None => current().merged.clone(),
    }
}

/// Drop every cache and re-read (the dashboard's /refresh).
pub fn reload() -> Arc<Params> {
    let p = Arc::new(build_from_disk());
    *GLOBAL.write().unwrap_or_else(|e| e.into_inner()) = Some(p.clone());
    p
}

/// Install a merged params dict as the process-global (tests, `--from-snapshot`).
pub fn install(merged: Value) -> Arc<Params> {
    install_with_active(merged, None)
}

/// [`install`] with the active harness version record (snapshot `params.active`).
pub fn install_with_active(merged: Value, active: Option<Value>) -> Arc<Params> {
    let active = active.filter(|a| !a.is_null());
    let p = Arc::new(Params::new(merged, active));
    *GLOBAL.write().unwrap_or_else(|e| e.into_inner()) = Some(p.clone());
    p
}

/// The active harness version record behind `load_params()` (None: packaged only).
pub fn active_version() -> Option<Value> {
    current().active.clone()
}

// ------------------------------------------------------------------------------ accessors

/// Run `f` on the derived values of `params` (computed fresh) or of the global (memoized).
fn with_derived<R>(params: Option<&Value>, pick: impl Fn(&Derived) -> R, fresh: impl Fn(&Value) -> R) -> R {
    match params {
        Some(p) => fresh(p),
        None => pick(&current().derived),
    }
}

/// {F, D, G} shrinkage k for the 3-season history toward the positional mean.
pub fn k_baseline(params: Option<&Value>) -> Map<String, f64> {
    with_derived(params, |d| d.k_baseline.clone(), k_baseline_all)
}

/// `'goalie'` for `'goalie'` / `'G'`, else `'skater'` (`_group_key` for strings).
pub fn group_key(group: &str) -> &'static str {
    let g = group.to_lowercase();
    if g == "goalie" || g == "g" {
        "goalie"
    } else {
        "skater"
    }
}

/// `k_inseason()` without a group: {skater, goalie}.
pub fn k_inseason(params: Option<&Value>) -> Map<String, f64> {
    with_derived(params, |d| d.k_inseason.clone(), k_inseason_all)
}

/// `k_inseason(group)` for 'skater' / 'goalie' / 'F' / 'D' / 'G'.
pub fn k_inseason_for(group: &str, params: Option<&Value>) -> f64 {
    let key = group_key(group);
    with_derived(params, |d| d.k_inseason[key], |p| k_inseason_all(p)[key])
}

/// `k_inseason(is_goalie)` (bool group).
pub fn k_inseason_goalie(is_goalie: bool, params: Option<&Value>) -> f64 {
    k_inseason_for(if is_goalie { "goalie" } else { "skater" }, params)
}

/// In-season k of each skater volume stat that has its own fitted k ({stat: k}; the other stats
/// use `k_inseason_for("skater")`).
///
/// The packaged values (SOG 5, HIT 8, BLK 30, PIM 100 games) are `fit_inseason` on the 2021-22 to
/// 2025-26 checkpoints of the backtest, each stat taken from the scoring preset that pays it most
/// (shots, hits and penalty minutes from `fantrax`, blocks 30 under both): a preset that pays a
/// stat little barely constrains its k (the `espn` fit gives shots k = 0). `fm backtest fit`
/// writes its single-preset fit as `inseason.k_skater_volume_fit`, which is not read here.
pub fn k_inseason_volume(params: Option<&Value>) -> Map<String, f64> {
    with_derived(params, |d| d.k_inseason_volume.clone(), k_inseason_volume_all)
}

/// Base recency weights (season / last30 / last15 / last7), summing to 1.
pub fn recency_weights(params: Option<&Value>) -> Map<String, f64> {
    with_derived(params, |d| d.recency_weights.clone(), recency_all)
}

/// Weight of the league projection vs the multi-season history (full-history players).
pub fn projection_weight(params: Option<&Value>) -> f64 {
    with_derived(params, |d| d.projection_weight, projection_weight_of)
}

/// `k_projection()` without a group: {skater, goalie}.
pub fn k_projection(params: Option<&Value>) -> Map<String, f64> {
    with_derived(params, |d| d.k_projection.clone(), k_projection_all)
}

/// `k_projection(group)`.
pub fn k_projection_for(group: &str, params: Option<&Value>) -> f64 {
    let key = group_key(group);
    with_derived(params, |d| d.k_projection[key], |p| k_projection_all(p)[key])
}

/// `k_projection(is_goalie)` (bool group).
pub fn k_projection_goalie(is_goalie: bool, params: Option<&Value>) -> f64 {
    k_projection_for(if is_goalie { "goalie" } else { "skater" }, params)
}

/// {status: (week multiplier, season multiplier)}.
pub fn availability_table(params: Option<&Value>) -> AvailabilityTable {
    with_derived(params, |d| d.availability.clone(), availability_all)
}

/// Availability multiplier of `status` for the 'week' or 'season' horizon (1.0 unknown).
pub fn availability(status: &str, horizon: Horizon, params: Option<&Value>) -> f64 {
    let (week, season) = with_derived(
        params,
        |d| d.availability.get(status).copied().unwrap_or((1.0, 1.0)),
        |p| availability_all(p).get(status).copied().unwrap_or((1.0, 1.0)),
    );
    match horizon {
        Horizon::Week => week,
        Horizon::Season => season,
    }
}

/// Backtest mean absolute error of a per-game projection for group F / D / G, in the ESPN points
/// of [`ESPN_REFERENCE_WEIGHTS`] (0 for an unknown group).
pub fn projection_mae(group: &str, params: Option<&Value>) -> f64 {
    with_derived(params, |d| d.projection_mae.get(group).copied().unwrap_or(0.0), |p| {
        projection_mae_all(p).get(group).copied().unwrap_or(0.0)
    })
}

/// Goalie start-share prior the observed share is shrunk toward.
pub fn start_share_prior(params: Option<&Value>) -> f64 {
    with_derived(params, |d| d.start_share_prior, start_share_prior_of)
}

/// Games of shrinkage of a goalie's start share toward the prior.
pub fn start_share_k(params: Option<&Value>) -> f64 {
    with_derived(params, |d| d.start_share_k, start_share_k_of)
}

/// Week-projection bonus per off-night game (`1 + bonus * offnight_games`).
pub fn offnight_bonus(params: Option<&Value>) -> f64 {
    with_derived(params, |d| d.offnight_bonus, offnight_bonus_of)
}

pub fn age_groups(params: Option<&Value>) -> Vec<String> {
    with_derived(params, |d| d.age_groups.clone(), age_groups_of)
}

/// {group: {age: year-over-year FPG factor}} for the aged groups (F, D).
pub fn age_yoy(params: Option<&Value>) -> AgeYoy {
    with_derived(params, |d| d.age_yoy.clone(), age_yoy_of)
}

/// Expected FPG multiplier from last season (played at `age_prev` on Oct 1) to this one. 1.0 for an
/// unknown age or a group without a curve; ages outside the curve use its ends. `yoy` / `groups`
/// override the global values like the Python keyword arguments.
pub fn age_factor(group: &str, age_prev: Option<f64>, yoy: Option<&AgeYoy>, groups: Option<&[String]>) -> f64 {
    let cur;
    let (groups, yoy): (&[String], &AgeYoy) = match (groups, yoy) {
        (Some(g), Some(y)) => (g, y),
        (g, y) => {
            cur = current();
            (g.unwrap_or(&cur.derived.age_groups), y.unwrap_or(&cur.derived.age_yoy))
        }
    };
    let Some(age_prev) = age_prev else { return 1.0 };
    if !groups.iter().any(|g| g == group) {
        return 1.0;
    }
    let Some(curve) = yoy.get(group) else { return 1.0 };
    if curve.is_empty() {
        return 1.0;
    }
    let lo = *curve.keys().min().unwrap();
    let hi = *curve.keys().max().unwrap();
    let a = (age_prev.floor() as i64).max(lo).min(hi);
    curve.get(&a).copied().unwrap_or(1.0)
}

/// {F, D}: ((age, level), ...) sorted by age, peak = 1.0 (goalies stay hand-set).
pub fn dynasty_age_curves(params: Option<&Value>) -> DynastyCurves {
    with_derived(params, |d| d.dynasty_age_curves.clone(), dynasty_curves_of)
}

// ------------------------------------------------------------------------------ provenance

fn py_str(v: Option<&Value>, default: &str) -> String {
    match v {
        None => default.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "None".to_string(),
        Some(Value::Bool(b)) => (if *b { "True" } else { "False" }).to_string(),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) if !n.is_f64() => i.to_string(),
            _ => repr_f64(n.as_f64().unwrap_or(f64::NAN)),
        },
        Some(other) => other.to_string(),
    }
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Short provenance label: 'fit 2026-09-28 (espn)', plus ' + harness v0003 (2026-11-17)' when a
/// harness version is active, or 'built-in fallback' without a packaged file.
pub fn source(params: Option<&Value>) -> String {
    let (packaged, active) = match params {
        Some(d) => (d.clone(), None),
        None => (load_packaged(None), active_version()),
    };
    let nonempty = packaged.as_object().map(|m| !m.is_empty()).unwrap_or(false);
    let mut base = if nonempty {
        format!(
            "fit {} ({})",
            first_chars(&py_str(packaged.get("generated"), "?"), 10),
            py_str(packaged.get("scoring"), "?")
        )
    } else {
        "built-in fallback".to_string()
    };
    if let Some(a) = active {
        let truthy = |k: &str| a.get(k).filter(|v| v.as_str().map(|s| !s.is_empty()).unwrap_or(!v.is_null()));
        let created = truthy("applied_at")
            .or_else(|| truthy("created"))
            .map(|v| py_str(Some(v), "?"))
            .unwrap_or_else(|| "?".to_string());
        base.push_str(&format!(
            " + harness {} ({})",
            py_str(a.get("version"), "?"),
            first_chars(&created, 10)
        ));
    }
    base
}

/// `json.dumps(v, sort_keys=True, separators=(",", ":"))` (ensure_ascii, Python float repr).
fn canonical_dumps(v: &Value) -> String {
    use fm_pycompat::pyjson::{dumps, DumpOpts};
    dumps(v, &DumpOpts { sort_keys: true, ..DumpOpts::compact() })
}

/// sha1 of the effective params (canonical JSON, sorted keys); identifies the parameter set a
/// snapshot was produced with (the built-in fallback hashes as '{}').
pub fn params_hash(params: Option<&Value>) -> String {
    let s = match params {
        Some(p) => canonical_dumps(p),
        None => canonical_dumps(&current().merged),
    };
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}
