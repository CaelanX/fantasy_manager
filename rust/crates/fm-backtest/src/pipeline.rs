//! Rust port of `fantasy_manager/backtest/pipeline.py`: orchestration used by `fm backtest`
//! (build data, evaluate, fit, write report / params).
//! Owner: work package `W4-backtest-models`.
//!
//! Deviations from the Python:
//! * Clock: `run_fit` takes `now` (the `generated` stamp) and `refresh_data` / `run_evaluation`
//!   take `today` explicitly (callers read the clock once, see ARCHITECTURE 4.5).
//! * `log` callbacks are `Option<&mut dyn FnMut(&str)>`.
//! * Files are written with `\n` line ends (Python's `write_text` writes `\r\n` on Windows).
//! * `run_fit` / `run_evaluation` return their dicts as `serde_json::Value` (`run_evaluation`
//!   as [`EvalOutput`]); NaN values become `null` (port-wide rule).
//! * The app constants `report.recommendations` compares a fit against come from
//!   [`app_constants`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use chrono::{NaiveDate, NaiveDateTime};
use fm_core::Map;
use fm_engine::valuation::blend::{k_baseline_snapshot, k_inseason_snapshot, recency_weights_snapshot};
use fm_engine::valuation::dynasty::ProductionCurves;
use fm_http::HttpCache;
use fm_pycompat::num::round_to;
use fm_pycompat::pyjson::{dumps, DumpOpts};
use fm_sources::nhl::NhlClient;
use serde_json::{json, Value};

use crate::data::{
    backtest_dir, build_table, build_windows, load_table, load_windows, prev_season, season_range, CountingFetcher,
    SeasonTable, LAST_SEASON,
};
use crate::evaluate::{evaluate_inseason, evaluate_preseason, inseason_observations, ResultRow, MIN_GP, MIN_GP_REST};
use crate::fit::{
    curve_get, dynasty_anchors, fit_inseason, fit_preseason, DYNASTY_ANCHOR_AGES, GROUPS, REPORT_AGES,
};
use crate::report::{write_report, AppConstants};
use crate::scoring::{load_scoring, missing_weights, scorer};

/// Reborrow an optional log callback for a nested call.
pub(crate) fn re_log<'a, 'b: 'a>(l: &'a mut Option<&'b mut (dyn FnMut(&str) + 'b)>) -> Option<&'a mut (dyn FnMut(&str) + 'a)> {
    match l {
        Some(x) => Some(&mut **x),
        None => None,
    }
}

pub const EVAL_FIRST: i64 = 20162017;
pub const INSEASON_FIRST: i64 = 20212022;

/// The packaged `valuation/fitted_params.json` path (`params.PARAMS_FILE`).
pub fn packaged_params_path() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fantasy_manager/valuation/fitted_params.json");
    p.canonicalize().unwrap_or(p).to_string_lossy().trim_start_matches(r"\\?\").to_string()
}

/// `blend.K_BASELINE` etc. as the report's [`AppConstants`] (`dynasty.PRODUCTION_CURVES`: F / D
/// from the packaged fit, G hand-set).
pub fn app_constants() -> AppConstants {
    let c = ProductionCurves::current();
    let mut production: Map<String, Vec<(f64, f64)>> = Map::new();
    production.insert("F".to_string(), c.f);
    production.insert("D".to_string(), c.d);
    production.insert("G".to_string(), c.g);
    AppConstants {
        k_baseline: k_baseline_snapshot().clone(),
        k_inseason: k_inseason_snapshot().clone(),
        recency_weights: recency_weights_snapshot().clone(),
        production_curves: production,
    }
}

pub fn default_eval_seasons(table: &SeasonTable) -> Vec<i64> {
    table.seasons().into_iter().filter(|&s| s >= EVAL_FIRST && table.season_rows(prev_season(s)).len() > 0).collect()
}

pub fn default_inseason_seasons(last: Option<i64>) -> Vec<i64> {
    season_range(INSEASON_FIRST, last.unwrap_or(LAST_SEASON))
}

