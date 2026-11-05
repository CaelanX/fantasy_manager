//! Ports of the models / fit / pipeline parts of tests/test_backtest.py plus parity runs of
//! `run_fit` / `run_evaluation` against the stored Python outputs in `data/backtest/`
//! (`fitted_params*.json`, `results-*.json`; the tests skip when that directory is absent, set
//! `FM_BACKTEST_DATA` to point elsewhere).

use std::path::{Path, PathBuf};

use fm_backtest::data::{age_on_oct1, load_table, save_table, PlayerSeason, SeasonTable, WindowLine, Windows};
use fm_backtest::evaluate::{
    evaluate_inseason, evaluate_preseason, head_to_head, inseason_observations, MIN_GP, INSEASON_MODELS,
};
use fm_backtest::fit::{
    components, dynasty_anchors, fit_age_curve, fit_inseason, fit_shrinkage_k_default, inseason_projection,
    k_grid, mae_components, recency_weights_for, AgeCurveOpts, Pair,
};
use fm_backtest::models::{
    get_models, make_context, project, FittedParams, FmCurrent, FmFitted, FmMulti, Marcel, NaiveLastSeason,
    DEFAULT_MODELS, MARCEL_K,
};
use fm_backtest::pipeline::{fitted_params_path, latest_report, run_evaluation, run_fit};
use fm_core::Map;
use fm_engine::scoring::PointsScoring;
use fm_engine::valuation::blend::{
    k_baseline_snapshot, k_inseason_snapshot, multi_season_baseline, recency_weights, recency_weights_snapshot,
};
use fm_engine::valuation::params;
use serde_json::Value;

const S1: i64 = 20202021;
const S2: i64 = 20212022;
const S3: i64 = 20222023;
const S4: i64 = 20232024;

fn goals() -> PointsScoring {
    PointsScoring::new([("G".to_string(), 1.0)].into_iter().collect(), None)
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * b.abs().max(1.0) || (a - b).abs() < 1e-12
}

fn row(pid: i64, season: i64, g_pg: f64, gp: i64, group: &str, age: Option<f64>, birth: Option<&str>, extra: &[(&str, f64)]) -> PlayerSeason {
    let mut stats: Map<String, f64> = Map::new();
    stats.insert("G".to_string(), g_pg * gp as f64);
    for (k, v) in extra {
        stats.insert(k.to_string(), v * gp as f64);
    }
    let pos = match group {
        "F" => "C",
        "D" => "D",
        _ => "G",
    };
    PlayerSeason {
        player_id: pid,
        season,
        name: format!("P{pid}"),
        group: group.to_string(),
        position: Some(pos.to_string()),
        gp,
        stats,
        team: None,
        birth_date: birth.map(String::from),
        age,
    }
}

fn r(pid: i64, season: i64, g_pg: f64, gp: i64) -> PlayerSeason {
    row(pid, season, g_pg, gp, "F", None, None, &[])
}

fn hist<'a>(t: &'a SeasonTable, pid: i64, before: i64) -> Vec<&'a PlayerSeason> {
    t.history(pid, before)
}

// --------------------------------------------------------------------------- models

#[test]
fn naive_uses_last_season() {
    let t = SeasonTable::new([r(1, S1, 0.2, 80), r(1, S2, 0.5, 80)]);
    let sc = goals();
    let ctx = make_context(&t, S3, &sc, None);
    assert!(approx(project(&NaiveLastSeason, &hist(&t, 1, S3), Some(25.0), &ctx).unwrap(), 0.5));
    assert!(NaiveLastSeason_rates_none(&ctx));
}

#[allow(non_snake_case)]
fn NaiveLastSeason_rates_none(ctx: &fm_backtest::models::ProjectionContext) -> bool {
    use fm_backtest::models::Model;
    NaiveLastSeason.rates(&[], Some(25.0), ctx).is_none()
}

