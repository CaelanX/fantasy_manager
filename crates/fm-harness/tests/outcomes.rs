//! W5-archive-outcomes: ports of tests/test_harness_outcomes.py (synthetic ledgers) and
//! tests/test_archive_v2.py (archive files), plus the archive parity test: the files written by
//! `fm_engine::archive` for a fixture league equal the ones Python's `backtest.archive` writes
//! from the same inputs (`parity/dump_archive.py`, run through the venv python; skipped when
//! the python or the fixtures are absent).

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{Duration, NaiveDate, NaiveDateTime};
use fm_backtest::data::PlayerSeason;
use fm_core::models::{GainUnits, LeagueContext, Player, Reason, RecKind, Recommendation, ScoringConfig};
use fm_core::snapshot::Snapshot;
use fm_core::values::PlayerValue;
use fm_core::Map;
use fm_engine::archive as A;
use fm_engine::valuation::params;
use fm_harness::episode_match::store_lineup_lock;
use fm_harness::ledger::{Ledger, Row};
use fm_harness::outcomes::{
    grade_outcomes, predicted_pts, score_archive, scoring_config, season_end, store_scoring, GAMES_PER_DAY,
};
use serde_json::{json, Value};

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn days(start: NaiveDate, n: i64) -> Vec<NaiveDate> {
    (0..n).map(|i| start + Duration::days(i)).collect()
}

fn iso(x: NaiveDate) -> String {
    x.format("%Y-%m-%d").to_string()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn approx(v: &Value, x: f64) -> bool {
    v.as_f64().is_some_and(|a| close(a, x))
}

fn row(v: Value) -> Row {
    v.as_object().unwrap().clone()
}

fn sc() -> ScoringConfig {
    serde_json::from_value(json!({"kind": "points",
        "weights": {"G": 3.0, "A": 2.0, "SOG": 0.5, "W": 4.0, "SV": 0.2, "GA": -1.0}}))
    .unwrap()
}

fn nhl(c: &str) -> Option<i64> {
    match c {
        "a" => Some(1),
        "d" => Some(2),
        "s" => Some(3),
        "b" => Some(4),
        "c" => Some(5),
        "r1" => Some(11),
        "r2" => Some(12),
        _ => None,
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    led: Ledger,
}

fn led() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    store_scoring(&led, "espn", &sc()).unwrap();
    store_scoring(&led, "fantrax", &sc()).unwrap();
    Env { _tmp: tmp, led }
}

/// Mark `dates` pulled and store `stats` [(nhl_id, date, stats)].
fn pull(l: &Ledger, dates: &[NaiveDate], stats: &[(i64, NaiveDate, Value)]) {
    let rows: Vec<Row> = stats
        .iter()
        .map(|(n, dt, st)| {
            row(json!({"nhl_id": n, "game_date": iso(*dt), "gp": 1, "stats_json": st.to_string()}))
        })
        .collect();
    l.upsert("realized_daily", &rows, &["nhl_id", "game_date"]).unwrap();
    let pulls: Vec<Row> = dates.iter().map(|x| row(json!({"game_date": iso(*x), "n_players": 1}))).collect();
    l.upsert("realized_pulls", &pulls, &["game_date"]).unwrap();
}

#[derive(Default)]
struct Ep<'a> {
    last: Option<NaiveDate>,
    adds: &'a [&'a str],
    drops: &'a [&'a str],
    status: Option<&'a str>,
    acted_on: Option<NaiveDate>,
    gain: Option<f64>,
    units: Option<&'a str>,
    horizon: Option<i64>,
    league: Option<&'a str>,
    matched: Option<Value>,
    strength: Option<f64>,
}

fn episode(l: &Ledger, eid: &str, kind: &str, first: NaiveDate, e: Ep) {
    let league = e.league.unwrap_or("espn");
    l.upsert(
        "rec_episodes",
        &[row(json!({"episode_id": eid, "league": league, "rec_key": eid, "kind": kind, "title": eid,
            "first_seen": iso(first), "last_seen": iso(e.last.unwrap_or(first)), "n_days": 1,
            "predicted_gain": e.gain, "gain_units": e.units, "horizon_days": e.horizon,
            "strength": e.strength, "status": e.status.unwrap_or("expired"),
            "acted_on": e.acted_on.map(iso), "match_json": e.matched.map(|m| m.to_string())}))],
        &["episode_id"],
    )
    .unwrap();
    let mut ps = Vec::new();
    for (side, cs) in [("add", e.adds), ("drop", e.drops)] {
        for c in cs {
            ps.push(row(json!({"league": league, "as_of": iso(first), "rec_key": eid, "side": side, "cid": c,
                "name": c, "nhl_id": nhl(c)})));
        }
    }
    l.upsert("rec_players", &ps, &["league", "as_of", "rec_key", "side", "cid"]).unwrap();
}

fn proj(l: &Ledger, day: NaiveDate, cid: &str, extra: Value) {
    let mut r = row(json!({"league": "espn", "as_of": iso(day), "cid": cid, "name": cid, "nhl_id": nhl(cid),
        "positions": "C", "fpg": 2.0}));
    for (k, v) in extra.as_object().unwrap() {
        if k == "inputs" {
            r.insert("inputs_json".into(), json!(v.to_string()));
        } else {
            r.insert(k.clone(), v.clone());
        }
    }
    l.upsert("projections", &[r], &["league", "as_of", "cid"]).unwrap();
}

fn rows(l: &Ledger, filter: &[(&str, Value)]) -> Vec<Row> {
    let mut out = l.query("SELECT * FROM outcomes ORDER BY episode_id, decision_id, basis, window", &[]).unwrap();
    for r in out.iter_mut() {
        let det: Value = serde_json::from_str(r["detail_json"].as_str().unwrap()).unwrap();
        r.insert("detail".into(), det);
    }
    out.into_iter().filter(|r| filter.iter().all(|(k, v)| r.get(*k) == Some(v))).collect()
}

fn one(l: &Ledger, filter: &[(&str, Value)]) -> Row {
    let got = rows(l, filter);
    assert_eq!(got.len(), 1, "{filter:?} -> {got:?}");
    got.into_iter().next().unwrap()
}

fn gain(r: &Row) -> f64 {
    r["realized_gain"].as_f64().unwrap_or_else(|| panic!("no gain in {r:?}"))
}

