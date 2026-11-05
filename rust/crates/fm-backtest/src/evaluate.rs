//! Rust port of `fantasy_manager/backtest/evaluate.py` (the scoring-statistics half).
//!
//! Owner: work package `W2-harness-ledger-stats`.
//!
//! Ported here: the metrics (`rankdata`, `pearson`, `spearman`, `top_n_precision`, `metrics`),
//! the result rows, `Prediction`, the pooling helper, and the summaries (`summarize`,
//! `head_to_head`) that `report.rs` consumes.
//!
//! NOT ported yet (blocked, they are written against types of later waves that are still stubs):
//! `preseason_predictions`, `evaluate_preseason`, `inseason_observations`, `evaluate_inseason`
//! need `SeasonTable` / `Windows` (`fm_backtest::data`, W3-harness-deploy-data), `Model`s
//! (`fm_backtest::models`) and `fit_preseason` / `fit_inseason` / `InseasonObs`
//! (`fm_backtest::fit`, both W4-backtest-models). Their package can add them to this file next to
//! the pieces below; `Prediction`, `ResultRow`, `row()`, `pools()` and the metric helpers are
//! the building blocks they use.
//!
//! NaN: Python's `float("nan")` metrics stay `f64::NAN` here (they are `Option`-free fields);
//! `ResultRow::to_dict` turns them into JSON `null` (the port-wide NaN rule).

use fm_core::Map;
use fm_pycompat::num::py_sum;
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;

pub const MIN_GP: i64 = 20;
pub const MIN_GP_REST: i64 = 10;
pub const TOP_N: [usize; 2] = [50, 100];
pub const INSEASON_MODELS: [&str; 5] = ["preseason", "to_date", "fm_shrink_only", "fm_current", "fm_fitted"];

// --------------------------------------------------------------------------- metrics

/// Python's `sorted(..., key=...)` on floats (NaN compares equal, keeping input order).
fn cmp_keys(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// Ranks starting at 1, ties get their average rank.
pub fn rankdata(xs: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..xs.len()).collect();
    order.sort_by(|&a, &b| cmp_keys(xs[a], xs[b]));
    let mut ranks = vec![0.0; xs.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j + 1 < order.len() && xs[order[j + 1]] == xs[order[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for t in i..=j {
            ranks[order[t]] = avg;
        }
        i = j + 1;
    }
    ranks
}

pub fn pearson(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len();
    if n < 2 {
        return f64::NAN;
    }
    let nf = n as f64;
    let (mx, my) = (py_sum(x.iter().copied()) / nf, py_sum(y.iter().copied()) / nf);
    let sxy = py_sum(x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)));
    let sxx = py_sum(x.iter().map(|a| (a - mx).powf(2.0)));
    let syy = py_sum(y.iter().map(|b| (b - my).powf(2.0)));
    if sxx > 0.0 && syy > 0.0 {
        sxy / (sxx * syy).sqrt()
    } else {
        f64::NAN
    }
}

pub fn spearman(x: &[f64], y: &[f64]) -> f64 {
    pearson(&rankdata(x), &rankdata(y))
}