#[test]
fn marcel_weights_regression_and_age() {
    let t = SeasonTable::new([r(1, S1, 0.3, 40), r(1, S2, 0.6, 80), r(1, S3, 0.9, 60)]);
    let sc = goals();
    let ctx = make_context(&t, S4, &sc, None);
    let raw = (5.0 * 60.0 * 0.9 + 4.0 * 80.0 * 0.6 + 3.0 * 40.0 * 0.3) / (5.0 * 60.0 + 4.0 * 80.0 + 3.0 * 40.0);
    let n = (5.0 * 60.0 + 4.0 * 80.0 + 3.0 * 40.0) / 5.0;
    let expect = (n * raw + MARCEL_K * 0.9) / (n + MARCEL_K);
    let h = hist(&t, 1, S4);
    let no_age = Marcel { age: false, ..Marcel::default() };
    assert!(approx(project(&no_age, &h, None, &ctx).unwrap(), expect));
    let young = project(&Marcel::default(), &h, Some(22.0), &ctx).unwrap();
    let old = project(&Marcel::default(), &h, Some(33.0), &ctx).unwrap();
    assert!(approx(young, expect * (1.0 + 0.006 * 5.0)) && approx(old, expect * (1.0 - 0.003 * 6.0)));
}

#[test]
fn fm_current_is_the_app_multi_season_baseline() {
    use fm_backtest::models::Model;
    let mut rows = vec![r(1, S2, 0.8, 30)];
    rows.extend((2..6).map(|i| r(i, S2, 0.2, 80)));
    let t = SeasonTable::new(rows);
    let sc = goals();
    let ctx = make_context(&t, S3, &sc, None);
    let mean = &ctx.means["F"];
    let f = params::age_factor("F", Some(24.0), None, None);
    let line = t.get(1, S2).unwrap().statline(fm_core::models::Split::Prior);
    let expect = multi_season_baseline(&[Some(&line)], "F", &ctx.means, None);
    let got = FmCurrent.rates(&hist(&t, 1, S3), Some(25.0), &ctx).unwrap();
    for (k, v) in &expect {
        assert!(approx(got[k], v * f), "{k}");
    }
    let kf = k_baseline_snapshot()["F"];
    assert!(approx(got["G"], f * (30.0 * 0.8 + kf * mean["G"]) / (30.0 + kf)));
    // a player who skipped last season still has a baseline from the season before (weight 4)
    let skipped = [r(9, S1, 0.5, 80)];
    let skipped_refs: Vec<&PlayerSeason> = skipped.iter().collect();
    let g = FmCurrent.rates(&skipped_refs, None, &ctx).unwrap()["G"];
    assert!(approx(g, (80.0 * 4.0 / 5.0 * 0.5 + kf * mean["G"]) / (80.0 * 4.0 / 5.0 + kf)));
    // ... but none without any of the last three seasons
    let old = [r(9, S1 - 20002, 0.5, 80)];
    let old_refs: Vec<&PlayerSeason> = old.iter().collect();
    assert!(FmCurrent.rates(&old_refs, Some(25.0), &ctx).is_none());
}

#[test]
fn fm_current_matches_fm_multi_with_the_packaged_params() {
    use fm_backtest::models::Model;
    let mut rows = vec![
        r(1, S1, 0.3, 40),
        r(1, S2, 0.6, 80),
        r(1, S3, 0.9, 60),
        row(2, S3, 0.1, 50, "D", None, None, &[]),
        row(2, S2, 0.2, 70, "D", None, None, &[]),
    ];
    rows.extend((3..6).map(|i| r(i, S3, 0.5, 80)));
    rows.extend((6..9).map(|i| row(i, S3, 0.1, 80, "D", None, None, &[])));
    let t = SeasonTable::new(rows);
    let sc = goals();
    let ctx = make_context(&t, S4, &sc, None);
    let packaged = FittedParams {
        k_multi: k_baseline_snapshot().clone(),
        age_yoy: params::age_yoy(None),
        age_groups: params::age_groups(None),
        ..FittedParams::default()
    };
    let multi = FmMulti::new(Some(packaged));
    for (pid, age) in [(1, Some(21.4)), (1, Some(33.0)), (2, Some(27.9)), (2, None)] {
        let a = FmCurrent.rates(&hist(&t, pid, S4), age, &ctx).unwrap();
        let b = multi.rates(&hist(&t, pid, S4), age, &ctx).unwrap();
        for (k, v) in &a {
            assert!(approx(*v, b[k]), "{pid} {k}");
        }
    }
}

