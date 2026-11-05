//! W6-harness-metrics-refit: ports of tests/test_harness_metrics.py, tests/test_harness_refit.py,
//! tests/test_track.py (and the harness half of test_health.py via `series`), plus the parity
//! tests against Python (`parity/dump_harness_metrics.py`, run through the venv python):
//! a copy of the fixture harness.db (grade_week / status_report / health / track / replay /
//! dry-run refit) and the synthetic refit ledger (dry runs, gate failures, an applied refit,
//! shadow scoring with automatic rollback; params files and ledger rows compared).
//!
//! Every test takes `LOCK`: the valuation params are a process-global and the params dir comes
//! from the environment (as in Python).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use chrono::{Duration, NaiveDate, NaiveDateTime};
use fm_backtest::fit::InseasonObs;
use fm_core::models::{GainUnits, RecKind, Recommendation, ScoringConfig};
use fm_core::Map;
use fm_engine::valuation::params as vp;
use fm_harness::health as H;
use fm_harness::ledger::{json_dumps, Ledger, Row};
use fm_harness::metrics as M;
use fm_harness::outcomes::store_scoring;
use fm_harness::params_store::{ParamsStore, PACKAGED};
use fm_harness::refit as R;
use fm_harness::track as T;
use fm_pycompat::random::PyRandom;
use serde_json::{json, Value};

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn iso(x: NaiveDate) -> String {
    x.format("%Y-%m-%d").to_string()
}

