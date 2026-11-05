//! Rust port of `fantasy_manager/backtest/scoring.py`: point presets for the backtest and
//! fantasy points per game under a `ScoringConfig`.
//!
//! Owner: work package `W2-harness-ledger-stats`.
//!
//! `scorer()` returns the engine's `fm_engine::scoring::PointsScoring` (`PointsScorer` is kept as
//! an alias of it).

use std::path::Path;
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Context, Result};
use fm_core::models::{ScoringConfig, ScoringKind};
use fm_core::Map;
use fm_engine::scoring::PointsScoring;
use serde_json::Value;

fn weights(pairs: &[(&str, f64)]) -> Map<String, f64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

fn points(w: &[(&str, f64)], goalie: &[(&str, f64)]) -> ScoringConfig {
    ScoringConfig {
        kind: ScoringKind::Points,
        weights: weights(w),
        categories: Vec::new(),
        goalie_weights: weights(goalie),
    }
}

/// `PRESETS`: "espn", "fantrax", "default" (in that order).
pub static PRESETS: LazyLock<Map<String, ScoringConfig>> = LazyLock::new(|| {
    let mut m = Map::new();
    m.insert(
        "espn".to_string(),
        points(
            &[
                ("G", 2.0), ("A", 1.0), ("PPP", 0.5), ("SHP", 0.5), ("SOG", 0.1), ("HIT", 0.1),
                ("BLK", 0.5), ("DEF", 0.1), ("HAT", 1.0), ("W", 2.0), ("SV", 0.2), ("GA", -1.5),
                ("SO", 2.0), ("OTL", 1.0),
            ],
            &[],
        ),
    );
    m.insert(
        "fantrax".to_string(),
        points(
            &[
                ("G", 4.0), ("A", 2.0), ("PPP", 1.0), ("SHG", 2.0), ("SOG", 0.5), ("HIT", 0.3),
                ("BLK", 0.5), ("PIM", 0.5), ("ENG", 2.0), ("FT", 3.0), ("W", 5.0), ("SV", 0.25),
                ("GA", -1.0), ("SO", 7.5),
            ],
            &[("G", 20.0), ("A", 3.0)],
        ),
    );
    // A generic points league (Yahoo/Fantrax-style defaults).
    m.insert(
        "default".to_string(),
        points(
            &[
                ("G", 3.0), ("A", 2.0), ("PPP", 1.0), ("SHP", 1.0), ("SOG", 0.4), ("HIT", 0.2),
                ("BLK", 0.4), ("W", 4.0), ("SV", 0.2), ("GA", -1.0), ("SO", 3.0), ("OTL", 1.0),
            ],
            &[],
        ),
    );
    m
});

/// Stats the backtest data cannot supply (their weight is ignored).
pub const UNAVAILABLE: [&str; 6] = ["HAT", "DEF", "FT", "STP", "GAA", "SVPCT"];

/// Python `repr(str)` for the error message (single quotes unless the text has one and no `"`).
fn py_repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `('espn', cfg)` for a preset name, or `(stem, cfg)` for a JSON file holding a ScoringConfig
/// (either bare or under a "scoring" key, as saved from `fm settings --json`).
pub fn load_scoring(name_or_path: &str) -> Result<(String, ScoringConfig)> {
    let trimmed = name_or_path.trim();
    let key = if trimmed.is_empty() { "espn" } else { trimmed };
    let lower = key.to_lowercase();
    if let Some(cfg) = PRESETS.get(&lower) {
        return Ok((lower, cfg.clone()));
    }
    let p = Path::new(key);
    if !p.exists() {
        let names: Vec<&str> = PRESETS.keys().map(|s| s.as_str()).collect();
        bail!("unknown scoring {}; use one of {} or a JSON file", py_repr_str(key), names.join(", "));
    }
    let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    let d: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
    let inner = d.get("scoring").unwrap_or(&d).clone();
    let cfg: ScoringConfig =
        serde_json::from_value(inner).map_err(|e| anyhow!("invalid scoring config in {}: {e}", p.display()))?;
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    Ok((stem, cfg))
}

/// The backtest's scorer is the engine's points scoring (`scoring.PointsScoring`).
pub type PointsScorer = PointsScoring;

pub fn scorer(cfg: &ScoringConfig) -> Result<PointsScoring> {
    if cfg.kind != ScoringKind::Points {
        bail!("the backtest scores points leagues only");
    }
    Ok(PointsScoring::new(cfg.weights.clone(), Some(cfg.goalie_weights.clone())))
}

/// Fantasy points per game of per-game rates (0.0 for empty rates).
pub fn fpg(rates: Option<&Map<String, f64>>, scoring: &PointsScoring) -> f64 {
    match rates {
        Some(r) if !r.is_empty() => scoring.value(r),
        _ => 0.0,
    }
}

/// `sorted(k for k in {**weights, **goalie_weights} if k in UNAVAILABLE)`.
pub fn missing_weights(cfg: &ScoringConfig) -> Vec<String> {
    let mut keys: Vec<String> = cfg
        .weights
        .keys()
        .chain(cfg.goalie_weights.keys())
        .filter(|k| UNAVAILABLE.contains(&k.as_str()))
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    keys
}
