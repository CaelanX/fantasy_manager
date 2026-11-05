//! Season outlook / fantasy-playoff plan / matchup calibration parity (work package
//! W8-outlook-playoffs-calib):
//!
//! * `cli/outlook.json` (`fm outlook --json`, minus errors/meta) from `build_outlook` +
//!   `outlook_json` with the seeded Monte Carlo (`Random(7)`);
//! * `cli/playoffs.json` (`fm playoffs --json`, minus meta) from `playoff_plan`;
//! * matchup calibration vs `fixtures/_w8_golden/matchup_calib.json` (generator:
//!   `tmp/w8/dump_matchup_calib.py`, which seeds synthetic matchup rows into a temp copy of
//!   `_datadir/harness.db` with the Python recorders): the Rust grading of the same rows, plus
//!   `snapshot_row`. Skips when the golden is absent.

use std::sync::Mutex;
use std::time::Instant;

use chrono::NaiveDate;
use fm_core::Map;
use fm_engine::analysis::matchup::Matchup;
use fm_engine::analysis::outlook::{build_outlook, outlook_json, SIMS};
use fm_engine::analysis::playoffs::{playoff_plan, TARGETS};
use fm_engine::analysis::schedule_grid::OFFNIGHT_THRESHOLD;
use fm_engine::recommend::trades::OutlookView;
use fm_engine::valuation::params;
use fm_harness::ledger::{Ledger, Row};
use fm_harness::matchup_calib::{calibration_report, pending_periods, snapshot_row, summary_line, MIN_MATCHUPS};
use fm_harness::health::matchup_warning;
use fm_http::Tape;
use fm_parity::compare::{compare, report};
use fm_parity::{skip, Fixtures, Tolerance};
use fm_providers::snapshot::SnapshotProvider;
use serde_json::{json, Value};

/// The valuation params are a process-global: one league at a time.
static LOCK: Mutex<()> = Mutex::new(());

fn strip(mut v: Value, keys: &[&str]) -> Value {
    if let Value::Object(o) = &mut v {
        for k in keys {
            o.shift_remove(*k);
        }
    }
    v
}

fn check(failures: &mut Vec<String>, name: &str, expected: &Value, actual: &Value, tol: &Tolerance) {
    let diffs = compare(expected, actual, tol);
    if diffs.is_empty() {
        let exact = compare(expected, actual, &Tolerance::exact()).len();
        eprintln!("{name}: ok ({exact} numbers differ in the last bits)");
    } else {
        let dir = fm_parity::compare::out_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(format!("{name}.actual.json")), serde_json::to_string_pretty(actual).unwrap());
        let _ = std::fs::write(dir.join(format!("{name}.expected.json")), serde_json::to_string_pretty(expected).unwrap());
        failures.push(format!("{name}: {} diffs\n{}", diffs.len(), report(&diffs[..diffs.len().min(30)])));
    }
}

