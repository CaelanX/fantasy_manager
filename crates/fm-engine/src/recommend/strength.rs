//! Rust port of `fantasy_manager/recommend/strength.py`: absolute 0-10 recommendation strength,
//! comparable across kinds and days (piecewise-linear maps of `predicted_gain` per kind).
//! Owner: work package `W5-recommend-base`.
//!
//! `apply_strength` / `apply_ranks` work in place on `&mut [Recommendation]`. Python's
//! "never let a display field sink a recommender" try/except has nothing to catch here: none of
//! the code paths can fail.

use fm_core::models::{GainUnits, RecKind, Recommendation};
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::{py_sum, round_to};
use fm_pycompat::text::py_strip;

use super::base::confidence;
use super::headline::apply_headlines;

pub type Scale = [(f64, f64)];

pub const WAIVER_SCALE: [(f64, f64); 5] = [(0.0, 0.0), (0.3, 3.0), (1.0, 6.0), (2.0, 9.0), (3.0, 10.0)];
pub const WEEK_SCALE: [(f64, f64); 5] = [(0.0, 0.0), (1.0, 3.0), (3.0, 6.0), (6.0, 9.0), (8.0, 10.0)];
pub const DAY_SCALE: [(f64, f64); 6] = [(0.0, 0.0), (1.0, 2.0), (3.0, 4.0), (6.0, 6.0), (12.0, 8.0), (20.0, 10.0)];
pub const TRADE_SCALE: [(f64, f64); 5] = [(0.0, 0.0), (0.5, 4.0), (1.0, 6.0), (2.0, 9.0), (3.0, 10.0)];
pub const TRADE_EV_SCALE: [(f64, f64); 5] = [(0.0, 0.0), (0.15, 3.0), (0.35, 6.0), (0.6, 8.0), (1.0, 10.0)];
pub const FLAG_SCALE: [(f64, f64); 4] = [(0.0, 0.0), (0.35, 4.0), (0.6, 7.0), (1.0, 10.0)];
pub const IR_FLOOR: f64 = 5.0;

/// `SEVERITY_STRENGTH.get(status, 0.0)`
pub fn severity_strength(status: &str) -> f64 {
    match status {
        "dtd" => 2.5,
        "suspended" => 5.0,
        "out" => 5.0,
        "ir" => 7.5,
        "ltir" => 10.0,
        _ => 0.0,
    }
}

/// `KIND_GROUP.get(kind, kind)`: sell_high and buy_low share "flags".
pub fn kind_group(kind: RecKind) -> &'static str {
    match kind {
        RecKind::SellHigh | RecKind::BuyLow => "flags",
        k => k.as_str(),
    }
}

/// Piecewise-linear map of `x` through `scale` anchors; 0 below 0, capped at 10.
pub fn interp(x: f64, scale: &Scale) -> f64 {
    if x <= scale[0].0 {
        return 0.0;
    }
    for w in scale.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if x <= x1 {
            return y0 + (y1 - y0) * (x - x0) / (x1 - x0);
        }
    }
    10.0f64.min(scale[scale.len() - 1].1)
}

fn reason_value(rec: &Recommendation, code: &str) -> Option<f64> {
    rec.reasons.iter().find(|r| r.code == code && r.value.is_some()).and_then(|r| r.value)
}

/// Waivers: the add's confidence; trades: mean over every player involved.
pub fn rec_confidence(rec: &Recommendation) -> f64 {
    if rec.kind == RecKind::Waiver {
        return rec.add.first().map_or(1.0, confidence);
    }
    let n = rec.add.len() + rec.drop.len();
    if n == 0 {
        return 1.0;
    }
    py_sum(rec.add.iter().chain(rec.drop.iter()).map(confidence)) / n as f64
}

fn gain_strength(gain: f64, units: Option<GainUnits>) -> f64 {
    match units {
        Some(GainUnits::WeekPts) | Some(GainUnits::MatchupPts) => interp(gain, &WEEK_SCALE),
        Some(GainUnits::DayPts) => interp(gain, &DAY_SCALE),
        Some(GainUnits::LineupFpg) | Some(GainUnits::Dynasty) => interp(gain, &TRADE_SCALE),
        _ => interp(gain, &WAIVER_SCALE),
    }
}

