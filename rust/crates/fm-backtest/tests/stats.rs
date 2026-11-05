//! Ports of the scoring / metrics / report parts of tests/test_backtest.py, plus parity runs of
//! the metrics and the markdown report against the Python implementation (the repo venv python is
//! used as an oracle; the tests that need it skip with a message when it is absent).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use fm_backtest::evaluate::{
    head_to_head, metrics, pearson, rankdata, row, spearman, summarize, top_n_precision, Metrics, ResultRow,
};
use fm_backtest::report::{self, AppConstants};
use fm_backtest::scoring::{fpg, load_scoring, missing_weights, scorer, PRESETS};
use fm_core::Map;
use serde_json::{json, Value};

fn worktree() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.ancestors()
        .find(|p| p.join("fantasy_manager").is_dir())
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::var("FM_WORKTREE").ok().map(PathBuf::from))
        .expect("worktree root (set FM_WORKTREE when building outside the repo)")
}

fn python() -> Option<PathBuf> {
    let p = PathBuf::from("C:/code/fantasy_manager/.venv/Scripts/python.exe");
    p.exists().then_some(p)
}

/// Run a Python snippet (argv[1] = worktree root, JSON on stdin) and parse its stdout JSON.
fn py_json(script: &str, input: &Value) -> Option<Value> {
    let py = python()?;
    let mut child = Command::new(py)
        .args(["-I", "-c", script])
        .arg(worktree())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    Some(serde_json::from_slice(&out.stdout).unwrap())
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-12 * (1.0 + a.abs().max(b.abs()))
}

// --------------------------------------------------------------------------- scoring

#[test]
fn presets_match_live_fixtures() {
    for name in ["espn", "fantrax"] {
        let p = worktree().join("tests/fixtures/backtest").join(format!("scoring_{name}.json"));
        let fx: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let fx = &fx["scoring"];
        let cfg = &PRESETS[name];
        let as_map = |v: &Value| -> Map<String, f64> {
            v.as_object().unwrap().iter().map(|(k, x)| (k.clone(), x.as_f64().unwrap())).collect()
        };
        // dict equality in Python ignores order
        let sorted = |m: &Map<String, f64>| {
            let mut v: Vec<_> = m.iter().map(|(k, x)| (k.clone(), *x)).collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };
        assert_eq!(sorted(&cfg.weights), sorted(&as_map(&fx["weights"])), "{name} weights");
        assert_eq!(sorted(&cfg.goalie_weights), sorted(&as_map(&fx["goalie_weights"])), "{name} goalie weights");
    }
    assert_eq!(PRESETS.keys().map(|s| s.as_str()).collect::<Vec<_>>(), ["espn", "fantrax", "default"]);
    assert_eq!(load_scoring("ESPN").unwrap().0, "espn");
    assert_eq!(load_scoring("").unwrap().0, "espn");
    let err = load_scoring("nope").unwrap_err().to_string();
    assert_eq!(err, "unknown scoring 'nope'; use one of espn, fantrax, default or a JSON file");
}

#[test]
fn load_scoring_from_a_json_file_bare_or_wrapped() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = json!({"kind": "points", "weights": {"G": 3.0}});
    let bare = dir.path().join("mine.json");
    std::fs::write(&bare, cfg.to_string()).unwrap();
    let wrapped = dir.path().join("wrapped.json");
    std::fs::write(&wrapped, json!({"scoring": cfg}).to_string()).unwrap();
    for (p, stem) in [(bare, "mine"), (wrapped, "wrapped")] {
        let (name, c) = load_scoring(p.to_str().unwrap()).unwrap();
        assert_eq!(name, stem);
        assert_eq!(c.weights["G"], 3.0);
    }
}

#[test]
fn fpg_goalie_weights_apply_to_goalie_lines() {
    let sc = scorer(&PRESETS["fantrax"]).unwrap();
    let line = |pairs: &[(&str, f64)]| -> Map<String, f64> { pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect() };
    assert_eq!(fpg(Some(&line(&[("G", 1.0)])), &sc), 4.0);
    assert_eq!(fpg(Some(&line(&[("G", 1.0), ("GS", 1.0), ("SV", 0.0)])), &sc), 20.0);
    assert_eq!(fpg(None, &sc), 0.0);
    assert_eq!(fpg(Some(&Map::new()), &sc), 0.0);
    let cats = fm_core::models::ScoringConfig {
        kind: fm_core::models::ScoringKind::Categories,
        weights: Map::new(),
        categories: vec![],
        goalie_weights: Map::new(),
    };
    assert_eq!(scorer(&cats).unwrap_err().to_string(), "the backtest scores points leagues only");
    assert_eq!(missing_weights(&PRESETS["espn"]), ["DEF", "HAT"]);
    assert_eq!(missing_weights(&PRESETS["fantrax"]), ["FT"]);
}

