//! Rust port of `fantasy_manager/web/health_view.py`: the /health page's view model, chart specs
//! (SVG from [`crate::charts`] plus a data-table fallback) built from `fm_harness::health::series`
//! output, and small formatting helpers. Owner: work package `W9-web-views`.
//!
//! For W10 (templates): the template global `hv` exposes `BASELINE_LABEL`, `KIND_TITLE`,
//! `METRIC_LABEL`, `ORIGIN_LABEL`, `pct`, `num`, `signed_pct`. They are provided here as
//! constants / functions on `Option<f64>`; register them as filters or globals as needed. The
//! `svg` and `legend` strings of every chart spec are HTML: mark them safe.

use fm_core::Map;
use fm_pycompat::fmt::{fixed, general, repr_f64};
use serde_json::{json, Value};

use crate::charts::{self, ChartOpts, Line, DASHES};

pub const POOL_TITLE: [(&str, &str); 3] = [("F", "Forwards"), ("D", "Defense"), ("G", "Goalies")];
pub const BASELINE_LABEL: [(&str, &str); 4] =
    [("to_date", "Season to date"), ("preseason", "Preseason fm"), ("provider", "Provider projection"), ("last_season", "Last season")];
pub const KIND_TITLE: [(&str, &str); 5] =
    [("waiver", "Waiver"), ("lineup", "Lineup"), ("injury", "Injury"), ("sell_high", "Sell-high"), ("buy_low", "Buy-low")];
pub const ORIGIN_LABEL: [(&str, &str); 4] =
    [("followed", "Followed"), ("partial", "Partly followed"), ("ignored", "Ignored"), ("user_only", "Your own moves")];
pub const METRIC_LABEL: [(&str, &str); 6] = [
    ("proj_fpg_mae", "Projection MAE (28-day FPG)"),
    ("proj_fpg_skill", "Skill vs season-to-date"),
    ("proj_week_mae", "Week points MAE"),
    ("hit_rate", "Hit rate"),
    ("calibration", "Calibration"),
    ("counterfactual", "Your moves vs the model's"),
];

