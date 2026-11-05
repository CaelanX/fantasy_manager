//! Rust port of `fantasy_manager/backtest/scoring.py`: point presets for the backtest and
//! fantasy points per game under a `ScoringConfig`.
//!
//! Owner: work package `W2-harness-ledger-stats`.
//!
//! Deviation: Python's `scorer()` returns `fantasy_manager.scoring.PointsScoring`
//! (`fm_engine::scoring`, owned by W2-valuation-base and not written yet). Until it lands this
//! module carries [`PointsScorer`], a line-for-line copy of the two methods the backtest uses
//! (`weights_for` and `value`); swap it for the engine type once available (see the report).

use std::borrow::Cow;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Context, Result};
use fm_core::models::{ScoringConfig, ScoringKind};
use fm_core::Map;
use fm_pycompat::num::py_sum;
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

/// Copy of `fm_engine::scoring::PointsScoring` (weights + goalie overrides).
#[derive(Clone, Debug, PartialEq)]
pub struct PointsScorer {
    pub weights: Map<String, f64>,
    pub goalie_weights: Map<String, f64>,
}

impl PointsScorer {
    /// Stats that mark a stat dict as a goalie line (goalie_weights then apply).
    pub const GOALIE_MARKERS: [&'static str; 4] = ["GS", "SA", "SV", "GA"];

    /// `{**weights, **goalie_weights}` for a goalie-looking line, else the plain weights.
    pub fn weights_for(&self, per_game_stats: &Map<String, f64>) -> Cow<'_, Map<String, f64>> {
        if !self.goalie_weights.is_empty()
            && Self::GOALIE_MARKERS.iter().any(|m| per_game_stats.contains_key(*m))
        {
            let mut w = self.weights.clone();
            for (k, v) in &self.goalie_weights {
                w.insert(k.clone(), *v);
            }
            return Cow::Owned(w);
        }
        Cow::Borrowed(&self.weights)
    }

    /// `sum(v * stats[k] for k, v in w.items() if k in stats)`.
    pub fn value(&self, per_game_stats: &Map<String, f64>) -> f64 {
        let w = self.weights_for(per_game_stats);
        py_sum(w.iter().filter_map(|(k, v)| per_game_stats.get(k).map(|s| v * s)))
    }
}

pub fn scorer(cfg: &ScoringConfig) -> Result<PointsScorer> {
    if cfg.kind != ScoringKind::Points {
        bail!("the backtest scores points leagues only");
    }
    Ok(PointsScorer { weights: cfg.weights.clone(), goalie_weights: cfg.goalie_weights.clone() })
}

/// Fantasy points per game of per-game rates (0.0 for empty rates).
pub fn fpg(rates: Option<&Map<String, f64>>, scoring: &PointsScorer) -> f64 {
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