// --------------------------------------------------------------------------- metrics

#[test]
fn rank_and_spearman() {
    assert_eq!(rankdata(&[10.0, 20.0, 20.0, 5.0]), [2.0, 3.5, 3.5, 1.0]);
    assert!(close(spearman(&[1.0, 2.0, 3.0, 4.0], &[10.0, 20.0, 30.0, 40.0]), 1.0));
    assert!(close(spearman(&[1.0, 2.0, 3.0, 4.0], &[4.0, 3.0, 2.0, 1.0]), -1.0));
    assert!(pearson(&[1.0], &[2.0]).is_nan());
    assert!(pearson(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]).is_nan()); // zero variance
}

#[test]
fn metrics_and_top_n() {
    let m = metrics(&[1.0, 2.0, 3.0], &[1.0, 1.0, 5.0], false);
    assert!(close(m.mae, 1.0) && close(m.rmse, (5.0f64 / 3.0).sqrt()) && close(m.bias, -1.0 / 3.0));
    assert_eq!((m.top50, m.top100), (None, None));
    let pred: Vec<f64> = (0..100).map(|i| i as f64).collect();
    let mut act = pred.clone();
    act.swap(99, 0); // swap best and worst
    assert!(close(top_n_precision(&pred, &act, 10).unwrap(), 0.9));
    assert_eq!(top_n_precision(&pred[..5], &act[..5], 10), None);
    let empty = metrics(&[], &[], true);
    assert_eq!(empty.n, 0);
    assert!(empty.mae.is_nan() && empty.spearman.is_nan());
}

const PY_METRICS: &str = r#"
import sys, json, math
sys.path.insert(0, sys.argv[1])
from fantasy_manager.backtest import evaluate as E
inp = json.load(sys.stdin)
def j(x):
    return None if isinstance(x, float) and math.isnan(x) else x
out = []
for case in inp:
    m = E.metrics(case["pred"], case["actual"], top=case["top"])
    out.append({"rank": E.rankdata(case["pred"]), "spearman": j(E.spearman(case["pred"], case["actual"])),
                "pearson": j(E.pearson(case["pred"], case["actual"])),
                "metrics": {k: j(v) for k, v in vars(m).items()}})
print(json.dumps(out))
"#;

/// Deterministic pseudo-random values rounded to a coarse grid (so there are ties).
fn series(seed: u64, n: usize, scale: f64) -> Vec<f64> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (((x >> 33) % 400) as f64 / 100.0 * scale * 4.0).round() / 4.0
        })
        .collect()
}