#[test]
fn waiver_window_delta_with_known_stats() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 10, 5);
    episode(l, "w", "waiver", start, Ep { adds: &["a"], drops: &["d"], gain: Some(0.5), units: Some("season_fpg"), ..Default::default() });
    proj(l, start, "a", json!({"games_next7": 3}));
    pull(
        l,
        &days(d(2026, 10, 6), 40),
        &[
            (1, d(2026, 10, 6), json!({"G": 1})),
            (1, d(2026, 10, 10), json!({"A": 1})),
            (1, d(2026, 10, 20), json!({"G": 1, "SOG": 2})),
            (2, d(2026, 10, 7), json!({"SOG": 2})),
        ],
    );
    let s = grade_outcomes(l, "espn", d(2026, 11, 10)).unwrap();
    assert_eq!(s.scoring, "meta");
    assert_eq!(s.graded, 3);
    let w7 = one(l, &[("window", json!("7d"))]);
    assert_eq!(gain(&w7), 4.0);
    assert_eq!((&w7["hit"], &w7["complete"], &w7["partial"]), (&json!(1), &json!(1), &json!(0)));
    assert_eq!(w7["origin"], "ignored");
    assert_eq!(w7["basis"], "first_seen");
    assert_eq!(w7["kind"], "waiver");
    assert_eq!((&w7["window_start"], &w7["window_end"]), (&json!("2026-10-06"), &json!("2026-10-12")));
    assert!(approx(&w7["predicted_pts"], 0.5 * 3.0 / 7.0 * 7.0));
    assert_eq!(w7["detail"]["pts_in"], 5.0);
    assert_eq!(w7["detail"]["pts_out"], 1.0);
    assert_eq!(gain(&one(l, &[("window", json!("28d"))])), 8.0);
    let ros = one(l, &[("window", json!("ros"))]);
    assert_eq!(ros["complete"], 0);
    assert_eq!(ros["window_end"], iso(season_end(start)));
}

#[test]
fn incomplete_window_is_stored_partial_and_regraded() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 10, 5);
    episode(l, "w", "waiver", start, Ep { adds: &["a"], drops: &["d"], ..Default::default() });
    let pulled: Vec<NaiveDate> = days(d(2026, 10, 6), 28).into_iter().filter(|x| *x != d(2026, 10, 9)).collect();
    pull(l, &pulled, &[(1, d(2026, 10, 6), json!({"G": 1}))]);
    grade_outcomes(l, "espn", d(2026, 11, 10)).unwrap();
    assert_eq!(one(l, &[("window", json!("7d"))])["complete"], 0); // 10-09 never pulled
    assert_eq!(one(l, &[("window", json!("28d"))])["complete"], 0);
    // mid-window: graded so far, partial
    grade_outcomes(l, "espn", d(2026, 10, 9)).unwrap();
    let w7 = one(l, &[("window", json!("7d"))]);
    assert_eq!((&w7["complete"], &w7["partial"]), (&json!(0), &json!(1)));
    assert_eq!(w7["detail"]["graded_through"], "2026-10-08");
    assert_eq!(gain(&w7), 3.0);
    assert!(!rows(l, &[("window", json!("ros"))]).is_empty()); // started: stored
    pull(l, &[d(2026, 10, 9)], &[]);
    grade_outcomes(l, "espn", d(2026, 11, 10)).unwrap();
    assert_eq!(one(l, &[("window", json!("7d"))])["complete"], 1);
    assert_eq!(one(l, &[("window", json!("28d"))])["complete"], 1);
}

#[test]
fn window_not_started_is_not_stored() {
    let e = led();
    episode(&e.led, "w", "waiver", d(2026, 10, 5), Ep { adds: &["a"], ..Default::default() });
    assert_eq!(grade_outcomes(&e.led, "espn", d(2026, 10, 6)).unwrap().graded, 0); // first day is 10-06, not over yet
}

#[test]
fn grading_is_idempotent_and_drops_vanished_subjects() {
    let e = led();
    let l = &e.led;
    episode(l, "w", "waiver", d(2026, 10, 5), Ep { adds: &["a"], drops: &["d"], ..Default::default() });
    pull(l, &days(d(2026, 10, 6), 28), &[(1, d(2026, 10, 6), json!({"G": 1}))]);
    grade_outcomes(l, "espn", d(2026, 11, 10)).unwrap();
    let strip = |rs: Vec<Row>| -> Vec<Row> {
        rs.into_iter().map(|mut r| {
            r.shift_remove("graded_at");
            r
        }).collect()
    };
    let before = strip(rows(l, &[]));
    grade_outcomes(l, "espn", d(2026, 11, 10)).unwrap();
    assert_eq!(strip(rows(l, &[])), before);
    assert_eq!(l.count("outcomes", "", &[]).unwrap(), 3);
    l.execute("DELETE FROM rec_episodes", &[]).unwrap();
    grade_outcomes(l, "espn", d(2026, 11, 10)).unwrap();
    assert_eq!(l.count("outcomes", "", &[]).unwrap(), 0);
}

#[test]
fn espn_lineup_delta_only_on_shared_game_days() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 10, 5);
    episode(l, "l", "lineup", start, Ep { adds: &["a"], drops: &["d"], gain: Some(6.0), units: Some("week_pts"), horizon: Some(7), ..Default::default() });
    pull(
        l,
        &days(start, 7),
        &[
            (1, start, json!({"G": 1})),
            (1, d(2026, 10, 6), json!({"G": 2})),
            (1, d(2026, 10, 7), json!({"A": 1})),
            (2, start, json!({"A": 1})),
            (2, d(2026, 10, 7), json!({"SOG": 2})),
            (2, d(2026, 10, 8), json!({"G": 1})),
        ],
    );
    grade_outcomes(l, "espn", d(2026, 10, 20)).unwrap();
    let r = one(l, &[("kind", json!("lineup"))]);
    assert_eq!(r["window"], "7d");
    assert_eq!((&r["window_start"], &r["window_end"]), (&json!("2026-10-05"), &json!("2026-10-11")));
    assert_eq!(r["detail"]["shared_days"], 2);
    assert!(close(gain(&r), (3.0 - 2.0) + (2.0 - 1.0))); // 10-06 / 10-08: only one side played
    assert_eq!(r["predicted_pts"], 6.0);
    assert_eq!((&r["complete"], &r["hit"]), (&json!(1), &json!(1)));
    assert!(rows(l, &[("window", json!("28d"))]).is_empty()); // lineup recs: one weekly window
}

#[test]
fn lineup_without_shared_days_has_no_gain() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 10, 5);
    episode(l, "l", "lineup", start, Ep { adds: &["a"], drops: &["d"], ..Default::default() });
    pull(l, &days(start, 7), &[(1, start, json!({"G": 1})), (2, d(2026, 10, 6), json!({"G": 1}))]);
    grade_outcomes(l, "espn", d(2026, 10, 20)).unwrap();
    let r = one(l, &[("kind", json!("lineup"))]);
    assert!(r["realized_gain"].is_null() && r["hit"].is_null());
    assert_eq!(r["detail"]["shared_days"], 0);
}

