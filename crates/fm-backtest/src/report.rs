//! Rust port of `fantasy_manager/backtest/report.py`: the markdown report of a backtest run.
//!
//! Owner: work package `W2-harness-ledger-stats`.
//!
//! Deviations from the Python:
//! * `fitted` / `info` are `serde_json::Value`s (Python: `Mapping[str, Any]`); numbers keep the
//!   int/float distinction, which `_f` relies on (an int prints as `str(v)`, a float with
//!   `.{d}f`).
//! * `recommendations` / `render` / `write_report` take the app constants Python imports lazily
//!   from `valuation.blend` / `valuation.dynasty` (`K_BASELINE`, `K_INSEASON`, `RECENCY_WEIGHTS`,
//!   `PRODUCTION_CURVES`) as an explicit [`AppConstants`]; they are only read when `fitted` is
//!   given. The caller fills it from `fm_engine` (those modules are written by W2-valuation-base
//!   and W4-valuate).
//! * `season_label` and `REPORT_AGES` are re-exported from `data` / `fit`.
//! * `write_report` writes `\n` line ends (Python's `write_text` writes `\r\n` on Windows).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fm_core::Map;
use fm_pycompat::fmt::{fixed, general, percent, repr_f64};
use indexmap::IndexMap;
use serde_json::Value;

use crate::evaluate::{head_to_head, summarize, ResultRow, INSEASON_MODELS};

pub const PRESEASON_ORDER: [&str; 5] = ["naive", "marcel", "fm_current", "fm_fitted", "fm_multi"];
pub use crate::data::season_label;
pub use crate::fit::REPORT_AGES;

/// The valuation constants `recommendations` compares a fit against.
#[derive(Clone, Debug, Default)]
pub struct AppConstants {
    /// `blend.K_BASELINE` {F, D, G}
    pub k_baseline: Map<String, f64>,
    /// `blend.K_INSEASON` {skater, goalie}
    pub k_inseason: Map<String, f64>,
    /// `blend.RECENCY_WEIGHTS` {season, last30, last15, last7}
    pub recency_weights: Map<String, f64>,
    /// `dynasty.PRODUCTION_CURVES`: group -> (age, multiplier) anchors
    pub production_curves: Map<String, Vec<(f64, f64)>>,
}

// --------------------------------------------------------------------------- python str()

