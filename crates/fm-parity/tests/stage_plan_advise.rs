//! Weekly plan / claims / advise parity (work package W8-plan-claims-advise): on the snapshot with
//! the fixture values,
//! * `advise` (lineup, waivers, flags, injuries, alerts) merged with the trade engine called with
//!   the trade-block wants must reproduce `engines/advise_merged.json` and the engine part of
//!   `cli/advise.json` (`fm advise --json`, limit 15);
//! * `plan_claims` must reproduce `cli/claims.json` (without `meta`);
//! * `build_week_plan` (plan memory on a temp copy of the fixture data dir, matchup scores from
//!   the tape-backed SnapshotProvider, else from `cli/matchup.json`) must reproduce
//!   `cli/plan.json` (without `meta`); the `win_*` fields get the documented set-order tolerance.
//! The plan is built twice on fresh data dirs and must be identical (parallel beam search).
//! Times are printed (`--nocapture`).

use std::sync::Mutex;
use std::time::Instant;

use fm_engine::recommend::trades::pin_python_acceptance;
use fm_engine::scoring::disable_point_scale;
use fm_engine::recommend::started::disable_started_games;
use fm_engine::analysis::matchup::{current_matchup, MatchupOpts, ScoreInfo};
use fm_engine::recommend::advise::{advise, merge_normalized, AdviseOpts};
use fm_engine::recommend::claims::{plan_claims, CLAIM_LIMIT};
use fm_engine::recommend::injuries::{History, StatusHistory};
use fm_engine::recommend::lineup::disable_goalie_matchup;
use fm_engine::recommend::plan::{build_week_plan, PlanOpts};
use fm_engine::recommend::trades::{recommend_trades, EngineOpts, TradeOpts};
use fm_engine::valuation::params;
use fm_engine::valuation::schedule::ScheduleMemo;
use fm_http::Tape;
use fm_parity::compare::{compare, report};
use fm_parity::{normalize, skip, Fixtures, Tolerance};
use fm_providers::snapshot::SnapshotProvider;
use serde_json::Value;

/// The valuation params are a process-global: one league at a time.
static LOCK: Mutex<()> = Mutex::new(());

fn date(v: &Value) -> Option<chrono::NaiveDate> {
    v.as_str().and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
}

/// The ScoreInfo the Python run saw, read back from its matchup output.
fn scores_from(out: &Value) -> Option<ScoreInfo> {
    if out.get("source")?.as_str()? == "projection only" {
        return None;
    }
    Some(ScoreInfo {
        period: out.get("period").and_then(|v| v.as_i64()),
        start: date(&out["start"]),
        end: date(&out["end"]),
        opponent_id: out.get("opponent_id").and_then(|v| v.as_str()).map(|s| s.to_string()),
        my_points: out["my_points_so_far"].as_f64().unwrap_or(0.0),
        their_points: out["their_points_so_far"].as_f64().unwrap_or(0.0),
        playoffs: out["playoffs"].as_bool().unwrap_or(false),
        source: out["source"].as_str().unwrap_or("").to_string(),
    })
}

fn without_meta(mut v: Value) -> Value {
    if let Value::Object(o) = &mut v {
        o.shift_remove("meta");
    }
    v
}

/// Day-line goalies come from a frozenset in Python (hash-random order, ARCHITECTURE.md 4.3; Rust
/// uses cid order): sort each day's list and the "G: ..." part of its text on both sides.
fn canon_goalies(mut v: Value) -> Value {
    if let Some(days) = v.get_mut("days").and_then(|d| d.as_array_mut()) {
        for day in days {
            let Some(gs) = day.get("goalies").and_then(|g| g.as_array()).cloned() else { continue };
            let names: Vec<String> = gs.iter().filter_map(|g| g.as_str().map(str::to_string)).collect();
            let mut sorted = names.clone();
            sorted.sort();
            if let Some(Value::String(t)) = day.get_mut("text") {
                *t = t.replace(&format!("G: {}", names.join(", ")), &format!("G: {}", sorted.join(", ")));
            }
            day["goalies"] = Value::from(sorted);
        }
    }
    v
}