#[test]
fn fm_fitted_matches_fm_current_at_app_k_and_applies_age() {
    let mut rows = vec![r(1, S2, 0.8, 30)];
    rows.extend((2..6).map(|i| r(i, S2, 0.2, 80)));
    let t = SeasonTable::new(rows);
    let sc = goals();
    let ctx = make_context(&t, S3, &sc, None);
    let app_like = FmFitted::new(Some(FittedParams {
        k: k_baseline_snapshot().clone(),
        age_yoy: params::age_yoy(None),
        ..FittedParams::default()
    }));
    let h = hist(&t, 1, S3);
    assert!(approx(project(&app_like, &h, Some(25.0), &ctx).unwrap(), project(&FmCurrent, &h, Some(25.0), &ctx).unwrap()));
    let same = FmFitted::new(Some(FittedParams::default()));
    let aged_params = FittedParams {
        k: [("F".to_string(), 20.0)].into_iter().collect(),
        age_yoy: [("F".to_string(), [(23, 1.1), (24, 1.1), (25, 0.9)].into_iter().collect())].into_iter().collect(),
        ..FittedParams::default()
    };
    let aged = FmFitted::new(Some(aged_params.clone()));
    let base = project(&same, &h, Some(25.0), &ctx).unwrap();
    assert!(approx(project(&aged, &h, Some(25.5), &ctx).unwrap(), base * 1.1)); // age 24.5 last season
    assert!(approx(project(&aged, &h, Some(40.0), &ctx).unwrap(), base * 0.9)); // clamps to oldest bin
    let fp = FittedParams::from_dict(&aged_params.to_dict()).unwrap();
    assert_eq!(fp.age_factor("F", Some(24.2)), 1.1);
    assert_eq!(fp.age_factor("G", Some(24.2)), 1.0);
}

#[test]
fn fm_multi_weights_three_seasons() {
    let mut rows = vec![r(1, S1, 0.3, 40), r(1, S2, 0.6, 80), r(1, S3, 0.9, 60)];
    rows.extend((2..5).map(|i| r(i, S3, 0.5, 80)));
    let t = SeasonTable::new(rows);
    let sc = goals();
    let ctx = make_context(&t, S4, &sc, None);
    let p = FittedParams { k_multi: [("F".to_string(), 0.0)].into_iter().collect(), ..FittedParams::default() };
    let raw = (5.0 * 60.0 * 0.9 + 4.0 * 80.0 * 0.6 + 3.0 * 40.0 * 0.3) / (5.0 * 60.0 + 4.0 * 80.0 + 3.0 * 40.0);
    assert!(approx(project(&FmMulti::new(Some(p)), &hist(&t, 1, S4), None, &ctx).unwrap(), raw));
    assert!(get_models(Some(&["bogus".to_string()])).is_err());
    assert_eq!(get_models(None).unwrap().len(), DEFAULT_MODELS.len());
}