/// Python `repr(str)`.
fn repr_str(s: &str) -> String {
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

/// Python `repr(obj)` of a JSON-derived object.
fn pyrepr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.to_string(),
            None => match n.as_u64() {
                Some(u) => u.to_string(),
                None => repr_f64(n.as_f64().unwrap_or(f64::NAN)),
            },
        },
        Value::String(s) => repr_str(s),
        Value::Array(a) => format!("[{}]", a.iter().map(pyrepr).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter().map(|(k, x)| format!("{}: {}", repr_str(k), pyrepr(x))).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Python `str(obj)` / f-string interpolation of a JSON-derived object (`None` for a missing key).
fn pystr(v: Option<&Value>) -> String {
    match v {
        None => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => pyrepr(other),
    }
}

// --------------------------------------------------------------------------- cells

/// `_f(v, d)` for a float metric (None / NaN -> "-").
fn f_opt(v: Option<f64>, d: usize) -> String {
    match v {
        None => "-".to_string(),
        Some(x) if x.is_nan() => "-".to_string(),
        Some(x) => fixed(x, d, false),
    }
}

/// `_f(v, d)` for a JSON value: null -> "-", float -> `.{d}f`, anything else -> `str(v)`.
fn f_val(v: Option<&Value>, d: usize) -> String {
    match v {
        None | Some(Value::Null) => "-".to_string(),
        Some(Value::Number(n)) if n.is_f64() => f_opt(n.as_f64(), d),
        Some(other) => pystr(Some(other)),
    }
}

/// `_pct(v)`.
fn pct(v: Option<f64>) -> String {
    match v {
        None => "-".to_string(),
        Some(x) if x.is_nan() => "-".to_string(),
        Some(x) => percent(x, 0, false),
    }
}

fn table(headers: &[String], rows: &[Vec<String>]) -> Vec<String> {
    let mut out = vec![
        format!("| {} |", headers.join(" | ")),
        format!("|{}|", headers.iter().map(|_| "---").collect::<Vec<_>>().join("|")),
    ];
    for r in rows {
        out.push(format!("| {} |", r.join(" | ")));
    }
    out
}

fn s(x: &str) -> String {
    x.to_string()
}

/// `_ordered`: models in `order` first, then the rest in their given order.
fn ordered(models: &[String], order: &[&str]) -> Vec<String> {
    let mut out: Vec<String> =
        order.iter().filter(|m| models.iter().any(|x| x == *m)).map(|m| m.to_string()).collect();
    out.extend(models.iter().filter(|m| !order.contains(&m.as_str())).cloned());
    out
}

fn nan_or(v: Option<f64>) -> f64 {
    v.unwrap_or(f64::NAN)
}

// --------------------------------------------------------------------------- sections

pub fn summary_table(rows: &[ResultRow], kind: &str, pool: &str, order: &[&str]) -> Vec<String> {
    let sm = summarize(rows, kind, pool);
    if sm.is_empty() {
        return vec!["(no rows)".to_string()];
    }
    let names: Vec<String> = sm.keys().cloned().collect();
    let mut body = Vec::new();
    for m in ordered(&names, order) {
        let v = &sm[&m];
        body.push(vec![
            format!("`{m}`"),
            f_opt(v.mae, 3),
            f_opt(v.rmse, 3),
            f_opt(v.bias, 3),
            f_opt(v.spearman, 3),
            pct(v.top50),
            pct(v.top100),
            v.n.to_string(),
        ]);
    }
    let headers: Vec<String> =
        ["model", "MAE", "RMSE", "bias", "Spearman", "top-50", "top-100", "n (player-seasons)"]
            .iter()
            .map(|h| s(h))
            .collect();
    table(&headers, &body)
}

pub fn per_season_table(rows: &[ResultRow], kind: &str, pool: &str, order: &[&str]) -> Vec<String> {
    let sel: Vec<&ResultRow> = rows.iter().filter(|r| r.kind == kind && r.pool == pool).collect();
    let mut names: Vec<String> = sel.iter().map(|r| r.model.clone()).collect();
    names.sort();
    names.dedup();
    let models = ordered(&names, order);
    let mut keys: Vec<(i64, Option<String>)> = sel.iter().map(|r| (r.season, r.checkpoint.clone())).collect();
    keys.sort();
    keys.dedup();
    let mut idx: IndexMap<(i64, Option<String>, &str), &ResultRow> = IndexMap::new();
    for r in &sel {
        idx.insert((r.season, r.checkpoint.clone(), r.model.as_str()), r);
    }
    let mut headers = vec![s(if kind == "preseason" { "season" } else { "checkpoint" }), s("n")];
    headers.extend(models.iter().map(|m| format!("{m} MAE")));
    headers.extend(models.iter().map(|m| format!("{m} rho")));
    let mut body = Vec::new();
    for (sn, cp) in &keys {
        let get = |m: &String| idx.get(&(*sn, cp.clone(), m.as_str())).copied();
        let first = models.iter().find_map(|m| get(m));
        let label = match cp {
            None => season_label(*sn),
            Some(c) => format!("{} {}", season_label(*sn), c.chars().skip(5).collect::<String>()),
        };
        let mut cells = vec![label, first.map(|f| f.n.to_string()).unwrap_or_else(|| s("-"))];
        let maes: Vec<(&String, f64)> = models.iter().filter_map(|m| get(m).map(|r| (m, r.mae))).collect();
        // Python min(): first value unless a later one is strictly smaller.
        let best = maes.iter().map(|(_, v)| *v).reduce(|a, b| if b < a { b } else { a });
        for m in &models {
            let v = maes.iter().find(|(k, _)| *k == m).map(|(_, v)| *v);
            cells.push(match v {
                Some(x) if Some(x) == best => format!("**{}**", f_opt(Some(x), 3)),
                other => f_opt(other, 3),
            });
        }
        for m in &models {
            cells.push(match get(m) {
                Some(r) => f_opt(Some(r.spearman), 3),
                None => s("-"),
            });
        }
        body.push(cells);
    }
    table(&headers, &body)
}

pub fn verdict(rows: &[ResultRow]) -> Vec<String> {
    let sm = summarize(rows, "preseason", "skaters");
    let Some(cur) = sm.get("fm_current") else {
        return vec!["`fm_current` was not evaluated.".to_string()];
    };
    let mut out = Vec::new();
    for other in ["naive", "marcel", "fm_fitted", "fm_multi"] {
        let Some(o) = sm.get(other) else { continue };
        let (w_mae, n) = head_to_head(rows, "fm_current", other, "preseason", "skaters", "mae");
        let (w_rho, _) = head_to_head(rows, "fm_current", other, "preseason", "skaters", "spearman");
        let (cm, om) = (nan_or(cur.mae), nan_or(o.mae));
        let better = cm < om;
        let rel = if o.mae.is_some() && om != 0.0 { (om - cm) / om } else { 0.0 };
        out.push(format!(
            "- `fm_current` vs `{other}` (skaters): MAE {} vs {} ({} by {}); lower MAE in {w_mae}/{n} seasons, \
             higher Spearman in {w_rho}/{n} (rho {} vs {}).",
            fixed(cm, 3, false),
            fixed(om, 3, false),
            if better { "better" } else { "worse" },
            percent(rel.abs(), 1, false),
            fixed(nan_or(cur.spearman), 3, false),
            fixed(nan_or(o.spearman), 3, false),
        ));
    }
    let g = summarize(rows, "preseason", "goalies");
    if g.contains_key("fm_current") {
        let names: Vec<String> = g.keys().cloned().collect();
        let parts: Vec<String> = ordered(&names, &PRESEASON_ORDER)
            .iter()
            .map(|m| format!("`{m}` {}", fixed(nan_or(g[m].mae), 3, false)))
            .collect();
        out.push(format!("- Goalies (MAE): {}.", parts.join(", ")));
    }
    let ins = summarize(rows, "inseason", "skaters");
    if !ins.is_empty() {
        let names: Vec<String> = ins.keys().cloned().collect();
        let parts: Vec<String> = ordered(&names, &INSEASON_MODELS)
            .iter()
            .map(|m| format!("`{m}` {}", fixed(nan_or(ins[m].mae), 3, false)))
            .collect();
        out.push(format!("- In-season rest-of-season skaters (MAE): {}.", parts.join(", ")));
    }
    out
}

/// `d.get(key) or {}` -> the object if it is a non-empty object.
fn truthy_obj<'a>(v: Option<&'a Value>) -> Option<&'a serde_json::Map<String, Value>> {
    v.and_then(|x| x.as_object()).filter(|m| !m.is_empty())
}