fn approx(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

fn row(v: Value) -> Row {
    v.as_object().unwrap().clone()
}

/// Point the params layer at `<dir>/harness/params` (Python `_set_env`); `None` restores.
fn set_params_dir(dir: Option<&Path>) {
    match dir {
        Some(d) => {
            std::env::set_var(vp::OVERRIDE_ENV, "1");
            std::env::set_var(vp::PARAMS_DIR_ENV, d.join("harness").join("params"));
        }
        None => {
            std::env::remove_var(vp::PARAMS_DIR_ENV);
        }
    }
    vp::reload();
}

/// A temp data dir whose params dir is active for the test's lifetime.
struct Env {
    dir: tempfile::TempDir,
    _g: MutexGuard<'static, ()>,
}

impl Env {
    fn new() -> Env {
        let g = lock();
        let dir = tempfile::tempdir().unwrap();
        set_params_dir(Some(dir.path()));
        Env { dir, _g: g }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn pdir(&self) -> PathBuf {
        self.dir.path().join("harness").join("params")
    }
    fn ledger(&self) -> Ledger {
        Ledger::open(self.path()).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        set_params_dir(None);
    }
}

// =========================================================================== metrics

fn sc_metrics() -> ScoringConfig {
    serde_json::from_value(json!({"kind": "points",
        "weights": {"G": 3.0, "A": 2.0, "SOG": 0.5, "W": 4.0, "SV": 0.2, "GA": -1.0}}))
    .unwrap()
}

const WEEK: (i32, u32, u32) = (2026, 11, 9);

fn week() -> NaiveDate {
    d(WEEK.0, WEEK.1, WEEK.2)
}

fn metrics_ledger(env: &Env) -> Ledger {
    let l = env.ledger();
    store_scoring(&l, "espn", &sc_metrics()).unwrap();
    l
}

#[test]
fn wilson_interval() {
    for (k, n, lo, hi) in [(5, 10, 0.2366, 0.7634), (20, 20, 0.8389, 1.0), (0, 20, 0.0, 0.1611), (30, 50, 0.4618, 0.7239)] {
        let (a, b) = M::wilson(k, n);
        assert!(approx(a.unwrap(), lo, 1e-4) && approx(b.unwrap(), hi, 1e-4), "{k}/{n}");
    }
    assert_eq!(M::wilson(0, 0), (None, None));
}

#[test]
fn trust_labels() {
    for (n, label) in [(0, "hidden"), (19, "hidden"), (20, "provisional"), (49, "provisional"), (50, "reliable"), (500, "reliable")] {
        assert_eq!(M::hit_trust(n), label);
    }
    for (n, weeks, pool, wk, label) in [
        (149, 9, "F", d(2027, 2, 1), "hidden"),
        (150, 1, "F", d(2026, 10, 26), "provisional"),
        (399, 8, "D", d(2026, 12, 7), "provisional"),
        (400, 3, "F", d(2026, 12, 7), "provisional"),
        (400, 4, "F", d(2026, 12, 7), "reliable"),
        (900, 10, "G", d(2026, 12, 28), "provisional"),
        (400, 4, "G", d(2027, 1, 4), "reliable"),
        (100, 10, "G", d(2027, 1, 4), "hidden"),
    ] {
        assert_eq!(M::projection_trust(n, weeks, pool, wk), label, "{n} {weeks} {pool}");
    }
    for (n, label) in [(3, "hidden"), (99, "hidden"), (100, "reliable")] {
        assert_eq!(M::calibration_trust(n), label);
    }
}

#[test]
fn calibration_bins_terciles_then_deciles() {
    assert!(M::calibration_bins(&[1.0, 2.0], &[0.0, 1.0]).is_empty());
    let pred = [9.0, 1.0, 5.0, 2.0, 8.0, 4.0, 3.0, 7.0, 6.0];
    let real: Vec<f64> = pred.iter().map(|p| p * 2.0).collect();
    let bins = M::calibration_bins(&pred, &real);
    assert_eq!(bins.iter().map(|b| b["n"].as_i64().unwrap()).collect::<Vec<_>>(), [3, 3, 3]);
    assert_eq!(bins.iter().map(|b| b["mean_pred"].as_f64().unwrap()).collect::<Vec<_>>(), [2.0, 5.0, 8.0]);
    assert_eq!(bins.iter().map(|b| b["mean_real"].as_f64().unwrap()).collect::<Vec<_>>(), [4.0, 10.0, 16.0]);
    assert_eq!(bins[0]["pred_lo"], json!(1.0));
    assert_eq!(bins[2]["pred_hi"], json!(9.0));
    let p: Vec<f64> = (0..100).map(|i| i as f64).collect();
    let many = M::calibration_bins(&p, &[0.0; 100]);
    assert!(many.len() == 10 && many.iter().all(|b| b["n"] == json!(10)));
    let p: Vec<f64> = (0..99).map(|i| i as f64).collect();
    assert_eq!(M::calibration_bins(&p, &[1.0; 99])[0]["n"], json!(33));
}

#[test]
fn skill_score_and_player_clustered_bootstrap() {
    let pairs: Vec<(i64, f64, f64)> = [(1, 1.0), (1, 2.0), (2, 0.4), (2, 0.6), (3, 3.0), (3, 1.0), (4, 2.0), (4, 2.0)]
        .iter()
        .map(|&(p, e)| (p, 0.5 * e, e))
        .collect();
    let a: Vec<f64> = pairs.iter().map(|p| p.1).collect();
    let b: Vec<f64> = pairs.iter().map(|p| p.2).collect();
    assert!(approx(M::skill_score(&a, &b).unwrap(), 0.5, 1e-12));
    let (lo, hi) = M::bootstrap_skill_default(&pairs);
    assert!(approx(lo.unwrap(), 0.5, 1e-12) && approx(hi.unwrap(), 0.5, 1e-12));
    let mixed = [(1, 0.5, 1.0), (1, 0.5, 1.0), (2, 1.0, 1.0), (3, 0.2, 1.0), (4, 1.5, 1.0), (5, 0.3, 1.0)];
    let (lo, hi) = M::bootstrap_skill(&mixed, 200, 7, 0.95);
    let a: Vec<f64> = mixed.iter().map(|p| p.1).collect();
    let b: Vec<f64> = mixed.iter().map(|p| p.2).collect();
    let point = M::skill_score(&a, &b).unwrap();
    assert!(lo.unwrap() < point && point < hi.unwrap());
    assert_eq!((lo, hi), M::bootstrap_skill(&mixed, 200, 7, 0.95));
    assert_eq!(M::bootstrap_skill_default(&[(1, 0.5, 1.0), (1, 0.4, 1.0)]), (None, None));
    assert_eq!(M::skill_score(&[], &[]), None);
}

#[test]
fn pool_and_weekly_snapshot_helpers() {
    let pools: Vec<&str> =
        [Some("C,F"), Some("D"), Some("G"), Some("F,D"), Some("C,F,G"), Some("LW,RW,F"), None].iter().map(|p| M::pool_of(*p)).collect();
    assert_eq!(pools, ["F", "D", "G", "F", "F", "F", "F"]);
    let snaps = M::weekly_snapshots(["2026-10-07", "2026-10-06", "2026-10-12", "2026-10-14", "2026-10-21"]);
    assert_eq!(snaps, vec![(d(2026, 10, 5), d(2026, 10, 6)), (d(2026, 10, 12), d(2026, 10, 12)), (d(2026, 10, 19), d(2026, 10, 21))]);
}

fn pull(l: &Ledger, dates: &[NaiveDate], stats: &[(i64, Vec<(NaiveDate, Value)>)]) {
    let mut rows = Vec::new();
    for (nid, by) in stats {
        for (day, st) in by {
            rows.push(row(json!({"nhl_id": nid, "game_date": iso(*day), "gp": 1, "stats_json": json_dumps(st, false)})));
        }
    }
    l.upsert("realized_daily", &rows, &["nhl_id", "game_date"]).unwrap();
    let pulls: Vec<Row> = dates.iter().map(|x| row(json!({"game_date": iso(*x), "n_players": 1}))).collect();
    l.upsert("realized_pulls", &pulls, &["game_date"]).unwrap();
}

/// Snapshot Monday 2026-10-05: three skaters and a goalie, 14 games each over the next 28 days.
fn projection_ledger(l: &Ledger) -> NaiveDate {
    let snap = d(2026, 10, 5);
    let players = [
        ("f1", 1, "C,F", 3.0, 3.0, 9.0, json!({"G": 1}), json!({"gp": 2, "stats": {"G": 2}}), json!({"gp": 80, "stats": {"G": 40}})),
        ("f2", 2, "LW,F", 1.0, 1.0, 4.0, json!({"A": 1}), json!({"gp": 2, "stats": {"A": 2}}), Value::Null),
        ("d1", 3, "D", 2.0, 2.0, 6.0, json!({"SOG": 2}), Value::Null, json!({"gp": 80, "stats": {"SOG": 80}})),
        ("g1", 4, "G", 5.0, 5.0, 10.0, json!({"W": 1, "SV": 5}), json!({"gp": 1, "stats": {"W": 1}}), Value::Null),
    ];
    let mut rows = Vec::new();
    let mut stats = Vec::new();
    for (cid, nid, pos, fpg, fw, pw, per_game, season, projection) in players {
        let inputs = json!({"season": season, "projection": projection, "history": {"rates": {"G": 0.5}, "gp": 82}});
        rows.push(row(json!({"league": "espn", "as_of": iso(snap), "cid": cid, "name": cid, "nhl_id": nid,
            "positions": pos, "fpg": fpg, "fpg_week": fw, "proj_week": pw, "inputs_json": json_dumps(&inputs, false)})));
        stats.push((nid as i64, (1..29).step_by(2).map(|i| (snap + Duration::days(i), per_game.clone())).collect()));
    }
    l.upsert("projections", &rows, &["league", "as_of", "cid"]).unwrap();
    let dates: Vec<NaiveDate> = (1..40).map(|i| snap + Duration::days(i)).collect();
    pull(l, &dates, &stats);
    snap
}

fn by_metric(rows: &[Row]) -> std::collections::HashMap<(String, String), Row> {
    rows.iter()
        .map(|r| ((r["metric"].as_str().unwrap().to_string(), r["pool"].as_str().unwrap().to_string()), r.clone()))
        .collect()
}

fn detail(r: &Row) -> Value {
    serde_json::from_str(r["detail_json"].as_str().unwrap()).unwrap()
}

fn fnum(v: &Value) -> f64 {
    v.as_f64().unwrap()
}

#[test]
fn projection_accuracy_against_realized_and_baselines() {
    let env = Env::new();
    let l = metrics_ledger(&env);
    projection_ledger(&l);
    let (rows, info) = M::compute_week(&l, "espn", week()).unwrap();
    assert_eq!((info["matured_28d"].clone(), info["matured_7d"].clone()), (json!(1), json!(1)));
    let by = by_metric(&rows);
    let f = &by[&("proj_fpg_mae".into(), "F".into())];
    let dd = detail(f);
    assert!(f["n"] == json!(2) && approx(fnum(&f["value"]), 0.5, 1e-9) && f["trust"] == json!("hidden"));
    assert_eq!(dd["need"], json!(148));
    assert_eq!(dd["weeks"], json!(1));
    assert_eq!(dd["per_week"], json!([{"week": "2026-10-05", "n": 2, "mae": 0.5}]));
    let base = &dd["baselines"];
    assert!(base["to_date"]["n"] == json!(2) && approx(fnum(&base["to_date"]["mae"]), 0.0, 1e-9));
    assert!(base["provider"]["n"] == json!(1) && approx(fnum(&base["provider"]["mae"]), 1.5, 1e-9));
    assert!(approx(fnum(&base["preseason"]["mae"]), 0.5, 1e-9) && base["last_season"]["n"] == json!(2));
    let sk = &by[&("proj_fpg_skill".into(), "F".into())];
    assert!(sk["n"] == json!(2) && sk["value"].is_null());
    let g = &by[&("proj_fpg_mae".into(), "G".into())];
    assert!(g["n"] == json!(1) && approx(fnum(&g["value"]), 0.0, 1e-9));
    let wk = &by[&("proj_week_mae".into(), "F".into())];
    let wd = detail(wk);
    assert!(wk["n"] == json!(2) && approx(fnum(&wk["value"]), 3.5, 1e-9));
    assert!(approx(fnum(&wd["rate_mae"]), 2.0, 1e-9) && approx(fnum(&wd["avail_mae"]), 1.5, 1e-9));
    // per-week baselines for the sparklines
    let pw = &dd["per_week_base"][0];
    assert!(pw["week"] == json!("2026-10-05") && pw["n_to_date"] == json!(2));
    assert!(approx(fnum(&pw["base_mae"]["to_date"]), 0.0, 1e-9) && approx(fnum(&pw["base_mae"]["provider"]), 1.5, 1e-9));
    assert!(pw["skill"]["fm"].is_null());
    M::grade_week(&l, "espn", week()).unwrap();
    let s = H::series(&l, "espn").unwrap();
    assert_eq!(s["mae"]["F"]["x"], json!(["2026-10-05"]));
    assert_eq!(s["mae"]["F"]["lines"]["fm"], json!([0.5]));
    assert_eq!(s["mae"]["F"]["lines"]["provider"], json!([1.5]));
    assert_eq!(s["mae"]["F"]["trust"], json!("hidden"));
    assert_eq!(s["skill"]["F"]["lines"]["to_date"], json!([0.0]));
    assert_eq!(s["hit"], json!({}));
}

#[test]
fn projection_windows_mature_by_week() {
    let env = Env::new();
    let l = metrics_ledger(&env);
    projection_ledger(&l);
    let (_, info) = M::compute_week(&l, "espn", d(2026, 10, 12)).unwrap();
    assert_eq!(info["matured_7d"], json!(0));
    let (_, info) = M::compute_week(&l, "espn", d(2026, 10, 19)).unwrap();
    assert!(info["matured_7d"] == json!(1) && info["matured_28d"] == json!(0));
    let (_, info) = M::compute_week(&l, "espn", d(2026, 10, 7)).unwrap();
    assert!(info["week"] == json!("2026-10-05") && info["matured_7d"] == json!(0));
    l.execute("DELETE FROM realized_pulls WHERE game_date='2026-10-20'", &[]).unwrap();
    l.commit().unwrap();
    let (_, info) = M::compute_week(&l, "espn", week()).unwrap();
    assert!(info["matured_28d"] == json!(0) && info["unpulled"] == json!(1));
}

#[derive(Clone)]
struct Oc {
    id: String,
    kind: &'static str,
    origin: &'static str,
    hit: i64,
    gain: f64,
    pred: Option<f64>,
    window: &'static str,
    units: &'static str,
    complete: i64,
    end: &'static str,
    detail: Option<Value>,
    basis: &'static str,
    decision: Option<String>,
}

fn oc(id: &str) -> Oc {
    Oc {
        id: id.into(),
        kind: "waiver",
        origin: "followed",
        hit: 1,
        gain: 2.0,
        pred: Some(1.5),
        window: "28d",
        units: "season_fpg",
        complete: 1,
        end: "2026-11-01",
        detail: None,
        basis: "first_seen",
        decision: None,
    }
}

fn put(l: &Ledger, o: Oc) {
    let det = o.detail.clone().unwrap_or_else(|| json!({"title": format!("t{}", o.id), "day": "2026-10-04"}));
    let r = row(json!({"outcome_id": o.id, "league": "espn",
        "episode_id": if o.decision.is_some() { Value::Null } else { json!(format!("e{}", o.id)) },
        "decision_id": o.decision, "window": o.window, "basis": o.basis, "predicted_gain": 0.5,
        "realized_gain": o.gain, "gain_units": o.units, "hit": o.hit, "partial": 1 - o.complete, "kind": o.kind,
        "origin": o.origin, "complete": o.complete, "window_start": "2026-10-05", "window_end": o.end,
        "predicted_pts": o.pred, "detail_json": json_dumps(&det, false)}));
    l.upsert("outcomes", &[r], &["outcome_id"]).unwrap();
}

#[test]
fn hit_rates_trust_and_trades_never_aggregated() {
    let env = Env::new();
    let l = metrics_ledger(&env);
    for i in 0..25 {
        put(&l, Oc { hit: (i < 15) as i64, gain: if i < 15 { 1.0 } else { -1.0 }, ..oc(&format!("f{i}")) });
    }
    for i in 0..5 {
        put(&l, Oc { origin: "ignored", hit: 0, gain: -0.5, ..oc(&format!("i{i}")) });
    }
    put(&l, Oc { end: "2026-11-20", ..oc("late") });
    put(&l, Oc { complete: 0, ..oc("part") });
    put(&l, Oc { window: "7d", ..oc("w7") });
    put(&l, Oc { basis: "acted_on", ..oc("acted") });
    put(&l, Oc { kind: "trade", window: "ros", complete: 0, units: "lineup_fpg", ..oc("tr") });
    let (rows, _) = M::compute_week(&l, "espn", week()).unwrap();
    let hits: std::collections::HashMap<String, Row> = rows
        .iter()
        .filter(|r| r["metric"] == json!("hit_rate"))
        .map(|r| (r["pool"].as_str().unwrap().to_string(), r.clone()))
        .collect();
    let fol = &hits["waiver:followed"];
    assert!(fol["n"] == json!(25) && approx(fnum(&fol["value"]), 0.6, 1e-9) && fol["trust"] == json!("provisional"));
    let (lo, hi) = M::wilson(15, 25);
    assert!(approx(fnum(&fol["ci_lo"]), lo.unwrap(), 1e-4) && approx(fnum(&fol["ci_hi"]), hi.unwrap(), 1e-4));
    assert!(approx(fnum(&detail(fol)["mean_gain"]), 5.0 / 25.0, 1e-9));
    let ign = &hits["waiver:ignored"];
    assert!(ign["n"] == json!(5) && ign["trust"] == json!("hidden") && detail(ign)["need"] == json!(15));
    assert!(hits["lineup:followed"]["n"] == json!(0) && hits["lineup:followed"]["value"].is_null());
    assert!(!hits.keys().any(|k| k.starts_with("trade")));
    let cases = rows.iter().find(|r| r["metric"] == json!("trade_cases")).unwrap();
    assert!(cases["trust"] == json!("cases") && cases["value"].is_null());
    assert_eq!(detail(cases)["cases"][0]["label"], json!("partial"));
    let cal = rows.iter().find(|r| r["metric"] == json!("calibration")).unwrap();
    let cd = detail(cal);
    assert!(cal["n"] == json!(30) && cal["trust"] == json!("hidden") && cd["binning"] == json!("tercile"));
    assert_eq!(cd["bins"].as_array().unwrap().len(), 3);
    assert!(approx(fnum(&cal["value"]), (15.0 - 10.0 - 2.5) / (30.0 * 1.5), 1e-4));
}

#[test]
fn primary_window_follows_the_units() {
    let env = Env::new();
    let l = metrics_ledger(&env);
    for i in 0..3 {
        put(&l, Oc { window: "7d", units: "week_pts", ..oc(&format!("wk{i}")) });
        put(&l, Oc { window: "28d", units: "week_pts", ..oc(&format!("wk28{i}")) });
        put(&l, Oc { kind: "lineup", window: "7d", units: "week_pts", ..oc(&format!("lu{i}")) });
    }
    let (rows, _) = M::compute_week(&l, "espn", week()).unwrap();
    let get = |p: &str| rows.iter().find(|r| r["metric"] == json!("hit_rate") && r["pool"] == json!(p)).unwrap().clone();
    assert_eq!(detail(&get("waiver:followed"))["windows"], json!(["7d"]));
    assert!(get("waiver:followed")["n"] == json!(3) && get("lineup:followed")["n"] == json!(3));
}

#[test]
fn counterfactual_my_moves_vs_the_models() {
    let env = Env::new();
    let l = metrics_ledger(&env);
    for i in 0..4 {
        let det = json!({"alt": if i < 3 { json!({"episode_id": "e", "gain": 3.0}) } else { Value::Null },
                         "model_fpg_delta": if i > 0 { 0.2 } else { -0.1 }});
        put(&l, Oc { origin: "user_only", gain: 1.0 + i as f64, decision: Some(format!("tx:{i}")), basis: "decision",
                     detail: Some(det), pred: None, ..oc(&format!("u{i}")) });
    }
    let (rows, _) = M::compute_week(&l, "espn", week()).unwrap();
    let cf = rows.iter().find(|r| r["metric"] == json!("counterfactual")).unwrap();
    let dd = detail(cf);
    assert!(cf["n"] == json!(3) && dd["moves"] == json!(4) && cf["trust"] == json!("hidden"));
    assert!(approx(fnum(&dd["my_mean"]), 2.0, 1e-9) && approx(fnum(&dd["model_mean"]), 3.0, 1e-9));
    assert!(approx(fnum(&cf["value"]), -1.0, 1e-9) && approx(fnum(&dd["model_better"]), 2.0 / 3.0, 1e-9));
    assert!(approx(fnum(&dd["model_agreed"]), 0.75, 1e-9) && approx(fnum(&dd["my_mean_all"]), 2.5, 1e-9));
    let n = rows.iter().find(|r| r["pool"] == json!("waiver:user_only")).unwrap()["n"].clone();
    assert_eq!(n, json!(4));
}

#[test]
fn headline_is_none_until_trustworthy() {
    let env = Env::new();
    let l = metrics_ledger(&env);
    assert_eq!(M::headline(&l, "espn").unwrap(), None);
    for i in 0..19 {
        put(&l, Oc { hit: (i % 2 == 0) as i64, ..oc(&format!("f{i}")) });
    }
    M::grade_week(&l, "espn", week()).unwrap();
    assert_eq!(M::headline(&l, "espn").unwrap(), None);
    let rep = M::status_report(&l, None).unwrap();
    let nj = rep["leagues"]["espn"]["not_judgeable"].as_array().unwrap();
    assert!(nj.contains(&json!({"metric": "hit_rate", "pool": "waiver:followed", "n": 19, "need": 1})));
    assert!(nj.iter().any(|x| x["metric"] == json!("proj_fpg_mae") && x["pool"] == json!("F") && x["need"] == json!(150)));
    put(&l, Oc { hit: 1, ..oc("f19") });
    let info = M::grade_week(&l, "espn", week()).unwrap();
    assert_eq!(info["trust"]["provisional"], json!(1));
    let line = M::headline(&l, "espn").unwrap().unwrap();
    assert!(line.starts_with("Model: waiver recs followed hit 55%") && line.contains("provisional, n=20"), "{line}");
    // the digest block appears with it
    let block = H::health_block(&l, "espn").unwrap().unwrap();
    assert!(block[1].contains("waiver 55% (n=20, provisional)"), "{block:?}");
    assert!(block[2].starts_with("Params: packaged (hash "));
}

#[test]
fn headline_reports_projection_skill() {
    let env = Env::new();
    let l = metrics_ledger(&env);
    let rows = vec![
        row(json!({"snapshot_id": "s1", "week": "2026-12-07", "league": "espn", "metric": "proj_fpg_skill", "pool": "F",
                   "value": 0.12, "n": 420, "ci_lo": 0.05, "ci_hi": 0.18, "trust": "reliable", "detail_json": "{}"})),
        row(json!({"snapshot_id": "s2", "week": "2026-12-07", "league": "espn", "metric": "proj_fpg_skill", "pool": "G",
                   "value": 0.3, "n": 20, "ci_lo": null, "ci_hi": null, "trust": "hidden", "detail_json": "{}"})),
    ];
    l.upsert("metric_snapshots", &rows, &["snapshot_id"]).unwrap();
    assert_eq!(
        M::headline(&l, "espn").unwrap().unwrap(),
        "Model: forwards projections beat season-to-date by 12%, 95% CI 5% to 18% (reliable, n=420)"
    );
}

#[test]
fn grade_week_replaces_its_rows_and_status_report_shape() {
    let env = Env::new();
    let l = metrics_ledger(&env);
    let first = M::grade_week(&l, "espn", week()).unwrap();
    let n = l.count("metric_snapshots", "", &[]).unwrap();
    assert!(first["rows"] == json!(n) && n > 0);
    M::grade_week(&l, "espn", week() + Duration::days(2)).unwrap();
    assert_eq!(l.count("metric_snapshots", "", &[]).unwrap(), n);
    let rep = M::status_report(&l, None).unwrap();
    assert!(rep["params"]["version"] == json!("packaged") && rep["params"]["hash"].as_str().is_some_and(|h| !h.is_empty()));
    let lr = &rep["leagues"]["espn"];
    assert!(lr["graded"] == json!(true) && lr["week"] == json!(iso(week())) && lr["headline"].is_null());
    let metrics: std::collections::BTreeSet<&str> =
        lr["projection"].as_array().unwrap().iter().map(|r| r["metric"].as_str().unwrap()).collect();
    assert_eq!(metrics.into_iter().collect::<Vec<_>>(), ["proj_fpg_mae", "proj_fpg_skill", "proj_week_mae"]);
    assert!(lr["calibration"]["trust"] == json!("hidden") && lr["counterfactual"]["n"] == json!(0) && lr["trades"] == json!([]));
}

#[test]
fn status_before_any_grading_lists_what_is_needed() {
    let env = Env::new();
    let l = metrics_ledger(&env);
    l.upsert(
        "rec_episodes",
        &[row(json!({"episode_id": "e", "league": "fantrax", "rec_key": "k", "kind": "waiver",
                     "first_seen": "2026-10-05", "last_seen": "2026-10-05", "n_days": 1}))],
        &["episode_id"],
    )
    .unwrap();
    let rep = M::status_report(&l, None).unwrap();
    let lr = &rep["leagues"]["fantrax"];
    assert!(lr["graded"] == json!(false) && lr["week"].is_null());
    assert!(lr["not_judgeable"].as_array().unwrap().contains(&json!({"metric": "calibration", "pool": "all", "n": 0, "need": 100})));
}

// =========================================================================== track

const TODAY_TRACK: (i32, u32, u32) = (2026, 12, 1);

fn today_track() -> NaiveDate {
    d(TODAY_TRACK.0, TODAY_TRACK.1, TODAY_TRACK.2)
}

/// rows: (kind, title, gain_units, window, hit, gain, origin, window_end)
type TrackRow = (&'static str, &'static str, Option<&'static str>, &'static str, i64, f64, &'static str, String);

fn seed(dir: &Path, rows: &[TrackRow]) {
    let l = Ledger::open(dir).unwrap();
    for (i, (kind, title, units, window, hit, gain, origin, end)) in rows.iter().enumerate() {
        let ep = format!("ep{i}");
        l.upsert(
            "rec_episodes",
            &[row(json!({"episode_id": ep, "league": "espn", "rec_key": ep, "kind": kind, "title": title,
                         "first_seen": "2026-11-01", "last_seen": "2026-11-01", "n_days": 1}))],
            &["episode_id"],
        )
        .unwrap();
        l.upsert(
            "outcomes",
            &[row(json!({"outcome_id": format!("o{i}"), "league": "espn", "episode_id": ep, "window": window,
                         "basis": "first_seen", "realized_gain": gain, "gain_units": units, "hit": hit, "kind": kind,
                         "origin": origin, "complete": 1, "window_end": end}))],
            &["outcome_id"],
        )
        .unwrap();
    }
}

fn streams(n: usize, hits: usize, end: &str) -> Vec<TrackRow> {
    (0..n)
        .map(|i| {
            ("waiver", "Add X, drop Y for this matchup", Some("matchup_pts"), "7d", (i < hits) as i64,
             if i < hits { 1.0 } else { -1.0 }, "ignored", end.to_string())
        })
        .collect()
}

#[test]
fn track_no_ledger_is_not_created_and_says_why() {
    let _g = lock();
    let dir = tempfile::tempdir().unwrap();
    let book = T::TrackBook::load(dir.path(), "espn", today_track(), T::TRACK_DAYS).unwrap();
    assert!(!dir.path().join("harness.db").exists() && !book.available);
    let text = T::track_text(&book, &Recommendation::new(RecKind::Waiver, 1.0, "Add A, drop B"));
    assert!(text.starts_with("Track record: starts once the daily harness run"));
}

#[test]
fn track_rate_by_subkind_with_ci() {
    let _g = lock();
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &streams(25, 15, "2026-11-20"));
    let tr = T::track_record("waiver", Some("stream"), dir.path(), "espn", today_track()).unwrap();
    assert!(tr.n == 25 && tr.hits == 15 && tr.enough && tr.subkind.as_deref() == Some("stream"));
    assert!(approx(tr.hit_rate.unwrap(), 0.6, 1e-12) && approx(tr.avg_gain.unwrap(), 0.2, 1e-12));
    let (lo, hi) = tr.ci;
    assert!(lo.unwrap() < 0.6 && 0.6 < hi.unwrap());
    assert!(T::record_text(&tr).starts_with("Similar moves (this-matchup pickups): 60% gained points (n=25, last 60 days"));
}

#[test]
fn track_below_min_n_is_honest() {
    let _g = lock();
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &streams(3, 2, "2026-11-20"));
    let book = T::TrackBook::load(dir.path(), "espn", today_track(), T::TRACK_DAYS).unwrap();
    let mut rec = Recommendation::new(RecKind::Waiver, 1.0, "Add A, drop B for this matchup");
    rec.gain_units = Some(GainUnits::MatchupPts);
    assert_eq!(
        T::track_text(&book, &rec),
        format!("Track record: not enough history yet (n=3 graded this-matchup pickups, shown from {})", T::MIN_N)
    );
}

#[test]
fn track_thin_subkind_falls_back_to_kind() {
    let _g = lock();
    let dir = tempfile::tempdir().unwrap();
    let mut rows: Vec<TrackRow> =
        (0..22).map(|_| ("waiver", "Add A, drop B", Some("season_fpg"), "28d", 1, 0.5, "followed", "2026-11-20".to_string())).collect();
    rows.extend(streams(2, 1, "2026-11-20"));
    seed(dir.path(), &rows);
    let book = T::TrackBook::load(dir.path(), "espn", today_track(), T::TRACK_DAYS).unwrap();
    let tr = book.record("waiver", Some("stream"));
    assert!(tr.subkind.is_none() && tr.n == 24 && tr.enough);
    let strip = T::strip(&book, &T::STRIP_KINDS);
    assert_eq!(strip[0]["text"], json!("96% gained (n=24)"));
    assert_eq!(strip[1]["text"], json!("0/20 graded"));
}

#[test]
fn track_old_wrong_window_and_user_moves_are_excluded() {
    let _g = lock();
    let dir = tempfile::tempdir().unwrap();
    let mut rows = streams(2, 2, "2026-09-01");
    rows.push(("waiver", "Add A for this matchup", Some("matchup_pts"), "28d", 1, 1.0, "ignored", "2026-11-20".into()));
    rows.push(("waiver", "my own add", None, "28d", 1, 1.0, "user_only", "2026-11-20".into()));
    seed(dir.path(), &rows);
    assert_eq!(T::TrackBook::load(dir.path(), "espn", today_track(), T::TRACK_DAYS).unwrap().record("waiver", None).n, 0);
}

#[test]
fn track_subkinds() {
    assert_eq!(T::subkind_of("waiver", Some("Add A, drop B for this matchup"), None), Some("stream"));
    assert_eq!(T::subkind_of("waiver", Some("Add A, drop B"), Some("season_fpg")), Some("season"));
    assert_eq!(T::subkind_of("lineup", Some("Swap IR slots: activate A, move B to IR"), None), Some("ir"));
    assert_eq!(T::subkind_of("lineup", Some("Start Kirill Kaprizov over Ivan Irwin today"), None), Some("swap"));
    assert_eq!(T::subkind_of("injury", Some("Hold Brad Marchand through his injury"), None), Some("hold_cut"));
    assert_eq!(T::subkind_of("trade", Some("Trade A for B"), None), None);
}

// =========================================================================== refit

const FIRST_SNAP: (i32, u32, u32) = (2026, 10, 5);
const N_WEEKS: i64 = 6;
const N_PLAYERS: i64 = 80;

fn first_snap() -> NaiveDate {
    d(FIRST_SNAP.0, FIRST_SNAP.1, FIRST_SNAP.2)
}

fn as_of() -> NaiveDate {
    d(2026, 12, 9)
}

fn rate(i: i64) -> f64 {
    0.2 + 0.02 * i as f64
}

fn sc_refit() -> ScoringConfig {
    serde_json::from_value(json!({"kind": "points", "weights": {"G": 1.0, "A": 0.5, "W": 2.0, "SV": 0.1, "GA": -1.0}})).unwrap()
}

/// tests/test_harness_refit.py `build_ledger`: weekly snapshots of skaters whose to-date rate is
/// their true rate and whose projection is biased toward 1.0: a lower k_inseason is better.
fn build_ledger(l: &Ledger) {
    store_scoring(l, "espn", &sc_refit()).unwrap();
    let proj_bias = 0.6;
    let mut rows = Vec::new();
    for w in 0..N_WEEKS {
        let snap = first_snap() + Duration::days(7 * w);
        let key = format!("means:espn:{}", iso(snap));
        let val = json_dumps(&json!({"F": {"G": 1.0, "A": 0.0}}), false);
        l.execute(
            "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            &[&key, &val],
        )
        .unwrap();
        l.commit().unwrap();
        let gp = 5 + 3 * w;
        for i in 0..N_PLAYERS {
            let r = rate(i);
            let proj = r + proj_bias * (1.0 - r);
            let inputs = json!({"season": {"gp": gp, "stats": {"G": r * gp as f64, "A": 0.0, "GP": gp}, "zeros": true},
                "history": null, "projection": {"gp": 82, "stats": {"G": proj * 82.0, "GP": 82}},
                "status": "healthy", "games_next7": 4, "offnight_next7": 0, "start_share": null, "positions": ["C"]});
            rows.push(row(json!({"league": "espn", "as_of": iso(snap), "cid": format!("p{i}"), "name": format!("p{i}"),
                "nhl_id": 1000 + i, "positions": "C", "status": "healthy", "fpg": null, "proj_week": null,
                "inputs_json": json_dumps(&inputs, false), "archive_version": 2})));
        }
    }
    l.upsert("projections", &rows, &["league", "as_of", "cid"]).unwrap();
    let last = first_snap() + Duration::days(7 * (N_WEEKS - 1) + 40);
    let days: Vec<NaiveDate> = (1..=(last - first_snap()).num_days()).map(|k| first_snap() + Duration::days(k)).collect();
    let pulls: Vec<Row> = days.iter().map(|x| row(json!({"game_date": iso(*x), "n_players": N_PLAYERS}))).collect();
    l.upsert("realized_pulls", &pulls, &["game_date"]).unwrap();
    let mut daily = Vec::new();
    for i in 0..N_PLAYERS {
        for (k, x) in days.iter().enumerate() {
            if k % 2 == 0 {
                daily.push(row(json!({"nhl_id": 1000 + i, "game_date": iso(*x), "gp": 1,
                                      "stats_json": json_dumps(&json!({"G": rate(i)}), false)})));
            }
        }
    }
    l.upsert("realized_daily", &daily, &["nhl_id", "game_date"]).unwrap();
}

fn hobs(i: i64, gp_td: i64, fpg_td: f64, fpg_rest: f64) -> InseasonObs {
    InseasonObs {
        season: 2024,
        day: "11-01".into(),
        player_id: i,
        group: "F".into(),
        gp_td,
        fpg_td,
        fpg_base: 1.0,
        has_base: true,
        splits: Map::new(),
        gp_rest: 40,
        fpg_rest,
        fpg_app: 0.0,
        fpg_proj: None,
        fpg_hist: None,
        status: None,
        gp_hist3: None,
        start_share: None,
    }
}

/// Historical checkpoints the in-season k barely matters for (gp_td 0: baseline only).
fn hist_flat() -> Map<String, Vec<InseasonObs>> {
    let v = (0..200).map(|i| hobs(i, 0, 0.0, 1.0 + if i % 2 == 1 { 0.1 } else { -0.1 })).collect();
    [("espn".to_string(), v)].into_iter().collect()
}

/// Checkpoints where the baseline is right and to-date is noisy: a lower k is worse.
fn hist_prefers_high_k() -> Map<String, Vec<InseasonObs>> {
    let v = (0..200).map(|i| hobs(i, 10, 1.0 + if i % 2 == 1 { 0.5 } else { -0.5 }, 1.0)).collect();
    [("espn".to_string(), v)].into_iter().collect()
}

fn opts<'a>(mode: &str, store: &'a ParamsStore<'a>, hist: Option<&'a Map<String, Vec<InseasonObs>>>) -> R::RefitOptions<'a> {
    R::RefitOptions { mode: mode.into(), store: Some(store), hist, ..Default::default() }
}