#[test]
fn evaluate_preseason_on_synthetic_history() {
    let mut rows = Vec::new();
    for pid in 1..=60 {
        let base = 0.1 + pid as f64 * 0.01;
        for s in [S1, S2, S3, S4] {
            rows.push(row(pid, s, base, 70, "F", Some(20.0 + (pid % 15) as f64 + (s - S1) as f64 / 10001.0), None, &[]));
        }
    }
    let t = SeasonTable::new(rows);
    let sc = goals();
    let (res, _) = evaluate_preseason(&t, &sc, &[S3, S4], None, MIN_GP, MIN_GP, None).unwrap();
    let by = |s: i64, m: &str| res.iter().find(|x| x.season == s && x.model == m && x.pool == "skaters").unwrap().clone();
    assert!(by(S4, "naive").mae.abs() < 1e-9 && approx(by(S4, "naive").spearman, 1.0));
    // flat careers: fm_current (3-season baseline + the fitted age factor) is off only by aging
    let c = by(S4, "fm_current");
    assert!(c.n == 60 && c.spearman > 0.99 && c.mae < 0.05);
    let names: std::collections::HashSet<&str> = res.iter().map(|x| x.model.as_str()).collect();
    assert_eq!(names, DEFAULT_MODELS.iter().copied().collect());
    let (_, total) = head_to_head(&res, "naive", "fm_current", "preseason", "skaters", "mae");
    assert_eq!(total, 2);
}

// --------------------------------------------------------------------------- fitting

fn pair(pid: i64, age: f64, prev: f64, nxt: f64) -> Pair {
    Pair::simple(pid, S2, "F", Some(age), 80, prev, 0.5, nxt)
}

#[test]
fn age_curve_delta_method_recovers_trajectory() {
    let mut pairs = Vec::new();
    let mut pid = 0;
    for age in 18..41 {
        let ratio = if age < 25 { 1.10 } else { 0.90 };
        for j in 0..30 {
            pid += 1;
            let prev = 0.5 + 0.01 * j as f64;
            pairs.push(pair(pid, age as f64 + 0.5, prev, prev * ratio));
        }
    }
    let c = fit_age_curve(&pairs, "F", &AgeCurveOpts { boot: 20, window: 0, min_pooled: 5, ..Default::default() });
    assert_eq!(c.peak_age(), 25);
    assert!(approx(c.level[&25], 1.0));
    assert!(approx(c.level[&24], 1.0 / 1.10) && approx(c.level[&26], 0.90));
    assert!(approx(c.level[&20], 1.10f64.powi(-5)));
    assert!(c.lo[&26] <= c.level[&26] && c.level[&26] <= c.hi[&26] && c.n[&30] == 30);
    assert_eq!(dynasty_anchors(&c, &[25, 26]), vec![(25, 1.0), (26, 0.9)]);
}

#[test]
fn age_curve_sparse_ages_are_flat() {
    let pairs: Vec<Pair> = (0..40).map(|i| pair(i, 27.2, 1.0, 0.8)).collect();
    let c = fit_age_curve(&pairs, "F", &AgeCurveOpts { boot: 0, window: 0, min_pooled: 25, ..Default::default() });
    assert!(approx(c.yoy[&27], 0.8) && c.yoy[&19] == 1.0 && c.yoy[&35] == 1.0);
}

#[test]
fn shrinkage_k_grid_picks_the_extremes() {
    let persistent: Vec<Pair> = (0..50).map(|i| pair(i, 25.0, 0.2 + 0.01 * i as f64, 0.2 + 0.01 * i as f64)).collect();
    assert_eq!(fit_shrinkage_k_default(&persistent).0, 0.0);
    let noise: Vec<Pair> = (0..50).map(|i| pair(i, 25.0, 0.5 + if i % 2 == 1 { 0.3 } else { -0.3 }, 0.5)).collect();
    let (best, curve) = fit_shrinkage_k_default(&noise);
    let max_k = k_grid().into_iter().fold(0.0, f64::max);
    assert_eq!(best, max_k);
    let at = |k: f64| curve.iter().find(|(kk, _)| *kk == k).unwrap().1;
    assert!(at(best) < at(0.0));
}

