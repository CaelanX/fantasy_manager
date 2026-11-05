//! `fm harness review` (move review + free-agent ranking) and the daily catch-up of missed NHL
//! days, on synthetic ledgers.

use chrono::{Duration, NaiveDate};
use fm_core::models::ScoringConfig;
use fm_harness::ledger::{Ledger, Row};
use fm_harness::outcomes::{grade_outcomes, store_scoring};
use fm_harness::realized::{backfill_start, unpulled_days, BACKFILL_MAX_DAYS};
use fm_harness::review::{fa_ranking, move_review, TOP_K};
use serde_json::{json, Value};

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn iso(x: NaiveDate) -> String {
    x.format("%Y-%m-%d").to_string()
}

fn row(v: Value) -> Row {
    v.as_object().unwrap().clone()
}

fn ledger() -> (tempfile::TempDir, Ledger) {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    let sc: ScoringConfig = serde_json::from_value(json!({"kind": "points", "weights": {"G": 3.0, "A": 2.0}})).unwrap();
    store_scoring(&led, "espn", &sc).unwrap();
    (tmp, led)
}

/// Mark `n` days from `start` pulled and store `(nhl_id, day, goals)` games.
fn pull(l: &Ledger, start: NaiveDate, n: i64, games: &[(i64, NaiveDate, f64)]) {
    let rows: Vec<Row> = games
        .iter()
        .map(|(id, day, g)| row(json!({"nhl_id": id, "game_date": iso(*day), "gp": 1, "stats_json": json!({"G": g}).to_string()})))
        .collect();
    l.upsert("realized_daily", &rows, &["nhl_id", "game_date"]).unwrap();
    let pulls: Vec<Row> = (0..n).map(|i| row(json!({"game_date": iso(start + Duration::days(i)), "n_players": 1}))).collect();
    l.upsert("realized_pulls", &pulls, &["game_date"]).unwrap();
}

fn proj(l: &Ledger, day: NaiveDate, cid: &str, nhl_id: i64, fpg: f64, pct_owned: f64) {
    l.upsert(
        "projections",
        &[row(json!({"league": "espn", "as_of": iso(day), "cid": cid, "name": cid, "nhl_id": nhl_id, "fpg": fpg, "pct_owned": pct_owned}))],
        &["league", "as_of", "cid"],
    )
    .unwrap();
}

#[test]
fn waiver_move_is_marked_so_far_with_a_pro_rated_prediction() {
    let (_t, l) = ledger();
    let day = d(2026, 10, 5);
    l.upsert(
        "rec_episodes",
        &[row(json!({"episode_id": "w", "league": "espn", "rec_key": "w", "kind": "waiver", "title": "Add A, drop D",
            "first_seen": iso(day), "last_seen": iso(day), "n_days": 1, "predicted_gain": 0.5, "gain_units": "season_fpg",
            "status": "expired"}))],
        &["episode_id"],
    )
    .unwrap();
    let ps: Vec<Row> = [("add", "a", 1), ("drop", "dd", 2)]
        .iter()
        .map(|(side, c, n)| row(json!({"league": "espn", "as_of": iso(day), "rec_key": "w", "side": side, "cid": c, "nhl_id": n})))
        .collect();
    l.upsert("rec_players", &ps, &["league", "as_of", "rec_key", "side", "cid"]).unwrap();
    proj(&l, day, "a", 1, 2.0, 10.0);
    // 14 of the 28 days played: A scores 2 goals (6 pts), D one (3 pts)
    pull(&l, day + Duration::days(1), 14, &[(1, d(2026, 10, 6), 2.0), (2, d(2026, 10, 8), 1.0)]);
    grade_outcomes(&l, "espn", d(2026, 10, 20)).unwrap();

    let rv = move_review(&l, "espn", 10).unwrap();
    assert_eq!(rv["n_moves"], 1);
    let m = &rv["moves"][0];
    assert_eq!(m["window"], "28d");
    assert_eq!(m["group"], "not_followed");
    assert_eq!((&m["days_graded"], &m["window_days"]), (&json!(14), &json!(28)));
    assert_eq!(m["realized"], 3.0);
    assert_eq!(m["verdict"], "ahead");
    assert_eq!(m["early"], false);
    // predicted over the full window pro-rated to the 14 graded days
    let full = m["predicted_pts"].as_f64().unwrap();
    assert!((m["predicted_so_far"].as_f64().unwrap() - full / 2.0).abs() < 0.011, "{m}");
    assert_eq!(rv["total"]["ahead"], 1);
    assert!(rv["total"]["final_rate"].is_null());
}