#[test]
fn replay_table_targets_and_weeks() {
    let env = Env::new();
    let l = env.ledger();
    build_ledger(&l);
    let (obs, info) = R::replay_table(&l, as_of(), None).unwrap();
    assert_eq!(info["leagues"], json!({"espn": N_PLAYERS * N_WEEKS}));
    let o = obs.iter().find(|x| x.player == 1010 && x.snap == first_snap()).unwrap();
    assert!(o.gp28 == 14 && approx(o.real_fpg28.unwrap(), rate(10), 1e-9) && o.gp7 == 4);
    assert!(approx(o.real_pts7.unwrap(), 4.0 * rate(10), 1e-9));
    assert_eq!(obs.iter().map(|x| x.week).collect::<std::collections::BTreeSet<_>>().len(), N_WEEKS as usize);
    let (early, _) = R::replay_table(&l, first_snap() + Duration::days(29), None).unwrap();
    let snaps: std::collections::BTreeSet<NaiveDate> = early.iter().filter(|x| x.real_fpg28.is_some()).map(|x| x.snap).collect();
    assert_eq!(snaps.into_iter().collect::<Vec<_>>(), vec![first_snap()]);
    // the FPG-space view carries the live fields
    let iv = o.to_inseason();
    assert!(iv.gp_td == 5 && iv.status.as_deref() == Some("healthy") && iv.fpg_proj.is_some() && iv.fpg_hist.is_none());
}