#[test]
fn fantrax_lineup_graded_over_the_locked_week() {
    let e = led();
    let l = &e.led;
    let wed = d(2026, 10, 7);
    episode(l, "l", "lineup", wed, Ep { adds: &["a"], drops: &["d"], league: Some("fantrax"), gain: Some(3.0), units: Some("week_pts"), ..Default::default() });
    pull(
        l,
        &days(d(2026, 10, 6), 14),
        &[
            (1, d(2026, 10, 10), json!({"G": 5})),
            (1, d(2026, 10, 13), json!({"G": 1})),
            (2, d(2026, 10, 15), json!({"A": 1})),
            (2, d(2026, 10, 19), json!({"G": 3})),
        ],
    );
    grade_outcomes(l, "fantrax", d(2026, 10, 25)).unwrap();
    let r = one(l, &[("kind", json!("lineup"))]);
    assert_eq!((&r["window_start"], &r["window_end"]), (&json!("2026-10-12"), &json!("2026-10-18"))); // Monday lock to Sunday
    assert!(close(gain(&r), 3.0 - 2.0)); // 10-10 (pre-lock) and 10-19 excluded
    assert_eq!(r["detail"]["lock"], true);
    assert_eq!(r["complete"], 1);
}

#[test]
fn daily_lock_fantrax_lineup_graded_from_its_own_day_on_shared_days() {
    // Weights: G 3, A 2, SOG 0.5. "a" scores 3 on 10-08, 15 on 10-10 and 3 on 10-14; "d" scores 2
    // on 10-08, 3 on 10-11 and 1 on 10-12.
    let e = led();
    let l = &e.led;
    let wed = d(2026, 10, 7);
    episode(l, "l", "lineup", wed, Ep { adds: &["a"], drops: &["d"], league: Some("fantrax"), gain: Some(3.0), units: Some("week_pts"), ..Default::default() });
    pull(
        l,
        &days(d(2026, 10, 6), 14),
        &[
            (1, d(2026, 10, 8), json!({"G": 1})),
            (1, d(2026, 10, 10), json!({"G": 5})),
            (1, d(2026, 10, 14), json!({"G": 1})),
            (2, d(2026, 10, 8), json!({"A": 1})),
            (2, d(2026, 10, 11), json!({"G": 1})),
            (2, d(2026, 10, 12), json!({"SOG": 2})),
        ],
    );
    // no stored lock: Fantrax defaults to weekly, the week from Monday 10-12 counts whole: 3 - 1
    grade_outcomes(l, "fantrax", d(2026, 10, 25)).unwrap();
    let r = one(l, &[("kind", json!("lineup"))]);
    assert_eq!((&r["window_start"], &r["window_end"]), (&json!("2026-10-12"), &json!("2026-10-18")));
    assert!(close(gain(&r), 3.0 - 1.0));
    assert_eq!(r["detail"]["lock"], true);
    // daily lock: 10-07..10-13 from the rec's own day, and only 10-08 has both players: 3 - 2
    store_lineup_lock(l, "fantrax", "daily").unwrap();
    grade_outcomes(l, "fantrax", d(2026, 10, 25)).unwrap();
    let r = one(l, &[("kind", json!("lineup"))]);
    assert_eq!((&r["window_start"], &r["window_end"]), (&json!("2026-10-07"), &json!("2026-10-13")));
    assert_eq!(r["detail"]["shared_days"], 1);
    assert!(close(gain(&r), 3.0 - 2.0));
    assert_eq!(r["detail"]["lock"], false);
    assert_eq!((&r["complete"], &r["hit"]), (&json!(1), &json!(1)));
}

#[test]
fn flags_are_graded_directionally_against_l15() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 10, 5);
    let hot = json!({"last15": {"gp": 5, "stats": {"G": 5}}}); // L15 FPG 3.0
    proj(l, start, "s", json!({"inputs": hot}));
    proj(l, start, "b", json!({"inputs": hot}));
    episode(l, "sh", "sell_high", start, Ep { drops: &["s"], gain: Some(-0.8), units: Some("season_fpg"), horizon: Some(28), ..Default::default() });
    episode(l, "bl", "buy_low", start, Ep { adds: &["b"], gain: Some(0.5), units: Some("season_fpg"), horizon: Some(28), ..Default::default() });
    let window = days(d(2026, 10, 6), 28);
    let mut stats = Vec::new();
    for (i, dt) in window[..10].iter().enumerate() {
        stats.push((3, *dt, if i % 2 == 0 { json!({"G": 1}) } else { json!({"SOG": 1}) }));
        stats.push((4, *dt, json!({"G": 1, "A": 1})));
    }
    pull(l, &window, &stats);
    grade_outcomes(l, "espn", d(2026, 11, 10)).unwrap();
    let sh = one(l, &[("episode_id", json!("sh"))]);
    assert_eq!(sh["window"], "28d");
    assert_eq!(sh["gain_units"], "season_fpg");
    assert_eq!(sh["detail"]["l15_fpg"], 3.0);
    assert_eq!(sh["detail"]["gp"], 10);
    assert!(close(gain(&sh), (5.0 * 3.0 + 5.0 * 0.5) / 10.0 - 3.0));
    assert_eq!(sh["hit"], 1); // cooled off
    let bl = one(l, &[("episode_id", json!("bl"))]);
    assert!(close(gain(&bl), 5.0 - 3.0));
    assert_eq!(bl["hit"], 1); // bounced up
    assert!(bl["predicted_pts"].is_null());
}

#[test]
fn buy_low_miss_and_no_games_is_ungraded() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 10, 5);
    proj(l, start, "b", json!({"inputs": {"last15": {"gp": 5, "stats": {"A": 5}}}})); // 2.0 FPG
    proj(l, start, "s", json!({"inputs": {"last15": {"gp": 5, "stats": {"A": 5}}}}));
    episode(l, "bl", "buy_low", start, Ep { adds: &["b"], ..Default::default() });
    episode(l, "sh", "sell_high", start, Ep { drops: &["s"], ..Default::default() });
    pull(l, &days(d(2026, 10, 6), 28), &[(4, d(2026, 10, 6), json!({"SOG": 2}))]);
    grade_outcomes(l, "espn", d(2026, 11, 10)).unwrap();
    assert_eq!(one(l, &[("episode_id", json!("bl"))])["hit"], 0);
    let sh = one(l, &[("episode_id", json!("sh"))]);
    assert!(sh["realized_gain"].is_null() && sh["hit"].is_null()); // 0 GP in the window
}