fn run_league(league: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let test = format!("stage_outlook_playoffs_{league}");
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let snap = fx.snapshot();
    params::install_with_active(snap.params.merged.clone(), Some(snap.params.active.clone()));
    let values = fx.values(&snap);
    let dynasty = fx.dynasty(&snap);
    let ctx = &snap.ctx;
    let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let tape = fx.tape_path().map(|p| Tape::load(&p).expect("tape loads"));
    let prov = SnapshotProvider::new(&snap, tape, &data_dir);
    let tol = Tolerance::default();
    let mut failures: Vec<String> = Vec::new();

    // fm outlook --json
    let t = Instant::now();
    let o = build_outlook(ctx, &values, dynasty.as_ref(), Some(&prov), SIMS);
    let t_outlook = t.elapsed();
    let expected = strip(fx.cli_output("outlook"), &["errors", "meta"]);
    let actual = json!({"outlook": outlook_json(&o)});
    check(&mut failures, &format!("{test}_outlook"), &expected, &actual, &tol);
    // the OutlookView the need-trade search reads
    let view: &dyn OutlookView = &o;
    assert_eq!(view.recommended(), expected["outlook"]["recommended"].as_str());
    for t in expected["outlook"]["teams"].as_array().unwrap() {
        assert_eq!(view.direction(t["team_id"].as_str().unwrap()), t["label"].as_str());
    }

    // fm playoffs --json
    let t = Instant::now();
    let plan = playoff_plan(ctx, &values, Some(&prov), None, TARGETS, OFFNIGHT_THRESHOLD);
    let t_playoffs = t.elapsed();
    let expected = strip(fx.cli_output("playoffs"), &["meta"]);
    check(&mut failures, &format!("{test}_playoffs"), &expected, &plan.to_json(), &tol);
    eprintln!("{league}: outlook {t_outlook:?}, playoffs {t_playoffs:?}");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn stage_outlook_playoffs_espn() {
    run_league("espn");
}

#[test]
fn stage_outlook_playoffs_fantrax() {
    run_league("fantrax");
}

// --------------------------------------------------------------------------- matchup calibration

fn row_of(v: &Value) -> Row {
    v.as_object().expect("row object").clone()
}

#[test]
fn stage_matchup_calibration() {
    let test = "stage_matchup_calibration";
    let golden_path = Fixtures::root().join("_w8_golden").join("matchup_calib.json");
    let Ok(text) = std::fs::read_to_string(&golden_path) else {
        return skip(test, "no _w8_golden/matchup_calib.json (run tmp/w8/dump_matchup_calib.py)");
    };
    let golden: Value = serde_json::from_str(&text).expect("golden json");
    let Some(src) = Fixtures::harness_db() else {
        return skip(test, "no _datadir/harness.db");
    };
    // a temp copy of the fixture harness.db (never touch the shared one)
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(&src, dir.path().join("harness.db")).unwrap();
    let ledger = Ledger::open(dir.path()).expect("ledger");
    ledger.execute("DELETE FROM matchup_snapshots", &[]).unwrap();
    ledger.execute("DELETE FROM matchup_results", &[]).unwrap();
    ledger.commit().unwrap();
    let snaps: Vec<Row> = golden["seed"]["snapshots"].as_array().unwrap().iter().map(row_of).collect();
    ledger.upsert("matchup_snapshots", &snaps, &["league", "day"]).unwrap();
    let results: Vec<Row> = golden["seed"]["results"].as_array().unwrap().iter().map(row_of).collect();
    ledger.upsert("matchup_results", &results, &["league", "period", "period_end"]).unwrap();
    let today = NaiveDate::parse_from_str(golden["today"].as_str().unwrap(), "%Y-%m-%d").unwrap();
    let tol = Tolerance::default();
    let mut failures: Vec<String> = Vec::new();

    for (lg, exp) in golden["leagues"].as_object().unwrap() {
        let rep = calibration_report(&ledger, lg, MIN_MATCHUPS).expect("report");
        check(&mut failures, &format!("{test}_{lg}_report"), &exp["report"], &rep, &tol);
        assert_eq!(Value::String(summary_line(&rep)), exp["summary"], "{lg}: summary line");
        let warning = matchup_warning(&ledger, lg).map_or(Value::Null, Value::String);
        assert_eq!(warning, exp["warning"], "{lg}: matchup warning");
        let pend: Vec<Value> = pending_periods(&ledger, lg, today).unwrap().into_iter().map(Value::Object).collect();
        check(&mut failures, &format!("{test}_{lg}_pending"), &exp["pending"], &Value::Array(pend), &tol);
    }

    // snapshot_row parity
    let c = &golden["snapshot_row"]["input"];
    let d = |k: &str| NaiveDate::parse_from_str(c[k].as_str().unwrap(), "%Y-%m-%d").unwrap();
    let f2 = |k: &str| (c[k][0].as_f64().unwrap(), c[k][1].as_f64().unwrap());
    let (mine, theirs, sds) = (f2("mine"), f2("theirs"), f2("sds"));
    let m = Matchup {
        league: "espn".into(),
        my_team: "Me".into(),
        opponent_team: Some("Opp".into()),
        opponent_id: Some("t2".into()),
        period: c["period"].as_i64(),
        start: Some(d("start")),
        end: Some(d("end")),
        from_day: None,
        days_left: c["days_left"].as_i64().unwrap(),
        playoffs: false,
        my_points_so_far: mine.0,
        their_points_so_far: theirs.0,
        my_projected_remaining: mine.1,
        their_projected_remaining: theirs.1,
        my_games_left: 0,
        their_games_left: 0,
        my_sd: sds.0,
        their_sd: sds.1,
        win_probability: c["p"].as_f64(),
        stance: None,
        key_players_theirs: vec![],
        gap_by_position: Map::new(),
        advice: vec![],
        my_players: vec![],
        their_players: vec![],
        source: "test".into(),
        lineup_basis: String::new(),
        warnings: vec![],
        draws: 0,
    };
    let mut row = Value::Object(snapshot_row("espn", NaiveDate::from_ymd_opt(2026, 11, 4).unwrap(), &m).expect("row"));
    row = strip(row, &["recorded_at"]);
    check(&mut failures, &format!("{test}_snapshot_row"), &golden["snapshot_row"]["row"], &row, &tol);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