#[test]
fn refit_applies_a_better_k_within_bounds_and_rollback_restores_the_hash() {
    let env = Env::new();
    let l = env.ledger();
    build_ledger(&l);
    let h0 = vp::params_hash(None);
    let store = ParamsStore::new(Some(&env.pdir()), Some(&l), None);
    let hist = hist_flat();
    let res = R::run_refit(&l, as_of(), &opts("apply", &store, Some(&hist))).unwrap();
    let gate = res.gate.clone().unwrap();
    assert!(gate.passed, "{:?}", gate.reasons);
    assert!(res.action == "applied" && res.version.as_deref() == Some("v0001"));
    assert!(res.groups_selected.iter().any(|g| g == "k_inseason") && res.groups_selected.len() <= 2);
    assert!(approx(res.candidate["k_inseason.skater"], 20.0, 1e-9));
    assert!(approx(vp::k_inseason_for("skater", None), 20.0, 1e-9));
    let fo = &res.objectives["fpg"];
    assert!(fnum(&fo["gain"]) >= 0.01 && fnum(&fo["ci_lo"]) > 0.0 && fnum(&fo["hist_change"]) <= 0.005);
    let rec = Value::Object(store.get("v0001").unwrap());
    assert!(rec["status"] == json!("active") && rec["parent"] == json!(PACKAGED) && rec["hash"] == json!(vp::params_hash(None)));
    assert!(approx(fnum(&rec["params"]["inseason"]["k_skater"]), 20.0, 1e-9));
    for k in ["holdout_before", "holdout_after", "hist_before", "hist_after", "n_live"] {
        assert!(rec["metrics"].get(k).is_some(), "{k}");
    }
    assert_eq!(rec["metrics"]["n_live"], json!(N_PLAYERS * N_WEEKS));
    let ptr: Value = serde_json::from_str(&std::fs::read_to_string(env.pdir().join("active.json")).unwrap()).unwrap();
    assert_eq!(ptr["version"], json!("v0001"));
    let rows = l.query("SELECT version, status, params_hash FROM param_versions", &[]).unwrap();
    assert_eq!(rows, vec![row(json!({"version": "v0001", "status": "active", "params_hash": rec["hash"]}))]);
    let params: Vec<&str> = res.rows.iter().map(|r| r["param"].as_str().unwrap()).collect();
    assert!(params.contains(&"k_inseason.skater") && params.contains(&"k_inseason.goalie"));
    let goalie = res.rows.iter().find(|r| r["param"] == json!("k_inseason.goalie")).unwrap();
    assert_eq!(goalie["changed"], json!(false)); // < 150 goalie obs: frozen
    let out = store.rollback(None, "manual", None).unwrap();
    assert_eq!(out, json!({"from": "v0001", "to": PACKAGED, "hash": h0}));
    assert_eq!(vp::params_hash(None), h0);
    assert_eq!(store.get("v0001").unwrap()["status"], json!("rolled_back"));
    assert_eq!(l.query("SELECT status FROM param_versions", &[]).unwrap()[0]["status"], json!("rolled_back"));
}