fn lookup(table: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// `KIND_TITLE.get(kind, kind)`.
pub fn kind_title(kind: &str) -> String {
    lookup(&KIND_TITLE, kind).unwrap_or(kind).to_string()
}

/// `pct(v)`: "45%" or "-".
pub fn pct(v: Option<f64>) -> String {
    match v {
        None => "-".into(),
        Some(v) => format!("{}%", fixed(v * 100.0, 0, false)),
    }
}

/// `num(v, nd)`: fixed decimals or "-".
pub fn num(v: Option<f64>, nd: usize) -> String {
    match v {
        None => "-".into(),
        Some(v) => fixed(v, nd, false),
    }
}

/// `signed_pct(v)`: "+45%" or "-".
pub fn signed_pct(v: Option<f64>) -> String {
    match v {
        None => "-".into(),
        Some(v) => format!("{}%", fixed(v * 100.0, 0, true)),
    }
}

/// Python `str(x)` of a JSON value (ints print as ints, floats as repr).
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => {
            if n.is_f64() {
                repr_f64(n.as_f64().unwrap_or(0.0))
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn opt_f64(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64())
}

fn float_list(v: Option<&Value>) -> Vec<Option<f64>> {
    v.and_then(|v| v.as_array()).map(|a| a.iter().map(|x| x.as_f64()).collect()).unwrap_or_default()
}

type Fmt = fn(Option<f64>) -> String;

fn table(x: &[String], lines: &[Line], fmt: Fmt, extra: &[(String, Vec<Value>)]) -> Value {
    let mut head: Vec<String> = vec!["Week".into()];
    head.extend(lines.iter().map(|l| l.label.clone()));
    head.extend(extra.iter().map(|(k, _)| k.clone()));
    let mut rows: Vec<Value> = Vec::new();
    for (i, wk) in x.iter().enumerate() {
        let mut row: Vec<String> = vec![wk.clone()];
        for ln in lines {
            row.push(if i < ln.values.len() { fmt(ln.values[i]) } else { "-".into() });
        }
        for (_, v) in extra {
            row.push(match v.get(i) {
                Some(Value::Null) | None => "-".into(),
                Some(x) => py_str(x),
            });
        }
        rows.push(json!(row));
    }
    json!({"head": head, "rows": rows})
}

struct ChartKw<'a> {
    band: Option<(Vec<Option<f64>>, Vec<Option<f64>>)>,
    zero_line: bool,
    y_domain: Option<(f64, f64)>,
    _p: std::marker::PhantomData<&'a ()>,
}

fn chart(cid: &str, s: &Value, lines: &[Line], title: &str, desc: &str, fmt: Fmt, kw: ChartKw<'_>) -> Map<String, Value> {
    let x: Vec<String> = s
        .get("x")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(py_str).collect())
        .unwrap_or_default();
    let trust = s.get("trust").cloned().unwrap_or_else(|| json!("hidden"));
    let visible = trust.as_str() != Some("hidden") && !x.is_empty();
    let mut out: Map<String, Value> = Map::new();
    out.insert("id".into(), json!(cid));
    out.insert("title".into(), json!(title));
    out.insert("trust".into(), trust);
    out.insert("n".into(), s.get("n").cloned().unwrap_or_else(|| json!(0)));
    out.insert("need".into(), s.get("need").cloned().unwrap_or(Value::Null));
    out.insert("visible".into(), json!(visible));
    out.insert("points".into(), json!(x.len()));
    if visible {
        let y_fmt = |v: f64| fmt(Some(v));
        let opts = ChartOpts {
            band: kw.band.as_ref().map(|(a, b)| (a.as_slice(), b.as_slice())),
            y_fmt: &y_fmt,
            zero_line: kw.zero_line,
            y_domain: kw.y_domain,
            ..ChartOpts::default()
        };
        out.insert("svg".into(), json!(charts::line_chart(cid, &x, lines, title, desc, &opts)));
        out.insert("legend".into(), json!(charts::legend(lines, kw.band.as_ref().map(|_| "95% CI (Wilson)"))));
        let mut extra: Vec<(String, Vec<Value>)> = Vec::new();
        if let Some((lo, hi)) = &kw.band {
            let ci: Vec<Value> = lo
                .iter()
                .zip(hi)
                .map(|(a, b)| match (a, b) {
                    (Some(_), Some(_)) => json!(format!("{} to {}", fmt(*a), fmt(*b))),
                    _ => Value::Null,
                })
                .collect();
            extra.push(("95% CI".into(), ci));
        }
        if let Some(nbw) = s.get("n_by_week").and_then(|v| v.as_array()).filter(|a| !a.is_empty()) {
            extra.push(("N".into(), nbw.clone()));
        }
        out.insert("table".into(), table(&x, lines, fmt, &extra));
    }
    out
}

fn baseline_lines(values: &Value, main_label: &str) -> Vec<Line> {
    let mut lines = vec![Line::new("fm", main_label, float_list(values.get("fm")), "main", "")];
    for (i, (b, label)) in BASELINE_LABEL.iter().enumerate() {
        let Some(v) = values.get(*b) else { continue };
        let vals = float_list(Some(v));
        if vals.iter().any(|x| x.is_some()) || v.as_array().is_some_and(|a| a.iter().any(|x| !x.is_null())) {
            lines.push(Line::new(b, label, vals, "base", DASHES[i % DASHES.len()]));
        }
    }
    lines
}

fn or_empty<'a>(v: Option<&'a Value>) -> &'a Value {
    static EMPTY: Value = Value::Null;
    v.filter(|v| !v.is_null()).unwrap_or(&EMPTY)
}

fn num2(v: Option<f64>) -> String {
    num(v, 2)
}