#[test]
fn two_for_one_trade_is_adjusted_for_the_replacement() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 10, 5);
    proj(l, start, "r1", json!({"vorp": 0.05}));
    proj(l, start, "r2", json!({"vorp": -0.1}));
    proj(l, start, "a", json!({"vorp": 1.5}));
    episode(
        l,
        "t",
        "trade",
        start,
        Ep {
            adds: &["a", "b"],
            drops: &["c"],
            status: Some("followed"),
            acted_on: Some(start),
            gain: Some(0.4),
            units: Some("lineup_fpg"),
            matched: Some(json!({"got": ["a", "b"], "gave": ["c"]})),
            ..Default::default()
        },
    );
    let d1 = d(2026, 10, 6);
    pull(
        l,
        &days(d1, 28),
        &[
            (1, d1, json!({"G": 2})),
            (4, d1, json!({"A": 1})),
            (5, d1, json!({"G": 1})),
            (11, d1, json!({"A": 1})),
            (12, d1, json!({"SOG": 2})),
        ],
    );
    grade_outcomes(l, "espn", d(2026, 11, 10)).unwrap();
    let r = one(l, &[("episode_id", json!("t")), ("basis", json!("first_seen")), ("window", json!("7d"))]);
    let repl = (2.0 + 1.0) / 2.0; // the players closest to replacement level (|vorp| ~ 0)
    assert_eq!(r["detail"]["replacement_pts"], repl);
    assert!(close(gain(&r), 6.0 + 2.0 - 3.0 - repl));
    assert!((r["predicted_pts"].as_f64().unwrap() - 0.4 * GAMES_PER_DAY * 7.0).abs() < 1e-4);
    let acted = one(l, &[("episode_id", json!("t")), ("basis", json!("acted_on")), ("window", json!("7d"))]);
    assert_eq!(acted["origin"], "followed");
    assert_eq!(acted["realized_gain"], r["realized_gain"]);
    let ros = one(l, &[("episode_id", json!("t")), ("window", json!("ros")), ("basis", json!("first_seen"))]);
    assert_eq!(ros["detail"]["label"], "partial");
}

#[test]
fn injury_rec_is_the_waiver_adds_gain_and_ir_only_is_skipped() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 10, 5);
    episode(l, "i", "injury", start, Ep { adds: &["a"], gain: Some(4.0), units: Some("week_pts"), horizon: Some(7), ..Default::default() });
    episode(l, "ir", "injury", start + Duration::days(1), Ep::default()); // IR move only: nothing to grade
    pull(l, &days(d(2026, 10, 6), 28), &[(1, d(2026, 10, 7), json!({"G": 1}))]);
    grade_outcomes(l, "espn", d(2026, 11, 10)).unwrap();
    let r = one(l, &[("episode_id", json!("i")), ("window", json!("7d"))]);
    assert_eq!(gain(&r), 3.0);
    assert_eq!(r["predicted_pts"], 4.0);
    assert!(rows(l, &[("episode_id", json!("ir"))]).is_empty());
}

#[test]
fn user_only_move_has_the_models_view_and_alternative() {
    let e = led();
    let l = &e.led;
    let day = d(2026, 10, 8);
    proj(l, d(2026, 10, 5), "b", json!({"fpg": 1.2, "games_next7": 4}));
    proj(l, d(2026, 10, 5), "c", json!({"fpg": 1.5}));
    episode(l, "w", "waiver", d(2026, 10, 6), Ep { last: Some(d(2026, 10, 9)), adds: &["a"], drops: &["c"], gain: Some(0.9), units: Some("season_fpg"), strength: Some(7.0), ..Default::default() });
    episode(l, "w2", "waiver", d(2026, 10, 7), Ep { adds: &["d"], drops: &["c"], gain: Some(0.2), units: Some("season_fpg"), ..Default::default() });
    l.upsert(
        "decisions",
        &[row(json!({"decision_id": "tx:espn:g1", "league": "espn", "day": iso(day), "origin": "user_only",
            "kind": "waiver", "adds_json": "[\"b\"]", "drops_json": "[\"c\"]"}))],
        &["decision_id"],
    )
    .unwrap();
    pull(
        l,
        &days(d(2026, 10, 7), 40),
        &[(4, d(2026, 10, 9), json!({"A": 1})), (1, d(2026, 10, 10), json!({"G": 2})), (5, d(2026, 10, 12), json!({"SOG": 2}))],
    );
    grade_outcomes(l, "espn", d(2026, 11, 20)).unwrap();
    let r = one(l, &[("decision_id", json!("tx:espn:g1")), ("window", json!("7d"))]);
    assert_eq!(r["origin"], "user_only");
    assert_eq!(r["basis"], "decision");
    assert!(r["episode_id"].is_null());
    assert!(close(gain(&r), 2.0 - 1.0));
    assert!(approx(&r["detail"]["model_fpg_delta"], 1.2 - 1.5)); // the model disliked my move
    assert!(approx(&r["predicted_gain"], -0.3));
    assert!((r["predicted_pts"].as_f64().unwrap() - -1.2).abs() < 1e-4);
    let alt = &r["detail"]["alt"];
    assert_eq!(alt["episode_id"], "w");
    assert!(approx(&alt["gain"], 6.0 - 1.0)); // same window as my move
}

#[test]
fn dropped_player_without_an_id_is_matched_by_a_unique_name() {
    let e = led();
    let l = &e.led;
    let day = d(2026, 10, 8);
    // "zz" was dropped before any ESPN projection was archived: no id in this league
    l.upsert(
        "transactions",
        &[row(json!({"league": "espn", "tx_id": "t1", "action": "DROP", "cid": "zz", "team_id": "1", "ts": "2026-10-08T11:00:00",
            "day": iso(day), "player_name": "Zed Player", "is_me": 1}))],
        &["league", "tx_id", "action", "cid", "team_id"],
    )
    .unwrap();
    proj(l, day, "a", json!({}));
    // the other league knows him
    for (cid, name, id) in [("f1", "Zed Player", 77)] {
        l.upsert(
            "projections",
            &[row(json!({"league": "fantrax", "as_of": iso(day), "cid": cid, "name": name, "nhl_id": id, "fpg": 1.0}))],
            &["league", "as_of", "cid"],
        )
        .unwrap();
    }
    l.upsert(
        "decisions",
        &[row(json!({"decision_id": "tx:espn:t1", "league": "espn", "day": iso(day), "origin": "user_only",
            "kind": "waiver", "adds_json": "[\"a\"]", "drops_json": "[\"zz\"]"}))],
        &["decision_id"],
    )
    .unwrap();
    pull(l, &days(d(2026, 10, 9), 7), &[(1, d(2026, 10, 9), json!({"G": 1})), (77, d(2026, 10, 10), json!({"A": 1}))]);
    grade_outcomes(l, "espn", d(2026, 10, 20)).unwrap();
    let r = one(l, &[("decision_id", json!("tx:espn:t1")), ("window", json!("7d"))]);
    assert_eq!(r["detail"]["unmatched"], json!([]));
    assert_eq!(gain(&r), 3.0 - 2.0);
}

