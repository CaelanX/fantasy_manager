//! Rust port of `fantasy_manager/harness/track.py`: the track record shown where decisions are
//! made (how moves like this one turned out). Read-only over the graded `outcomes`.
//! Owner: work package `W6-harness-metrics-refit`.
//!
//! Deviations: `TrackBook::load` / `track_record` take `today` (Python defaulted to
//! `date.today()`); records are [`TrackRecord`] structs serialized in Python's dict key order.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;
use chrono::{Duration, NaiveDate};
use fm_core::models::Recommendation;
use fm_core::Map;
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::py_sum;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

use crate::ledger::{Ledger, Row, DB_NAME};
use crate::metrics::{f_of, i_of, iso, primary_window, s_of, wilson, HIT_KINDS, HIT_PROVISIONAL};
use crate::outcomes::FLAG_KINDS;

pub const TRACK_DAYS: i64 = 60;
pub const MIN_N: usize = HIT_PROVISIONAL;

pub fn subkind_label(kind: &str, subkind: &str) -> Option<&'static str> {
    Some(match (kind, subkind) {
        ("waiver", "stream") => "this-matchup pickups",
        ("waiver", "season") => "season pickups",
        ("lineup", "ir") => "IR swaps and activations",
        ("lineup", "swap") => "start / sit changes",
        ("injury", "ir") => "IR moves",
        ("injury", "hold_cut") => "hold-or-cut calls",
        _ => return None,
    })
}

pub fn kind_noun(kind: &str) -> &str {
    match kind {
        "waiver" => "pickups",
        "lineup" => "lineup changes",
        "injury" => "injury moves",
        "sell_high" => "sell-high calls",
        "buy_low" => "buy-low calls",
        "alert" => "followed alerts",
        other => other,
    }
}

fn ir_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bir\b").expect("regex"))
}

/// Sub-group of a recommendation (see the Python module docstring); None when the kind has none.
pub fn subkind_of(kind: &str, title: Option<&str>, gain_units: Option<&str>) -> Option<&'static str> {
    let t = title.unwrap_or("").to_lowercase();
    match kind {
        "waiver" => Some(if gain_units == Some("matchup_pts") || t.ends_with("for this matchup") { "stream" } else { "season" }),
        "lineup" => Some(if ir_re().is_match(&t) { "ir" } else { "swap" }),
        "injury" => Some(if t.starts_with("hold ") || t.starts_with("cut ") { "hold_cut" } else { "ir" }),
        _ => None,
    }
}

/// `{n, hits, hit_rate, avg_gain, ci, kind, subkind, enough, min_n, days, units, label, available}`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrackRecord {
    pub n: usize,
    pub hits: i64,
    pub hit_rate: Option<f64>,
    pub avg_gain: Option<f64>,
    pub ci: (Option<f64>, Option<f64>),
    pub kind: String,
    pub subkind: Option<String>,
    pub enough: bool,
    pub min_n: usize,
    pub days: i64,
    pub units: String,
    pub label: Option<String>,
    pub available: bool,
}

/// Graded outcomes of one league, grouped by (kind, subkind).
#[derive(Clone, Debug, Default)]
pub struct TrackBook {
    pub league: String,
    pub days: i64,
    pub groups: Map<(String, Option<String>), Vec<Row>>,
    /// False: no harness ledger yet
    pub available: bool,
}

impl TrackBook {
    pub fn load(data_dir: &Path, league: &str, today: NaiveDate, days: i64) -> Result<TrackBook> {
        let mut book = TrackBook { league: league.to_string(), days, ..Default::default() };
        if !data_dir.join(DB_NAME).exists() {
            return Ok(book);
        }
        let since = iso(today - Duration::days(days));
        let ledger = Ledger::open(data_dir)?;
        let lg = league.to_string();
        let rows = ledger.query(
            "SELECT o.kind, o.gain_units, o.window, o.hit, o.realized_gain, o.origin, e.title \
             FROM outcomes o LEFT JOIN rec_episodes e ON e.episode_id = o.episode_id \
             WHERE o.league=? AND o.complete=1 AND o.hit IS NOT NULL AND o.window_end >= ? \
             AND COALESCE(o.basis, 'first_seen') != 'acted_on' AND COALESCE(o.origin, '') != 'user_only'",
            &[&lg, &since],
        )?;
        book.available = true;
        for r in rows {
            let kind = s_of(&r, "kind").unwrap_or("").to_string();
            if !HIT_KINDS.contains(&kind.as_str()) || s_of(&r, "window") != Some(primary_window(&kind, s_of(&r, "gain_units"))) {
                continue;
            }
            let sub = subkind_of(&kind, s_of(&r, "title"), s_of(&r, "gain_units"));
            book.groups.entry((kind.clone(), None)).or_default().push(r.clone());
            if let Some(sub) = sub {
                book.groups.entry((kind, Some(sub.to_string()))).or_default().push(r);
            }
        }
        Ok(book)
    }