fn inseason_fixture() -> (SeasonTable, Windows) {
    let t = SeasonTable::new([
        r(1, S1, 0.5, 80),
        r(2, S1, 0.3, 80),
        r(3, S1, 0.1, 80),
        r(1, S2, 0.6, 80),
        r(2, S2, 0.2, 80),
        r(3, S2, 0.2, 80),
        r(4, S2, 0.4, 60),
    ]);
    let w = |gp: i64, g: f64| WindowLine { gp, stats: [("G".to_string(), g)].into_iter().collect() };
    let m = |v: Vec<(i64, WindowLine)>| -> Map<i64, WindowLine> { v.into_iter().collect() };
    let mut wins: Map<String, Map<i64, WindowLine>> = Map::new();
    wins.insert("to_date".into(), m(vec![(1, w(20, 14.0)), (2, w(20, 3.0)), (4, w(18, 8.0))]));
    wins.insert("last30".into(), m(vec![(1, w(13, 10.0)), (2, w(10, 1.0)), (4, w(12, 6.0))]));
    wins.insert("last15".into(), m(vec![(1, w(6, 6.0)), (4, w(7, 2.0))]));
    wins.insert("last7".into(), m(vec![(1, w(3, 0.0))]));
    wins.insert("rest".into(), m(vec![(1, w(60, 36.0)), (2, w(55, 11.0)), (4, w(9, 4.0))]));
    let mut days: Map<String, Map<String, Map<i64, WindowLine>>> = Map::new();
    days.insert("2021-12-01".into(), wins);
    let mut windows = Windows::new();
    windows.insert(S2, days);
    (t, windows)
}

#[test]
fn inseason_scalar_matches_app_blend() {
    let (t, windows) = inseason_fixture();
    let sc = goals();
    let obs = inseason_observations(&t, &windows, &sc, &[S2], 10);
    let mut ids: Vec<i64> = obs.iter().map(|o| o.player_id).collect();
    ids.sort();
    assert_eq!(ids, vec![1, 2]); // player 4 has < 10 GP after
    let ks = k_inseason_snapshot()["skater"];
    let rw = recency_weights_snapshot().clone();
    for o in &obs {
        assert!(approx(inseason_projection(o, ks, &rw), o.fpg_app));
        let comps = components(std::slice::from_ref(o), ks);
        assert!(approx(mae_components(&comps, &rw), (o.fpg_app - o.fpg_rest).abs()));
    }
    let fit = fit_inseason(&obs, 2);
    assert!(fit.mae_fitted.unwrap() <= fit.mae_app.unwrap() + 1e-12);
    assert!((fit.recency_weights.values().sum::<f64>() - 1.0).abs() < 1e-9);
    let (rows, _) = evaluate_inseason(&obs, true);
    let names: std::collections::HashSet<&str> = rows.iter().map(|x| x.model.as_str()).collect();
    assert_eq!(names, INSEASON_MODELS.iter().copied().collect());
}

#[test]
fn recency_weights_for_matches_app() {
    let gps: Map<String, i64> = [("last30", 5), ("last15", 2), ("last7", 4)].iter().map(|(k, v)| (k.to_string(), *v)).collect();
    let got = recency_weights_for(recency_weights_snapshot(), &gps);
    let want = recency_weights(5, 2, 4, None);
    for (k, v) in &want {
        assert!(approx(got[k], *v), "{k}");
    }
}

// --------------------------------------------------------------------------- pipeline

fn synthetic_store(dir: &Path) {
    let mut rows = Vec::new();
    for pid in 1..=40i64 {
        for (i, s) in [S1, S2, S3, S4].iter().enumerate() {
            let g = 0.2 + 0.005 * pid as f64 + 0.02 * ((pid + i as i64) % 3) as f64;
            let birth = format!("{}-03-01", 1990 + pid % 12);
            rows.push(row(
                pid,
                *s,
                g,
                70,
                if pid % 4 == 0 { "D" } else { "F" },
                age_on_oct1(Some(&birth), *s),
                Some(&birth),
                &[("SOG", 2.0)],
            ));
        }
    }
    save_table(&SeasonTable::new(rows), dir).unwrap();
}