/// Absolute 0-10 strength of one recommendation (see the Python module docstring).
pub fn strength_for(rec: &Recommendation) -> f64 {
    let gain = rec.predicted_gain;
    match rec.kind {
        RecKind::SellHigh | RecKind::BuyLow => match reason_value(rec, "FORM_RATIO") {
            Some(ratio) => round_to(interp((ratio - 1.0).abs(), &FLAG_SCALE), 2),
            None => 0.0,
        },
        RecKind::Trade => {
            if let Some(p) = reason_value(rec, "P_ACCEPT") {
                let edge = if rec.gain_units == Some(GainUnits::Dynasty) { gain } else { reason_value(rec, "MY_EDGE") };
                let edge = if edge.is_none() { gain } else { edge };
                let Some(edge) = edge else { return 0.0 };
                return round_to(interp(edge * p * rec_confidence(rec), &TRADE_EV_SCALE), 2);
            }
            let Some(gain) = gain else { return 0.0 };
            let scale: &Scale = if rec.gain_units != Some(GainUnits::WeekPts) { &TRADE_SCALE } else { &WEEK_SCALE };
            let mut s = interp(gain * rec_confidence(rec), scale);
            if let Some(dyn_) = reason_value(rec, "DYNASTY_DELTA") {
                if dyn_.abs() > 1e-9 {
                    s += if dyn_ > 0.0 { 1.0 } else { -1.0 };
                }
            }
            round_to(10.0f64.min(0.0f64.max(s)), 2)
        }
        RecKind::Waiver => match gain {
            None => 0.0,
            Some(g) => round_to(gain_strength(g * rec_confidence(rec), rec.gain_units), 2),
        },
        RecKind::Injury => {
            let mut s = gain.map_or(0.0, |g| gain_strength(g, rec.gain_units));
            if rec.reasons.iter().any(|r| r.code == "IR_SLOT") {
                s = s.max(IR_FLOOR);
            } else if gain.is_none() {
                if let Some(first) = rec.subjects.first() {
                    s = severity_strength(first.status.as_str());
                }
            }
            round_to(10.0f64.min(s), 2)
        }
        // lineup (and anything else, as in Python's fall-through)
        _ => match gain {
            None => {
                if rec.reasons.iter().any(|r| r.code == "STATUS") {
                    IR_FLOOR
                } else {
                    0.0
                }
            }
            Some(g) => round_to(gain_strength(g, rec.gain_units), 2),
        },
    }
}

/// Set `strength` (in place) on every rec that has none.
pub fn apply_strength(recs: &mut [Recommendation]) {
    for r in recs.iter_mut() {
        if r.strength.is_none() {
            r.strength = Some(strength_for(r));
        }
    }
}

/// Set `rank_in_kind` / `kind_total` (in place) from the given order, per kind group (sell_high
/// and buy_low share "flags"), and each rec's missing `headline` (every recommender ends with
/// this call).
pub fn apply_ranks(recs: &mut [Recommendation]) {
    apply_headlines(recs);
    let mut groups: Vec<(&'static str, i64)> = Vec::new();
    let mut rank: Vec<(usize, i64)> = Vec::with_capacity(recs.len());
    for r in recs.iter() {
        let g = kind_group(r.kind);
        let gi = match groups.iter().position(|(k, _)| *k == g) {
            Some(gi) => gi,
            None => {
                groups.push((g, 0));
                groups.len() - 1
            }
        };
        groups[gi].1 += 1;
        rank.push((gi, groups[gi].1));
    }
    for (r, (gi, n)) in recs.iter_mut().zip(rank) {
        r.rank_in_kind = Some(n);
        r.kind_total = Some(groups[gi].1);
    }
}

/// '+0.95 FPG', '+3.2 pts/wk', '+1.5 pts (3d left)', '+0.80 lineup FPG', '+1.10 dyn' or '-'.
/// matchup_pts are the points over the days left in this matchup, not a weekly rate.
pub fn gain_text(rec: &Recommendation) -> String {
    let Some(g) = rec.predicted_gain else { return "-".to_string() };
    let mut unit = match rec.gain_units {
        Some(GainUnits::WeekPts) => "pts/wk",
        Some(GainUnits::SeasonFpg) => "FPG",
        Some(GainUnits::LineupFpg) => "lineup FPG",
        Some(GainUnits::Dynasty) => "dyn",
        _ => "",
    };
    if rec.gain_units == Some(GainUnits::MatchupPts) {
        let tail = match rec.horizon_days.filter(|d| *d != 0) {
            Some(d) => format!(" ({d}d left)"),
            None => " (this matchup)".to_string(),
        };
        return format!("{} pts{tail}", fixed(g, 1, true));
    }
    if rec.gain_units == Some(GainUnits::SeasonFpg) && rec.horizon_days == Some(7) {
        unit = "FPG (wk)";
    }
    if rec.gain_units == Some(GainUnits::SeasonFpg) && matches!(rec.kind, RecKind::SellHigh | RecKind::BuyLow) {
        unit = "FPG vs L15";
    }
    let num = if rec.gain_units == Some(GainUnits::WeekPts) { fixed(g, 1, true) } else { fixed(g, 2, true) };
    py_strip(&format!("{num} {unit}")).to_string()
}

pub fn rank_text(rec: &Recommendation) -> String {
    match (rec.rank_in_kind.filter(|r| *r != 0), rec.kind_total.filter(|t| *t != 0)) {
        (Some(r), Some(t)) => format!("{r}/{t}"),
        _ => "-".to_string(),
    }
}