#[test]
fn dry_run_and_propose_write_nothing_active() {
    let env = Env::new();
    let l = env.ledger();
    build_ledger(&l);
    let store = ParamsStore::new(Some(&env.pdir()), Some(&l), None);
    let hist = hist_flat();
    let res = R::run_refit(&l, as_of(), &opts("dry-run", &store, Some(&hist))).unwrap();
    assert!(res.gate.as_ref().unwrap().passed && res.action == "dry-run" && store.versions().is_empty());
    let res = R::run_refit(&l, as_of(), &opts("propose", &store, Some(&hist))).unwrap();
    assert_eq!(res.action, "proposed");
    assert_eq!(store.get(res.version.as_deref().unwrap()).unwrap()["status"], json!("proposed"));
    assert!(store.active_name() == PACKAGED && vp::k_inseason_for("skater", None) == 25.0);
}

#[test]
fn candidate_that_wins_holdout_but_worsens_history_is_rejected() {
    let env = Env::new();
    let l = env.ledger();
    build_ledger(&l);
    let store = ParamsStore::new(Some(&env.pdir()), Some(&l), None);
    let hist = hist_prefers_high_k();
    let o = R::RefitOptions { only: Some(vec!["k_inseason".into()]), hist_pool_k: 10.0, ..opts("apply", &store, Some(&hist)) };
    let res = R::run_refit(&l, as_of(), &o).unwrap();
    let fo = &res.objectives["fpg"];
    assert!(fnum(&fo["gain"]) >= 0.01 && fnum(&fo["ci_lo"]) > 0.0);
    assert!(fnum(&fo["hist_change"]) > 0.005);
    let gate = res.gate.unwrap();
    assert!(!gate.passed && res.action == "none");
    assert!(gate.reasons.iter().any(|r| r.contains("historical MAE")));
    assert_eq!(vp::k_inseason_for("skater", None), 25.0);
}

fn ostats(obj: &str, n: usize, hb: f64, ha: f64, lo: f64, hi: f64, hist_b: Option<f64>, hist_a: Option<f64>, nh: usize) -> R::ObjectiveStats {
    R::ObjectiveStats {
        objective: obj.into(),
        n_holdout: n,
        holdout_before: Some(hb),
        holdout_after: Some(ha),
        ci_lo: Some(lo),
        ci_hi: Some(hi),
        hist_before: hist_b,
        hist_after: hist_a,
        n_hist: nh,
    }
}

fn gstats(n_live: usize, n_goalie: usize, weeks: usize, n_week: usize, objs: &[R::ObjectiveStats]) -> R::GateStats {
    R::GateStats { n_live, n_goalie, weeks, n_week, objectives: objs.iter().map(|o| (o.objective.clone(), o.clone())).collect() }
}

fn packaged_knobs() -> R::Knobs {
    R::current_knobs(Some(&vp::load_packaged(None)))
}

#[test]
fn gate_rejects_history_regression_directly() {
    let _g = lock();
    let cur = packaged_knobs();
    let mut cand = cur.clone();
    cand.insert("k_inseason.skater".into(), 21.0);
    let ok = ostats("fpg", 200, 0.30, 0.28, 0.02, 0.1, Some(0.25), Some(0.2505), 1000);
    assert!(R::gate(&cand, &cur, &gstats(500, 0, 5, 500, &[ok.clone()])).passed);
    let bad = ostats("fpg", 200, 0.30, 0.28, 0.02, 0.1, Some(0.25), Some(0.2515), 1000);
    let res = R::gate(&cand, &cur, &gstats(500, 0, 5, 500, &[bad]));
    assert!(!res.passed && !res.checks["statistics"]);
    assert!(res.reasons.iter().any(|r| r == "fpg: historical MAE 0.2500 -> 0.2515 (+0.60%), may not worsen by more than 0.5%"), "{:?}", res.reasons);
    let weak = ostats("fpg", 200, 0.30, 0.298, -0.01, 0.05, Some(0.25), Some(0.25), 1000);
    let reasons = R::gate(&cand, &cur, &gstats(500, 0, 5, 500, &[weak])).reasons;
    assert!(reasons.iter().any(|r| r.contains("needs >= 1% better")) && reasons.iter().any(|r| r.contains("does not exclude 0")));
    assert!(reasons.iter().any(|r| r == "fpg: 90% bootstrap CI of the improvement [-1.00%, +5.00%] does not exclude 0"), "{reasons:?}");
    let mut goalie = cur.clone();
    goalie.insert("k_inseason.goalie".into(), 36.0);
    let res = R::gate(&goalie, &cur, &gstats(500, 100, 5, 500, &[ok]));
    assert!(!res.passed && res.reasons.iter().any(|r| r.contains("goalie") && r.contains("50 more")));
    // out of bounds / too many groups / ineligible keys
    let mut wild = cur.clone();
    wild.insert("k_inseason.skater".into(), 10.0);
    wild.insert("projection_weight".into(), 0.9);
    wild.insert("offnight_bonus".into(), 0.1);
    wild.insert("age_yoy".into(), 1.0);
    let res = R::gate(&wild, &cur, &gstats(500, 0, 5, 500, &[]));
    assert!(!res.checks["eligible"] && !res.checks["bounds"] && !res.checks["groups"]);
    assert!(res.reasons.iter().any(|r| r == "k_inseason.skater 25 -> 10 outside [20, 30] (+-20%)"), "{:?}", res.reasons);
    assert!(res.reasons.iter().any(|r| r == "not eligible for refitting: age_yoy"));
}