#[test]
fn run_fit_and_evaluation_write_files() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_store(dir.path());
    let now = chrono::NaiveDate::from_ymd_opt(2026, 9, 28).unwrap().and_hms_opt(12, 0, 0).unwrap();
    let fitted = run_fit(dir.path(), "espn", 5, None, now, None).unwrap();
    assert_eq!(fitted_params_path(dir.path(), "espn").unwrap().file_name().unwrap(), "fitted_params.json");
    let rep = fitted["age_curve_report"].as_object().unwrap();
    assert_eq!(rep.keys().collect::<Vec<_>>(), vec!["F", "D", "G"]);
    assert!(fitted["inseason"].is_null());
    assert_eq!(fitted["generated"], "2026-09-28T12:00:00");
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
    let out = run_evaluation(dir.path(), "espn", None, Some(&[S3, S4]), false, None, today, None, None).unwrap();
    let report = std::fs::read_to_string(&out.report).unwrap();
    assert!(out.report.ends_with("report-2026-09-28.md"));
    assert!(report.contains("## Verdict") && report.contains("fm_current") && report.contains("## Fitted parameters"));
    assert_eq!(latest_report(dir.path()).unwrap(), PathBuf::from(&out.report));
}

// --------------------------------------------------------------------------- parity

fn data_dir() -> Option<PathBuf> {
    let p = std::env::var("FM_BACKTEST_DATA").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("C:/code/fantasy_manager/data/backtest"));
    (p.join("player_seasons.json").exists() && p.join("windows.json").exists()).then_some(p)
}

/// Recursive comparison: ints/strings exact, floats within rel 1e-9 (abs 1e-12); null == NaN.
fn diff(a: &Value, b: &Value, path: &str, out: &mut Vec<String>, ignore: &[&str]) {
    if ignore.iter().any(|i| path.ends_with(i)) {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let kx: Vec<_> = x.keys().collect();
            let ky: Vec<_> = y.keys().collect();
            if kx != ky {
                out.push(format!("{path}: keys {kx:?} != {ky:?}"));
                return;
            }
            for (k, v) in x {
                diff(v, &y[k], &format!("{path}/{k}"), out, ignore);
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                out.push(format!("{path}: len {} != {}", x.len(), y.len()));
                return;
            }
            for (i, (u, v)) in x.iter().zip(y).enumerate() {
                diff(u, v, &format!("{path}[{i}]"), out, ignore);
            }
        }
        (Value::Number(x), Value::Number(y)) => {
            let (u, v) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            if x.is_f64() != y.is_f64() && u != v {
                out.push(format!("{path}: {x} != {y}"));
            } else if (u - v).abs() > 1e-9 * v.abs().max(u.abs()) && (u - v).abs() > 1e-12 {
                out.push(format!("{path}: {u} != {v}"));
            }
        }
        _ => {
            if a != b {
                out.push(format!("{path}: {a} != {b}"));
            }
        }
    }
}

fn fit_parity(scoring: &str, file: &str) {
    let Some(src) = data_dir() else {
        eprintln!("skip: no data/backtest");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let bt = dir.path().join("backtest");
    std::fs::create_dir_all(&bt).unwrap();
    for f in ["player_seasons.json", "windows.json"] {
        std::fs::copy(src.join(f), bt.join(f)).unwrap();
    }
    let now = chrono::NaiveDate::from_ymd_opt(2026, 9, 28).unwrap().and_hms_opt(21, 49, 28).unwrap();
    let t0 = std::time::Instant::now();
    let got = run_fit(dir.path(), scoring, 200, None, now, None).unwrap();
    eprintln!("run_fit({scoring}) took {:?}", t0.elapsed());
    let want: Value = serde_json::from_str(&std::fs::read_to_string(src.join(file)).unwrap()).unwrap();
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(fitted_params_path(dir.path(), scoring).unwrap()).unwrap()).unwrap();
    let mut errs = Vec::new();
    diff(&on_disk, &want, "", &mut errs, &["/generated"]);
    for e in errs.iter().take(40) {
        eprintln!("{e}");
    }
    assert!(errs.is_empty(), "{} differences vs {file}", errs.len());
    assert_eq!(got["scoring"], want["scoring"]);
}