#[test]
fn own_move_gets_a_title_from_the_players() {
    let (_t, l) = ledger();
    let day = d(2026, 10, 5);
    l.upsert(
        "decisions",
        &[row(json!({"decision_id": "m1", "league": "espn", "day": iso(day), "origin": "user_only", "kind": "waiver",
            "adds_json": "[\"a\"]", "drops_json": "[\"dd\"]"}))],
        &["decision_id"],
    )
    .unwrap();
    proj(&l, day, "a", 1, 2.0, 10.0);
    l.upsert("projections", &[row(json!({"league": "espn", "as_of": iso(day), "cid": "dd", "name": "Dee", "nhl_id": 2, "fpg": 1.0}))], &["league", "as_of", "cid"])
        .unwrap();
    pull(&l, day + Duration::days(1), 7, &[(1, d(2026, 10, 6), 1.0)]);
    grade_outcomes(&l, "espn", d(2026, 10, 13)).unwrap();
    let rv = move_review(&l, "espn", 10).unwrap();
    let m = &rv["moves"][0];
    assert_eq!(m["title"], "Add a; drop Dee");
    assert_eq!(m["group"], "own");
    assert_eq!(m["early"], false);
}

#[test]
fn free_agent_ranking_compares_top_k_with_the_market() {
    let (_t, l) = ledger();
    let day = d(2026, 10, 5);
    let n = TOP_K as i64 * 2;
    let mut games = Vec::new();
    for i in 0..n {
        let id = 100 + i;
        // the model ranks by i, the market by -i; players with a higher i score more
        proj(&l, day, &format!("p{i}"), id, i as f64, (n - i) as f64);
        games.push((id, day + Duration::days(1), i as f64));
    }
    // one rostered player that would top both lists
    proj(&l, day, "star", 999, 99.0, 99.0);
    games.push((999, day + Duration::days(1), 50.0));
    l.upsert(
        "lineup_days",
        &[row(json!({"league": "espn", "team_id": "1", "day": iso(day - Duration::days(1)), "cid": "star"}))],
        &["league", "team_id", "day", "cid"],
    )
    .unwrap();
    pull(&l, day + Duration::days(1), 7, &games);

    let rk = fa_ranking(&l, "espn", day + Duration::days(8)).unwrap();
    let w7 = &rk["windows"][0];
    assert_eq!((&w7["days"], &w7["evaluated"]), (&json!(7), &json!(1)));
    // goals x 3 pts: model's top 10 are i = 19..10, the market's i = 0..9
    let top = |lo: i64| (lo..lo + TOP_K as i64).map(|i| i as f64 * 3.0).sum::<f64>() / TOP_K as f64;
    assert_eq!(w7["model"].as_f64().unwrap(), top(TOP_K as i64));
    assert_eq!(w7["market"].as_f64().unwrap(), top(0));
    assert_eq!(w7["best"].as_f64().unwrap(), top(TOP_K as i64));
    assert_eq!(w7["beat_market"], 1);
    assert_eq!(w7["rho_model"].as_f64().unwrap(), 1.0);
    assert_eq!(w7["early"], true);
    // the 14 and 28 day windows have not been played yet
    assert_eq!(rk["windows"][1]["evaluated"], 0);
    assert_eq!(rk["windows"][1]["pending"], 1);
    assert_eq!(rk["days_without_rosters"], 0);
}

#[test]
fn ranking_skips_days_without_rosters() {
    let (_t, l) = ledger();
    let day = d(2026, 10, 5);
    for i in 0..TOP_K as i64 {
        proj(&l, day, &format!("p{i}"), 100 + i, i as f64, 1.0);
    }
    pull(&l, day + Duration::days(1), 7, &[]);
    let rk = fa_ranking(&l, "espn", day + Duration::days(8)).unwrap();
    assert_eq!(rk["days_without_rosters"], 1);
    assert_eq!(rk["windows"][0]["evaluated"], 0);
}

#[test]
fn catch_up_finds_missed_days_from_the_first_archived_day() {
    let (_t, l) = ledger();
    let through = d(2026, 11, 4);
    assert_eq!(backfill_start(&l, through).unwrap(), None);
    proj(&l, d(2026, 10, 16), "a", 1, 1.0, 1.0);
    assert_eq!(backfill_start(&l, through).unwrap(), Some(d(2026, 10, 16)));
    pull(&l, d(2026, 10, 16), 3, &[]);
    let missing = unpulled_days(&l, "realized_pulls", d(2026, 10, 16), through).unwrap();
    assert_eq!(missing.first(), Some(&d(2026, 10, 19)));
    assert_eq!(missing.last(), Some(&through));
    assert_eq!(missing.len(), 17);
    // never further back than the cap
    proj(&l, d(2026, 6, 1), "a", 1, 1.0, 1.0);
    assert_eq!(backfill_start(&l, through).unwrap(), Some(through - Duration::days(BACKFILL_MAX_DAYS)));
}