/// `{"mae": [...per pool], "skill": [...per pool], "hit": [...per kind]}` chart specs.
pub fn health_charts(series: Option<&Value>, league: &str) -> Value {
    let null = Value::Null;
    let series = series.unwrap_or(&null);
    let mut mae: Vec<Value> = Vec::new();
    let mut skill: Vec<Value> = Vec::new();
    let mut hit: Vec<Value> = Vec::new();
    let no_kw = || ChartKw { band: None, zero_line: false, y_domain: None, _p: std::marker::PhantomData };
    for (pool, pool_title) in POOL_TITLE {
        let s = or_empty(series.get("mae").and_then(|m| m.get(pool)));
        let lines = baseline_lines(or_empty(s.get("lines")), "fm model");
        let latest = match opt_f64(s.get("value")) {
            Some(v) => format!("latest {}", num(Some(v), 2)),
            None => "no value yet".to_string(),
        };
        let mut c = chart(
            &format!("{league}-mae-{pool}"),
            s,
            &lines,
            &format!("{pool_title}: projection MAE, next 28 days"),
            &format!(
                "Mean absolute error of projected fantasy points per game against the next 28 days, one point per weekly snapshot; fm model in bold, baselines as thin lines; lower is better; {latest}."
            ),
            num2,
            no_kw(),
        );
        c.insert("pool".into(), json!(pool));
        c.insert("label".into(), json!(pool_title));
        mae.push(Value::Object(c.into_iter().collect()));

        let s = or_empty(series.get("skill").and_then(|m| m.get(pool)));
        let lines = baseline_lines(or_empty(s.get("lines")), "fm model");
        let mut c = chart(
            &format!("{league}-skill-{pool}"),
            s,
            &lines,
            &format!("{pool_title}: skill vs season-to-date"),
            &format!(
                "Skill score 1 - MAE / MAE of season-to-date FPG per weekly snapshot; above zero beats season-to-date; season-to-date itself is the zero line; latest fm skill {}.",
                pct(opt_f64(s.get("value")))
            ),
            pct,
            ChartKw { zero_line: true, ..no_kw() },
        );
        c.insert("pool".into(), json!(pool));
        c.insert("label".into(), json!(pool_title));
        skill.push(Value::Object(c.into_iter().collect()));
    }
    if let Some(kinds) = series.get("hit").and_then(|h| h.as_object()) {
        for (kind, s) in kinds {
            let band = or_empty(s.get("band"));
            let lo = float_list(band.get("lo"));
            let hi = float_list(band.get("hi"));
            let lines = vec![Line::new("rate", "Hit rate", float_list(or_empty(s.get("lines")).get("rate")), "main", "")];
            let title = kind_title(kind);
            let n = s.get("n").map(py_str).unwrap_or_else(|| "0".into());
            let mut c = chart(
                &format!("{league}-hit-{kind}"),
                s,
                &lines,
                &format!("{title} recommendations: hit rate"),
                &format!(
                    "Share of the model's graded recommendations of this kind (followed, partly followed or ignored) that gained points, as of each graded week, with a Wilson 95% band; latest {} of {n}.",
                    pct(opt_f64(s.get("value")))
                ),
                pct,
                ChartKw { band: Some((lo, hi)), y_domain: Some((0.0, 1.0)), ..no_kw() },
            );
            c.insert("kind".into(), json!(kind));
            c.insert("label".into(), json!(title));
            hit.push(Value::Object(c.into_iter().collect()));
        }
    }
    json!({"mae": mae, "skill": skill, "hit": hit})
}

/// The dry-run proposal rows, formatted.
pub fn refit_rows(result: Option<&Value>) -> Vec<Value> {
    let mut rows = Vec::new();
    let Some(list) = result.and_then(|r| r.get("rows")).and_then(|r| r.as_array()) else { return rows };
    for r in list {
        let Some(obj) = r.as_object() else { continue };
        let mut o = obj.clone();
        let g4 = |k: &str| general(obj.get(k).and_then(|v| v.as_f64()).unwrap_or(f64::NAN), Some(4), false);
        let n4 = |k: &str| num(opt_f64(obj.get(k)), 4);
        o.insert("current_s".into(), json!(g4("current")));
        o.insert("candidate_s".into(), json!(g4("candidate")));
        o.insert("holdout".into(), json!(format!("{} → {}", n4("holdout_before"), n4("holdout_after"))));
        o.insert("hist".into(), json!(format!("{} → {}", n4("hist_before"), n4("hist_after"))));
        rows.push(Value::Object(o));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatters() {
        assert_eq!(pct(None), "-");
        assert_eq!(pct(Some(0.456)), "46%");
        assert_eq!(num(Some(1.23456), 3), "1.235");
        assert_eq!(signed_pct(Some(0.456)), "+46%");
        assert_eq!(signed_pct(Some(-0.004)), "-0%");
    }

    #[test]
    fn hidden_series_have_no_svg() {
        let c = health_charts(None, "espn");
        assert_eq!(c["mae"].as_array().unwrap().len(), 3);
        assert_eq!(c["mae"][0]["visible"], json!(false));
        assert!(c["mae"][0].get("svg").is_none());
        assert_eq!(c["hit"].as_array().unwrap().len(), 0);
    }
}