#[test]
fn run_fit_reproduces_fitted_params_espn() {
    fit_parity("espn", "fitted_params.json");
}

#[test]
fn run_fit_reproduces_fitted_params_fantrax() {
    fit_parity("fantrax", "fitted_params-fantrax.json");
}

fn eval_parity(scoring: &str, file: &str) {
    let Some(src) = data_dir() else {
        eprintln!("skip: no data/backtest");
        return;
    };
    let want: Value = serde_json::from_str(&std::fs::read_to_string(src.join(file)).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let bt = dir.path().join("backtest");
    std::fs::create_dir_all(&bt).unwrap();
    for f in ["player_seasons.json", "windows.json"] {
        std::fs::copy(src.join(f), bt.join(f)).unwrap();
    }
    let seasons: Vec<i64> = want["info"]["seasons"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
    let t0 = std::time::Instant::now();
    let out = run_evaluation(dir.path(), scoring, None, Some(&seasons), true, None, today, None, None).unwrap();
    eprintln!("run_evaluation({scoring}) took {:?}", t0.elapsed());
    let got: Value = serde_json::from_str(&std::fs::read_to_string(&out.results).unwrap()).unwrap();
    let mut errs = Vec::new();
    diff(&got, &want, "", &mut errs, &["elapsed_seconds"]);
    for e in errs.iter().take(40) {
        eprintln!("{e}");
    }
    assert!(errs.is_empty(), "{} differences vs {file}", errs.len());
}

#[test]
fn run_evaluation_reproduces_results_espn() {
    eval_parity("espn", "results-2026-09-28.json");
}

#[test]
fn run_evaluation_reproduces_results_fantrax() {
    eval_parity("fantrax", "results-2026-09-28-fantrax.json");
}

/// xG backtest from an offline copy of cache.db (set `FM_XG_CACHE_DIR` to a directory holding a
/// copy of the data dir's cache.db with the MoneyPuck season summaries): the markdown report
/// equals the stored Python one apart from the runtime.
#[test]
fn xg_backtest_reproduces_python_report() {
    let (Ok(cache_dir), Some(src)) = (std::env::var("FM_XG_CACHE_DIR"), data_dir()) else {
        eprintln!("skip: FM_XG_CACHE_DIR / data/backtest not available");
        return;
    };
    let want_path = src.join("xg-report-2026-09-29.md");
    if !want_path.exists() {
        eprintln!("skip: no stored xg report");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bt = dir.path().join("backtest");
    std::fs::create_dir_all(&bt).unwrap();
    std::fs::copy(src.join("player_seasons.json"), bt.join("player_seasons.json")).unwrap();
    let cache = fm_http::HttpCache::open(Path::new(&cache_dir), true).unwrap();
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
    let t0 = std::time::Instant::now();
    let (path, res) = fm_backtest::xg_backtest::run_xg_backtest(&cache, dir.path(), None, None, None, None, today, None, true).unwrap();
    eprintln!("run_xg_backtest took {:?}", t0.elapsed());
    let mask = |s: &str| -> String {
        let s = s.replace("\r\n", "\n");
        match (s.find("Runtime "), s.find("s.\n")) {
            (Some(a), Some(b)) if b > a => format!("{}{}", &s[..a], &s[b..]),
            _ => s,
        }
    };
    let got = mask(&std::fs::read_to_string(path.unwrap()).unwrap());
    let want = mask(&std::fs::read_to_string(want_path).unwrap());
    if got != want {
        for (i, (a, b)) in got.lines().zip(want.lines()).enumerate() {
            if a != b {
                eprintln!("line {i}:\n  got : {a}\n  want: {b}");
            }
        }
    }
    assert_eq!(got.lines().count(), want.lines().count());
    assert!(got == want, "xg report differs");
    assert!(!res.rows.is_empty());
}

#[test]
fn table_roundtrip_loads() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_store(dir.path());
    assert_eq!(load_table(dir.path()).unwrap().seasons(), vec![S1, S2, S3, S4]);
}