#[test]
fn calendar_lock_and_force() {
    let env = Env::new();
    let l = env.ledger();
    build_ledger(&l);
    let store = ParamsStore::new(Some(&env.pdir()), Some(&l), None);
    let res = R::run_refit(&l, d(2026, 10, 20), &opts("dry-run", &store, None)).unwrap();
    assert!(res.locked && res.lock_reason.as_deref().unwrap().contains("locked until 2026-11-02") && res.gate.is_none());
    let o = R::RefitOptions { force: true, ..opts("dry-run", &store, None) };
    let res = R::run_refit(&l, d(2026, 10, 20), &o).unwrap();
    let gate = res.gate.unwrap();
    assert!(!res.locked && !gate.passed);
    assert!(res.n_live == 0 && gate.reasons.iter().any(|r| r.contains("need 400 (400 more)")));
    let hist = hist_flat();
    assert!(!R::run_refit(&l, as_of(), &opts("dry-run", &store, Some(&hist))).unwrap().locked);
    let o = R::RefitOptions { only: Some(vec!["age_yoy".into()]), ..opts("dry-run", &store, None) };
    let err = R::run_refit(&l, as_of(), &o).unwrap_err().to_string();
    assert!(err.contains("not eligible") && err.contains("unknown parameter group(s) age_yoy"));
}

#[test]
fn refit_days_and_auto_apply_policy() {
    assert!(!R::is_refit_day(d(2026, 10, 19)) && R::is_refit_day(d(2026, 11, 2)));
    assert!(R::is_refit_day(d(2026, 11, 16)) && !R::is_refit_day(d(2026, 11, 9)) && R::is_refit_day(d(2026, 11, 30)));
    assert_eq!(R::next_refit_day(d(2026, 9, 29)), d(2026, 11, 2));
    assert_eq!(R::next_refit_day(d(2026, 11, 3)), d(2026, 11, 16));
    assert_eq!(R::daily_refit_mode(d(2026, 11, 2), true), Some("propose"));
    assert_eq!(R::daily_refit_mode(d(2026, 11, 16), true), Some("auto"));
    assert_eq!(R::daily_refit_mode(d(2026, 11, 16), false), Some("propose"));
    assert_eq!(R::daily_refit_mode(d(2026, 11, 9), true), None);
    assert_eq!(R::allowed_groups("auto", d(2027, 1, 11)), R::TIER_A.to_vec());
    assert_eq!(R::allowed_groups("apply", d(2026, 11, 16)), R::TIER_A.to_vec());
    assert_eq!(R::allowed_groups("apply", d(2026, 12, 1)), R::GROUPS.to_vec());
    assert_eq!(R::allowed_groups("propose", d(2026, 11, 2)), R::GROUPS.to_vec());
}

#[test]
fn auto_mode_applies_only_tier_a() {
    let env = Env::new();
    let l = env.ledger();
    build_ledger(&l);
    let store = ParamsStore::new(Some(&env.pdir()), Some(&l), None);
    let hist = hist_flat();
    let o = R::RefitOptions { only: Some(vec!["availability".into(), "offnight_bonus".into()]), ..opts("auto", &store, Some(&hist)) };
    let res = R::run_refit(&l, as_of(), &o).unwrap();
    assert!(res.groups_searched.is_empty() && res.action == "none" && store.versions().is_empty());
    assert!(res.notes.iter().any(|n| n.contains("Tier B")));
    let res = R::run_refit(&l, as_of(), &opts("auto", &store, Some(&hist))).unwrap();
    assert_eq!(res.action, "applied");
    assert!(res.groups_selected.iter().all(|g| R::TIER_A.contains(&g.as_str())));
    assert_eq!(store.get(res.version.as_deref().unwrap()).unwrap()["applied_by"], json!("auto"));
}

fn promote_bad_version(store: &ParamsStore<'_>, k: f64, since: NaiveDate) {
    let mut knobs = R::current_knobs(None);
    knobs.insert("k_inseason.skater".into(), k);
    let keys = vec!["k_inseason.skater".to_string()];
    let rec = store.propose(&R::knobs_override(&knobs, Some(&keys)), &keys, None, None, None, "refit", None).unwrap();
    store.apply(rec["version"].as_str().unwrap(), "manual", None, Some(since)).unwrap();
}

#[test]
fn shadow_scoring_rolls_back_after_two_losing_weeks() {
    let env = Env::new();
    let l = env.ledger();
    build_ledger(&l);
    let h0 = vp::params_hash(None);
    let store = ParamsStore::new(Some(&env.pdir()), Some(&l), None);
    promote_bad_version(&store, 60.0, d(2026, 10, 1));
    assert_eq!(vp::k_inseason_for("skater", None), 60.0);
    let info = M::grade_week(&l, "espn", d(2026, 11, 9)).unwrap();
    let sh = &info["shadow"];
    assert!(sh["winner"] == json!("shadow") && sh["n"] == json!(N_PLAYERS) && sh.get("rolled_back").is_none(), "{sh}");
    assert_eq!(store.active_name(), "v0001");
    M::grade_week(&l, "espn", d(2026, 11, 9)).unwrap();
    assert_eq!(store.active_name(), "v0001");
    let info = M::grade_week(&l, "espn", d(2026, 11, 16)).unwrap();
    assert_eq!(info["shadow"]["rolled_back"]["to"], json!(PACKAGED));
    assert!(store.active_name() == PACKAGED && vp::params_hash(None) == h0);
    let rec = Value::Object(store.get("v0001").unwrap());
    assert_eq!(rec["status"], json!("rolled_back"));
    assert!(rec["changelog"].as_array().unwrap().iter().any(|e| e["by"] == json!("auto-rollback")));
    assert!(!l.query("SELECT command FROM runs WHERE command='auto-rollback'", &[]).unwrap().is_empty());
}

#[test]
fn shadow_scoring_keeps_a_winning_version() {
    let env = Env::new();
    let l = env.ledger();
    build_ledger(&l);
    let store = ParamsStore::new(Some(&env.pdir()), Some(&l), None);
    promote_bad_version(&store, 20.0, d(2026, 10, 1));
    for wk in [d(2026, 11, 9), d(2026, 11, 16), d(2026, 11, 23)] {
        assert_eq!(M::grade_week(&l, "espn", wk).unwrap()["shadow"]["winner"], json!("active"));
    }
    assert_eq!(store.active_name(), "v0001");
    assert_eq!(store.get("v0001").unwrap()["shadow"]["weeks"].as_object().unwrap().len(), 3);
}

#[test]
fn apply_shadows_the_previous_version_and_retires_older_ones() {
    let env = Env::new();
    let store = ParamsStore::new(Some(&env.pdir()), None, None);
    let cur = R::current_knobs(None);
    let mut k1 = cur.clone();
    k1.insert("k_inseason.skater".into(), 22.0);
    let v1 = store.propose(&R::knobs_override(&k1, Some(&["k_inseason.skater".into()])), &["x".into()], None, None, None, "refit", None).unwrap();
    store.apply(v1["version"].as_str().unwrap(), "manual", None, None).unwrap();
    let mut k2 = cur.clone();
    k2.insert("projection_weight".into(), 0.65);
    let over = vp::deep_merge(&store.active_params(), &R::knobs_override(&k2, Some(&["projection_weight".into()])));
    let v2 = store.propose(&over, &["projection_weight".into()], None, None, None, "refit", None).unwrap();
    assert!(v2["parent"] == json!("v0001") && v2["params"]["inseason"]["k_skater"] == json!(22.0));
    store.apply(v2["version"].as_str().unwrap(), "manual", None, None).unwrap();
    assert!(store.get("v0001").unwrap()["status"] == json!("shadow") && store.get("v0002").unwrap()["shadow"]["version"] == json!("v0001"));
    let v3 = store.propose(&store.active_params(), &["none".into()], None, None, None, "refit", None).unwrap();
    store.apply(v3["version"].as_str().unwrap(), "manual", None, None).unwrap();
    let st: Vec<Value> = ["v0001", "v0002", "v0003"].iter().map(|v| store.get(v).unwrap()["status"].clone()).collect();
    assert_eq!(st, vec![json!("retired"), json!("shadow"), json!("active")]);
    assert_eq!(store.rollback(Some("v0001"), "manual", None).unwrap()["to"], json!("v0001"));
    assert_eq!(vp::k_inseason_for("skater", None), 22.0);
    assert_eq!(vp::projection_weight(None), 0.6);
    assert!(store.rollback(Some("v0042"), "manual", None).is_err());
    // the Health changelog: newest first, rollback targets
    let cl = H::changelog(&store);
    assert_eq!(cl["active"], json!("v0001"));
    assert_eq!(cl["rows"][0]["version"], json!("v0003"));
    assert_eq!(cl["targets"], json!(["packaged", "v0003", "v0002"]));
}