#[test]
fn metrics_equal_python() {
    let cases: Vec<Value> = [(1, 7, false), (2, 150, true), (3, 60, true), (4, 400, true)]
        .iter()
        .map(|&(seed, n, top)| json!({"pred": series(seed, n, 1.0), "actual": series(seed + 100, n, 1.0), "top": top}))
        .collect();
    let Some(golden) = py_json(PY_METRICS, &Value::Array(cases.clone())) else {
        eprintln!("skipped: repo venv python not found");
        return;
    };
    for (case, g) in cases.iter().zip(golden.as_array().unwrap()) {
        let f = |k: &str| -> Vec<f64> { case[k].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect() };
        let (pred, actual) = (f("pred"), f("actual"));
        let want_rank: Vec<f64> = g["rank"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        assert_eq!(rankdata(&pred), want_rank);
        let opt = |v: &Value| v.as_f64();
        assert_eq!(Some(spearman(&pred, &actual)).filter(|x| !x.is_nan()), opt(&g["spearman"]));
        assert_eq!(Some(pearson(&pred, &actual)).filter(|x| !x.is_nan()), opt(&g["pearson"]));
        let m = metrics(&pred, &actual, case["top"].as_bool().unwrap());
        let gm = &g["metrics"];
        assert_eq!(m.n as u64, gm["n"].as_u64().unwrap());
        assert_eq!(Some(m.mae), opt(&gm["mae"]));
        assert_eq!(Some(m.rmse), opt(&gm["rmse"]));
        assert_eq!(Some(m.bias), opt(&gm["bias"]));
        assert_eq!(Some(m.spearman).filter(|x| !x.is_nan()), opt(&gm["spearman"]));
        assert_eq!(m.top50, opt(&gm["top50"]));
        assert_eq!(m.top100, opt(&gm["top100"]));
    }
}

// --------------------------------------------------------------------------- summaries / report

fn synthetic_rows() -> Vec<ResultRow> {
    let mut rows = Vec::new();
    let mk = |i: usize, scale: f64| Metrics {
        n: 100 + i * 7,
        mae: 0.2 + scale * 0.013 * (i as f64 + 1.0),
        rmse: 0.3 + scale * 0.021 * (i as f64 + 1.0),
        bias: -0.01 * scale * i as f64,
        spearman: 0.6 - 0.02 * scale * (i as f64),
        top50: Some(0.3 + 0.01 * i as f64),
        top100: Some(0.4 + 0.005 * i as f64),
    };
    for (si, season) in [20212022i64, 20222023, 20232024].iter().enumerate() {
        for (mi, model) in ["naive", "marcel", "fm_current", "fm_fitted", "fm_multi", "extra"].iter().enumerate() {
            // fm_multi is skipped in the first season, the way a model without a fit would be
            if *model == "fm_multi" && si == 0 {
                continue;
            }
            let scale = 1.0 + ((mi * 7 + si * 3) % 5) as f64 * 0.1;
            rows.push(row("preseason", *season, None, model, "skaters", &mk(mi + si, scale)));
            let mut g = mk(mi + si, scale * 1.4);
            g.top50 = None;
            g.top100 = None;
            rows.push(row("preseason", *season, None, model, "goalies", &g));
        }
        for (mi, model) in ["preseason", "to_date", "fm_shrink_only", "fm_current", "fm_fitted"].iter().enumerate() {
            for cp in ["2022-11-01", "2022-12-01"] {
                let scale = 1.0 + ((mi * 3 + si) % 4) as f64 * 0.1;
                rows.push(row("inseason", *season, Some(cp), model, "skaters", &mk(mi, scale)));
                let mut g = mk(mi, scale * 1.2);
                g.top50 = None;
                g.top100 = None;
                rows.push(row("inseason", *season, Some(cp), model, "goalies", &g));
            }
        }
    }
    // a model with an undefined (NaN) metric in one row
    let mut nan_row = row("preseason", 20232024, None, "naive", "skaters", &mk(1, 1.0));
    nan_row.spearman = f64::NAN;
    rows.push(nan_row);
    rows
}

#[test]
fn summarize_and_head_to_head_basics() {
    let rows = synthetic_rows();
    let s = summarize(&rows, "preseason", "skaters");
    assert_eq!(s.keys().next().unwrap(), "naive"); // first-seen order
    let naive: Vec<&ResultRow> = rows.iter().filter(|r| r.kind == "preseason" && r.pool == "skaters" && r.model == "naive").collect();
    assert_eq!(s["naive"].seasons, naive.len());
    assert_eq!(s["naive"].n, naive.iter().map(|r| r.n as i64).sum::<i64>());
    // the NaN spearman is left out of the mean
    let want: f64 = naive.iter().map(|r| r.spearman).filter(|x| !x.is_nan()).sum::<f64>()
        / naive.iter().filter(|r| !r.spearman.is_nan()).count() as f64;
    assert!(close(s["naive"].spearman.unwrap(), want));
    let (w, t) = head_to_head(&rows, "marcel", "naive", "preseason", "skaters", "mae");
    assert!(t == 3 && w <= t);
    // a model absent from a season is not compared in it
    assert_eq!(head_to_head(&rows, "fm_multi", "naive", "preseason", "skaters", "mae").1, 2);
    assert_eq!(rows[0].to_dict().keys().map(|s| s.as_str()).collect::<Vec<_>>(), [
        "kind", "season", "checkpoint", "model", "pool", "n", "mae", "rmse", "bias", "spearman", "top50", "top100"
    ]);
}

const PY_REPORT: &str = r#"
import sys, json, math
sys.path.insert(0, sys.argv[1])
from fantasy_manager.backtest import report as R
from fantasy_manager.backtest.evaluate import ResultRow
from fantasy_manager.valuation.blend import K_BASELINE, K_INSEASON, RECENCY_WEIGHTS
from fantasy_manager.valuation.dynasty import PRODUCTION_CURVES
inp = json.load(sys.stdin)
nan = float("nan")
rows = []
for r in inp["rows"]:
    for k in ("mae", "rmse", "bias", "spearman"):
        if r[k] is None:
            r[k] = nan
    rows.append(ResultRow(**r))
print(json.dumps({
    "consts": {"k_baseline": dict(K_BASELINE), "k_inseason": dict(K_INSEASON),
               "recency_weights": dict(RECENCY_WEIGHTS),
               "production_curves": {g: [list(map(float, p)) for p in c] for g, c in PRODUCTION_CURVES.items()}},
    "report": R.render(rows, inp["info"], inp["fitted"]),
    "recs": R.recommendations(rows, inp["fitted"]),
}))
"#;

fn consts_from(v: &Value) -> AppConstants {
    let m = |k: &str| -> Map<String, f64> {
        v[k].as_object().unwrap().iter().map(|(a, b)| (a.clone(), b.as_f64().unwrap())).collect()
    };
    AppConstants {
        k_baseline: m("k_baseline"),
        k_inseason: m("k_inseason"),
        recency_weights: m("recency_weights"),
        production_curves: v["production_curves"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(g, pts)| {
                (
                    g.clone(),
                    pts.as_array().unwrap().iter().map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap())).collect(),
                )
            })
            .collect(),
    }
}