    /// The record of `kind` / `subkind`. A subkind below MIN_N falls back to the whole kind when
    /// that has enough history.
    pub fn record(&self, kind: &str, subkind: Option<&str>) -> TrackRecord {
        let empty: Vec<Row> = Vec::new();
        let sub = match subkind {
            Some(s) => self.groups.get(&(kind.to_string(), Some(s.to_string()))).unwrap_or(&empty),
            None => &empty,
        };
        let whole = self.groups.get(&(kind.to_string(), None)).unwrap_or(&empty);
        let use_sub = subkind.is_some() && (sub.len() >= MIN_N || whole.len() < MIN_N);
        let rows = if use_sub { sub } else { whole };
        let n = rows.len();
        let hits: i64 = rows.iter().map(|r| i_of(r, "hit").unwrap_or(0)).sum();
        let gains: Vec<f64> = rows.iter().filter_map(|r| f_of(r, "realized_gain")).collect();
        let ci = wilson(hits, n as i64);
        let label = if use_sub {
            subkind.and_then(|s| subkind_label(kind, s)).map(String::from)
        } else {
            Some(kind_noun(kind).to_string())
        };
        TrackRecord {
            n,
            hits,
            hit_rate: (n > 0).then(|| hits as f64 / n as f64),
            avg_gain: (!gains.is_empty()).then(|| py_sum(gains.iter().copied()) / gains.len() as f64),
            ci,
            kind: kind.to_string(),
            subkind: if use_sub { subkind.map(String::from) } else { None },
            enough: n >= MIN_N,
            min_n: MIN_N,
            days: self.days,
            units: if FLAG_KINDS.contains(&kind) { "fpg change" } else { "pts" }.to_string(),
            label,
            available: self.available,
        }
    }

    pub fn to_json(rec: &TrackRecord) -> Value {
        serde_json::to_value(rec).unwrap_or(Value::Null)
    }
}

/// One line: "Similar moves: 62% gained points (n=34, last 60 days)", or why there is no rate yet.
pub fn record_text(tr: &TrackRecord) -> String {
    if !HIT_KINDS.contains(&tr.kind.as_str()) {
        return "Track record: trades are judged case by case (Health page)".into();
    }
    if !tr.available {
        return "Track record: starts once the daily harness run has graded a few weeks of moves".into();
    }
    let label = tr.label.clone().unwrap_or_else(|| "None".into());
    if !tr.enough {
        return format!(
            "Track record: not enough history yet (n={} graded {label}, shown from {})",
            tr.n, tr.min_n
        );
    }
    let verb = if tr.units == "fpg change" { "played out" } else { "gained points" };
    let ci = match tr.ci {
        (Some(lo), Some(hi)) => format!(", 95% CI {}-{}%", fixed(lo * 100.0, 0, false), fixed(hi * 100.0, 0, false)),
        _ => String::new(),
    };
    format!(
        "Similar moves ({label}): {}% {verb} (n={}, last {} days{ci})",
        fixed(tr.hit_rate.unwrap_or(0.0) * 100.0, 0, false),
        tr.n,
        tr.days
    )
}

/// The record for a Recommendation.
pub fn rec_record(book: &TrackBook, rec: &Recommendation) -> TrackRecord {
    let kind = rec.kind.as_str();
    book.record(kind, subkind_of(kind, Some(&rec.title), rec.gain_units.map(|g| g.as_str())))
}

pub fn track_text(book: &TrackBook, rec: &Recommendation) -> String {
    record_text(&rec_record(book, rec))
}

/// Compact per-kind summary for the dashboard header: [{kind, label, n, enough, hit_rate, min_n, text}].
pub fn strip(book: &TrackBook, kinds: &[&str]) -> Vec<Value> {
    kinds
        .iter()
        .map(|k| {
            let tr = book.record(k, None);
            let text = if tr.enough {
                format!("{}% gained (n={})", fixed(tr.hit_rate.unwrap_or(0.0) * 100.0, 0, false), tr.n)
            } else {
                format!("{}/{} graded", tr.n, tr.min_n)
            };
            json!({"kind": k, "label": kind_noun(k), "n": tr.n, "enough": tr.enough, "hit_rate": tr.hit_rate,
                   "min_n": tr.min_n, "text": text})
        })
        .collect()
}

pub const STRIP_KINDS: [&str; 3] = ["waiver", "lineup", "injury"];

/// `{n, hit_rate, avg_gain, ci, ...}` for one kind / subkind (`TrackBook.record`).
pub fn track_record(kind: &str, subkind: Option<&str>, data_dir: &Path, league: &str, today: NaiveDate) -> Result<TrackRecord> {
    Ok(TrackBook::load(data_dir, league, today, TRACK_DAYS)?.record(kind, subkind))
}

pub fn as_json(book: &TrackBook, kinds: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for k in kinds {
        out.insert(k.to_string(), TrackBook::to_json(&book.record(k, None)));
    }
    Value::Object(out)
}