/// Hypothesis-style property test (seeded): any candidate that passes the gate stays inside
/// the step bounds, changes 1..=2 groups and keeps the recency weights summing to 1.
#[test]
fn gated_candidates_respect_bounds_and_group_limit() {
    let _g = lock();
    let current = packaged_knobs();
    let names: Vec<&str> = R::KNOBS.iter().map(|k| k.name).collect();
    let ok = ["fpg", "week"].map(|o| {
        let h = (o == "fpg").then_some(0.3);
        ostats(o, 500, 1.0, 0.9, 0.05, 0.15, h, h, 1000)
    });
    let stats = gstats(1000, 400, 8, 1000, &ok);
    let mut rng = PyRandom::new(42);
    let mut passed = 0;
    for _ in 0..1500 {
        let mut cand = current.clone();
        let n = rng.randrange(0, 9);
        for _ in 0..n {
            let name = names[rng.randrange(0, names.len() as i64) as usize];
            let rel = rng.uniform(-2.0, 2.0) * if rng.random() < 0.7 { 0.1 } else { 1.0 };
            let base = cand[name];
            let k = R::knob_by_name(name).unwrap();
            cand.insert(name.into(), if k.relative { base * (1.0 + rel) } else { base + rel * 0.2 });
        }
        if rng.random() < 0.5 {
            let s: f64 = R::RECENCY_SPLITS.iter().map(|s| cand[format!("recency.{s}").as_str()]).sum();
            cand.insert("recency.season".into(), 1.0 - s);
        }
        let res = R::gate(&cand, &current, &stats);
        if res.passed {
            passed += 1;
            let groups = R::changed_groups(&cand, &current);
            assert!(!groups.is_empty() && groups.len() <= R::MAX_GROUPS);
            for name in R::changed_knobs(&cand, &current) {
                let (lo, hi) = R::knob_by_name(&name).unwrap().bounds(current[name.as_str()]);
                assert!(lo - 1e-9 <= cand[name.as_str()] && cand[name.as_str()] <= hi + 1e-9);
            }
            let s: f64 = ["season", "last30", "last15", "last7"].iter().map(|s| cand[format!("recency.{s}").as_str()]).sum();
            assert!((s - 1.0).abs() <= 1e-9);
        } else {
            assert!(!res.reasons.is_empty());
        }
    }
    assert!(passed > 10, "the generator should produce some passing candidates ({passed})");
}

#[test]
fn search_moves_stay_in_bounds_and_recency_sums_to_one() {
    let _g = lock();
    let base = packaged_knobs();
    let ok = ["fpg", "week"].map(|o| {
        let h = (o == "fpg").then_some(0.3);
        ostats(o, 500, 1.0, 0.9, 0.05, 0.15, h, h, 1000)
    });
    let stats = gstats(1000, 400, 8, 1000, &ok);
    let mut rng = PyRandom::new(7);
    for _ in 0..200 {
        let (w30, w15, w7) = (rng.uniform(0.0, 0.5), rng.uniform(0.0, 0.3), rng.uniform(0.0, 0.2));
        let (at30, at15) = (rng.uniform(-0.05, 0.05), rng.uniform(-0.05, 0.05));
        let mut cur = base.clone();
        for (k, v) in [("recency.last30", w30), ("recency.last15", w15), ("recency.last7", w7), ("recency.season", 1.0 - w30 - w15 - w7)] {
            cur.insert(k.into(), v);
        }
        let mut at = cur.clone();
        at.insert("recency.last30".into(), (w30 + at30).max(0.0));
        at.insert("recency.last15".into(), (w15 + at15).max(0.0));
        let s = 1.0 - at["recency.last30"] - at["recency.last15"] - w7;
        at.insert("recency.season".into(), s);
        for cand in R::recency_moves(&cur, &at, R::GRID_POINTS) {
            let mut full = cur.clone();
            for (k, v) in &cand {
                full.insert(k.clone(), *v);
            }
            let sum: f64 = ["season", "last30", "last15", "last7"].iter().map(|s| full[format!("recency.{s}").as_str()]).sum();
            assert!((sum - 1.0).abs() <= 1e-9);
            for s in ["season", "last30", "last15", "last7"] {
                let n = format!("recency.{s}");
                let (lo, hi) = R::knob_by_name(&n).unwrap().bounds(cur[n.as_str()]);
                assert!(lo - 1e-9 <= full[n.as_str()] && full[n.as_str()] <= hi + 1e-9 && full[n.as_str()] >= -1e-12);
            }
            assert!(R::gate(&full, &cur, &stats).checks["bounds"]);
        }
        for k in R::KNOBS.iter() {
            let v0 = if k.group == "recency_weights" { cur[k.name] } else { base[k.name] * (1.0 + at30) };
            for v in R::knob_grid(k, v0, R::GRID_POINTS) {
                let (lo, hi) = k.bounds(v0);
                assert!(lo - 1e-12 <= v && v <= hi + 1e-12);
            }
        }
    }
}

#[test]
fn knobs_override_and_current_knobs() {
    let _g = lock();
    let cur = packaged_knobs();
    assert_eq!(cur.len(), 17);
    let mut k = cur.clone();
    k.insert("recency.last7".into(), 0.05);
    let season = k["recency.season"] - 0.05;
    k.insert("recency.season".into(), season);
    let over = R::knobs_override(&k, Some(&["recency.last7".into()]));
    // recency always written as a complete set
    assert_eq!(over["inseason"]["recency_weights"].as_object().unwrap().len(), 4);
    let p = R::params_for(&k, Some(&vp::load_packaged(None)));
    assert_eq!(R::current_knobs(Some(&p))["recency.last7"], 0.05);
    assert_eq!(R::changed_knobs(&k, &cur), vec!["recency.last7".to_string(), "recency.season".to_string()]);
    assert_eq!(R::changed_groups(&k, &cur), vec!["recency_weights".to_string()]);
    assert_eq!(R::knob_by_name("k_inseason.skater").unwrap().bound_label(), "+-20%");
    assert_eq!(R::knob_by_name("offnight_bonus").unwrap().bound_label(), "+-0.02");
}

// =========================================================================== parity

fn fixtures_root() -> PathBuf {
    std::env::var_os("FM_PARITY_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures"))
}

/// A `legacy-python` checkout (`FM_LEGACY_ROOT`, default `../fm-legacy` beside the repo) and its
/// python (`FM_PYTHON`, default the checkout's `.venv`). None: the Python parity checks skip.
fn legacy() -> Option<(PathBuf, PathBuf)> {
    let root = std::env::var_os("FM_LEGACY_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../fm-legacy"));
    if !root.join("fantasy_manager").is_dir() {
        return None;
    }
    let py = std::env::var_os("FM_PYTHON")
        .map(PathBuf::from)
        .or_else(|| ["Scripts/python.exe", "bin/python"].iter().map(|p| root.join(".venv").join(p)).find(|p| p.is_file()))?;
    py.is_file().then_some((root, py))
}

fn python() -> Option<PathBuf> {
    legacy().map(|(_, py)| py)
}

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/dump_harness_metrics.py")
}

/// Keys whose values are wall-clock timestamps (or W8's not-yet-ported matchup calibration).
const MASKED: [&str; 7] = ["created_at", "created", "applied_at", "at", "updated", "matchup_calibration", "finished_at"];

/// First difference between two JSON values: same key order, same int / float kind, floats within
/// rel 1e-9 (abs 1e-12); JSON-encoded strings (`*_json`) are compared decoded.
fn diff(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.is_f64() != y.is_f64() {
                return Some(format!("{path}: number kind {x} vs {y}"));
            }
            let (p, q) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            ((p - q).abs() > 1e-12_f64.max(1e-9 * p.abs().max(q.abs()))).then(|| format!("{path}: {x} != {y}"))
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(format!("{path}: len {} != {}", x.len(), y.len()));
            }
            x.iter().zip(y).enumerate().find_map(|(i, (p, q))| diff(p, q, &format!("{path}[{i}]")))
        }
        (Value::Object(x), Value::Object(y)) => {
            let kx: Vec<&String> = x.keys().filter(|k| !MASKED.contains(&k.as_str())).collect();
            let ky: Vec<&String> = y.keys().filter(|k| !MASKED.contains(&k.as_str())).collect();
            if kx != ky {
                return Some(format!("{path}: keys {kx:?} != {ky:?}"));
            }
            kx.into_iter().find_map(|k| {
                let (p, q) = (&x[k], &y[k]);
                if k.ends_with("_json") || k == "changelog" {
                    if let (Some(ps), Some(qs)) = (p.as_str(), q.as_str()) {
                        let pv: Value = serde_json::from_str(ps).unwrap_or(Value::String(ps.into()));
                        let qv: Value = serde_json::from_str(qs).unwrap_or(Value::String(qs.into()));
                        return diff(&pv, &qv, &format!("{path}.{k}"));
                    }
                }
                diff(p, q, &format!("{path}.{k}"))
            })
        }
        (Value::String(x), Value::String(y)) if x != y => {
            // params source labels carry the apply date (wall clock)
            if path.ends_with(".source") && x.split(" + harness").next() == y.split(" + harness").next() {
                return None;
            }
            Some(format!("{path}: {x:?} != {y:?}"))
        }
        _ => (a != b).then(|| format!("{path}: {a} != {b}")),
    }
}

fn check(expected: &Value, got: &Value, what: &str) {
    if let Some(m) = diff(got, expected, what) {
        panic!("parity {what} (rust vs python): {m}");
    }
}