fn num(v: Option<&Value>) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(f64::NAN)
}

pub fn fitted_section(fitted: &Value) -> Vec<String> {
    let pre = fitted.get("preseason").unwrap_or(&Value::Null);
    let mut out = vec![format!(
        "Fitted on scoring `{}` ({}), {} season pairs.",
        pystr(fitted.get("scoring")),
        pystr(fitted.get("generated")),
        pystr(pre.get("pairs"))
    )];
    let empty = serde_json::Map::new();
    let app_k = truthy_obj(pre.get("app_k")).unwrap_or(&empty);
    let at = |name: &str, g: &str| pre.get(name).and_then(|m| m.get(g));
    let groups = ["F", "D", "G"];
    if app_k.contains_key("F") {
        // the app's 3-season baseline k per group
        out.extend([
            s(""),
            s("**Preseason shrinkage k** toward the positional mean (1 season = `fm_fitted`, 3 seasons = \
               `fm_multi`, which the app uses):"),
            s(""),
        ]);
        let headers: Vec<String> = [
            "group",
            "fitted k (1 season)",
            "MAE at k=0",
            "MAE at fitted k",
            "fitted k (3 seasons)",
            "MAE at fitted k (3 seasons)",
            "app k",
            "MAE at app k (3 seasons)",
        ]
        .iter()
        .map(|h| s(h))
        .collect();
        let body: Vec<Vec<String>> = groups
            .iter()
            .map(|g| {
                vec![
                    s(g),
                    f_val(at("k", g), 0),
                    f_val(at("mae_by_k", g).and_then(|m| m.get("0")), 4),
                    f_val(at("mae_at_fitted_k", g), 4),
                    f_val(truthy_obj(pre.get("k_multi")).and_then(|m| m.get(*g)), 0),
                    f_val(truthy_obj(pre.get("mae_at_fitted_k_multi")).and_then(|m| m.get(*g)), 4),
                    f_val(app_k.get(*g), 0),
                    f_val(at("mae_at_app_k", g), 4),
                ]
            })
            .collect();
        out.extend(table(&headers, &body));
    } else {
        // files written before the app switched to the 3-season baseline
        out.extend([
            s(""),
            format!(
                "**Preseason shrinkage k** (prior season toward positional mean; app used {} skaters / {} goalies):",
                pystr(app_k.get("skater")),
                pystr(app_k.get("goalie"))
            ),
            s(""),
        ]);
        let headers: Vec<String> =
            ["group", "fitted k", "MAE at k=0", "MAE at app k", "MAE at fitted k"].iter().map(|h| s(h)).collect();
        let body: Vec<Vec<String>> = groups
            .iter()
            .map(|g| {
                vec![
                    s(g),
                    f_val(at("k", g), 0),
                    f_val(at("mae_by_k", g).and_then(|m| m.get("0")), 4),
                    f_val(at("mae_at_app_k", g), 4),
                    f_val(at("mae_at_fitted_k", g), 4),
                ]
            })
            .collect();
        out.extend(table(&headers, &body));
    }
    out.push(format!("\nSkaters pooled: k = {}.", f_val(pre.get("k_skater_pooled"), 0)));
    if let Some(km) = truthy_obj(pre.get("k_multi")) {
        out.push(format!(
            "`fm_multi` (5/4/3-weighted seasons) k: F {}, D {}, G {} (in games of the most recent season).",
            fixed(num(km.get("F")), 0, false),
            fixed(num(km.get("D")), 0, false),
            fixed(num(km.get("G")), 0, false)
        ));
    }
    if let Some(ins) = truthy_obj(fitted.get("inseason")) {
        let ins = Value::Object(ins.clone());
        let w = ins.get("recency_weights").unwrap_or(&Value::Null);
        let app = ins.get("app").unwrap_or(&Value::Null);
        let aw = app.get("recency_weights").unwrap_or(&Value::Null);
        let n_seasons = ins.get("seasons").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
        let wf = |v: &Value, k: &str| fixed(num(v.get(k)), 2, false);
        out.push(String::new());
        out.push(format!(
            "**In-season** ({} seasons, {} skater checkpoint obs): k skater {} (app {}), k goalie {} (app {}); \
             recency weights season/L30/L15/L7 = {}/{}/{}/{} (app {}/{}/{}/{}). In-sample MAE app {} -> fitted {} \
             (shrink only, fitted k: {}).",
            n_seasons,
            pystr(ins.get("n_skater_obs")),
            fixed(num(ins.get("k_skater")), 0, false),
            pystr(app.get("k_skater")),
            fixed(num(ins.get("k_goalie")), 0, false),
            pystr(app.get("k_goalie")),
            wf(w, "season"),
            wf(w, "last30"),
            wf(w, "last15"),
            wf(w, "last7"),
            wf(aw, "season"),
            wf(aw, "last30"),
            wf(aw, "last15"),
            wf(aw, "last7"),
            fixed(num(ins.get("mae_app")), 4, false),
            fixed(num(ins.get("mae_fitted")), 4, false),
            fixed(num(ins.get("mae_shrink_only_fitted_k")), 4, false),
        ));
    }
    if let Some(rep) = truthy_obj(fitted.get("age_curve_report")) {
        out.extend([
            s(""),
            s("**Age curve** (FPG level, peak = 1.00; 90% bootstrap interval; n = pairs at that age):"),
            s(""),
        ]);
        let mut body = Vec::new();
        for g in groups {
            let Some(rg) = rep.get(g) else { continue };
            let peak = fitted
                .get("age_curve")
                .and_then(|c| c.get(g))
                .and_then(|c| c.get("peak_age"));
            let mut cells = vec![format!("{g} (peak {})", pystr(peak))];
            for a in REPORT_AGES {
                let v = rg.get(a.to_string()).filter(|v| !matches!(v, Value::Null) && v.as_object().map(|m| !m.is_empty()).unwrap_or(true));
                cells.push(match v {
                    None => s("-"),
                    Some(v) => format!(
                        "{} [{}-{}] n={}",
                        fixed(num(v.get("level")), 2, false),
                        f_val(v.get("lo"), 2),
                        f_val(v.get("hi"), 2),
                        pystr(v.get("n"))
                    ),
                });
            }
            body.push(cells);
        }
        let mut headers = vec![s("group")];
        headers.extend(REPORT_AGES.iter().map(|a| a.to_string()));
        out.extend(table(&headers, &body));
    }
    out
}

