//! Injuries / flags / alerts / matchup parity (work package W7-injuries-flags-alerts-matchup):
//! each engine called on the snapshot with the fixture values must reproduce
//! `fixtures/<league>/engines/{injuries,flags,line_alerts,role_alerts}.json`, the engine parts of
//! `cli/{injuries,flags}.json`, and `cli/matchup.json` (`fm matchup --seed 7`, without `meta`).
//! The status history is read from a temp copy of the fixture data dir. Matchup scores come from
//! the tape-backed SnapshotProvider when the tape is present, else from the fixture's own output.

use std::sync::Mutex;
use std::time::Instant;

use fm_core::values::Horizon;
use fm_engine::analysis::matchup::{current_matchup, MatchupOpts, ScoreInfo};
use fm_engine::recommend::alerts::{recommend_line_alerts, LineAlertOpts};
use fm_engine::recommend::flags::recommend_role_alerts;
use fm_engine::recommend::flags::recommend_flags;
use fm_engine::recommend::injuries::{recommend_injuries, History, StatusHistory};
use fm_engine::valuation::params;
use fm_engine::valuation::schedule::ScheduleMemo;
use fm_http::Tape;
use fm_parity::{assert_parity, skip, Fixtures, Tolerance};
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

fn run_league(league: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let test = format!("stage_alerts_matchup_{league}");
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let snap = fx.snapshot();
    params::install_with_active(snap.params.merged.clone(), Some(snap.params.active.clone()));
    let values = fx.values(&snap);
    let dynasty = fx.dynasty(&snap);
    let memo = ScheduleMemo::new(Some(Fixtures::datadir_path()));
    let ctx = &snap.ctx;
    let now = &snap.now;
    let tol = Tolerance::default();

    // injuries (status history: a temp copy of the fixture data dir)
    let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let hist = StatusHistory::new(&data_dir).expect("status history");
    let t = Instant::now();
    let injuries =
        recommend_injuries(ctx, &values, History::Store(&hist), Horizon::Week, dynasty.as_ref(), now, &memo).expect("injuries");
    let t_inj = t.elapsed();
    let inj_json = serde_json::to_value(&injuries).unwrap();
    assert_parity!(&format!("{test}_injuries"), fx.engine("injuries"), inj_json.clone(), tol.clone());
    let cli_inj = fx.cli_output("injuries");
    assert_parity!(&format!("{test}_cli_injuries"), cli_inj["recommendations"].clone(), inj_json, tol.clone());

    // flags
    let flags = recommend_flags(ctx, &values, None, None).expect("flags");
    let flags_json = serde_json::to_value(&flags).unwrap();
    assert_parity!(&format!("{test}_flags"), fx.engine("flags"), flags_json, tol.clone());
    let cli_flags = fx.cli_output("flags");
    for (key, kind) in [("sell_high", "sell_high"), ("buy_low", "buy_low")] {
        let mine: Vec<&fm_core::models::Recommendation> = flags.iter().filter(|r| r.kind.as_str() == kind).collect();
        if let Some(Value::Array(exp)) = cli_flags.get(key) {
            if exp.first().is_some_and(|e| e.get("kind").is_some()) || exp.is_empty() {
                assert_parity!(&format!("{test}_cli_flags_{key}"), cli_flags[key].clone(), serde_json::to_value(&mine).unwrap(), tol.clone());
            }
        }
    }

    // alerts
    let line = recommend_line_alerts(ctx, Some(&values), &LineAlertOpts::default());
    assert_parity!(&format!("{test}_line_alerts"), fx.engine("line_alerts"), serde_json::to_value(&line).unwrap(), tol.clone());
    let t = Instant::now();
    let role = recommend_role_alerts(ctx, None, None);
    let t_role = t.elapsed();
    assert_parity!(&format!("{test}_role_alerts"), fx.engine("role_alerts"), serde_json::to_value(&role).unwrap(), tol.clone());

    // matchup (fm matchup --seed 7)
    let mut expected = fx.cli_output("matchup");
    if let Value::Object(o) = &mut expected {
        o.remove("meta");
    }
    let tape = fx.tape_path().map(|p| Tape::load(&p).expect("tape loads"));
    let have_tape = tape.is_some();
    if !have_tape && league == "fantrax" {
        // the Fantrax scoring periods (and so the matchup warnings) come from the HTTP tape
        return skip(&format!("{test}_matchup"), "no fantrax tape.json.gz (gitignored; parity/dump_fixtures.py)");
    }
    let prov = SnapshotProvider::new(&snap, tape, &data_dir);
    let scores = if have_tape { None } else { scores_from(&expected) };
    let t = Instant::now();
    let opts = MatchupOpts { seed: Some(7), scores, ..MatchupOpts::default() };
    let m = current_matchup(ctx, Some(&prov), Some(&values), opts, now, &memo).expect("matchup");
    let t_matchup = t.elapsed();
    eprintln!("{league}: win probability {:?} (Python {})", m.win_probability, expected["win_probability"]);
    // documented set-order exception (ARCHITECTURE 4.3): the Monte Carlo player order
    let mtol = tol
        .clone()
        .with_path_tol("$.win_probability", 0.0, 0.02)
        .with_normalize("$.advice[*]", r"\(\d+% to win\)", "(N% to win)");
    assert_parity!(&format!("{test}_matchup"), expected, m.to_json(), mtol);
    eprintln!(
        "{league}: injuries {t_inj:?}, role alerts {t_role:?}, matchup {t_matchup:?} (tape: {have_tape}; {} injuries, {} flags, {} line alerts, {} role alerts)",
        injuries.len(),
        flags.len(),
        line.len(),
        role.len()
    );
}

#[test]
fn stage_alerts_matchup_espn() {
    run_league("espn");
}

#[test]
fn stage_alerts_matchup_fantrax() {
    run_league("fantrax");
}