fn packaged_fit() -> Value {
    let p = worktree().join("fantasy_manager/valuation/fitted_params.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn report_case(name: &str, rows: &[ResultRow], info: Value, fitted: Value) {
    let rows_json: Vec<Value> = rows.iter().map(|r| serde_json::to_value(r).unwrap()).collect();
    let input = json!({"rows": rows_json, "info": info, "fitted": fitted});
    let Some(golden) = py_json(PY_REPORT, &input) else {
        eprintln!("skipped {name}: repo venv python not found");
        return;
    };
    let consts = consts_from(&golden["consts"]);
    let fit = if input["fitted"].is_null() { None } else { Some(&input["fitted"]) };
    let got = report::render(rows, &input["info"], fit, None, &consts);
    let want = golden["report"].as_str().unwrap();
    if got != want {
        for (i, (a, b)) in got.lines().zip(want.lines()).enumerate() {
            assert_eq!(a, b, "{name}: first differing line {}", i + 1);
        }
        assert_eq!(got.lines().count(), want.lines().count(), "{name}: line count");
    }
    assert_eq!(got, want, "{name}");
    let recs = report::recommendations(rows, fit, &consts);
    let want_recs: Vec<&str> = golden["recs"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    assert_eq!(recs, want_recs, "{name}: recommendations");
}

fn info() -> Value {
    json!({"scoring": "espn", "seasons": [20212022, 20222023, 20232024],
           "table_seasons": [20102011, 20112012, 20232024], "unscored_stats": ["DEF", "HAT"],
           "data": "cache 3 files"})
}

#[test]
fn report_with_the_packaged_fit_equals_python() {
    report_case("packaged", &synthetic_rows(), info(), packaged_fit());
}

#[test]
fn report_without_a_fit_or_inseason_rows_equals_python() {
    let rows: Vec<ResultRow> = synthetic_rows().into_iter().filter(|r| r.kind == "preseason").collect();
    report_case("no fit", &rows, json!({"scoring": "fantrax", "seasons": [], "data": null}), Value::Null);
    report_case("no rows", &[], json!({"scoring": "espn", "seasons": []}), Value::Null);
}

#[test]
fn report_recommendations_for_a_drifted_fit_equal_python() {
    let rows = synthetic_rows();
    let mut f = packaged_fit();
    f["scoring"] = json!("fantrax");
    f["preseason"]["k_multi"] = json!({"F": 61.0, "D": 20, "G": 12.5});
    f["inseason"]["k_skater"] = json!(99.0);
    f["inseason"]["recency_weights"] = json!({"season": 0.2, "last30": 0.3, "last15": 0.25, "last7": 0.25});
    f["dynasty_age_curves"]["F"] = json!([[18, 0.5], [30, 1.0]]);
    report_case("drifted", &rows, info(), f.clone());
    // files written before the 3-season baseline: app_k holds skater / goalie only
    f["preseason"]["app_k"] = json!({"skater": 12, "goalie": 40.0});
    f["preseason"]["k_multi"] = Value::Null;
    f["scoring"] = json!("espn");
    report_case("legacy app_k", &rows, info(), f);
}
