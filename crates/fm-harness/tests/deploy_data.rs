//! Ports of tests/test_deployment.py (ledger pull, summaries, goalie counts), the data parts of
//! tests/test_backtest.py, plus params_store / ingest behaviour tests and Python-vs-Rust parity runs
//! on copies of the real harness.db (skipped when FM_PARITY_FIXTURES or a legacy-python checkout (FM_LEGACY_ROOT) is absent).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{Duration, NaiveDate};
use fm_backtest::data as D;
use fm_core::Map;
use fm_harness::deployment::{
    b2b_second_night_rate, deployment_summaries, deployment_summary, goalie_start_counts, pull_deployment,
    summarize_rows, team_goalie_counts, DeploymentSource, Prior, Schedule,
};
use fm_harness::ingest::{group_days, ingest_archive, roll_episodes};
use fm_harness::ledger::{Ledger, Row, DEPLOYMENT_TABLES, TABLES};
use fm_harness::params_store::{ParamsStore, PACKAGED};
use fm_http::{FetchJson, ParamValue, Params};
use fm_sources::nhl::{
    NhlClient, NhlGame, NhlGoalieGame, NhlSkaterPpGame, NhlSkaterToiGame, NhlTeamPpGame, NhlWeekSchedule,
};
use serde_json::{json, Value};

const SEASON: i64 = 20252026;

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn day() -> NaiveDate {
    d(2026, 4, 1)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fix(name: &str) -> Value {
    let p = repo_root().join("tests/fixtures/nhl").join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}

fn param<'a>(params: &'a Params, k: &str) -> Option<&'a str> {
    params.iter().find(|(key, _)| key == k).and_then(|(_, v)| match v {
        ParamValue::Str(s) => Some(s.as_str()),
        _ => None,
    })
}

/// Serves the 2026-04-01 COL-VAN fixtures for that date, empty reports for any other window.
fn fake_fetch(fail: &'static [&'static str]) -> Box<FetchJson<'static>> {
    Box::new(move |url, params, _ttl| {
        let path = url.split("/stats/rest/en/").last().unwrap_or("").to_string();
        if fail.contains(&path.as_str()) {
            return Err(anyhow::anyhow!("boom"));
        }
        let is_game = param(params, "isGame").unwrap_or("");
        let file = match (path.as_str(), is_game) {
            ("skater/timeonice", "true") => "deploy_toi_games_2026-04-01.json",
            ("skater/powerplay", "true") => "deploy_pp_games_2026-04-01.json",
            ("goalie/summary", "true") => "deploy_goalie_games_2026-04-01.json",
            ("team/powerplay", "true") => "deploy_team_pp_games_2026-04-01.json",
            ("skater/timeonice", "false") => "deploy_season_timeonice_20252026.json",
            ("skater/powerplay", "false") => "deploy_season_powerplay_20252026.json",
            _ => return Err(anyhow::anyhow!("unexpected {url}")),
        };
        let cay = param(params, "cayenneExp").unwrap_or("");
        if is_game == "true" && !cay.contains("\"2026-04-01\"") {
            return Ok(json!({"data": [], "total": 0}));
        }
        if is_game == "false" && !cay.contains("seasonId=20252026") {
            return Ok(json!({"data": [], "total": 0}));
        }
        Ok(fix(file))
    })
}

fn client(fail: &'static [&'static str]) -> NhlClient {
    NhlClient::new(fake_fetch(fail), Some(SEASON), day())
}

fn col_van_schedule() -> Schedule {
    let mut s = Schedule::new();
    s.insert("COL".into(), vec![day() - Duration::days(1), day()]);
    s.insert("VAN".into(), vec![day()]);
    s
}

fn f(r: &Row, k: &str) -> f64 {
    r[k].as_f64().unwrap_or_else(|| panic!("{k} is not a number: {:?}", r[k]))
}

fn approx(a: f64, b: f64, tol: f64) {
    assert!((a - b).abs() <= tol, "{a} != {b} (tol {tol})");
}

// --------------------------------------------------------------------------- ledger pull

#[test]
fn pull_deployment_stores_rows_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    assert!(DEPLOYMENT_TABLES.iter().all(|t| !TABLES.contains(t)));
    let c = client(&[]);
    let sched = col_van_schedule();
    let out = pull_deployment(&led, &c, day(), None, None, Some(&sched), true).unwrap();
    assert_eq!(out["skaters"], 12);
    assert_eq!(out["goalies"], 3);
    assert_eq!(out["team_pp_source"], "report");
    let again = pull_deployment(&led, &c, day(), None, None, Some(&sched), true).unwrap();
    assert_eq!(again["skaters"], 12);
    assert_eq!(led.count("deployment_daily", "", &[]).unwrap(), 12);
    assert_eq!(led.count("goalie_starts", "", &[]).unwrap(), 3);
    let mack = &led.query("SELECT * FROM deployment_daily WHERE nhl_id=8477492", &[]).unwrap()[0];
    approx(f(mack, "toi"), 24.35, 1e-9);
    approx(f(mack, "pp_toi"), 320.0 / 60.0, 1e-3);
    approx(f(mack, "team_pp_toi"), 6.0, 1e-9);
    approx(f(mack, "pp_share"), 0.8889, 1e-4);
    assert_eq!(mack["shifts"], 21);
    assert_eq!(mack["team"], "COL");
    assert_eq!(mack["opponent"], "VAN");
    let rossi = &led.query("SELECT * FROM deployment_daily WHERE nhl_id=8482079", &[]).unwrap()[0];
    approx(f(rossi, "pp_share"), 201.0 / 254.0, 1e-4);
    let kiv = &led.query("SELECT * FROM deployment_daily WHERE nhl_id=8481641", &[]).unwrap()[0];
    assert_eq!(f(kiv, "pp_share"), 0.0);
    let g: BTreeMap<i64, Row> = led
        .query("SELECT * FROM goalie_starts", &[])
        .unwrap()
        .into_iter()
        .map(|r| (r["nhl_id"].as_i64().unwrap(), r))
        .collect();
    assert_eq!(g[&8478406]["started"], 1);
    assert_eq!(g[&8478406]["back_to_back"], 1); // COL played the day before
    assert_eq!(g[&8475809]["started"], 0);
    assert_eq!(g[&8480947]["back_to_back"], 0);
    approx(f(&g[&8478406], "toi"), 2121.0 / 60.0, 1e-3);
    assert_eq!(led.query("SELECT * FROM deployment_pulls", &[]).unwrap()[0]["n_skaters"], 12);
}

#[test]
fn pull_without_games_is_a_clean_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    let mut sched = Schedule::new();
    sched.insert("EDM".into(), vec![d(2026, 9, 29)]);
    let out = pull_deployment(&led, &client(&[]), d(2026, 9, 29), None, None, Some(&sched), true).unwrap();
    assert_eq!(
        out,
        json!({"start": "2026-09-29", "end": "2026-09-29", "skaters": 0, "goalies": 0, "team_pp_source": null})
    );
    assert_eq!(led.count("deployment_daily", "", &[]).unwrap(), 0);
    let p = led.query("SELECT n_skaters, n_goalies FROM deployment_pulls", &[]).unwrap();
    assert_eq!(p.len(), 1);
    assert_eq!((p[0]["n_skaters"].as_i64(), p[0]["n_goalies"].as_i64()), (Some(0), Some(0)));
}

#[test]
fn record_empty_false_leaves_unfinished_dates_unmarked() {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    let mut sched = Schedule::new();
    sched.insert("EDM".into(), vec![d(2026, 9, 29)]);
    let out = pull_deployment(&led, &client(&[]), d(2026, 9, 29), None, None, Some(&sched), false).unwrap();
    assert_eq!(out["not_final"], json!(["2026-09-29"]));
    assert_eq!(led.count("deployment_pulls", "", &[]).unwrap(), 0);
    // a schedule with no game that day is final
    let out = pull_deployment(&led, &client(&[]), d(2026, 9, 30), None, None, Some(&{
        let mut s = Schedule::new();
        s.insert("EDM".into(), vec![d(2026, 9, 29)]);
        s
    }), false)
    .unwrap();
    assert_eq!(out["not_final"], json!([]));
    assert_eq!(led.count("deployment_pulls", "", &[]).unwrap(), 1);
}