/// |projected top n  intersect  actual top n| / n (None if fewer than n players).
pub fn top_n_precision(pred: &[f64], actual: &[f64], n: usize) -> Option<f64> {
    if pred.len() < n {
        return None;
    }
    let top = |v: &[f64]| -> std::collections::HashSet<usize> {
        let mut idx: Vec<usize> = (0..pred.len()).collect();
        idx.sort_by(|&a, &b| cmp_keys(-v[a], -v[b]));
        idx.into_iter().take(n).collect()
    };
    let (tp, ta) = (top(pred), top(actual));
    Some(tp.intersection(&ta).count() as f64 / n as f64)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Metrics {
    pub n: usize,
    pub mae: f64,
    pub rmse: f64,
    pub bias: f64,
    pub spearman: f64,
    pub top50: Option<f64>,
    pub top100: Option<f64>,
}

pub fn metrics(pred: &[f64], actual: &[f64], top: bool) -> Metrics {
    let n = pred.len();
    if n == 0 {
        let nan = f64::NAN;
        return Metrics { n: 0, mae: nan, rmse: nan, bias: nan, spearman: nan, top50: None, top100: None };
    }
    let nf = n as f64;
    let err: Vec<f64> = pred.iter().zip(actual).map(|(p, a)| p - a).collect();
    Metrics {
        n,
        mae: py_sum(err.iter().map(|e| e.abs())) / nf,
        rmse: (py_sum(err.iter().map(|e| e * e)) / nf).sqrt(),
        bias: py_sum(err.iter().copied()) / nf,
        spearman: spearman(pred, actual),
        top50: if top { top_n_precision(pred, actual, 50) } else { None },
        top100: if top { top_n_precision(pred, actual, 100) } else { None },
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResultRow {
    /// preseason / inseason
    pub kind: String,
    pub season: i64,
    pub checkpoint: Option<String>,
    pub model: String,
    /// skaters / goalies
    pub pool: String,
    pub n: usize,
    pub mae: f64,
    pub rmse: f64,
    pub bias: f64,
    pub spearman: f64,
    pub top50: Option<f64>,
    pub top100: Option<f64>,
}

impl ResultRow {
    /// `asdict(row)` (field order of the dataclass; NaN becomes null).
    pub fn to_dict(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m.into_iter().collect(),
            _ => Map::new(),
        }
    }

    /// `getattr(row, metric)` for the float metrics (`None` for an unset top-N).
    pub fn metric(&self, name: &str) -> Option<f64> {
        match name {
            "mae" => Some(self.mae),
            "rmse" => Some(self.rmse),
            "bias" => Some(self.bias),
            "spearman" => Some(self.spearman),
            "top50" => self.top50,
            "top100" => self.top100,
            _ => None,
        }
    }
}

/// `_row`.
pub fn row(kind: &str, season: i64, checkpoint: Option<&str>, model: &str, pool: &str, m: &Metrics) -> ResultRow {
    ResultRow {
        kind: kind.to_string(),
        season,
        checkpoint: checkpoint.map(|s| s.to_string()),
        model: model.to_string(),
        pool: pool.to_string(),
        n: m.n,
        mae: m.mae,
        rmse: m.rmse,
        bias: m.bias,
        spearman: m.spearman,
        top50: m.top50,
        top100: m.top100,
    }
}

// --------------------------------------------------------------------------- preseason

#[derive(Clone, Debug, PartialEq)]
pub struct Prediction {
    pub season: i64,
    pub player_id: i64,
    pub name: String,
    pub group: String,
    pub model: String,
    pub projected: f64,
    pub actual: f64,
}

/// `_pools`, already sorted by `(model, pool)` as `evaluate_preseason` iterates it; the
/// predictions inside keep their input order.
pub fn pools<'a>(preds: &'a [Prediction]) -> Vec<((String, String), Vec<&'a Prediction>)> {
    let mut out: IndexMap<(String, String), Vec<&Prediction>> = IndexMap::new();
    for p in preds {
        let pool = if p.group == "G" { "goalies" } else { "skaters" };
        out.entry((p.model.clone(), pool.to_string())).or_default().push(p);
    }
    let mut v: Vec<_> = out.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

// --------------------------------------------------------------------------- summaries

/// One model's entry of `summarize`: mean of each metric over seasons (and checkpoints).
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub mae: Option<f64>,
    pub rmse: Option<f64>,
    pub bias: Option<f64>,
    pub spearman: Option<f64>,
    pub top50: Option<f64>,
    pub top100: Option<f64>,
    pub n: i64,
    pub seasons: usize,
}

impl Summary {
    /// `s[name]` for the metric keys.
    pub fn get(&self, name: &str) -> Option<f64> {
        match name {
            "mae" => self.mae,
            "rmse" => self.rmse,
            "bias" => self.bias,
            "spearman" => self.spearman,
            "top50" => self.top50,
            "top100" => self.top100,
            _ => None,
        }
    }
}

/// model -> mean of each metric over seasons (and checkpoints).
pub fn summarize(rows: &[ResultRow], kind: &str, pool: &str) -> IndexMap<String, Summary> {
    let mut by: IndexMap<&str, Vec<&ResultRow>> = IndexMap::new();
    for r in rows {
        if r.kind == kind && r.pool == pool {
            by.entry(r.model.as_str()).or_default().push(r);
        }
    }
    let mut out = IndexMap::new();
    for (model, rs) in by {
        let avg = |attr: &str| -> Option<f64> {
            let vals: Vec<f64> =
                rs.iter().filter_map(|r| r.metric(attr)).filter(|v| !v.is_nan()).collect();
            if vals.is_empty() {
                None
            } else {
                Some(py_sum(vals.iter().copied()) / vals.len() as f64)
            }
        };
        out.insert(
            model.to_string(),
            Summary {
                mae: avg("mae"),
                rmse: avg("rmse"),
                bias: avg("bias"),
                spearman: avg("spearman"),
                top50: avg("top50"),
                top100: avg("top100"),
                n: rs.iter().map(|r| r.n as i64).sum(),
                seasons: rs.len(),
            },
        );
    }
    out
}

/// (seasons where `a` is better than `b`, seasons compared) on `metric` (lower MAE/RMSE, higher
/// Spearman/top-N is better).
pub fn head_to_head(rows: &[ResultRow], a: &str, b: &str, kind: &str, pool: &str, metric: &str) -> (i64, i64) {
    let lower = metric == "mae" || metric == "rmse";
    let mut idx: IndexMap<(i64, Option<String>), IndexMap<String, Option<f64>>> = IndexMap::new();
    for r in rows {
        if r.kind == kind && r.pool == pool && (r.model == a || r.model == b) {
            idx.entry((r.season, r.checkpoint.clone())).or_default().insert(r.model.clone(), r.metric(metric));
        }
    }
    let (mut wins, mut total) = (0, 0);
    for v in idx.values() {
        if let (Some(Some(va)), Some(Some(vb))) = (v.get(a), v.get(b)) {
            total += 1;
            let better = if lower { va < vb } else { va > vb };
            if better {
                wins += 1;
            }
        }
    }
    (wins, total)
}