#[test]
fn scoring_from_archive_header_then_meta() {
    let tmp = tempfile::tempdir().unwrap();
    let arch = tmp.path().join("archive");
    std::fs::create_dir_all(&arch).unwrap();
    std::fs::write(
        arch.join("projections-fantrax-2026-09-28.json"),
        json!({"version": 2, "scoring": {"kind": "points", "weights": {"G": 4.0}, "goalie_weights": {"G": 20.0}},
               "players": []})
        .to_string(),
    )
    .unwrap();
    let l = Ledger::open(tmp.path()).unwrap();
    let (cfg, src) = scoring_config(&l, "fantrax").unwrap();
    let cfg = cfg.unwrap();
    assert_eq!(src, "archive");
    assert_eq!(cfg.weights.get("G"), Some(&4.0));
    assert_eq!(cfg.goalie_weights.get("G"), Some(&20.0));
    assert_eq!(scoring_config(&l, "fantrax").unwrap().1, "meta");
    assert_eq!(scoring_config(&l, "espn").unwrap().1, "preset");
    let (none_cfg, none_src) = scoring_config(&l, "yahoo").unwrap();
    assert!(none_cfg.is_none() && none_src == "none");
    assert!(grade_outcomes(&l, "yahoo", d(2026, 11, 1)).unwrap().note.is_some());
}

#[test]
fn predicted_pts_units() {
    let g = GAMES_PER_DAY;
    assert_eq!(predicted_pts(Some(7.0), Some("week_pts"), 28, g), Some(28.0));
    assert!(close(predicted_pts(Some(0.5), Some("season_fpg"), 7, 3.0 / 7.0).unwrap(), 1.5));
    assert_eq!(predicted_pts(Some(2.5), Some("day_pts"), 1, g), Some(2.5));
    assert_eq!(predicted_pts(None, Some("week_pts"), 7, g), None);
    assert_eq!(predicted_pts(Some(1.0), None, 7, g), None);
}

#[test]
fn default_games_rate_is_the_84_game_season() {
    // 1 lineup FPG over the whole 194-day 2026-27 season is 84 games, not 82 / 186 x 194 = 85.5
    assert!(close(predicted_pts(Some(1.0), Some("lineup_fpg"), 194, GAMES_PER_DAY).unwrap(), 84.0));
    // a week at the default rate: 7 x 84 / 194 = 3.031 games
    assert!(close(predicted_pts(Some(1.0), Some("lineup_fpg"), 7, GAMES_PER_DAY).unwrap(), 7.0 * 84.0 / 194.0));
}

#[test]
fn old_ledger_gets_the_new_outcome_columns() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let db = rusqlite::Connection::open(tmp.path().join("harness.db")).unwrap();
        db.execute(
            "CREATE TABLE outcomes (outcome_id TEXT PRIMARY KEY, league TEXT, episode_id TEXT, decision_id TEXT,\
             window TEXT NOT NULL, basis TEXT, predicted_gain REAL, realized_gain REAL, gain_units TEXT,\
             hit INTEGER, partial INTEGER, graded_at TEXT, detail_json TEXT)",
            [],
        )
        .unwrap();
    }
    let l = Ledger::open(tmp.path()).unwrap();
    let cols: Vec<String> = l
        .query("PRAGMA table_info(outcomes)", &[])
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    for c in ["kind", "origin", "complete", "window_start", "window_end", "predicted_pts"] {
        assert!(cols.contains(&c.to_string()), "missing {c}");
    }
    assert_eq!(l.query("SELECT value FROM meta WHERE key='schema_version'", &[]).unwrap()[0]["value"], "2");
}

#[test]
fn lineup_filling_an_empty_slot_counts_his_points() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 10, 5);
    episode(l, "l", "lineup", start, Ep { adds: &["a"], gain: Some(4.0), units: Some("week_pts"), ..Default::default() });
    episode(l, "b", "lineup", start, Ep { drops: &["d"], ..Default::default() }); // "Bench X (out)"
    pull(l, &days(start, 7), &[(1, start, json!({"G": 1})), (1, d(2026, 10, 8), json!({"A": 1}))]);
    grade_outcomes(l, "espn", d(2026, 10, 20)).unwrap();
    let r = one(l, &[("episode_id", json!("l"))]);
    assert_eq!(gain(&r), 5.0);
    assert_eq!(r["detail"]["shared_days"], 2);
    assert_eq!(r["hit"], 1);
    assert!(one(l, &[("episode_id", json!("b"))])["realized_gain"].is_null());
}

#[test]
fn win_probability_start_is_not_graded_as_a_miss() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 10, 5);
    // a last-day goalie start predicted at -0.57 pts (made for win probability): he scores 0
    episode(l, "l", "lineup", start, Ep { adds: &["a"], gain: Some(-0.57), units: Some("day_pts"), ..Default::default() });
    pull(l, &days(start, 7), &[(1, start, json!({"SOG": 0}))]);
    grade_outcomes(l, "espn", d(2026, 10, 20)).unwrap();
    let r = one(l, &[("episode_id", json!("l"))]);
    assert_eq!(gain(&r), 0.0);
    assert!(r["hit"].is_null());
    assert_eq!(r["predicted_pts"], -0.57);
}

#[test]
fn no_games_is_not_a_miss_and_unpulled_windows_wait() {
    let e = led();
    let l = &e.led;
    let start = d(2026, 9, 28); // preseason: no NHL games
    episode(l, "w", "waiver", start, Ep { adds: &["a"], drops: &["d"], ..Default::default() });
    episode(l, "l", "lineup", start, Ep { adds: &["a"], drops: &["d"], league: Some("fantrax"), ..Default::default() });
    assert_eq!(grade_outcomes(l, "espn", d(2026, 10, 1)).unwrap().graded, 0); // nothing pulled yet: not stored
    pull(l, &days(d(2026, 9, 28), 8), &[]);
    grade_outcomes(l, "espn", d(2026, 10, 10)).unwrap();
    grade_outcomes(l, "fantrax", d(2026, 10, 10)).unwrap();
    let w7 = one(l, &[("episode_id", json!("w")), ("window", json!("7d"))]);
    assert!(w7["complete"] == 1 && w7["realized_gain"].is_null() && w7["hit"].is_null());
    let lu = one(l, &[("episode_id", json!("l"))]);
    assert_eq!(lu["window_start"], "2026-09-28");
    assert!(lu["realized_gain"].is_null() && lu["hit"].is_null());
}