/// Fetch missing seasons / checkpoints through the cache; returns the summary dict.
pub fn refresh_data(
    cache: Arc<HttpCache>,
    data_dir: &Path,
    seasons: &[i64],
    inseason_seasons: &[i64],
    force: bool,
    today: NaiveDate,
    mut log: Option<&mut dyn FnMut(&str)>,
) -> Result<Value> {
    let fetcher = Arc::new(CountingFetcher::new(cache, today, 0.0));
    let client = NhlClient::new(Arc::clone(&fetcher).into_fetch(), None, today);
    let t0 = Instant::now();
    let table = build_table(&client, data_dir, seasons, force, re_log(&mut log))?;
    let windows = if inseason_seasons.is_empty() {
        Default::default()
    } else {
        build_windows(&client, data_dir, inseason_seasons, force, today, re_log(&mut log))?
    };
    Ok(json!({
        "seasons": table.seasons(),
        "rows": table.rows.len(),
        "checkpoints": windows.values().map(|v| v.len()).sum::<usize>(),
        "network_requests": fetcher.network(),
        "cache_hits": fetcher.cached(),
        "network_seconds": round_to(fetcher.seconds(), 1),
        "wall_seconds": round_to(t0.elapsed().as_secs_f64(), 1),
    }))
}

/// '' for the default ESPN preset (the files the app reads), '-<name>' otherwise.
fn suffix(scoring_name: &str) -> String {
    if scoring_name == "espn" {
        String::new()
    } else {
        format!("-{scoring_name}")
    }
}

pub fn fitted_params_path(data_dir: &Path, scoring_name: &str) -> Result<PathBuf> {
    Ok(backtest_dir(data_dir)?.join(format!("fitted_params{}.json", suffix(scoring_name))))
}

fn nan_round(v: Option<f64>, nd: i32) -> f64 {
    round_to(v.unwrap_or(f64::NAN), nd)
}