#[test]
fn pull_derives_team_pp_when_team_report_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    let c = client(&["team/powerplay"]);
    let mut sched = Schedule::new();
    sched.insert("COL".into(), vec![day()]);
    sched.insert("VAN".into(), vec![day()]);
    let out = pull_deployment(&led, &c, day(), None, None, Some(&sched), true).unwrap();
    assert_eq!(out["team_pp_source"], "derived");
    // the fixture keeps 6 of each team's skaters, so sum/5 covers part of the team's PP time
    let want = (320.0 + 278.0 + 276.0 + 272.0 + 168.0 + 0.0) / 5.0 / 60.0;
    for r in led.query("SELECT * FROM deployment_daily WHERE team='COL'", &[]).unwrap() {
        approx(f(&r, "team_pp_toi"), want, 1e-3);
    }
}

/// Synthetic per-game rows keyed by date (back-to-back and summary tests).
#[derive(Default)]
struct FakeNhl {
    toi: BTreeMap<NaiveDate, Vec<NhlSkaterToiGame>>,
    goalies: BTreeMap<NaiveDate, Vec<NhlGoalieGame>>,
    weeks: Option<BTreeMap<NaiveDate, Vec<(String, String)>>>,
    week_calls: Cell<usize>,
}

impl DeploymentSource for FakeNhl {
    fn skater_toi_games(&self, _s: i64, a: NaiveDate, b: NaiveDate) -> anyhow::Result<Vec<NhlSkaterToiGame>> {
        Ok(self.toi.range(a..=b).flat_map(|(_, v)| v.clone()).collect())
    }
    fn skater_pp_games(&self, _s: i64, _a: NaiveDate, _b: NaiveDate) -> anyhow::Result<Vec<NhlSkaterPpGame>> {
        Ok(vec![])
    }
    fn team_pp_toi_games(&self, _s: i64, _a: NaiveDate, _b: NaiveDate) -> anyhow::Result<Vec<NhlTeamPpGame>> {
        Ok(vec![])
    }
    fn goalie_games(&self, _s: i64, a: NaiveDate, b: NaiveDate) -> anyhow::Result<Vec<NhlGoalieGame>> {
        Ok(self.goalies.range(a..=b).flat_map(|(_, v)| v.clone()).collect())
    }
    fn week_schedule(&self, dy: NaiveDate) -> anyhow::Result<NhlWeekSchedule> {
        self.week_calls.set(self.week_calls.get() + 1);
        let Some(w) = &self.weeks else { anyhow::bail!("offline") };
        let games = w
            .get(&dy)
            .map(|v| {
                v.iter()
                    .enumerate()
                    .map(|(i, (a, h))| NhlGame {
                        game_id: i as i64,
                        season: None,
                        game_type: 2,
                        date: dy,
                        start_time_utc: None,
                        away: a.clone(),
                        home: h.clone(),
                        state: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut days = Map::new();
        days.insert(dy, games);
        Ok(NhlWeekSchedule { days, ..Default::default() })
    }
}

fn toi_row(pid: i64, dy: NaiveDate, team: &str, toi: f64, pp: f64) -> NhlSkaterToiGame {
    NhlSkaterToiGame {
        player_id: pid,
        game_id: None,
        date: dy,
        team: Some(team.into()),
        opponent: None,
        home: None,
        name: None,
        position: None,
        toi: Some(toi * 60.0),
        ev_toi: Some((toi - pp) * 60.0),
        pp_toi: Some(pp * 60.0),
        sh_toi: Some(0.0),
        ot_toi: None,
        shifts: Some(20),
    }
}

fn goalie_row(pid: i64, dy: NaiveDate, team: &str) -> NhlGoalieGame {
    NhlGoalieGame {
        player_id: pid,
        game_id: None,
        date: dy,
        team: Some(team.into()),
        opponent: None,
        home: None,
        name: None,
        started: true,
        sa: 30.0,
        sv: 27.0,
        ga: 3.0,
        toi: Some(3600.0),
        decision: None,
    }
}

#[test]
fn back_to_back_from_ledger_and_week_schedule() {
    let (d1, d2) = (d(2026, 10, 10), d(2026, 10, 11));
    let mut fake = FakeNhl::default();
    fake.goalies.insert(d1, vec![goalie_row(1, d1, "AAA")]);
    fake.goalies.insert(d2, vec![goalie_row(1, d2, "AAA"), goalie_row(2, d2, "BBB")]);
    let mut weeks = BTreeMap::new();
    weeks.insert(d1 - Duration::days(1), vec![("AAA".to_string(), "CCC".to_string())]);
    fake.weeks = Some(weeks);
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    pull_deployment(&led, &fake, d1, None, None, None, true).unwrap(); // previous day from the week schedule
    let r = led.query("SELECT back_to_back FROM goalie_starts WHERE game_date='2026-10-10'", &[]).unwrap();
    assert_eq!(r[0]["back_to_back"], 1);
    let calls = fake.week_calls.get();
    pull_deployment(&led, &fake, d2, None, None, None, true).unwrap(); // previous day in the ledger: no request
    assert_eq!(fake.week_calls.get(), calls);
    let b2b: BTreeMap<i64, i64> = led
        .query("SELECT nhl_id, back_to_back FROM goalie_starts WHERE game_date='2026-10-11'", &[])
        .unwrap()
        .iter()
        .map(|r| (r["nhl_id"].as_i64().unwrap(), r["back_to_back"].as_i64().unwrap()))
        .collect();
    assert_eq!(b2b, BTreeMap::from([(1, 1), (2, 0)]));
    // a range pull decides within the range from its own rows; offline + unknown -> None
    let tmp2 = tempfile::tempdir().unwrap();
    let led2 = Ledger::open(tmp2.path()).unwrap();
    let off = FakeNhl { goalies: fake.goalies.clone(), ..Default::default() };
    pull_deployment(&led2, &off, d1, Some(d2), None, None, true).unwrap();
    let rows: BTreeMap<(i64, String), Value> = led2
        .query("SELECT * FROM goalie_starts", &[])
        .unwrap()
        .iter()
        .map(|r| ((r["nhl_id"].as_i64().unwrap(), r["game_date"].as_str().unwrap().to_string()), r["back_to_back"].clone()))
        .collect();
    assert_eq!(rows[&(1, "2026-10-10".to_string())], Value::Null);
    assert_eq!(rows[&(1, "2026-10-11".to_string())], 1);
    assert_eq!(rows[&(2, "2026-10-11".to_string())], 0);
}

// --------------------------------------------------------------------------- summaries

fn rows_of(tois: &[f64], pps: Option<&[f64]>, team_pp: f64) -> Vec<Row> {
    let start = d(2026, 10, 8);
    tois.iter()
        .enumerate()
        .map(|(i, t)| {
            let p = pps.map(|p| p[i]).unwrap_or(1.0);
            let mut r = Row::new();
            r.insert("game_date".into(), (start + Duration::days(2 * i as i64)).format("%Y-%m-%d").to_string().into());
            r.insert("toi".into(), (*t).into());
            r.insert("pp_toi".into(), p.into());
            r.insert("team_pp_toi".into(), team_pp.into());
            r
        })
        .collect()
}

#[test]
fn summary_math_with_season_baseline() {
    let rows = rows_of(
        &[15., 15., 16., 14., 15., 18., 18., 17., 19., 18.],
        Some(&[0.4, 0.4, 0.4, 0.4, 0.4, 2.4, 2.4, 2.4, 2.4, 2.4]),
        4.0,
    );
    let s = summarize_rows(&rows, 5, None);
    assert_eq!(s["gp"], 10);
    assert_eq!(s["baseline_source"], "season");
    approx(f(&s, "toi_avg"), 18.0, 1e-9);
    approx(f(&s, "baseline_toi"), 15.0, 1e-9);
    approx(f(&s, "toi_trend"), 3.0, 1e-9);
    approx(f(&s, "pp_share_avg"), 0.6, 1e-9);
    approx(f(&s, "baseline_pp_share"), 0.1, 1e-9);
    approx(f(&s, "pp_share_trend"), 0.5, 1e-9);
    approx(f(&s, "season_toi"), 16.5, 1e-9);
    approx(f(&s, "season_pp_share"), 0.35, 1e-9);
    approx(f(&s, "pp_toi_avg"), 2.4, 1e-9);
    approx(f(&s, "season_pp_toi"), 1.4, 1e-9);
    assert_eq!(s.keys().map(|k| k.as_str()).collect::<Vec<_>>(), fm_harness::deployment::SUMMARY_KEYS);
}

#[test]
fn summary_prior_baseline_and_short_windows() {
    let mut rows = rows_of(&[20., 20., 21., 19., 20.], Some(&[2.0; 5]), 4.0);
    let prior: Prior = Map::from_iter([("toi".to_string(), Some(17.0)), ("pp_share".to_string(), Some(0.2))]);
    let s = summarize_rows(&rows, 5, Some(&prior));
    assert_eq!(s["baseline_source"], "prior");
    approx(f(&s, "toi_trend"), 3.0, 1e-9);
    approx(f(&s, "pp_share_trend"), 0.3, 1e-9);
    assert_eq!(summarize_rows(&rows, 5, None)["toi_trend"], Value::Null); // no baseline at all
    let short = summarize_rows(&rows[..3], 5, Some(&prior));
    assert_eq!(short["gp"], 3);
    assert_eq!(short["toi_trend"], Value::Null);
    approx(f(&short, "toi_avg"), 20.333, 1e-3);
    let empty = summarize_rows(&[], 5, None);
    assert_eq!(empty["gp"], 0);
    assert_eq!(empty["toi_avg"], Value::Null);
    // a game without a team power play does not dilute the PP share
    rows[0].insert("team_pp_toi".into(), 0.0.into());
    approx(f(&summarize_rows(&rows, 5, None), "pp_share_avg"), 0.5, 1e-9);
}

#[test]
fn deployment_summary_from_ledger() {
    let start = d(2026, 10, 7);
    let mut fake = FakeNhl::default();
    for i in 0..12 {
        let dy = start + Duration::days(i);
        let own = if i < 7 { 0.5 } else { 1.5 };
        let mut v = vec![
            toi_row(7, dy, "AAA", if i < 7 { 14.0 } else { 17.0 }, own),
            toi_row(99, dy, "AAA", 15.0, 2.0 - own),
        ];
        v.extend((0..4).map(|k| toi_row(100 + k, dy, "AAA", 15.0, 2.0)));
        fake.toi.insert(dy, v);
    }
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    let mut sched = Schedule::new();
    sched.insert("AAA".into(), vec![]);
    // an empty schedule behaves like none (falsy)
    pull_deployment(&led, &fake, start, Some(start + Duration::days(11)), None, Some(&sched), true).unwrap();
    let s = deployment_summary(&led, 7, start + Duration::days(11), 5, None).unwrap();
    assert_eq!(s["gp"], 12);
    approx(f(&s, "toi_avg"), 17.0, 1e-9);
    approx(f(&s, "baseline_toi"), 14.0, 1e-9);
    approx(f(&s, "toi_trend"), 3.0, 1e-9);
    approx(f(&s, "pp_share_avg"), 0.75, 1e-9);
    approx(f(&s, "baseline_pp_share"), 0.25, 1e-9);
    approx(f(&s, "pp_share_trend"), 0.5, 1e-9);
    approx(f(&s, "season_pp_share"), (7.0 * 0.5 + 5.0 * 1.5) / 24.0, 1e-3);
    // as_of cuts the season; last season's games are outside this season
    assert_eq!(deployment_summary(&led, 7, start + Duration::days(3), 5, None).unwrap()["gp"], 4);
    assert_eq!(deployment_summary(&led, 7, d(2027, 9, 15), 5, None).unwrap()["gp"], 0);
    let many = deployment_summaries(&led, &[7, 100, 999], start + Duration::days(11), 10, None).unwrap();
    assert_eq!(many.keys().copied().collect::<Vec<_>>(), vec![7, 100]);
    assert_eq!(many[&7]["last_n"], 10);
}

#[test]
fn b2b_rate_and_start_counts_from_ledger() {
    let start = d(2026, 10, 7);
    let mut fake = FakeNhl::default();
    let mut team_dates = Vec::new();
    // AAA plays pairs of consecutive days (6 back-to-backs); #1 (id 1) starts every first night and
    // 2 of the 6 second nights; #2 (id 2) the other 4
    for k in 0..6 {
        let d1 = start + Duration::days(3 * k);
        let d2 = d1 + Duration::days(1);
        team_dates.extend([d1, d2]);
        fake.goalies.insert(d1, vec![goalie_row(1, d1, "AAA")]);
        fake.goalies.insert(d2, vec![goalie_row(if k < 2 { 1 } else { 2 }, d2, "AAA")]);
    }
    let as_of = *team_dates.last().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    let mut sched = Schedule::new();
    sched.insert("AAA".into(), team_dates.clone());
    pull_deployment(&led, &fake, start, Some(as_of), None, Some(&sched), true).unwrap();
    let counts = team_goalie_counts(&led, as_of).unwrap();
    let a = &counts["AAA"];
    assert_eq!(a.games, 12);
    assert_eq!(a.starts, Map::from_iter([(1, 8), (2, 4)]));
    assert_eq!((a.starter, a.b2b, a.b2b_starter), (Some(1), 6, 2));
    assert_eq!(goalie_start_counts(&led, 1, as_of, None).unwrap(), (8, 12));
    // #2 first appeared in the team's 6th game: a late arrival, measured against the 7 games
    // from then on, not the 5 before he was there
    assert_eq!(a.since_first, Map::from_iter([(1, 12), (2, 7)]));
    assert_eq!((a.games_for(1), a.games_for(2), a.games_for(3)), (12, 7, 12));
    assert_eq!(goalie_start_counts(&led, 2, as_of, None).unwrap(), (4, 7));
    // one game earlier and he is a backup waiting for a start: the whole season counts
    let early = fm_harness::deployment::TeamGoalieCounts { since_first: Map::from_iter([(2, 8)]), ..a.clone() };
    assert_eq!(early.games_for(2), 12);
    // games missed through injury are left out. The team plays days 0,1 3,4 6,7 9,10 12,13 15,16;
    // #1 played days 0,1,3,4,6,9,12,15 and sat the second nights on days 7, 10, 13 and 16.
    let day = |n: i64| (start + Duration::days(n)).format("%Y-%m-%d").to_string();
    assert_eq!(a.games_available(1, &[]), 12);
    // recorded out on day 7: that game is not charged to him. He played again on day 9, so the
    // record is stale from then on and day 10 is a healthy night off.
    assert_eq!(a.games_available(1, &[(day(7), true), (day(11), false)]), 11);
    assert_eq!(a.games_available(1, &[(day(0), true)]), 12, "a record he has played since excuses nothing");
    // recorded out on day 14, after the injury: day 13, since his last game, is excused too
    assert_eq!(a.games_available(1, &[(day(14), true)]), 11);
    // still out at the end: everything after his last game on day 15
    assert_eq!(a.games_available(1, &[(day(16), true)]), 11);
    // #2 (a late arrival: days 7,9,10,12,13,15,16 count) played 7,10,13,16; out on day 11
    assert_eq!(a.games_available(2, &[(day(11), true)]), 6, "day 12 only");
    assert_eq!(a.games_available(2, &[(day(0), true), (day(7), false)]), 7);
    assert_eq!(goalie_start_counts(&led, 3, as_of, Some("AAA")).unwrap(), (0, 12));
    assert_eq!(goalie_start_counts(&led, 3, as_of, None).unwrap(), (0, 0));
    let (p, n) = b2b_second_night_rate(&led, "AAA", as_of, None).unwrap();
    assert_eq!(n, 6);
    approx(p, (2.0 + 5.0 * 0.35) / (6.0 + 5.0), 1e-12);
    // fewer than 5 back-to-backs: the 0.35 default
    assert_eq!(b2b_second_night_rate(&led, "AAA", team_dates[7], None).unwrap(), (0.35, 4));
    assert_eq!(b2b_second_night_rate(&led, "ZZZ", as_of, None).unwrap(), (0.35, 0));
}

// --------------------------------------------------------------------------- backtest data

#[test]
fn seasons_and_ages() {
    let (s1, s2, s3, s4) = (20202021, 20212022, 20222023, 20232024);
    assert_eq!(D::parse_seasons(Some("20212022-20232024"), &[]).unwrap(), vec![s2, s3, s4]);
    assert_eq!(D::parse_seasons(Some("2021,2023"), &[]).unwrap(), vec![s2, s4]);
    assert_eq!(D::parse_seasons(None, &[s1]).unwrap(), vec![s1]);
    assert_eq!(D::season_label(s2), "2021-22");
    approx(D::age_on_oct1(Some("1997-01-13"), 20152016).unwrap(), 18.72, 0.01);
    assert_eq!(D::position_group(Some("LW")), "F");
    assert_eq!(D::position_group(Some("D")), "D");
    assert_eq!(D::position_group(Some("G")), "G");
    assert_eq!(D::checkpoint_dates(s2), vec![d(2021, 11, 1), d(2021, 12, 1), d(2022, 1, 1)]);
}

fn ps(pid: i64, season: i64, g_pg: f64, gp: i64, group: &str, birth: Option<&str>, age: Option<f64>) -> D::PlayerSeason {
    let pos = match group {
        "F" => "C",
        "D" => "D",
        _ => "G",
    };
    D::PlayerSeason {
        player_id: pid,
        season,
        name: format!("P{pid}"),
        group: group.into(),
        position: Some(pos.into()),
        gp,
        stats: Map::from_iter([("G".to_string(), g_pg * gp as f64), ("HIT".to_string(), 2.0 * gp as f64)]),
        team: None,
        birth_date: birth.map(String::from),
        age,
    }
}

#[test]
fn player_season_rows_roundtrip_and_player() {
    let (s1, s2, s3) = (20202021, 20212022, 20222023);
    let r = ps(1, s2, 0.5, 40, "F", Some("2000-06-01"), Some(21.3));
    assert_eq!(r.per_game(), Map::from_iter([("G".to_string(), 0.5), ("HIT".to_string(), 2.0)]));
    let p = r.to_player(fm_core::models::Split::Prior).unwrap();
    assert_eq!(p.lines["prior"].gp, 40);
    assert_eq!(p.positions, vec!["C".to_string()]);
    assert_eq!(p.nhl_id(), Some(1));
    let t = D::SeasonTable::new(vec![r, ps(1, s1, 0.4, 80, "F", None, None), ps(2, s2, 0.1, 80, "D", None, None)]);
    let tmp = tempfile::tempdir().unwrap();
    D::save_table(&t, tmp.path()).unwrap();
    let t2 = D::load_table(tmp.path()).unwrap();
    assert_eq!(t2.seasons(), vec![s1, s2]);
    assert_eq!(t2.history(1, s3).iter().map(|h| h.season).collect::<Vec<_>>(), vec![s1, s2]);
    assert_eq!(t2.age(1, s3), D::age_on_oct1(Some("2000-06-01"), s3));
}

fn pages() -> Value {
    let s2 = 20212022;
    json!({
        "skater/summary": [
            {"playerId": 1, "skaterFullName": "A B", "positionCode": "L", "seasonId": s2, "gamesPlayed": 10,
             "goals": 5, "assists": 3, "points": 8, "ppPoints": 2, "ppGoals": 1, "shPoints": 0, "shGoals": 0,
             "shots": 30, "penaltyMinutes": 4, "teamAbbrevs": "EDM"},
            {"playerId": 3, "skaterFullName": "No Realtime", "positionCode": "D", "seasonId": s2,
             "gamesPlayed": 2, "goals": 0, "assists": 1, "points": 1, "shots": 2}],
        "skater/realtime": [{"playerId": 1, "hits": 12, "blockedShots": 7, "emptyNetGoals": 1}],
        "skater/faceoffwins": [{"playerId": 1, "totalFaceoffWins": 11, "totalFaceoffLosses": 9}],
        "skater/bios": [{"playerId": 1, "birthDate": "2000-10-02"}],
        "goalie/summary": [{"playerId": 2, "goalieFullName": "G K", "seasonId": s2, "gamesPlayed": 5,
                            "gamesStarted": 5, "wins": 3, "losses": 1, "otLosses": 1, "goalsAgainst": 10,
                            "shotsAgainst": 150, "saves": 140, "shutouts": 1, "assists": 1}],
        "goalie/bios": [{"playerId": 2, "birthDate": "1995-01-01"}],
    })
}

fn pages_client(pages: Value) -> NhlClient {
    NhlClient::new(
        Box::new(move |url, _params, _ttl| {
            let segs: Vec<&str> = url.rsplitn(3, '/').collect();
            let key = format!("{}/{}", segs[1], segs[0]);
            let rows = pages.get(&key).cloned().unwrap_or(json!([]));
            let n = rows.as_array().map(|a| a.len()).unwrap_or(0);
            Ok(json!({"data": rows, "total": n}))
        }),
        None,
        d(2026, 4, 1),
    )
}

#[test]
fn fetch_season_parses_reports() {
    let rows: BTreeMap<i64, D::PlayerSeason> =
        D::fetch_season(&pages_client(pages()), 20212022).unwrap().into_iter().map(|r| (r.player_id, r)).collect();
    let a = &rows[&1];
    assert_eq!((a.group.as_str(), a.position.as_deref(), a.gp), ("F", Some("LW"), 10));
    assert_eq!(a.stats["HIT"], 12.0);
    assert_eq!(a.stats["BLK"], 7.0);
    assert_eq!(a.stats["PPA"], 1.0);
    assert_eq!(a.stats["ENG"], 1.0);
    approx(a.age.unwrap(), 20.99, 0.01);
    assert_eq!(rows[&3].stats["HIT"], 0.0); // no realtime row -> zeros
    assert_eq!(rows[&3].group, "D");
    let g = &rows[&2];
    assert_eq!((g.group.as_str(), g.stats["SV"], g.stats["W"], g.stats["A"]), ("G", 140.0, 3.0, 1.0));
}

#[test]
fn counting_fetcher_ttl() {
    let today = d(2026, 9, 28);
    let p = |cay: &str| -> Params { vec![("cayenneExp".to_string(), ParamValue::Str(cay.to_string()))] };
    assert_eq!(D::ttl_for(today, &p("seasonId=20232024 and gameTypeId=2")), D::TTL_HISTORICAL);
    assert_eq!(D::ttl_for(today, &p("seasonId=20262027 and gameTypeId=2")), D::TTL_CURRENT);
}

#[test]
fn windows_roundtrip() {
    let mut w = D::Windows::new();
    let line = D::WindowLine { gp: 3, stats: Map::from_iter([("G".to_string(), 2.0)]) };
    let mut wins = Map::new();
    wins.insert("to_date".to_string(), Map::from_iter([(5i64, line)]));
    let mut days = Map::new();
    days.insert("2021-11-01".to_string(), wins);
    w.insert(20212022, days);
    let tmp = tempfile::tempdir().unwrap();
    D::save_windows(&w, tmp.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("backtest/windows.json")).unwrap(),
        r#"{"20212022":{"2021-11-01":{"to_date":{"5":{"gp":3,"stats":{"G":2.0}}}}}}"#
    );
    assert_eq!(D::load_windows(tmp.path()).unwrap(), w);
}

// --------------------------------------------------------------------------- params store

#[test]
fn params_store_propose_apply_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    let pdir = tmp.path().join("params");
    let store = ParamsStore::new(Some(&pdir), Some(&led), None);
    assert_eq!(store.active_name(), PACKAGED);
    assert!(store.rollback(None, "t", None).is_err());
    let v1 = store.propose(&json!({"k_inseason": {"skater": 22.0}}), &["k_inseason.skater".to_string()], None, None, None, "refit", None).unwrap();
    assert_eq!(v1["version"], "v0001");
    assert_eq!(v1["parent"], PACKAGED);
    assert_eq!(v1["status"], "proposed");
    let text = std::fs::read_to_string(pdir.join("v0001.json")).unwrap();
    assert!(text.ends_with("}\n") && text.contains("\n  \"changelog\": ["));
    store.apply("v0001", "manual", None, Some(d(2026, 11, 4))).unwrap();
    assert_eq!(store.active_name(), "v0001");
    let v2 = store.propose(&json!({"k_inseason": {"skater": 18.0}}), &[], None, None, None, "refit", None).unwrap();
    assert_eq!(v2["parent"], "v0001");
    store.apply("v0002", "manual", None, Some(d(2026, 11, 5))).unwrap();
    assert_eq!(store.get("v0001").unwrap()["status"], "shadow");
    assert_eq!(store.get("v0002").unwrap()["shadow"]["version"], "v0001");
    assert_eq!(store.get("v0002").unwrap()["shadow"]["since"], "2026-11-05");
    let out = store.rollback(None, "manual", None).unwrap();
    assert_eq!((out["from"].as_str(), out["to"].as_str()), (Some("v0002"), Some("v0001")));
    assert_eq!(store.get("v0002").unwrap()["status"], "rolled_back");
    assert_eq!(store.get("v0001").unwrap()["status"], "active");
    assert!(store.rollback(Some("v0001"), "manual", None).is_err());
    let out = store.rollback(Some(PACKAGED), "manual", None).unwrap();
    assert_eq!(out["to"], PACKAGED);
    assert_eq!(out["hash"].as_str().unwrap(), fm_harness::params_store::merged_hash(&json!({})));
    assert_eq!(store.active_name(), PACKAGED);
    assert_eq!(store.get("v0001").unwrap()["status"], "rolled_back");
    assert_eq!(store.next_version(), "v0003");
    // the ledger mirror follows the files
    assert_eq!(led.count("param_versions", "", &[]).unwrap(), 2);
    led.execute("DELETE FROM param_versions", &[]).unwrap();
    assert_eq!(store.sync_ledger(None).unwrap(), 2);
    assert_eq!(led.count("param_versions", "", &[]).unwrap(), 2);
}

// --------------------------------------------------------------------------- ingest

fn write_archive(data_dir: &Path) {
    let a = data_dir.join("archive");
    std::fs::create_dir_all(&a).unwrap();
    let w = |name: &str, v: Value| std::fs::write(a.join(name), serde_json::to_string_pretty(&v).unwrap()).unwrap();
    let player = |cid: &str, name: &str, nhl: Value, fpg: f64| {
        json!({"cid": cid, "name": name, "nhl_id": nhl, "team": "EDM", "positions": ["C", "LW"], "status": "ACTIVE",
               "pct_owned": 55.5, "fm": {"fpg": fpg, "fpg_season": 0.1 + fpg, "fpg_week": 1e-05, "proj_week": 3.0,
               "vorp": -0.5, "rates": {"G": 0.3, "A": 0.4}}, "projected": {"G": 0.25},
               "inputs": {"games_next7": 3, "x": [1, 2.5]}})
    };
    w("projections-espn-2026-10-01.json", json!({"version": 2, "as_of": "2026-10-01", "params_hash": "ph", "code_hash": "ch",
        "position_means": {"F": 0.31, "D": 0.2},
        "players": [player("espn:1", "Connor McDavid", json!(8478402), 1.5), player("espn:2", "Zed", json!("8480000"), 0.5),
                    {"cid": "", "name": "no cid"}]}));
    let rec = |kind: &str, title: &str, add: Vec<&str>, drop: Vec<&str>, cp: Value, subjects: Value| {
        let mk = |ids: Vec<&str>| ids.iter().map(|c| json!({"cid": c, "name": c, "nhl_id": 5})).collect::<Vec<_>>();
        let mut v = json!({"kind": kind, "title": title, "score": 1.5, "strength": "high", "predicted_gain": 0.75,
            "gain_units": "fpts", "horizon_days": 7, "counterparty": cp, "reasons": [{"text": "r", "weight": 0.5}],
            "add": mk(add), "drop": mk(drop)});
        if !subjects.is_null() {
            v["subjects"] = subjects;
        }
        v
    };
    let waiver = rec("waiver", "Add X", vec!["espn:1"], vec!["espn:9"], Value::Null, json!([]));
    let inj = rec("injury", "Move Q to IR", vec![], vec![], Value::Null, json!([{"cid": "espn:2", "name": "Zed", "nhl_id": 8480000}]));
    let trade = rec("trade", "Trade", vec!["espn:3"], vec!["espn:4"], json!("Opp A"), json!([]));
    for (day, recs) in [
        ("2026-10-01", vec![waiver.clone(), waiver.clone(), inj.clone()]),
        ("2026-10-02", vec![waiver.clone(), trade.clone()]),
        ("2026-10-06", vec![waiver.clone()]),
    ] {
        w(&format!("recs-espn-{day}.json"), json!({"version": 2, "as_of": day, "params_hash": "ph", "code_hash": "ch", "recommendations": recs}));
    }
    // v1 files: IR target recovered from the title and the same day's projections
    w("projections-fantrax-2026-09-30.json", json!({"version": 1, "as_of": "2026-09-30",
        "players": [{"cid": "fantrax:7", "name": "Foo Bar", "nhl_id": 77, "team": "CGY", "positions": ["D"], "status": "ACTIVE"}]}));
    let mut v1 = rec("injury", "Move Foo Bar to IR", vec![], vec![], Value::Null, Value::Null);
    v1.as_object_mut().unwrap().remove("reasons");
    let mut v1b = rec("waiver", "Baz now OUT (was DTD)", vec![], vec![], Value::Null, Value::Null);
    v1b.as_object_mut().unwrap().remove("counterparty");
    w("recs-fantrax-2026-09-30.json", json!({"version": 1, "as_of": "2026-09-30", "recommendations": [v1, v1b]}));
    std::fs::write(a.join("notes.json"), "{}").unwrap();
    std::fs::write(a.join("recs-espn-bad.json"), "{}").unwrap();
}

#[test]
fn ingest_archive_is_idempotent_and_rolls_episodes() {
    let tmp = tempfile::tempdir().unwrap();
    write_archive(tmp.path());
    let led = Ledger::open(tmp.path()).unwrap();
    let st = ingest_archive(&led, tmp.path(), None, false).unwrap();
    assert_eq!((st.files_seen, st.files_ingested), (6, 6), "{:?}", st);
    assert!(st.errors.is_empty());
    assert_eq!(st.projections, 3);
    assert_eq!(st.recs, 2 + 2 + 1 + 2); // espn d1 (dup dropped), d2, d6; fantrax 2
    // espn waiver: days 1,2 (one episode), 6 (gap > 2: second); injury, trade one each; fantrax two
    assert_eq!(st.episodes, 2 + 1 + 1 + 2);
    let again = ingest_archive(&led, tmp.path(), None, false).unwrap();
    assert_eq!((again.files_seen, again.files_ingested), (6, 0));
    assert_eq!(again.episodes, st.episodes);
    // v1 subject recovered from the title + projections; unknown name -> name:<x>
    let subs = led.query("SELECT cid FROM rec_players WHERE league='fantrax' AND side='subject' ORDER BY cid", &[]).unwrap();
    assert_eq!(subs.iter().map(|r| r["cid"].as_str().unwrap()).collect::<Vec<_>>(), vec!["fantrax:7", "name:Baz"]);
    let means = led.query("SELECT value FROM meta WHERE key='means:espn:2026-10-01'", &[]).unwrap();
    assert_eq!(means[0]["value"], r#"{"D": 0.2, "F": 0.31}"#);
    assert_eq!(roll_episodes(&led, None).unwrap() as i64, st.episodes);
    let days = [d(2026, 10, 1), d(2026, 10, 2), d(2026, 10, 5), d(2026, 10, 9)];
    assert_eq!(group_days(days, 2).len(), 3);
}

// --------------------------------------------------------------------------- python parity

/// A `legacy-python` checkout (`FM_LEGACY_ROOT`, default `../fm-legacy` beside the repo) and its
/// python (`FM_PYTHON`, default the checkout's `.venv`). None: the Python parity checks skip.
fn legacy() -> Option<(PathBuf, PathBuf)> {
    let root = std::env::var_os("FM_LEGACY_ROOT").map(PathBuf::from).unwrap_or_else(|| repo_root().join("../fm-legacy"));
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

fn fixtures_root() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var("FM_PARITY_FIXTURES").ok()?);
    root.join("_datadir").join("harness.db").exists().then_some(root)
}

fn run_py(script: &str, args: &[&Path]) -> Value {
    let out = Command::new(python().unwrap())
        .args(["-I", "-c", script])
        .arg(legacy().unwrap().0)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("bad python output ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn strip(rows: Vec<Row>, drop: &[&str]) -> Vec<Value> {
    rows.into_iter()
        .map(|mut r| {
            for k in drop {
                r.remove(*k);
            }
            Value::Object(r)
        })
        .collect()
}

/// `{team: counts}` in the JSON shape Python's `team_goalie_counts` dumps to.
fn counts_json(c: &Map<String, fm_harness::deployment::TeamGoalieCounts>) -> Value {
    let mut m = serde_json::Map::new();
    for (t, d) in c {
        let starts: serde_json::Map<String, Value> = d.starts.iter().map(|(k, v)| (k.to_string(), (*v).into())).collect();
        m.insert(t.clone(), json!({"games": d.games, "starts": starts, "b2b": d.b2b, "b2b_starter": d.b2b_starter, "starter": d.starter}));
    }
    Value::Object(m)
}

const PY_SUMMARIES: &str = r#"
import sys, json, datetime
sys.path.insert(0, sys.argv[1])
from pathlib import Path
from fantasy_manager.harness.ledger import Ledger
from fantasy_manager.harness.deployment import deployment_summaries, team_goalie_counts, b2b_second_night_rate, goalie_start_counts
led = Ledger(Path(sys.argv[2]))
out = {}
for as_of in sys.argv[3:]:
    day = datetime.date.fromisoformat(as_of)
    ids = [r["nhl_id"] for r in led.query("SELECT DISTINCT nhl_id FROM deployment_daily")]
    sums = deployment_summaries(led, ids, day, 5)
    counts = team_goalie_counts(led, day)
    out[as_of] = {
        "summaries": {str(k): v for k, v in sums.items()},
        "counts": json.loads(json.dumps(counts)),
        "b2b": {t: list(b2b_second_night_rate(led, t, day, counts=counts)) for t in counts},
        "starts": {str(r["nhl_id"]): list(goalie_start_counts(led, r["nhl_id"], day)) for r in led.query("SELECT DISTINCT nhl_id FROM goalie_starts")},
    }
print(json.dumps(out))
"#;

#[test]
fn summaries_and_goalie_counts_on_the_real_harness_db_equal_python() {
    let (Some(root), Some(_)) = (fixtures_root(), python()) else {
        eprintln!("skipped: needs FM_PARITY_FIXTURES/_datadir/harness.db and a legacy-python checkout (FM_LEGACY_ROOT)");
        return;
    };
    let src = root.join("_datadir").join("harness.db");
    let (rust_dir, py_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::copy(&src, rust_dir.path().join("harness.db")).unwrap();
    std::fs::copy(&src, py_dir.path().join("harness.db")).unwrap();
    let dates = ["2026-10-20", "2026-11-04", "2026-12-01"];
    let args: Vec<PathBuf> = dates.iter().map(PathBuf::from).collect();
    let mut a: Vec<&Path> = vec![py_dir.path()];
    a.extend(args.iter().map(|p| p.as_path()));
    let golden = run_py(PY_SUMMARIES, &a);

    let led = Ledger::open(rust_dir.path()).unwrap();
    let ids: Vec<i64> = led
        .query("SELECT DISTINCT nhl_id FROM deployment_daily", &[])
        .unwrap()
        .iter()
        .map(|r| r["nhl_id"].as_i64().unwrap())
        .collect();
    let goalie_ids: Vec<i64> = led
        .query("SELECT DISTINCT nhl_id FROM goalie_starts", &[])
        .unwrap()
        .iter()
        .map(|r| r["nhl_id"].as_i64().unwrap())
        .collect();
    assert!(!ids.is_empty(), "fixture harness.db has no deployment rows");
    for as_of in dates {
        let day = NaiveDate::parse_from_str(as_of, "%Y-%m-%d").unwrap();
        let want = &golden[as_of];
        let sums = deployment_summaries(&led, &ids, day, 5, None).unwrap();
        let got: serde_json::Map<String, Value> = sums.iter().map(|(k, v)| (k.to_string(), Value::Object(v.clone()))).collect();
        assert_eq!(want["summaries"].as_object().unwrap().len(), got.len(), "{as_of} summary count");
        for (k, v) in &got {
            assert_eq!(&want["summaries"][k], v, "{as_of} summary for {k}");
        }
        let counts = team_goalie_counts(&led, day).unwrap();
        assert_eq!(want["counts"], counts_json(&counts), "{as_of} counts");
        for (t, _) in &counts {
            let (p, n) = b2b_second_night_rate(&led, t, day, Some(&counts)).unwrap();
            assert_eq!(want["b2b"][t], json!([p, n]), "{as_of} b2b {t}");
        }
        for gid in &goalie_ids {
            let (s, g) = goalie_start_counts(&led, *gid, day, None).unwrap();
            assert_eq!(want["starts"][gid.to_string()], json!([s, g]), "{as_of} starts {gid}");
        }
    }
}

fn find_players(v: &Value, out: &mut BTreeMap<String, (Option<i64>, Option<String>, bool)>) {
    match v {
        Value::Object(o) => {
            if let (Some(cid), Some(ids), Some(pos)) = (o.get("cid").and_then(|c| c.as_str()), o.get("ids"), o.get("positions")) {
                let nhl = ids.get("nhl").and_then(|n| n.as_str()).and_then(|s| s.parse::<i64>().ok());
                let team = o.get("team").and_then(|t| t.as_str()).map(String::from);
                let goalie = pos.as_array().map(|a| a.iter().any(|p| p == "G")).unwrap_or(false);
                out.insert(cid.to_string(), (nhl, team, goalie));
            }
            for x in o.values() {
                find_players(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| find_players(x, out)),
        _ => {}
    }
}

#[test]
fn snapshot_deployment_fields_recomputed_from_the_ledger_equal_the_snapshot() {
    let Some(root) = fixtures_root() else {
        eprintln!("skipped: needs FM_PARITY_FIXTURES/_datadir/harness.db");
        return;
    };
    let src = root.join("_datadir").join("harness.db");
    for league in ["espn", "fantrax"] {
        let snap_path = root.join(league).join("snapshot.json");
        if !snap_path.exists() {
            continue;
        }
        let snap: Value = serde_json::from_str(&std::fs::read_to_string(snap_path).unwrap()).unwrap();
        let ctx = &snap["ctx"];
        let as_of = NaiveDate::parse_from_str(snap["today"].as_str().unwrap(), "%Y-%m-%d").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::copy(&src, tmp.path().join("harness.db")).unwrap();
        let led = Ledger::open(tmp.path()).unwrap();
        let mut players = BTreeMap::new();
        find_players(ctx, &mut players);

        // deployment_details: summaries of every skater with ledger rows (prior baselines come from
        // the NHL season report, so those fields are only compared for "season" baselines)
        let details = ctx["deployment_details"].as_object().unwrap();
        let ids: Vec<i64> = details.keys().filter_map(|c| players.get(c).and_then(|p| p.0)).collect();
        let sums = deployment_summaries(&led, &ids, as_of, 5, None).unwrap();
        let (mut full, mut partial) = (0, 0);
        for (cid, want) in details {
            let nhl = players[cid].0.unwrap();
            let got = &sums[&nhl];
            let prior_based = want["baseline_source"] == "prior";
            for (k, v) in want.as_object().unwrap() {
                if prior_based && matches!(k.as_str(), "baseline_toi" | "baseline_pp_share" | "baseline_source" | "toi_trend" | "pp_share_trend") {
                    continue;
                }
                assert_eq!(&got[k], v, "{league} {cid} {k}");
            }
            if prior_based { partial += 1 } else { full += 1 }
        }
        assert!(full + partial > 0);

        // goalie_actual_starts / b2b_second_night for teams with >= 5 ledger games
        let counts = team_goalie_counts(&led, as_of).unwrap();
        let ready: Map<String, _> = counts.iter().filter(|(_, c)| c.games >= 5).map(|(t, c)| (t.clone(), c.clone())).collect();
        let b2b = ctx["b2b_second_night"].as_object().unwrap();
        assert_eq!(b2b.len(), ready.len(), "{league} b2b teams");
        for (t, want) in b2b {
            let (p, n) = b2b_second_night_rate(&led, t, as_of, Some(&counts)).unwrap();
            assert_eq!(want, &json!([p, n]), "{league} b2b {t}");
        }
        let actual = ctx["goalie_actual_starts"].as_object().unwrap();
        let mut expect = 0;
        for (cid, (nhl, team, goalie)) in &players {
            let team = team.as_deref().and_then(|t| fm_core::normalize::normalize_team(Some(t)));
            let (Some(nhl), Some(team)) = (nhl, team) else { continue };
            let Some(c) = ready.get(&team) else { continue };
            if !*goalie {
                continue;
            }
            expect += 1;
            let got = json!([c.starts.get(nhl).copied().unwrap_or(0), c.games_for(*nhl)]);
            assert_eq!(actual.get(cid), Some(&got), "{league} goalie_actual_starts {cid}");
        }
        assert_eq!(actual.len(), expect, "{league} goalie_actual_starts count");
    }
}

const PY_PULL: &str = r#"
import sys, json, datetime
sys.path.insert(0, sys.argv[1])
from pathlib import Path
from datetime import date, timedelta
from fantasy_manager.harness.ledger import Ledger
from fantasy_manager.harness.deployment import pull_deployment
from fantasy_manager.providers.nhl import NhlClient
FIX = Path(sys.argv[1]) / "tests" / "fixtures" / "nhl"
ROUTES = {("skater/timeonice", "true"): "deploy_toi_games_2026-04-01.json",
          ("skater/powerplay", "true"): "deploy_pp_games_2026-04-01.json",
          ("goalie/summary", "true"): "deploy_goalie_games_2026-04-01.json",
          ("team/powerplay", "true"): "deploy_team_pp_games_2026-04-01.json"}
def fetch(url, params=None):
    path = url.split("/stats/rest/en/")[-1]
    key = (path, str((params or {}).get("isGame")))
    if key not in ROUTES:
        raise AssertionError(url)
    if '"2026-04-01"' not in params["cayenneExp"]:
        return {"data": [], "total": 0}
    return json.loads((FIX / ROUTES[key]).read_text(encoding="utf-8"))
day = date(2026, 4, 1)
c = NhlClient(fetch_json=fetch, season=20252026)
out = {}
for name, kw in (("sched", dict(schedule={"COL": [day - timedelta(days=1), day], "VAN": [day]})),
                 ("nosched", dict(record_empty=False)),
                 ("range", dict(end=date(2026, 4, 3), record_empty=False))):
    led = Ledger(Path(sys.argv[2]) / name)
    res = pull_deployment(led, c, day, **kw)
    out[name] = {"result": res, **{t: led.query(f"SELECT * FROM {t} ORDER BY 1, 2") for t in ("deployment_daily", "goalie_starts", "deployment_pulls")}}
print(json.dumps(out))
"#;

#[test]
fn pull_deployment_rows_equal_python() {
    if python().is_none() {
        eprintln!("skipped: needs a legacy-python checkout (FM_LEGACY_ROOT)");
        return;
    }
    let (rust_dir, py_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let golden = run_py(PY_PULL, &[py_dir.path()]);
    let c = client(&[]);
    let sched = col_van_schedule();
    for (name, end, sched, record_empty) in
        [("sched", None, Some(&sched), true), ("nosched", None, None, false), ("range", Some(d(2026, 4, 3)), None, false)]
    {
        let led = Ledger::open(rust_dir.path().join(name)).unwrap();
        let res = pull_deployment(&led, &c, day(), end, None, sched, record_empty).unwrap();
        assert_eq!(res, golden[name]["result"], "{name} result");
        for t in ["deployment_daily", "goalie_starts", "deployment_pulls"] {
            let rows = strip(led.query(&format!("SELECT * FROM {t} ORDER BY 1, 2"), &[]).unwrap(), &["pulled_at"]);
            let want = strip(
                golden[name][t].as_array().unwrap().iter().map(|v| v.as_object().unwrap().clone()).collect(),
                &["pulled_at"],
            );
            assert_eq!(rows, want, "{name} {t}");
        }
    }
}

const PY_INGEST: &str = r#"
import sys, json
sys.path.insert(0, sys.argv[1])
from pathlib import Path
from fantasy_manager.harness.ledger import Ledger
from fantasy_manager.harness.ingest import ingest_archive
d = Path(sys.argv[2])
led = Ledger(d)
st = ingest_archive(led, d)
tables = {"projections": "league, as_of, cid", "recs": "league, as_of, rec_key", "rec_players": "league, as_of, rec_key, side, cid",
          "rec_episodes": "episode_id", "archive_files": "path"}
out = {"stats": st, **{t: led.query(f"SELECT * FROM {t} ORDER BY {o}") for t, o in tables.items()},
       "meta": led.query("SELECT * FROM meta WHERE key LIKE 'means:%' ORDER BY key")}
print(json.dumps(out))
"#;

#[test]
fn ingest_rows_equal_python() {
    if python().is_none() {
        eprintln!("skipped: needs a legacy-python checkout (FM_LEGACY_ROOT)");
        return;
    }
    let (rust_dir, py_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write_archive(rust_dir.path());
    write_archive(py_dir.path());
    let golden = run_py(PY_INGEST, &[py_dir.path()]);
    let led = Ledger::open(rust_dir.path()).unwrap();
    let st = ingest_archive(&led, rust_dir.path(), None, false).unwrap();
    assert_eq!(st.to_json(), golden["stats"]);
    let tables = [
        ("projections", "league, as_of, cid"),
        ("recs", "league, as_of, rec_key"),
        ("rec_players", "league, as_of, rec_key, side, cid"),
        ("rec_episodes", "episode_id"),
        ("archive_files", "path"),
    ];
    for (t, order) in tables {
        let rows = strip(led.query(&format!("SELECT * FROM {t} ORDER BY {order}"), &[]).unwrap(), &["updated_at", "ingested_at"]);
        let want = strip(
            golden[t].as_array().unwrap().iter().map(|v| v.as_object().unwrap().clone()).collect(),
            &["updated_at", "ingested_at"],
        );
        assert!(!rows.is_empty(), "{t} empty");
        assert_eq!(rows, want, "{t}");
    }
    let meta = led.query("SELECT * FROM meta WHERE key LIKE 'means:%' ORDER BY key", &[]).unwrap();
    assert_eq!(meta.into_iter().map(Value::Object).collect::<Vec<_>>(), golden["meta"].as_array().unwrap().clone());
}

const PY_PARAMS: &str = r#"
import sys, json, re, os
sys.path.insert(0, sys.argv[1])
from pathlib import Path
from datetime import date
pdir = Path(sys.argv[2]) / "params"
os.environ["FM_PARAMS_DIR"] = str(pdir)
from fantasy_manager.harness.ledger import Ledger
from fantasy_manager.harness.params_store import ParamsStore
led = Ledger(Path(sys.argv[2]))
s = ParamsStore(pdir, led)
s.propose({"k_inseason": {"skater": 22.0}, "note": "caf\u00e9"}, ["k_inseason.skater", "a.b"], {"holdout_before": 1.5}, note="first")
s.apply("v0001", as_of=date(2026, 11, 4))
s.propose({"k_inseason": {"skater": 18.0}}, [], None, extra={"shadow": None})
s.apply("v0002", by="auto", as_of=date(2026, 11, 5))
r1 = s.rollback(note="n")
r2 = s.rollback(to="packaged")
ts = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")
files = {p.name: ts.sub("TS", p.read_text(encoding="utf-8")) for p in sorted(pdir.glob("*.json"))}
rows = [{k: (ts.sub("TS", v) if isinstance(v, str) else v) for k, v in r.items()} for r in led.query("SELECT * FROM param_versions ORDER BY version")]
print(json.dumps({"files": files, "ledger": rows, "r1": [r1["from"], r1["to"]], "r2": [r2["from"], r2["to"]]}))
"#;

#[test]
fn params_store_files_and_ledger_rows_equal_python() {
    if python().is_none() {
        eprintln!("skipped: needs a legacy-python checkout (FM_LEGACY_ROOT)");
        return;
    }
    let (rust_dir, py_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let golden = run_py(PY_PARAMS, &[py_dir.path()]);
    let led = Ledger::open(rust_dir.path()).unwrap();
    let pdir = rust_dir.path().join("params");
    let s = ParamsStore::new(Some(&pdir), Some(&led), None);
    s.propose(
        &json!({"k_inseason": {"skater": 22.0}, "note": "caf\u{e9}"}),
        &["k_inseason.skater".to_string(), "a.b".to_string()],
        Some(&json!({"holdout_before": 1.5})),
        Some("first"),
        None,
        "refit",
        None,
    )
    .unwrap();
    s.apply("v0001", "manual", None, Some(d(2026, 11, 4))).unwrap();
    let extra = json!({"shadow": null}).as_object().unwrap().clone();
    s.propose(&json!({"k_inseason": {"skater": 18.0}}), &[], None, None, None, "refit", Some(&extra)).unwrap();
    s.apply("v0002", "auto", None, Some(d(2026, 11, 5))).unwrap();
    let r1 = s.rollback(None, "manual", Some("n")).unwrap();
    let r2 = s.rollback(Some("packaged"), "manual", None).unwrap();
    assert_eq!(json!([r1["from"], r1["to"]]), golden["r1"]);
    assert_eq!(json!([r2["from"], r2["to"]]), golden["r2"]);
    let ts = regex_ts();
    for (name, want) in golden["files"].as_object().unwrap() {
        if name == "active.json" {
            continue; // holds only a timestamp + pointer, checked below
        }
        let got = std::fs::read_to_string(pdir.join(name)).unwrap();
        assert_eq!(ts.replace_all(&got, "TS"), want.as_str().unwrap(), "{name}");
    }
    let active = std::fs::read_to_string(pdir.join("active.json")).unwrap();
    assert_eq!(ts.replace_all(&active, "TS"), golden["files"]["active.json"].as_str().unwrap());
    let rows: Vec<Value> = led
        .query("SELECT * FROM param_versions ORDER BY version", &[])
        .unwrap()
        .into_iter()
        .map(|mut r| {
            for v in r.values_mut() {
                if let Some(s) = v.as_str() {
                    *v = Value::from(ts.replace_all(s, "TS").to_string());
                }
            }
            Value::Object(r)
        })
        .collect();
    assert_eq!(rows, golden["ledger"].as_array().unwrap().clone());
}

fn regex_ts() -> regex::Regex {
    regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}").unwrap()
}

const PY_DATA: &str = r#"
import sys, json
sys.path.insert(0, sys.argv[1])
from pathlib import Path
from datetime import date
from fantasy_manager.backtest import data as D
from fantasy_manager.providers.nhl import NhlClient
pages = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
def fetch(url, params=None):
    key = "/".join(url.rsplit("/", 2)[-2:])
    return {"data": pages.get(key, []), "total": len(pages.get(key, []))}
c = NhlClient(fetch_json=fetch)
season = [r.to_dict() for r in D.fetch_season(c, 20212022)]
win = {str(k): {"gp": v.gp, "stats": v.stats} for k, v in D.fetch_window(c, 20212022, date(2021, 11, 1), date(2021, 11, 1), faceoffs=True).items()}
t = D.SeasonTable([D.PlayerSeason.from_dict(r) for r in season] + [D.PlayerSeason.from_dict({"player_id": 9, "season": 20202021, "name": "Zo\u00eb", "gp": 3, "stats": {"G": 1}})])
D.save_table(t, Path(sys.argv[3]))
w = {20212022: {"2021-11-01": {"to_date": D.fetch_window(c, 20212022, None, date(2021, 10, 31)), "rest": D.fetch_window(c, 20212022, date(2021, 11, 1), None)}}}
D.save_windows(w, Path(sys.argv[3]))
print(json.dumps({"season": season, "window": win}))
"#;

#[test]
fn backtest_data_equals_python() {
    if python().is_none() {
        eprintln!("skipped: needs a legacy-python checkout (FM_LEGACY_ROOT)");
        return;
    }
    let (rust_dir, py_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let pages_file = py_dir.path().join("pages.json");
    std::fs::write(&pages_file, serde_json::to_string(&pages()).unwrap()).unwrap();
    let out_dir = py_dir.path().join("out");
    let golden = run_py(PY_DATA, &[&pages_file, &out_dir]);

    let client = pages_client(pages());
    let season: Vec<Value> = D::fetch_season(&client, 20212022).unwrap().iter().map(|r| r.to_dict()).collect();
    assert_eq!(Value::Array(season.clone()), golden["season"]);
    let win = D::fetch_window(&client, 20212022, Some(d(2021, 11, 1)), Some(d(2021, 11, 1)), true).unwrap();
    let win: serde_json::Map<String, Value> =
        win.iter().map(|(k, v)| (k.to_string(), json!({"gp": v.gp, "stats": v.stats}))).collect();
    assert_eq!(Value::Object(win), golden["window"]);

    let mut rows: Vec<D::PlayerSeason> = season.iter().map(|v| D::PlayerSeason::from_dict(v).unwrap()).collect();
    rows.push(
        D::PlayerSeason::from_dict(&json!({"player_id": 9, "season": 20202021, "name": "Zo\u{eb}", "gp": 3, "stats": {"G": 1}}))
            .unwrap(),
    );
    D::save_table(&D::SeasonTable::new(rows), rust_dir.path()).unwrap();
    let mut w = D::Windows::new();
    let mut wins = Map::new();
    wins.insert("to_date".to_string(), D::fetch_window(&client, 20212022, None, Some(d(2021, 10, 31)), false).unwrap());
    wins.insert("rest".to_string(), D::fetch_window(&client, 20212022, Some(d(2021, 11, 1)), None, false).unwrap());
    let mut days = Map::new();
    days.insert("2021-11-01".to_string(), wins);
    w.insert(20212022, days);
    D::save_windows(&w, rust_dir.path()).unwrap();
    for f in ["player_seasons.json", "windows.json"] {
        assert_eq!(
            std::fs::read(rust_dir.path().join("backtest").join(f)).unwrap(),
            std::fs::read(out_dir.join("backtest").join(f)).unwrap(),
            "{f} bytes"
        );
    }
}