// --------------------------------------------------------------------------- archive (v2/v3)

fn line_json(split: &str, gp: i64, g: f64) -> Value {
    json!({"split": split, "gp": gp, "stats": {"G": g * gp as f64, "A": 0.0, "GP": gp}})
}

fn archive_ctx(as_of: NaiveDate) -> LeagueContext {
    let star = json!({"cid": "espn:1", "name": "Connor Test", "name_norm": "connor test",
        "ids": {"espn": "1", "nhl": "8478402"}, "team": "EDM", "positions": ["C"], "pct_owned": 99.0,
        "birth_date": "1997-01-13", "status": "dtd", "status_note": "Upper body",
        "lines": {"projected": line_json("projected", 80, 0.6), "prior": line_json("prior", 82, 0.55),
                  "prior2": line_json("prior2", 70, 0.5), "season": line_json("season", 6, 0.8),
                  "last7": line_json("last7", 3, 1.0), "last15": line_json("last15", 6, 0.8)}});
    let fa = json!({"cid": "espn:2", "name": "Free Agent", "name_norm": "free agent", "ids": {"espn": "2"},
        "team": "SJS", "positions": ["D"], "lines": {"projected": line_json("projected", 70, 0.1)}});
    serde_json::from_value(json!({
        "provider": "espn", "league_id": "1", "season": 2027, "name": "T",
        "scoring": {"kind": "points", "weights": {"G": 2.0}}, "roster_shape": {"C": 1, "D": 1},
        "teams": [{"team_id": "1", "name": "me", "owner_is_me": true,
                   "slots": [{"slot": "C", "player": star, "starting": true}]}],
        "free_agents": [fa], "matchup_period": 1, "as_of": iso(as_of)}))
    .unwrap()
}

fn now() -> NaiveDateTime {
    d(2026, 11, 4).and_hms_opt(10, 0, 5).unwrap()
}

fn pv(p: &Player, fpg: f64) -> PlayerValue {
    serde_json::from_value(json!({"player": p, "fpg": fpg, "fpg_season": fpg * 0.9, "fpg_week": fpg, "vorp": 0.5,
        "rates": {"G": 0.55}, "games_next7": 3, "offnight_next7": 1, "start_share": null}))
    .unwrap()
}

#[test]
fn v2_projection_archive_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = archive_ctx(d(2026, 10, 20));
    let mut values: Map<String, PlayerValue> = Map::new();
    for p in ctx.all_players() {
        values.insert(p.cid.clone(), pv(p, 1.2345678));
    }
    let (path, status) = A::archive_projections(&ctx, tmp.path(), Some(&values), None, now()).unwrap();
    assert_eq!(status, "created");
    let snap = A::load_snapshot(&path).unwrap();
    assert_eq!(snap["version"], A::ARCHIVE_VERSION);
    assert_eq!(snap["params_hash"], params::params_hash(None));
    assert!(snap["code_hash"].as_str().unwrap().starts_with("rust:"));
    let rec = snap["players"].as_array().unwrap().iter().find(|r| r["cid"] == "espn:1").unwrap();
    let inp = &rec["inputs"];
    assert_eq!(inp["season"]["gp"], 6);
    assert_eq!(inp["season"]["zeros"], true); // zero stats kept
    assert!(approx(&inp["season"]["stats"]["G"], 4.8) && inp["season"]["stats"]["GP"] == 6.0);
    assert!(inp["last7"]["gp"] == 3 && inp["last15"]["gp"] == 6 && inp.get("last30").is_none());
    assert_eq!(inp["history"]["gp"], 152);
    assert!(inp["history"]["rates"]["G"].as_f64().unwrap() > 0.0);
    assert_eq!(inp["projection"]["gp"], 80);
    assert!(inp["status"] == "dtd" && inp["status_note"] == "Upper body");
    assert_eq!(inp["birth_date"], "1997-01-13");
    assert!((inp["age"].as_f64().unwrap() - 29.7).abs() < 0.1);
    assert!(inp["pct_owned"] == 99.0 && inp["positions"] == json!(["C"]));
    for k in ["games_next7", "offnight_next7", "start_share"] {
        assert!(inp.get(k).is_some(), "{k}");
    }
    assert!((rec["fm"]["fpg"].as_f64().unwrap() - 1.2345678).abs() < 1e-4);
    assert!((rec["fm"]["fpg_season"].as_f64().unwrap() - 1.2345678 * 0.9).abs() < 1e-4);
    // same content -> unchanged (dedupe per day), different day -> a new file
    assert_eq!(A::archive_projections(&ctx, tmp.path(), Some(&values), None, now()).unwrap().1, "unchanged");
    let other = archive_ctx(d(2026, 10, 21));
    assert_eq!(A::archive_projections(&other, tmp.path(), Some(&values), None, now()).unwrap().1, "created");
    // changed content, same day -> updated
    let mut v2 = values.clone();
    v2.get_mut("espn:1").unwrap().fpg = 2.0;
    assert_eq!(A::archive_projections(&ctx, tmp.path(), Some(&v2), None, now()).unwrap().1, "updated");
}

fn waiver_rec(ctx: &LeagueContext) -> Recommendation {
    let mut rec = Recommendation::new(RecKind::Waiver, 0.7, "Add Free Agent");
    rec.add = ctx.free_agents.clone();
    rec.subjects = vec![ctx.teams[0].players()[0].clone()];
    rec.predicted_gain = Some(0.95);
    rec.gain_units = Some(GainUnits::SeasonFpg);
    rec.strength = Some(6.2);
    rec.reasons = vec![Reason::new("VORP_DELTA", "x").with_value(Some(0.95))];
    rec
}