pub fn recommendations(rows: &[ResultRow], fitted: Option<&Value>, consts: &AppConstants) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let sm = summarize(rows, "preseason", "skaters");
    let g = summarize(rows, "preseason", "goalies");
    // best = min(s, key=lambda m: s[m]["mae"]): the first model with the smallest MAE
    let best: Option<&String> = sm.keys().reduce(|a, b| if nan_or(sm[b].mae) < nan_or(sm[a].mae) { b } else { a });
    let best_differs = best.map(|b| b != "fm_current").unwrap_or(false) && sm.contains_key("fm_current");
    if let (true, Some(best)) = (best_differs, best) {
        let (b, c) = (&sm[best], &sm["fm_current"]);
        let goalies = if g.contains_key(best.as_str()) && g.contains_key("fm_current") {
            format!(
                "; goalies {} vs {}",
                fixed(nan_or(g[best.as_str()].mae), 3, false),
                fixed(nan_or(g["fm_current"].mae), 3, false)
            )
        } else {
            String::new()
        };
        out.push(format!(
            "- Preseason baseline: `{best}` is the most accurate skater model (MAE {} vs {}, Spearman {} vs {}{goalies}).",
            fixed(nan_or(b.mae), 3, false),
            fixed(nan_or(c.mae), 3, false),
            fixed(nan_or(b.spearman), 3, false),
            fixed(nan_or(c.spearman), 3, false),
        ));
    }
    if let Some(fitted) = fitted.filter(|f| f.as_object().map(|m| !m.is_empty()).unwrap_or(false)) {
        let kb = |g: &str| consts.k_baseline.get(g).copied().unwrap_or(f64::NAN);
        let pre = fitted.get("preseason").unwrap_or(&Value::Null);
        if let Some(km) = truthy_obj(pre.get("k_multi")) {
            let moved = ["F", "D", "G"]
                .iter()
                .filter(|g| km.contains_key(**g))
                .any(|g| (num(km.get(*g)) - kb(g)).abs() > 0.5);
            if moved {
                out.push(format!(
                    "- `blend.K_BASELINE` (3-season history toward the positional mean): data prefers \
                     F {} / D {} / G {} vs app {} / {} / {}.",
                    fixed(num(km.get("F")), 0, false),
                    fixed(num(km.get("D")), 0, false),
                    fixed(num(km.get("G")), 0, false),
                    general(kb("F"), None, false),
                    general(kb("D"), None, false),
                    general(kb("G"), None, false),
                ));
            }
        }
        if let Some(ins) = truthy_obj(fitted.get("inseason")) {
            let ki = |k: &str| consts.k_inseason.get(k).copied().unwrap_or(f64::NAN);
            let (ks, kg) = (num(ins.get("k_skater")), num(ins.get("k_goalie")));
            if (ks - ki("skater")).abs() > 0.5 || (kg - ki("goalie")).abs() > 0.5 {
                out.push(format!(
                    "- `blend.K_INSEASON` (season-to-date toward baseline): data prefers {} skaters / {} goalies \
                     vs app {} / {}.",
                    fixed(ks, 0, false),
                    fixed(kg, 0, false),
                    general(ki("skater"), None, false),
                    general(ki("goalie"), None, false),
                ));
            }
            if let Some(w) = ins.get("recency_weights").and_then(|w| w.as_object()) {
                let rw = &consts.recency_weights;
                let differs = w.iter().any(|(k, v)| match rw.get(k) {
                    Some(a) => (num(Some(v)) - a).abs() > 0.011,
                    None => false,
                });
                if differs {
                    let wf = |k: &str| fixed(num(w.get(k)), 2, false);
                    let af = |k: &str| fixed(rw.get(k).copied().unwrap_or(f64::NAN), 2, false);
                    out.push(format!(
                        "- `RECENCY_WEIGHTS`: season {} / L30 {} / L15 {} / L7 {} vs app {} / {} / {} / {}.",
                        wf("season"),
                        wf("last30"),
                        wf("last15"),
                        wf("last7"),
                        af("season"),
                        af("last30"),
                        af("last15"),
                        af("last7"),
                    ));
                }
            }
        }
        let dynv = truthy_obj(fitted.get("dynasty_age_curves"));
        let stale: Vec<&str> = ["F", "D"]
            .into_iter()
            .filter(|g| {
                let Some(d) = dynv.and_then(|d| d.get(*g)) else { return false };
                let fit_pts: Vec<(f64, f64)> = d
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|pt| {
                                let p = pt.as_array();
                                (
                                    num(p.and_then(|p| p.first())),
                                    num(p.and_then(|p| p.get(1))),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                consts.production_curves.get(*g).map(|c| *c != fit_pts).unwrap_or(true)
            })
            .collect();
        if !stale.is_empty() {
            out.push(format!(
                "- `dynasty.PRODUCTION_CURVES` ({}) differ from this fit's `dynasty_age_curves`.",
                stale.join(", ")
            ));
        }
        if out.len() == if best_differs { 1 } else { 0 } {
            out.push(s("- The app's constants (valuation/fitted_params.json) match this fit; nothing to change."));
        } else if fitted.get("scoring").map(|v| v.as_str() == Some("espn")).unwrap_or(true) {
            out.push(s(
                "- To adopt this fit, copy it over the packaged valuation/fitted_params.json (see `fm backtest fit`).",
            ));
        } else {
            out.push(format!(
                "- The app ships the ESPN-scoring fit for every league; these `{}` differences are \
                 scoring-specific and not applied.",
                pystr(fitted.get("scoring"))
            ));
        }
    }
    let ins_s = summarize(rows, "inseason", "skaters");
    if let (Some(sh), Some(cu)) = (ins_s.get("fm_shrink_only"), ins_s.get("fm_current")) {
        if nan_or(sh.mae) < nan_or(cu.mae) {
            out.push(format!(
                "- The app's current recency blend is worse than no recency blend at all on held-out checkpoints \
                 (MAE {} vs {}).",
                fixed(nan_or(cu.mae), 3, false),
                fixed(nan_or(sh.mae), 3, false)
            ));
        }
    }
    out
}

/// Python truthiness of `info.get(key)`.
fn truthy(v: Option<&Value>) -> Option<&Value> {
    v.filter(|x| match x {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    })
}

pub fn render(
    rows: &[ResultRow],
    info: &Value,
    fitted: Option<&Value>,
    _fits: Option<&Value>,
    consts: &AppConstants,
) -> String {
    let seasons: Vec<i64> = info
        .get("seasons")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();
    let mut lines: Vec<String> =
        vec![format!("# Projection backtest ({} scoring)", pystr(info.get("scoring"))), s("")];
    if let (Some(first), Some(last)) = (seasons.first(), seasons.last()) {
        let ts: Vec<i64> = info
            .get("table_seasons")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
            .unwrap_or_default();
        lines.push(format!(
            "Seasons evaluated: {} to {} ({}); history table: {} to {}.",
            season_label(*first),
            season_label(*last),
            seasons.len(),
            ts.first().map(|t| season_label(*t)).unwrap_or_default(),
            ts.last().map(|t| season_label(*t)).unwrap_or_default(),
        ));
    } else {
        lines.push(s("No seasons evaluated."));
    }
    if let Some(u) = truthy(info.get("unscored_stats")).and_then(|u| u.as_array()) {
        let names: Vec<String> = u.iter().map(|x| pystr(Some(x))).collect();
        lines.push(format!("Not in NHL season data (weight ignored): {}.", names.join(", ")));
    }
    lines.push(s(
        "Population: players with >= 20 GP in both N-1 and N (same position group); projections use seasons < N \
         only. FPG = fantasy points per game played.",
    ));
    lines.push(s(""));
    let push_all = |lines: &mut Vec<String>, items: Vec<String>| lines.extend(items);
    push_all(&mut lines, vec![s("## Verdict"), s("")]);
    push_all(&mut lines, verdict(rows));
    push_all(&mut lines, vec![s("")]);
    push_all(&mut lines, vec![s("## Preseason: skaters (mean over seasons)"), s("")]);
    push_all(&mut lines, summary_table(rows, "preseason", "skaters", &PRESEASON_ORDER));
    push_all(&mut lines, vec![s(""), s("### Per season (skaters; bold = lowest MAE)"), s("")]);
    push_all(&mut lines, per_season_table(rows, "preseason", "skaters", &PRESEASON_ORDER));
    push_all(&mut lines, vec![s(""), s("## Preseason: goalies"), s("")]);
    push_all(&mut lines, summary_table(rows, "preseason", "goalies", &PRESEASON_ORDER));
    push_all(&mut lines, vec![s(""), s("### Per season (goalies)"), s("")]);
    push_all(&mut lines, per_season_table(rows, "preseason", "goalies", &PRESEASON_ORDER));
    if rows.iter().any(|r| r.kind == "inseason") {
        push_all(
            &mut lines,
            vec![
                s(""),
                s("## In-season checkpoints (rest-of-season FPG)"),
                s(""),
                s("Players with >= 1 GP before the checkpoint and >= 10 GP after it. `preseason` = the app's \
                   baseline alone, `to_date` = season-to-date FPG, `fm_shrink_only` = the app's shrinkage with no \
                   recency blend, `fm_current` = the app's shrinkage + recency blend (blend.py functions), \
                   `fm_fitted` = fitted k and recency weights (leave-one-season-out)."),
                s(""),
            ],
        );
        push_all(&mut lines, vec![s("### Skaters"), s("")]);
        push_all(&mut lines, summary_table(rows, "inseason", "skaters", &INSEASON_MODELS));
        push_all(&mut lines, vec![s(""), s("### Goalies"), s("")]);
        push_all(&mut lines, summary_table(rows, "inseason", "goalies", &INSEASON_MODELS));
        push_all(&mut lines, vec![s(""), s("### Per checkpoint (skaters)"), s("")]);
        push_all(&mut lines, per_season_table(rows, "inseason", "skaters", &INSEASON_MODELS));
    }
    if let Some(f) = fitted.filter(|f| f.as_object().map(|m| !m.is_empty()).unwrap_or(false)) {
        push_all(&mut lines, vec![s(""), s("## Fitted parameters"), s("")]);
        push_all(&mut lines, fitted_section(f));
    }
    let recs = recommendations(rows, fitted, consts);
    if !recs.is_empty() {
        push_all(&mut lines, vec![s(""), s("## Recommended constant changes"), s("")]);
        push_all(&mut lines, recs);
    }
    push_all(
        &mut lines,
        vec![
            s(""),
            s("## Caveats"),
            s(""),
            s("- No historical ESPN/Fantrax projections exist, so `fm_current` is the app's no-projection path \
               (`valuate.baseline_rates`: the last three seasons weighted 5/4/3, shrunk toward the positional mean, \
               times the age factor). The commercial projections are archived daily by \
               `fm backtest archive` and can be scored at season end."),
            s("- `fm_current` uses the packaged constants (valuation/fitted_params.json), fitted on every season \
               including the evaluated ones, so it is slightly in-sample; `fm_multi` / `fm_fitted` are re-fitted \
               before each target season on earlier seasons only."),
            s("- Rookies and players with < 20 GP last season are not in the preseason population."),
            s("- The age curve uses survivors (>= 20 GP both seasons); decline at 33+ is if anything understated."),
            s("- Goalie FPG per game played is noisy (relief appearances, team effects); rank correlations are low \
               for every model."),
        ],
    );
    if let Some(d) = truthy(info.get("data")) {
        lines.push(s(""));
        lines.push(format!("Data build: {}", pystr(Some(d))));
    }
    lines.join("\n") + "\n"
}

pub fn write_report(
    path: &Path,
    rows: &[ResultRow],
    info: &Value,
    fitted: Option<&Value>,
    fits: Option<&Value>,
    consts: &AppConstants,
) -> Result<PathBuf> {
    std::fs::write(path, render(rows, info, fitted, fits, consts))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path.to_path_buf())
}