fn replay_rows(obs: &[R::ReplayObs]) -> Value {
    Value::Array(
        obs.iter()
            .map(|o| {
                json!({"league": o.league, "snap": iso(o.snap), "player": o.player, "pool": o.pool, "status": o.status,
                       "games": o.games, "exact": o.exact, "fpg_archived": o.fpg_archived, "real_fpg28": o.real_fpg28,
                       "gp28": o.gp28, "real_pts7": o.real_pts7, "gp7": o.gp7, "fpg": R::inseason_projection(o, None),
                       "proj_week": R::week_projection(o, None, None)})
            })
            .collect(),
    )
}

fn now() -> NaiveDateTime {
    d(2026, 11, 4).and_hms_opt(9, 30, 0).unwrap()
}

fn health_parts(l: &Ledger, leagues: &[String]) -> Value {
    let mut out = serde_json::Map::new();
    for lg in leagues {
        let (_, latest) = M::latest_rows(l, lg).unwrap();
        out.insert(
            lg.clone(),
            json!({"series": H::series(l, lg).unwrap(), "scorecard": H::scorecard(&latest),
                   "capture": H::capture(l, lg, now()).unwrap(), "health_block": H::health_block(l, lg).unwrap(),
                   "headline": M::headline(l, lg).unwrap()}),
        );
    }
    Value::Object(out)
}

fn tables(l: &Ledger, dir: &Path) -> Value {
    let rows = |sql: &str| Value::Array(l.query(sql, &[]).unwrap().into_iter().map(Value::Object).collect());
    let mut files = serde_json::Map::new();
    let pdir = dir.join("harness").join("params");
    if let Ok(rd) = std::fs::read_dir(&pdir) {
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".json")).collect();
        names.sort();
        for n in names {
            files.insert(n.clone(), serde_json::from_str(&std::fs::read_to_string(pdir.join(&n)).unwrap()).unwrap());
        }
    }
    json!({
        "metric_snapshots": rows("SELECT * FROM metric_snapshots ORDER BY league, week, metric, pool"),
        "param_versions": rows("SELECT * FROM param_versions ORDER BY version"),
        "rollback_runs": rows("SELECT command, league, as_of, status, summary_json FROM runs WHERE command='auto-rollback'"),
        "params_files": files,
    })
}

fn run_python(args: &[&Path], mode: &str) -> Value {
    let (root, py) = legacy().unwrap();
    let mut cmd = Command::new(py);
    cmd.env("FM_LEGACY_ROOT", root).arg(script()).arg(mode);
    for a in args {
        cmd.arg(a);
    }
    let res = cmd.output().expect("run python");
    assert!(res.status.success(), "dump_harness_metrics.py failed: {}", String::from_utf8_lossy(&res.stderr));
    let out = args.last().unwrap();
    serde_json::from_str(&std::fs::read_to_string(out).unwrap()).unwrap()
}

#[test]
fn parity_fixture_harness_db() {
    let _g = lock();
    let db = fixtures_root().join("_datadir/harness.db");
    if python().is_none() {
        return eprintln!("SKIP metrics parity: no legacy-python checkout (FM_LEGACY_ROOT)");
    }
    if !db.exists() {
        return eprintln!("SKIP metrics parity: harness.db missing");
    }
    let (py_dir, rs_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for dir in [&py_dir, &rs_dir] {
        std::fs::copy(&db, dir.path().join("harness.db")).unwrap();
    }
    let out = py_dir.path().join("py_out.json");
    let expected = run_python(&[py_dir.path(), &out], "fixture");

    set_params_dir(Some(rs_dir.path()));
    let l = Ledger::open(rs_dir.path()).unwrap();
    let today = d(2026, 11, 4);
    let leagues = M::known_leagues(&l).unwrap();
    check(&expected["leagues"], &json!(leagues), "leagues");
    let mut grades = Vec::new();
    for i in 0..8 {
        let wk = d(2026, 9, 28) + Duration::days(7 * i);
        for lg in &leagues {
            grades.push(Value::Object(M::grade_week(&l, lg, wk).unwrap()));
        }
    }
    check(&expected["grades"], &Value::Array(grades), "grades");
    check(&expected["status"], &M::status_report(&l, None).unwrap(), "status");
    check(&expected["health"], &health_parts(&l, &leagues), "health");
    check(&expected["changelog"], &H::changelog(&ParamsStore::new(None, Some(&l), None)), "changelog");
    check(&expected["calendar"], &H::calendar(today, rs_dir.path()), "calendar");
    let mut track = serde_json::Map::new();
    let mut strip = serde_json::Map::new();
    for lg in &leagues {
        let book = T::TrackBook::load(rs_dir.path(), lg, today, T::TRACK_DAYS).unwrap();
        track.insert(lg.clone(), T::as_json(&book, &M::HIT_KINDS));
        strip.insert(lg.clone(), Value::Array(T::strip(&book, &T::STRIP_KINDS)));
    }
    check(&expected["track"], &Value::Object(track), "track");
    check(&expected["track_strip"], &Value::Object(strip), "track_strip");
    let (obs, info) = R::replay_table(&l, d(2026, 11, 30), None).unwrap();
    check(&expected["replay_info"], &info, "replay_info");
    assert!(!obs.is_empty(), "the fixture has matured replay rows");
    check(&expected["replay"], &replay_rows(&obs), "replay");
    let store = ParamsStore::new(None, Some(&l), None);
    let o = R::RefitOptions { mode: "dry-run".into(), store: Some(&store), ..Default::default() };
    check(&expected["refit"], &R::run_refit(&l, d(2026, 11, 16), &o).unwrap().to_dict(), "refit");
    check(&expected["refit_locked"], &R::run_refit(&l, d(2026, 10, 20), &o).unwrap().to_dict(), "refit_locked");
    let t = tables(&l, rs_dir.path());
    for k in ["metric_snapshots", "param_versions", "rollback_runs", "params_files"] {
        check(&expected[k], &t[k], k);
    }
    assert!(expected["metric_snapshots"].as_array().unwrap().len() > 50);
    set_params_dir(None);
}

#[test]
fn parity_synthetic_refit_cycle() {
    let _g = lock();
    if python().is_none() {
        return eprintln!("SKIP refit parity: no legacy-python checkout (FM_LEGACY_ROOT)");
    }
    let root = tempfile::tempdir().unwrap();
    let (py_dir, rs_dir) = (root.path().join("py"), root.path().join("rs"));
    let out = root.path().join("py_out.json");
    let expected = run_python(&[&py_dir, &rs_dir, &out], "synthetic");

    set_params_dir(Some(&rs_dir));
    let l = Ledger::open(&rs_dir).unwrap();
    let store = ParamsStore::new(None, Some(&l), None);
    let (obs, info) = R::replay_table(&l, as_of(), None).unwrap();
    check(&expected["replay_info"], &info, "replay_info");
    check(&expected["replay"], &replay_rows(&obs), "replay");
    let flat = hist_flat();
    let high = hist_prefers_high_k();
    let run = |o: R::RefitOptions<'_>| R::run_refit(&l, as_of(), &o).unwrap().to_dict();
    check(&expected["refit"]["dry"], &run(opts("dry-run", &store, Some(&flat))), "refit.dry");
    let tier_b = vec!["availability".to_string(), "start_share".into(), "offnight_bonus".into()];
    check(&expected["refit"]["tier_b"], &run(R::RefitOptions { only: Some(tier_b), ..opts("propose", &store, Some(&flat)) }), "refit.tier_b");
    let o = R::RefitOptions { only: Some(vec!["k_inseason".into()]), hist_pool_k: 10.0, ..opts("apply", &store, Some(&high)) };
    check(&expected["refit"]["hist_worse"], &run(o), "refit.hist_worse");
    let o = R::RefitOptions { only: Some(vec!["availability".into(), "offnight_bonus".into()]), ..opts("auto", &store, Some(&flat)) };
    check(&expected["refit"]["auto_tier_b"], &run(o), "refit.auto_tier_b");
    check(&expected["refit"]["apply"], &run(opts("apply", &store, Some(&flat))), "refit.apply");
    check(&expected["hash_after_apply"], &json!(vp::params_hash(None)), "hash_after_apply");
    let mut knobs = R::current_knobs(None);
    knobs.insert("k_inseason.skater".into(), 60.0);
    let keys = vec!["k_inseason.skater".to_string()];
    let rec = store.propose(&R::knobs_override(&knobs, Some(&keys)), &keys, None, None, None, "refit", None).unwrap();
    store.apply(rec["version"].as_str().unwrap(), "manual", None, Some(d(2026, 10, 1))).unwrap();
    check(&expected["hash_bad"], &json!(vp::params_hash(None)), "hash_bad");
    let grades: Vec<Value> = [d(2026, 11, 9), d(2026, 11, 9), d(2026, 11, 16), d(2026, 11, 23), d(2026, 12, 7)]
        .iter()
        .map(|wk| Value::Object(M::grade_week(&l, "espn", *wk).unwrap()))
        .collect();
    check(&expected["grades"], &Value::Array(grades), "grades");
    check(&expected["status"], &M::status_report(&l, None).unwrap(), "status");
    check(&expected["health"], &health_parts(&l, &["espn".to_string()]), "health");
    check(&expected["changelog"], &H::changelog(&ParamsStore::new(None, Some(&l), None)), "changelog");
    check(&expected["hash_final"], &json!(vp::params_hash(None)), "hash_final");
    let t = tables(&l, &rs_dir);
    for k in ["metric_snapshots", "param_versions", "rollback_runs", "params_files"] {
        check(&expected[k], &t[k], k);
    }
    set_params_dir(None);
}