#[test]
fn v2_recommendation_records_carry_gain_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = archive_ctx(d(2026, 10, 20));
    let (path, status) =
        A::archive_recommendations(&[waiver_rec(&ctx)], "espn", tmp.path(), d(2026, 10, 20), Some("1"), now()).unwrap();
    assert_eq!(status, "created");
    let snap = A::load_snapshot(&path).unwrap();
    let r = &snap["recommendations"][0];
    assert!(snap["version"].as_i64().unwrap() >= 2 && snap["code_hash"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(snap["params_hash"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(
        (&r["predicted_gain"], &r["gain_units"], &r["horizon_days"], &r["strength"]),
        (&json!(0.95), &json!("season_fpg"), &Value::Null, &json!(6.2))
    );
    assert_eq!(r["subjects"][0]["cid"], "espn:1");
    // indent=1 file, deduped by content
    assert!(std::fs::read_to_string(&path).unwrap().starts_with("{\n \"version\": 3,"));
    let again = A::archive_recommendations(&[waiver_rec(&ctx)], "espn", tmp.path(), d(2026, 10, 20), Some("1"), now()).unwrap();
    assert_eq!(again.1, "unchanged");
}

fn harness_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/harness/recs-espn-2026-09-28.json")
}

#[test]
fn v1_files_still_load_and_score() {
    let fixture = harness_fixture();
    if fixture.exists() {
        let v1 = A::load_snapshot(&fixture).unwrap();
        assert!(v1["version"] == 1 && v1["code_hash"].is_null());
        for r in v1["recommendations"].as_array().unwrap() {
            assert!(r["predicted_gain"].is_null() && r["subjects"] == json!([]));
        }
    }
    // a v1 projections file (no inputs, fm with fpg + rates only) still scores
    let tmp = tempfile::tempdir().unwrap();
    let arch = tmp.path().join("archive");
    std::fs::create_dir_all(&arch).unwrap();
    let snap = json!({"version": 1, "provider": "espn", "as_of": "2026-09-28",
        "scoring": {"kind": "points", "weights": {"G": 2.0}},
        "players": [{"cid": "espn:1", "name": "Connor Test", "nhl_id": 8478402,
                     "projected": {"split": "projected", "gp": 80, "stats": {"G": 40.0, "GP": 80}},
                     "fm": {"fpg": 1.0, "rates": {"G": 0.5}}}]});
    let f = arch.join("projections-espn-2026-09-28.json");
    std::fs::write(&f, snap.to_string()).unwrap();
    assert!(A::load_snapshot(&f).unwrap()["players"][0]["inputs"].is_null());
    let actuals = vec![PlayerSeason {
        player_id: 8478402,
        season: 20262027,
        name: "Connor Test".into(),
        group: "F".into(),
        position: Some("C".into()),
        gp: 80,
        stats: [("G".to_string(), 40.0)].into_iter().collect(),
        team: None,
        birth_date: None,
        age: None,
    }];
    let out = score_archive(&actuals, tmp.path(), &["espn"], "first", None, 20, None, None).unwrap();
    assert_eq!(out["espn"]["snapshot"], "projections-espn-2026-09-28.json");
    assert_eq!(out["espn"]["pools"]["skaters"]["fm"]["n"], 1);
    assert_eq!(A::pick_snapshot(tmp.path(), "espn", "2026-09-28", None, None).unwrap().unwrap(), f);
    assert!(A::pick_snapshot(tmp.path(), "fantrax", "first", None, None).unwrap().is_none());
}

#[test]
fn signal_fields_roundtrip() {
    let mut ctx = archive_ctx(d(2026, 10, 20));
    ctx.teams[0].slots[0].player.as_mut().unwrap().pp_share = Some(0.123456789);
    ctx.teams[0].slots[0].player.as_mut().unwrap().line = Some("f1".into());
    ctx.teams[0].slots[0].player.as_mut().unwrap().confirmed_start = Some(true);
    let recs = A::projection_records(&ctx, None, None);
    let inp = &recs.iter().find(|r| r["cid"] == "espn:1").unwrap()["inputs"];
    assert!(inp["pp_share"] == 0.12346 && inp["line"] == "f1" && inp["confirmed_start"] == true);
    assert!(inp.get("adp").is_none());
    let sig = A::signal_inputs(Some(inp));
    assert_eq!(sig["pp_share"], 0.12346);
    assert!(sig["adp"].is_null() && A::signal_inputs(None)["line"].is_null());
}

// --------------------------------------------------------------------------- archive parity

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

/// Python with `FM_LEGACY_ROOT` set for the parity/*.py dump scripts.
fn python() -> Option<Command> {
    let (root, py) = legacy()?;
    let mut c = Command::new(py);
    c.env("FM_LEGACY_ROOT", root);
    Some(c)
}

/// First difference between two JSON values (exact floats; ints equal the same float).
fn diff(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.as_f64() == y.as_f64() { None } else { Some(format!("{path}: {x} != {y}")) }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(format!("{path}: len {} != {}", x.len(), y.len()));
            }
            x.iter().zip(y).enumerate().find_map(|(i, (p, q))| diff(p, q, &format!("{path}[{i}]")))
        }
        (Value::Object(x), Value::Object(y)) => {
            let kx: Vec<&String> = x.keys().collect();
            let ky: Vec<&String> = y.keys().collect();
            if kx != ky {
                return Some(format!("{path}: keys {kx:?} != {ky:?}"));
            }
            x.iter().find_map(|(k, v)| diff(v, &y[k], &format!("{path}.{k}")))
        }
        _ => (a != b).then(|| format!("{path}: {a} != {b}")),
    }
}

/// Python builds `rates` from set unions (hash-random key order, ARCHITECTURE 4.3): sort those
/// objects' keys on both sides.
fn sort_rates(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(sort_rates),
        Value::Object(o) => {
            for (k, x) in o.iter_mut() {
                if k == "rates" {
                    if let Value::Object(r) = x {
                        let mut e: Vec<(String, Value)> = std::mem::take(r).into_iter().collect();
                        e.sort_by(|a, b| a.0.cmp(&b.0));
                        r.extend(e);
                    }
                } else {
                    sort_rates(x);
                }
            }
        }
        _ => {}
    }
}

fn mask(v: &mut Value) {
    sort_rates(v);
    if let Some(o) = v.as_object_mut() {
        for k in ["archived_at", "code_hash", "params_hash"] {
            o.insert(k.into(), Value::Null);
        }
    }
}