/// Fit every constant on all stored history and write `fitted_params.json`.
pub fn run_fit(
    data_dir: &Path,
    scoring_name: &str,
    boot: usize,
    inseason_seasons: Option<&[i64]>,
    now: NaiveDateTime,
    _log: Option<&mut dyn FnMut(&str)>,
) -> Result<Value> {
    let (name, cfg) = load_scoring(scoring_name)?;
    let sc = scorer(&cfg)?;
    let table = load_table(data_dir)?;
    if table.seasons().is_empty() {
        return Err(anyhow!("no backtest data; run `fm backtest data` first"));
    }
    let targets: Vec<i64> = table.seasons().into_iter().filter(|&s| !table.season_rows(prev_season(s)).is_empty()).collect();
    let age_groups = ["F".to_string(), "D".to_string()];
    let (params, info) = fit_preseason(&table, &sc, &targets, boot, &age_groups);
    let windows = load_windows(data_dir)?;
    let ins_seasons: Vec<i64> = match inseason_seasons {
        Some(s) if !s.is_empty() => s.to_vec(),
        _ => {
            let mut k: Vec<i64> = windows.keys().copied().collect();
            k.sort();
            k
        }
    }
    .into_iter()
    .filter(|s| windows.contains_key(s))
    .collect();
    let ins = if ins_seasons.is_empty() {
        None
    } else {
        Some(fit_inseason(&inseason_observations(&table, &windows, &sc, &ins_seasons, MIN_GP_REST), 2))
    };
    let curves = &info.curves;
    let kb = k_baseline_snapshot();
    let k_in = k_inseason_snapshot();
    let by_group = |f: &dyn Fn(&str) -> Value| -> Value {
        Value::Object(GROUPS.iter().map(|g| (g.to_string(), f(g))).collect())
    };
    let mae_by_k = by_group(&|g| {
        let keep = [0.0, 6.0, 12.0, 20.0, 30.0, 40.0, 50.0, 60.0, 80.0, 100.0, 150.0, 200.0, 300.0];
        Value::Object(
            info.k_curves[g]
                .iter()
                .filter(|(k, _)| keep.contains(k))
                .map(|(k, v)| ((*k as i64).to_string(), json!(round_to(*v, 5))))
                .collect(),
        )
    });
    let mut out = json!({
        "generated": now.format("%Y-%m-%dT%H:%M:%S").to_string(),
        "scoring": name,
        "unscored_stats": missing_weights(&cfg),
        "train_target_seasons": targets,
    });
    let pre = json!({
        "pairs": info.pairs,
        "k": params.k,
        "k_skater_pooled": info.k_skater_pooled,
        "k_multi": params.k_multi,
        "mae_at_fitted_k_multi": by_group(&|g| json!(nan_round(curve_get(&info.k_multi_curves[g], params.k_multi[g]), 5))),
        "mae_at_fitted_k": by_group(&|g| json!(nan_round(curve_get(&info.k_curves[g], params.k[g]), 5))),
        // the app's baseline is the 3-season one, so its k is compared on the k_multi curve
        "mae_at_app_k": by_group(&|g| json!(nan_round(curve_get(&info.k_multi_curves[g], kb[g]), 5))),
        "app_k": by_group(&|g| json!(kb[g])),
        "mae_by_k": mae_by_k,
    });
    let inseason = match &ins {
        None => Value::Null,
        Some(i) => json!({
            "seasons": ins_seasons,
            "k_skater": i.k_skater, "k_goalie": i.k_goalie,
            "recency_weights": i.recency_weights,
            "app": {"k_skater": k_in["skater"], "k_goalie": k_in["goalie"],
                    "recency_weights": recency_weights_snapshot()},
            "mae_app": i.mae_app, "mae_fitted": i.mae_fitted,
            "mae_shrink_only_fitted_k": i.mae_shrink_only_fitted_k,
            "n_skater_obs": i.n_skater_obs, "n_goalie_obs": i.n_goalie_obs,
        }),
    };
    let age_curve = by_group(&|g| curves[g].to_dict());
    let age_curve_report = by_group(&|g| {
        let c = &curves[g];
        Value::Object(
            REPORT_AGES
                .iter()
                .map(|a| {
                    (
                        a.to_string(),
                        json!({
                            "level": round_to(c.level[a], 3),
                            "lo": nan_round(c.lo.get(a).copied(), 3),
                            "hi": nan_round(c.hi.get(a).copied(), 3),
                            "n": c.n.get(a).copied().unwrap_or(0),
                        }),
                    )
                })
                .collect(),
        )
    });
    let dynasty = by_group(&|g| {
        Value::Array(dynasty_anchors(&curves[g], &DYNASTY_ANCHOR_AGES).into_iter().map(|(a, l)| json!([a, l])).collect())
    });
    let obj = out.as_object_mut().expect("object");
    obj.insert("preseason".into(), pre);
    obj.insert("inseason".into(), inseason);
    obj.insert("age_curve".into(), age_curve);
    obj.insert("age_curve_report".into(), age_curve_report);
    obj.insert("dynasty_age_curves".into(), dynasty);
    obj.insert("fm_fitted".into(), params.to_dict());
    obj.insert(
        "notes".into(),
        json!([
            "age_curve levels are FPG trajectories normalized to peak = 1.0 (delta method, median \
             next/this FPG ratio pooled over age +-1, players with >= 20 GP in both seasons).",
            "yoy = expected FPG multiplier from the season at age a (Oct 1) to the next season.",
            "lo/hi = 90% player-clustered bootstrap interval of the level.",
            "dynasty_age_curves has the ((age, multiplier), ...) shape of valuation.dynasty.AGE_CURVES; \
             for a trajectory use level(age + y) / level(age).",
        ]),
    );
    let p = fitted_params_path(data_dir, &name)?;
    std::fs::write(&p, dumps(&out, &DumpOpts::cli()))?;
    let obj = out.as_object_mut().expect("object");
    obj.insert("path".into(), json!(p.to_string_lossy()));
    obj.insert("packaged_path".into(), json!(packaged_params_path()));
    Ok(out)
}