/// Compare and remember the failures (all comparisons run, the test fails at the end).
fn check(failures: &mut Vec<String>, name: &str, expected: &Value, actual: &Value, tol: &Tolerance) {
    let diffs = compare(expected, actual, tol);
    if diffs.is_empty() {
        let exact = compare(expected, actual, &Tolerance::exact()).len();
        eprintln!("{name}: ok ({exact} values differ within tolerance)");
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
    // the fixtures come from Python, whose lineup valued a goalie start without the opponent and
    // floored it at zero
    disable_goalie_matchup(true);
    // and used the ESPN-point thresholds unscaled in every league, with no error ranges
    disable_point_scale(true);
    disable_started_games(true);
    // and scored an offer that is even by market value at 50%, without checking it from their seat
    pin_python_acceptance(true);
    let test = format!("stage_plan_advise_{league}");
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let snap = fx.snapshot();
    params::install_with_active(snap.params.merged.clone(), Some(snap.params.active.clone()));
    let values = fx.values(&snap);
    let dynasty = fx.dynasty(&snap);
    let ctx = &snap.ctx;
    let now = &snap.now;
    let tol = Tolerance::default();
    let mut failures: Vec<String> = Vec::new();

    let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let memo = ScheduleMemo::new(Some(data_dir.clone()));
    let hist = StatusHistory::new(&data_dir).expect("status history");
    let history = History::Store(&hist);

    // advise_merged: advise() without trades + the trade engine with the trade-block wants
    let t = Instant::now();
    let include = ["lineup", "waivers", "flags", "injuries", "alerts"];
    let aopts = AdviseOpts { dynasty_values: dynasty.as_ref(), history: &history, include: &include, limit: None };
    let recs = advise(ctx, &values, &aopts, now, &memo);
    let engine = EngineOpts { wants: snap.trade_wants.as_ref(), offered: snap.trade_offered.as_ref(), ..EngineOpts::default() };
    let topts = TradeOpts { dynasty_values: dynasty.as_ref(), engine, ..TradeOpts::default() };
    let trades = recommend_trades(ctx, &values, &topts, None, None).expect("trades");
    let merged = merge_normalized(recs, trades, None);
    let t_advise = t.elapsed();
    let merged_json = serde_json::to_value(&merged).unwrap();
    check(&mut failures, &format!("{test}_advise_merged"), &fx.engine("advise_merged"), &merged_json, &tol);
    let cli_advise = fx.cli_output("advise");
    let first15 = serde_json::to_value(&merged[..merged.len().min(15)]).unwrap();
    check(&mut failures, &format!("{test}_cli_advise"), &cli_advise["recommendations"], &first15, &tol);

    // claims (fm claims)
    let tape = fx.tape_path().map(|p| Tape::load(&p).expect("tape loads"));
    let have_tape = tape.is_some();
    let prov = SnapshotProvider::new(&snap, tape, &data_dir);
    let t = Instant::now();
    let cp = plan_claims(ctx, &values, dynasty.as_ref(), Some(&prov), now, CLAIM_LIMIT, &memo).expect("claims");
    let t_claims = t.elapsed();
    check(&mut failures, &format!("{test}_cli_claims"), &without_meta(fx.cli_output("claims")), &serde_json::to_value(&cp).unwrap(), &tol);

    // plan (fm plan --no-archive): matchup from the provider (tape) or the fixture's scores
    let expected_plan = without_meta(fx.cli_output("plan"));
    let fixture_matchup = if have_tape {
        None
    } else {
        let mopts = MatchupOpts { scores: scores_from(&fx.cli_output("matchup")), streaming: false, ..MatchupOpts::default() };
        Some(current_matchup(ctx, Some(&prov), Some(&values), mopts, now, &memo).expect("matchup"))
    };
    let build = |dir: &std::path::Path, hist: &StatusHistory| {
        let history = History::Store(hist);
        let opts = PlanOpts {
            dynasty_values: dynasty.as_ref(),
            matchup: fixture_matchup.as_ref(),
            provider: Some(&prov),
            history: &history,
            data_dir: Some(dir),
            ..PlanOpts::default()
        };
        build_week_plan(ctx, &values, &opts, now, &memo).to_json()
    };
    let t = Instant::now();
    let plan = build(&data_dir, &hist);
    let t_plan = t.elapsed();
    let ptol = normalize::set_order_fields(tol.clone());
    check(&mut failures, &format!("{test}_cli_plan"), &canon_goalies(expected_plan), &canon_goalies(plan.clone()), &ptol);

    // deterministic: the same plan from a fresh copy of the data dir
    let (dir2, _guard2) = Fixtures::temp_datadir().expect("temp data dir");
    let hist2 = StatusHistory::new(&dir2).expect("status history");
    let t = Instant::now();
    let again = build(&dir2, &hist2);
    let t_again = t.elapsed();
    assert_eq!(serde_json::to_string(&plan).unwrap(), serde_json::to_string(&again).unwrap(), "{league}: plan not deterministic");

    eprintln!(
        "{league}: advise+trades {t_advise:?} ({} recs), claims {t_claims:?} ({} claims), plan {t_plan:?} / {t_again:?} \
         ({} moves; tape: {have_tape})",
        merged.len(),
        cp.claims.len(),
        plan["moves"].as_array().map_or(0, |a| a.len())
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn stage_plan_advise_espn() {
    run_league("espn");
}

#[test]
fn stage_plan_advise_fantrax() {
    run_league("fantrax");
}