fn parity(league: &str) {
    let dir = fixtures_root().join(league);
    let Some(mut py) = python() else { return eprintln!("SKIP archive parity {league}: no legacy-python checkout (FM_LEGACY_ROOT)") };
    if !dir.join("snapshot.json").exists() || !dir.join("values.json").exists() {
        return eprintln!("SKIP archive parity {league}: fixtures missing");
    }
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/dump_archive.py");
    let py_out = tempfile::tempdir().unwrap();
    let res = py.arg(&script).arg(&dir).arg(py_out.path()).output().expect("run python");
    assert!(res.status.success(), "dump_archive.py failed: {}", String::from_utf8_lossy(&res.stderr));

    let snap = Snapshot::load(dir.join("snapshot.json")).unwrap();
    let ctx = snap.ctx;
    let raw: Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(dir.join("values.json")).unwrap()).unwrap();
    let by_cid: std::collections::HashMap<&str, &Player> = ctx.all_players().into_iter().map(|p| (p.cid.as_str(), p)).collect();
    let mut values: Map<String, PlayerValue> = Map::new();
    for (cid, v) in &raw {
        let mut v = v.clone();
        v["player"] = serde_json::to_value(by_cid[cid.as_str()]).unwrap();
        values.insert(cid.clone(), serde_json::from_value(v).unwrap());
    }
    let rs_out = tempfile::tempdir().unwrap();
    let (rs_proj, status) = A::archive_projections(&ctx, rs_out.path(), Some(&values), None, now()).unwrap();
    assert_eq!(status, "created");

    let day = iso(ctx.as_of);
    let py_proj = py_out.path().join(format!("archive/projections-{}-{}.json", ctx.provider, day));
    let (mut a, mut b): (Value, Value) = (
        serde_json::from_str(&std::fs::read_to_string(&py_proj).unwrap()).unwrap(),
        serde_json::from_str(&std::fs::read_to_string(&rs_proj).unwrap()).unwrap(),
    );
    mask(&mut a);
    mask(&mut b);
    if let Some(m) = diff(&a, &b, "$") {
        panic!("{league} projections differ from Python: {m}");
    }
    assert!(!b["players"].as_array().unwrap().is_empty());
    // byte-level: with the three varying header values equalized the compact text is identical
    let text = |p: &Path| {
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        mask(&mut v);
        fm_pycompat::pyjson::dumps(&v, &fm_pycompat::pyjson::DumpOpts::compact())
    };
    assert_eq!(text(&py_proj), text(&rs_proj));
    let raw_py = std::fs::read_to_string(&py_proj).unwrap();
    assert!(!raw_py.contains('\n') && raw_py.starts_with("{\"version\":3,\"provider\":"));

    // recommendations: the two synthetic recs of dump_archive.py
    let mine = ctx.my_team().unwrap().players();
    let fa = &ctx.free_agents;
    let mut r1 = Recommendation::new(RecKind::Waiver, 0.73456, "Add A, drop B");
    r1.add = vec![fa[0].clone()];
    r1.drop = vec![mine[0].clone()];
    r1.predicted_gain = Some(0.951234);
    r1.gain_units = Some(GainUnits::SeasonFpg);
    r1.strength = Some(6.2468);
    r1.reasons = vec![
        Reason::new("VORP_DELTA", "x \u{e9}").with_value(Some(0.95)).with_baseline(Some(0.1)),
        Reason::new("PLAIN", "y"),
    ];
    let mut r2 = Recommendation::new(RecKind::Trade, 1.5, "Trade");
    r2.add = vec![fa[1].clone()];
    r2.drop = vec![mine[1].clone()];
    r2.counterparty = Some("2".into());
    r2.subjects = vec![mine[2].clone()];
    r2.predicted_gain = Some(2.0);
    r2.gain_units = Some(GainUnits::WeekPts);
    r2.horizon_days = Some(7);
    r2.strength = Some(3.0);
    let (rs_recs, _) =
        A::archive_recommendations(&[r1, r2], &ctx.provider, rs_out.path(), ctx.as_of, Some(&ctx.league_id), now()).unwrap();
    let py_recs = py_out.path().join(format!("archive/recs-{}-{}.json", ctx.provider, day));
    let (mut a, mut b): (Value, Value) = (
        serde_json::from_str(&std::fs::read_to_string(&py_recs).unwrap()).unwrap(),
        serde_json::from_str(&std::fs::read_to_string(&rs_recs).unwrap()).unwrap(),
    );
    mask(&mut a);
    mask(&mut b);
    if let Some(m) = diff(&a, &b, "$") {
        panic!("{league} recs differ from Python: {m}");
    }
    // Windows Python writes \r\n in the indented file; the text is otherwise identical
    let norm = |p: &Path| std::fs::read_to_string(p).unwrap().replace("\r\n", "\n");
    let strip = |s: String| s.lines().filter(|l| !l.contains("archived_at") && !l.contains("_hash")).collect::<Vec<_>>().join("\n");
    assert_eq!(strip(norm(&py_recs)), strip(norm(&rs_recs)));
}

#[test]
fn archive_parity_espn() {
    parity("espn");
}

#[test]
fn archive_parity_fantrax() {
    parity("fantrax");
}

// --------------------------------------------------------------------------- grading parity

/// `grade_outcomes` on copies of the real harness.db (gitignored fixture): Python and Rust must
/// write the same outcome rows (`parity/dump_outcomes.py`).
#[test]
fn grade_outcomes_parity_on_harness_db() {
    let db = fixtures_root().join("_datadir/harness.db");
    let Some(mut py) = python() else { return eprintln!("SKIP grade parity: no legacy-python checkout (FM_LEGACY_ROOT)") };
    if !db.exists() {
        return eprintln!("SKIP grade parity: harness.db missing");
    }
    let as_of = d(2026, 11, 4);
    let (py_dir, rs_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for dir in [&py_dir, &rs_dir] {
        std::fs::copy(&db, dir.path().join("harness.db")).unwrap();
    }
    let out = py_dir.path().join("py_out.json");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/dump_outcomes.py");
    let res = py
        .arg(&script)
        .arg(py_dir.path())
        .arg(&out)
        .arg(iso(as_of))
        .output()
        .expect("run python");
    assert!(res.status.success(), "dump_outcomes.py failed: {}", String::from_utf8_lossy(&res.stderr));
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();

    let l = Ledger::open(rs_dir.path()).unwrap();
    let mut summaries = Vec::new();
    for lg in ["espn", "fantrax"] {
        summaries.push(grade_outcomes(&l, lg, as_of).unwrap().to_dict());
    }
    let mut got = l.query("SELECT * FROM outcomes ORDER BY outcome_id", &[]).unwrap();
    for r in got.iter_mut() {
        r.shift_remove("graded_at");
    }
    assert_eq!(Value::Array(summaries), expected["summaries"], "summaries");
    let exp_rows = expected["rows"].as_array().unwrap();
    assert!(exp_rows.len() > 50, "fixture has outcomes to grade");
    assert_eq!(got.len(), exp_rows.len());
    for (g, e) in got.iter().zip(exp_rows) {
        let mut g = Value::Object(g.clone());
        let mut e = e.clone();
        let (gd, ed) = (g["detail_json"].take(), e["detail_json"].take());
        if let Some(m) = diff(&g, &e, "$") {
            panic!("outcome {} differs: {m}", e["outcome_id"]);
        }
        let (gd, ed): (Value, Value) =
            (serde_json::from_str(gd.as_str().unwrap()).unwrap(), serde_json::from_str(ed.as_str().unwrap()).unwrap());
        if let Some(m) = diff(&gd, &ed, "detail") {
            panic!("outcome {} detail differs: {m}", e["outcome_id"]);
        }
    }
}