/// The stored fit (`fitted_params*.json`), None when missing or unreadable.
pub fn load_fitted(data_dir: &Path, scoring_name: &str) -> Option<Value> {
    let p = fitted_params_path(data_dir, scoring_name).ok()?;
    if !p.exists() {
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()
}

pub struct EvalOutput {
    pub rows: Vec<ResultRow>,
    pub results: String,
    pub report: String,
    pub info: Value,
}

/// Evaluate, write `results-<date>.json` and `report-<date>.md`; returns paths + rows.
#[allow(clippy::too_many_arguments)]
pub fn run_evaluation(
    data_dir: &Path,
    scoring_name: &str,
    models: Option<&[String]>,
    seasons: Option<&[i64]>,
    inseason: bool,
    inseason_seasons: Option<&[i64]>,
    today: NaiveDate,
    meta: Option<&serde_json::Map<String, Value>>,
    mut log: Option<&mut dyn FnMut(&str)>,
) -> Result<EvalOutput> {
    let (name, cfg) = load_scoring(scoring_name)?;
    let sc = scorer(&cfg)?;
    let table = load_table(data_dir)?;
    if table.seasons().is_empty() {
        return Err(anyhow!("no backtest data; run `fm backtest data` first"));
    }
    let wanted: Vec<i64> = match seasons {
        Some(s) if !s.is_empty() => s.to_vec(),
        _ => default_eval_seasons(&table),
    };
    let seasons: Vec<i64> = wanted.into_iter().filter(|&s| !table.season_rows(s).is_empty()).collect();
    let t0 = Instant::now();
    let (mut rows, _) = evaluate_preseason(&table, &sc, &seasons, models, MIN_GP, MIN_GP, re_log(&mut log))?;
    let mut fits: Map<i64, Value> = Map::new();
    if inseason {
        let windows = load_windows(data_dir)?;
        let ins_seasons: Vec<i64> = match inseason_seasons {
            Some(s) if !s.is_empty() => s.to_vec(),
            _ => {
                let mut k: Vec<i64> = windows.keys().copied().collect();
                k.sort();
                k
            }
        }
        .into_iter()
        .filter(|s| windows.contains_key(s))
        .collect();
        if !ins_seasons.is_empty() {
            let obs = inseason_observations(&table, &windows, &sc, &ins_seasons, MIN_GP_REST);
            let (irows, ifits) = evaluate_inseason(&obs, true);
            rows.extend(irows);
            fits = ifits.into_iter().map(|(k, v)| (k, v.to_value())).collect();
        }
    }
    let out_dir = backtest_dir(data_dir)?;
    let day = today.format("%Y-%m-%d").to_string();
    let res_path = out_dir.join(format!("results-{day}{}.json", suffix(&name)));
    let mut info = serde_json::Map::new();
    info.insert("scoring".into(), json!(name));
    info.insert("weights".into(), serde_json::to_value(&cfg)?);
    info.insert("unscored_stats".into(), json!(missing_weights(&cfg)));
    info.insert("seasons".into(), json!(seasons));
    info.insert("models".into(), json!(models.unwrap_or(&[])));
    info.insert("elapsed_seconds".into(), json!(round_to(t0.elapsed().as_secs_f64(), 1)));
    info.insert("table_seasons".into(), json!(table.seasons()));
    for (k, v) in meta.into_iter().flatten() {
        info.insert(k.clone(), v.clone());
    }
    let info = Value::Object(info);
    let fits_value: serde_json::Map<String, Value> = fits.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    let payload = json!({
        "info": info,
        "rows": rows.iter().map(|r| Value::Object(r.to_dict().into_iter().collect())).collect::<Vec<_>>(),
        "inseason_fits": Value::Object(fits_value.clone()),
    });
    std::fs::write(&res_path, dumps(&payload, &DumpOpts { indent: Some(1), ..Default::default() }))?;
    let rep_path = write_report(
        &out_dir.join(format!("report-{day}{}.md", suffix(&name))),
        &rows,
        &info,
        load_fitted(data_dir, &name).as_ref(),
        Some(&Value::Object(fits_value)),
        &app_constants(),
    )?;
    Ok(EvalOutput {
        rows,
        results: res_path.to_string_lossy().into_owned(),
        report: rep_path.to_string_lossy().into_owned(),
        info,
    })
}

/// The newest `report-*.md` in the backtest dir.
pub fn latest_report(data_dir: &Path) -> Option<PathBuf> {
    let dir = backtest_dir(data_dir).ok()?;
    let mut reports: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("report-") && n.ends_with(".md")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    reports.sort_by(|a, b| a.0.cmp(&b.0));
    reports.pop().map(|r| r.1)
}
