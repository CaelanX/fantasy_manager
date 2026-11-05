//! W3-valuation-schedule: ports of tests/test_schedule.py, test_opponent_adjust.py,
//! test_goalie_teammates.py, test_roles.py and the rookie-model parts of test_rookie.py (at the
//! level of the schedule / roles / rookie functions; the valuate_league halves come with W4), goldens
//! from the Python reference (`gold_*` tests), and snapshot parity against `values.json`
//! (games_next7 / offnight_next7, OPP_ADJ_WEEK, B2B, established-skater role changes, the rookie
//! prior and the rookie estimate).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{Datelike, Duration, NaiveDate, TimeZone, Utc};
use fm_core::models::{LeagueContext, Player, Reason};
use fm_core::normalize::normalize_team;
use fm_core::snapshot::{LeagueSeason, RoleSignal, Snapshot};
use fm_core::{Map, PyDateTime};
use fm_engine::analysis::team_factors::*;
use fm_engine::valuation::params as vp;
use fm_engine::valuation::roles::*;
use fm_engine::valuation::rookie::{self, *};
use fm_engine::valuation::schedule::*;
use fm_sources::odds::TeamOddsContext;
use serde_json::{json, Value};

/// Tests touching the process-global params serialize on this.
static GLOBAL: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    GLOBAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}
macro_rules! assert_close {
    ($a:expr, $b:expr) => {{
        let (a, b): (f64, f64) = ($a, $b);
        assert!(close(a, b), "{} != {} ({}:{})", a, b, file!(), line!());
    }};
}

fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, dd).unwrap()
}
const D0: (i32, u32, u32) = (2026, 10, 7);
fn dd() -> NaiveDate {
    d(D0.0, D0.1, D0.2)
}
fn days(offsets: &[i64], start: NaiveDate) -> Vec<NaiveDate> {
    offsets.iter().map(|o| start + Duration::days(*o)).collect()
}

fn player(v: Value) -> Player {
    let mut base = json!({"name_norm": "x", "ids": {}, "team": null, "positions": ["C"]});
    for (k, x) in v.as_object().unwrap() {
        base[k] = x.clone();
    }
    if base.get("name").is_none() {
        base["name"] = base["cid"].clone();
    }
    serde_json::from_value(base).unwrap()
}

fn ctx_of(rostered: Vec<Player>, free_agents: Vec<Player>, as_of: NaiveDate) -> LeagueContext {
    let slots: Vec<Value> =
        rostered.iter().map(|p| json!({"slot": "BN", "player": p, "starting": false})).collect();
    let mut c: LeagueContext = serde_json::from_value(json!({
        "provider": "t", "league_id": "1", "season": 2027, "name": "T",
        "scoring": {"kind": "points", "weights": {"G": 1.0}},
        "roster_shape": {"C": 1, "BN": 3},
        "teams": [{"team_id": "1", "name": "me", "owner_is_me": true, "slots": slots}],
        "free_agents": free_agents, "matchup_period": 1, "as_of": as_of.to_string(),
    }))
    .unwrap();
    c.enrich = Arc::new(Default::default());
    c
}

fn sched(pairs: &[(&str, Vec<NaiveDate>)]) -> Map<String, Vec<NaiveDate>> {
    pairs.iter().map(|(t, ds)| (t.to_string(), ds.clone())).collect()
}

// ============================================================================ test_schedule.py

#[test]
fn games_in_next_window_is_half_open() {
    let ds = days(&[0, 2, 6, 7], dd());
    assert_eq!(games_in_next(&ds, dd(), 7), 3);
    assert_eq!(games_in_next(&ds, dd(), 8), 4);
    assert_eq!(games_in_next(&ds, dd() - Duration::days(10), 7), 0);
    // duplicates count once (Python iterates a set)
    let dup = days(&[0, 0, 2], dd());
    assert_eq!(games_in_next(&dup, dd(), 7), 2);
}

#[test]
fn off_night_games_counts() {
    let per_day: Map<NaiveDate, i64> =
        [(dd(), 3), (dd() + Duration::days(2), 12), (dd() + Duration::days(6), 7)].into_iter().collect();
    let ds = days(&[0, 2, 6], dd());
    assert_eq!(off_night_games(&ds, &per_day, dd(), 7, OFFNIGHT_THRESHOLD), 2);
    assert_eq!(off_night_games(&ds, &per_day, dd(), 7, 4), 1);
}

#[test]
fn proj_week_formula() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    assert_close!(proj_week(2.0, 1.0, 4, 0, None), 8.0);
    assert_close!(proj_week(2.0, 0.75, 4, 2, None), 2.0 * 0.75 * 4.0 * 1.10);
}

fn goalie_gs(gs: f64, gp: i64) -> Player {
    player(json!({"cid": "g", "team": "EDM", "positions": ["G"],
                  "lines": {"season": {"split": "season", "gp": gp, "stats": {"GS": gs, "GP": gp}}}}))
}

#[test]
fn start_share_shrinks_toward_half() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let ss = |p: &Player| start_share(p, None, None, None, None, ACTUAL_SHARE_K);
    assert_close!(ss(&goalie_gs(0.0, 0)), 0.5);
    assert_close!(ss(&goalie_gs(10.0, 10)), 15.0 / 20.0);
    assert_close!(ss(&goalie_gs(60.0, 60)), 65.0 / 70.0);
    let skater = player(json!({"cid": "s", "team": "EDM", "positions": ["C"]}));
    assert_eq!(ss(&skater), 1.0);
    // ledger actuals shrink toward the prior-line share (season line left out)
    let g = goalie_gs(30.0, 30);
    let got = start_share(&g, None, None, None, Some((8.0, 10.0)), ACTUAL_SHARE_K);
    assert_close!(got, (8.0 + 10.0 * 0.5) / 20.0);
    assert_close!(actual_start_share(0.6, 15.0, 10.0, 10.0), (10.0 + 6.0) / 20.0); // starts capped at games
    assert_close!(actual_start_share(0.6, 3.0, 0.0, 10.0), 0.6);
    // team games per split raise the denominator, never below own GP
    let tg: Map<String, f64> = [("season".to_string(), 50.0)].into_iter().collect();
    let (s, gs, t) = start_share_parts(&goalie_gs(20.0, 25), None, None, Some(&tg), &["season", "prior"]);
    assert_eq!((gs, t), (20.0, 50.0));
    assert_close!(s, 25.0 / 60.0);
}

#[test]
fn week_window_preseason_uses_season_start() {
    let s = sched(&[("EDM", days(&[0, 2, 4], dd())), ("CGY", days(&[1, 2], dd()))]);
    let gpd = fm_sources::nhl::games_per_day(&s);
    let w = week_window(d(2026, 9, 20), Some(dd()), &s, &gpd, 7).unwrap();
    assert!(w.preseason && w.start == dd());
    assert_close!(w.avg_team_games, 2.5);
    let w2 = week_window(dd() + Duration::days(1), Some(dd()), &s, &gpd, 7).unwrap();
    assert!(!w2.preseason && w2.start == dd() + Duration::days(1));
    let w3 = week_window(dd() - Duration::days(1), Some(dd()), &s, &gpd, 7).unwrap();
    assert!(!w3.preseason && w3.start == dd() - Duration::days(1));
    assert!(week_window(dd(), Some(dd()), &Map::new(), &Map::new(), 7).is_none());
    assert_eq!(w.label(), "Oct 07-Oct 13");
    assert_eq!(w.end(), d(2026, 10, 13));
}

#[test]
fn schedule_factor_unknown_team() {
    let s = sched(&[("EDM", days(&[0], dd()))]);
    let w = week_window(dd(), Some(dd()), &s, &fm_sources::nhl::games_per_day(&s), 7).unwrap();
    let mut p = player(json!({"cid": "x", "positions": ["C"]}));
    assert!(schedule_factor(&p, &s, &Map::new(), &w, OFFNIGHT_THRESHOLD, None, None).is_none());
    p.team = Some("EDM".into());
    assert_eq!(schedule_factor(&p, &s, &Map::new(), &w, OFFNIGHT_THRESHOLD, None, None).unwrap().games, 1);
}

#[test]
fn memo_window_is_per_context_state() {
    let mut a = ctx_of(vec![], vec![], dd());
    a.schedule = sched(&[("EDM", days(&[0, 2], dd()))]);
    let memo = ScheduleMemo::new(None);
    let w = memo.window(&a).unwrap();
    assert_eq!(w.start, dd());
    assert_eq!(memo_window(&a, &memo), Some(w));
    // a copy with another as_of gets its own window
    let mut b = a.clone();
    b.as_of = dd() + Duration::days(1);
    assert_eq!(memo.window(&b).unwrap().start, dd() + Duration::days(1));
    fn is_sync<T: Send + Sync>() {}
    is_sync::<ScheduleMemo>();
    is_sync::<OpponentAdjust>();
}

// ------------------------------------------------------------------------ back-to-backs

#[test]
fn back_to_back_week_share_goldens() {
    // goldens from the Python reference (week_start_share / night_start_probs)
    let ds = days(&[0, 1, 3, 4, 5, 8], d(2026, 11, 1));
    assert_close!(week_start_share(0.62, &ds, d(2026, 11, 1), 7, 0.4, SEASON_B2B_FRAC), 0.503529411764706);
    assert_close!(week_start_share(0.38, &ds, d(2026, 11, 1), 7, 0.4, SEASON_B2B_FRAC), 0.4964705882352941);
    assert_eq!(week_start_share(0.2, &ds, d(2026, 11, 1), 7, 0.4, SEASON_B2B_FRAC), 0.2);
    let (p1, p2) = night_start_probs(0.7, B2B_SECOND_NIGHT_P, SEASON_B2B_FRAC);
    assert_close!(p1, 0.7617647058823529);
    assert_close!(p2, 0.35);
    assert_eq!(second_nights(&ds, d(2026, 11, 1), 7), 3);
    // a second night whose first night falls before the window still counts
    assert_eq!(second_nights(&ds, d(2026, 11, 2), 7), 3);
    assert_close!(second_night_prob(0.6, 1.0, 0.35), 0.35);
    assert_close!(second_night_prob(0.6, 0.0, 0.35), RESTED_STARTER_P);
    assert_close!(second_night_prob(0.3, 1.0, 0.35), BACKUP_REPEAT_P);
    assert_close!(second_night_prob(0.3, 0.0, 0.35), 0.65);
    assert_eq!(second_night_prob(0.1, 0.5, 0.35), 0.1);
}

// ============================================================================ test_goalie_teammates.py

fn share_of(gs: f64) -> f64 {
    (gs + 10.0 * 0.5) / (82.0 + 10.0)
}

fn tm_goalie(cid: &str, gs: f64, status: &str, note: Option<&str>, nhl: Option<i64>) -> Player {
    let mut title = cid.to_string();
    title[..1].make_ascii_uppercase();
    let gp = (gs as i64).max(1);
    player(json!({"cid": cid, "name": format!("{title} {title}son"), "name_norm": cid,
                  "ids": nhl.map(|n| json!({"nhl": n.to_string()})).unwrap_or(json!({})),
                  "team": "EDM", "positions": ["G"], "status": status, "status_note": note,
                  "lines": {"prior": {"split": "prior", "gp": gp,
                                      "stats": {"GS": gs, "W": gs * 0.5, "SV": gs * 25.0, "GP": gs.max(1.0)}}}}))
}

fn tm_ctx(players: Vec<Player>, nhl_goalies: Map<String, Map<i64, String>>) -> LeagueContext {
    let mut c = ctx_of(players, vec![], dd());
    let edm: Vec<NaiveDate> = (0..82).map(|i| dd() + Duration::days(2 * i)).collect();
    let cgy: Vec<NaiveDate> = (0..82).map(|i| dd() + Duration::days(2 * i + 1)).collect();
    c.schedule = sched(&[("EDM", edm), ("CGY", cgy)]);
    c.games_per_day = fm_sources::nhl::games_per_day(&c.schedule);
    c.season_start = Some(dd());
    c.nhl_goalies = nhl_goalies;
    c
}

fn suspended_until(n: i64) -> String {
    format!("suspension (est. return {})", dd() + Duration::days(n))
}

fn base_of(pairs: &[(&str, f64)]) -> Map<String, f64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

#[test]
fn backup_takes_over_week_when_starter_suspended() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let note = suspended_until(30);
    let starter = tm_goalie("helle", 60.0, "suspended", Some(&note), None);
    let backup = tm_goalie("skin", 22.0, "healthy", None, None);
    let ctx = tm_ctx(vec![starter, backup.clone()], Map::new());
    let base = base_of(&[("helle", share_of(60.0)), ("skin", share_of(22.0))]);
    let w = context_window(&ctx, WEEK_DAYS);
    let out = teammate_shares(&ctx, &base, w.as_ref(), None, TEAMMATE_CAP);
    assert_eq!(out.keys().collect::<Vec<_>>(), vec!["skin"]);
    let s = &out["skin"];
    // golden from the Python reference
    assert_close!(s.week, 0.9);
    assert_close!(s.season, 0.4227200424178155);
    assert_close!(s.season, share_of(22.0) + share_of(60.0) * 15.0 / 82.0);
    assert_eq!(
        s.text(&backup),
        "Helleson suspended until ~Nov 6 (~15 of 82 games): Skinson's start share 0.29 -> 0.90 this week \
         (Oct 07-Oct 13), 0.29 -> 0.42 rest of season"
    );
}

#[test]
fn healthy_or_lower_share_teammate_changes_nothing() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let ctx = tm_ctx(
        vec![tm_goalie("helle", 60.0, "healthy", None, None), tm_goalie("skin", 22.0, "healthy", None, None)],
        Map::new(),
    );
    let base = base_of(&[("helle", share_of(60.0)), ("skin", share_of(22.0))]);
    assert!(teammate_shares(&ctx, &base, context_window(&ctx, 7).as_ref(), None, TEAMMATE_CAP).is_empty());
    let ctx = tm_ctx(
        vec![
            tm_goalie("helle", 60.0, "healthy", None, None),
            tm_goalie("skin", 22.0, "ir", Some("est. return 2026-12-01"), None),
        ],
        Map::new(),
    );
    assert!(teammate_shares(&ctx, &base, context_window(&ctx, 7).as_ref(), None, TEAMMATE_CAP).is_empty());
}

#[test]
fn three_goalie_team_splits_in_proportion() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let ctx = tm_ctx(
        vec![
            tm_goalie("helle", 60.0, "ltir", None, None),
            tm_goalie("skin", 22.0, "healthy", None, None),
            tm_goalie("third", 4.0, "healthy", None, None),
        ],
        Map::new(),
    );
    let (s0, s1, s2) = (share_of(60.0), share_of(22.0), share_of(4.0));
    let base = base_of(&[("helle", s0), ("skin", s1), ("third", s2)]);
    let out = teammate_shares(&ctx, &base, context_window(&ctx, 7).as_ref(), None, TEAMMATE_CAP);
    let (w1, w2) = (s1 + s0 * s1 / (s1 + s2), s2 + s0 * s2 / (s1 + s2));
    assert!(w1 < TEAMMATE_CAP);
    assert_close!(out["skin"].week, w1);
    assert_close!(out["third"].week, w2);
    assert_close!(out["skin"].season, w1); // LTIR without a timetable: season split == week's
    assert!(out["third"].text(ctx.all_players()[2]).contains("for the rest of the season"));
}

#[test]
fn partial_week_absence_uses_games_missed_in_window() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let note = suspended_until(4);
    let ctx = tm_ctx(
        vec![tm_goalie("helle", 60.0, "suspended", Some(&note), None), tm_goalie("skin", 22.0, "healthy", None, None)],
        Map::new(),
    );
    let base = base_of(&[("helle", share_of(60.0)), ("skin", share_of(22.0))]);
    let out = teammate_shares(&ctx, &base, context_window(&ctx, 7).as_ref(), None, TEAMMATE_CAP);
    assert_close!(out["skin"].week, share_of(22.0) + share_of(60.0) * 0.5);
    assert_close!(out["skin"].absent[0].week_frac, 0.5);
}

#[test]
fn redistribute_cap_spills_to_the_others() {
    let new = redistribute_shares(&base_of(&[("a", 0.7), ("b", 0.5), ("c", 0.1)]), &base_of(&[("a", 1.0)]), TEAMMATE_CAP);
    assert_close!(new["b"], 0.9);
    assert_eq!(new["c"], 0.40000000000000013); // Python golden, bit-exact
    let low = redistribute_shares(&base_of(&[("a", 0.7), ("b", 0.3)]), &base_of(&[("b", 1.0)]), TEAMMATE_CAP);
    assert_eq!(low, base_of(&[("a", 0.7)]));
}

#[test]
fn nhl_roster_excludes_minor_leaguers_and_counts_callups() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let note = suspended_until(30);
    let roster: Map<i64, String> =
        [(1, "Helle".to_string()), (2, "Skin".to_string()), (9, "Callup".to_string())].into_iter().collect();
    let ctx = tm_ctx(
        vec![
            tm_goalie("helle", 60.0, "suspended", Some(&note), Some(1)),
            tm_goalie("skin", 22.0, "healthy", None, Some(2)),
            tm_goalie("farm", 4.0, "healthy", None, Some(3)),
        ],
        [("EDM".to_string(), roster)].into_iter().collect(),
    );
    let base = base_of(&[("helle", share_of(60.0)), ("skin", share_of(22.0)), ("farm", share_of(4.0))]);
    let out = teammate_shares(&ctx, &base, context_window(&ctx, 7).as_ref(), None, TEAMMATE_CAP);
    assert!(!out.contains_key("farm"));
    let (s0, s1) = (share_of(60.0), share_of(22.0));
    assert_close!(out["skin"].week, s1 + s0 * s1 / (s1 + ROSTER_ONLY_SHARE));
}

// ============================================================================ test_opponent_adjust.py

fn mon_tue_wed() -> (NaiveDate, NaiveDate, NaiveDate) {
    (d(2026, 11, 2), d(2026, 11, 3), d(2026, 11, 4))
}

fn opp_ctx(rostered: Vec<Player>, extra: Vec<Player>) -> LeagueContext {
    let (_, tue, wed) = mon_tue_wed();
    let mut c = ctx_of(rostered, extra, tue);
    c.schedule = sched(&[("EDM", vec![tue, wed]), ("BOS", vec![tue]), ("CGY", vec![wed])]);
    let mut opps: Map<String, Map<NaiveDate, String>> = Map::new();
    opps.insert("EDM".into(), [(tue, "BOS".to_string()), (wed, "@CGY".to_string())].into_iter().collect());
    opps.insert("BOS".into(), [(tue, "@EDM".to_string())].into_iter().collect());
    opps.insert("CGY".into(), [(wed, "EDM".to_string())].into_iter().collect());
    c.opponents = opps;
    c
}

fn implied(t: f64) -> TeamOddsContext {
    TeamOddsContext {
        implied_total: Some(t),
        opp_implied_total: None,
        win_prob: None,
        opponent: String::new(),
        home: false,
        game_total: None,
        as_of: String::new(),
    }
}

fn odds_table(rows: Vec<(NaiveDate, Vec<(&str, f64)>)>) -> OddsFn {
    let table: Map<NaiveDate, DayOdds> = rows
        .into_iter()
        .map(|(day, ts)| (day, ts.into_iter().map(|(t, v)| (t.to_string(), implied(v))).collect()))
        .collect();
    Arc::new(move |day| table.get(&day).cloned().unwrap_or_default())
}

fn ga(pairs: &[(&str, f64)]) -> TeamFactors {
    pairs.iter().map(|(t, f)| (t.to_string(), (*f, 20))).collect()
}

#[test]
fn multiplier_formula_and_clamps() {
    assert_eq!(opponent_multiplier(None, 0.7), 1.0);
    assert_eq!(opponent_multiplier(Some(0.0), 0.7), 1.0);
    assert_close!(opponent_multiplier(Some(1.2), 0.7), 1.2_f64.powf(0.7));
    assert_close!(opponent_multiplier(Some(0.9), 0.5), 0.9_f64.powf(0.5));
    assert_eq!(opponent_multiplier(Some(3.0), 1.0), OPP_MULT_MAX);
    assert_eq!(opponent_multiplier(Some(0.3), 1.0), OPP_MULT_MIN);
}

#[test]
fn implied_ratio_shrinks_the_days_average() {
    let r = implied_ratio(Some(3.6), [Some(3.6), Some(3.4)]).unwrap();
    let avg = (3.6 + 3.4 + IMPLIED_AVG_K * LEAGUE_IMPLIED_TOTAL) / (2.0 + IMPLIED_AVG_K);
    assert_close!(r, 3.6 / avg);
    assert!(r > 3.6 / 3.5);
    assert!(implied_ratio(None, [Some(3.0)]).is_none());
    assert!(implied_ratio(Some(0.0), [Some(3.0)]).is_none());
    assert_close!(implied_ratio(Some(LEAGUE_IMPLIED_TOTAL), []).unwrap(), 1.0);
}

#[test]
fn odds_first_then_goals_against_then_neutral() {
    let (_, tue, wed) = mon_tue_wed();
    let ctx = opp_ctx(vec![player(json!({"cid": "a", "team": "EDM"}))], vec![]);
    let odds = odds_table(vec![(tue, vec![("EDM", 3.8), ("BOS", 2.6)])]);
    let adj = OpponentAdjust::new(Some(odds.clone()), Some(ga(&[("CGY", 1.2)])), Some(true), None);
    let t = adj.game(&ctx, Some("EDM"), tue);
    assert_eq!((t.source.as_str(), t.opponent.as_deref()), ("odds", Some("BOS")));
    assert_close!(t.mult, opponent_multiplier(implied_ratio(Some(3.8), [Some(3.8), Some(2.6)]), 0.7));
    assert!(adj.game(&ctx, Some("BOS"), tue).mult < 1.0 && 1.0 < t.mult);
    let w = adj.game(&ctx, Some("EDM"), wed);
    assert_eq!(w.source, "goals against");
    assert_close!(w.mult, 1.2_f64.powf(0.5));
    assert!(w.text("a").contains("x1.10") && w.text("a").contains("@CGY"));
    assert_eq!(
        w.text("a"),
        "a: today @CGY - opponent allows 1.20x the league's goals per game, per-game value x1.10"
    );
    let c = adj.game(&ctx, Some("CGY"), wed);
    assert_eq!((c.source.as_str(), c.mult), ("neutral", 1.0));
    assert_eq!(adj.game(&ctx, None, tue).mult, 1.0);
    let off = OpponentAdjust::new(Some(odds), Some(ga(&[("CGY", 1.2)])), Some(false), None);
    assert_eq!(off.mult(&ctx, Some("EDM"), tue), 1.0);
    assert_eq!(off.mult(&ctx, Some("EDM"), wed), 1.0);
}

#[test]
fn env_switch_parsing() {
    for v in ["0", "false", " No ", "OFF"] {
        assert!(!opp_adjust_enabled_from(Some(v)), "{v}");
    }
    assert!(opp_adjust_enabled_from(None) && opp_adjust_enabled_from(Some("1")) && opp_adjust_enabled_from(Some("yes")));
}

#[test]
fn team_ga_factors_from_goalie_lines_with_coverage() {
    let g = |cid: &str, team: &str, gp: i64, ga_: f64| {
        player(json!({"cid": cid, "team": team, "positions": ["G"],
                      "lines": {"season": {"split": "season", "gp": gp, "stats": {"GA": ga_}}}}))
    };
    let mut ctx = opp_ctx(
        vec![player(json!({"cid": "a", "team": "EDM"}))],
        vec![g("g1", "BOS", 20, 80.0), g("g2", "CGY", 20, 40.0), g("g3", "EDM", 5, 10.0)],
    );
    let ds: Vec<NaiveDate> = (5..25).map(|x| d(2026, 10, x)).collect();
    ctx.schedule = sched(&[("BOS", ds.clone()), ("CGY", ds.clone()), ("EDM", ds)]);
    let f = team_ga_factors(&ctx);
    assert_eq!(f.keys().cloned().collect::<HashSet<_>>(), ["BOS", "CGY"].iter().map(|s| s.to_string()).collect());
    let k = 10.0;
    assert_close!(f["BOS"].0, (20.0 * 4.0 / 3.0 + k) / (20.0 + k));
    assert_close!(f["CGY"].0, (20.0 * 2.0 / 3.0 + k) / (20.0 + k));
    assert_eq!(f["BOS"].1, 20);
    // goals for: skater season lines
    let sk = |cid: &str, team: &str, g_: f64| {
        player(json!({"cid": cid, "team": team, "lines": {"season": {"split": "season", "gp": 20, "stats": {"G": g_}}}}))
    };
    let mut ctx2 = ctx.clone();
    ctx2.free_agents = vec![sk("s1", "BOS", 60.0), sk("s2", "CGY", 30.0), sk("s3", "bos", 0.0)];
    let gf = team_gf_factors(&ctx2);
    assert_close!(gf["BOS"].0, (20.0 * (3.0 / 2.25) + k) / (20.0 + k));
    assert_eq!(gf.keys().cloned().collect::<Vec<_>>(), vec!["BOS", "CGY"]);
}

#[test]
fn opp_week_factor_is_the_mean_over_the_window() {
    let (_, tue, _) = mon_tue_wed();
    let a = player(json!({"cid": "a", "team": "EDM"}));
    let g = player(json!({"cid": "g", "team": "EDM", "positions": ["G"]}));
    let ctx = opp_ctx(vec![a.clone()], vec![]);
    let memo = ScheduleMemo::new(None);
    memo.install_opponent_adjust(
        &ctx,
        OpponentAdjust::new(Some(odds_table(vec![])), Some(ga(&[("BOS", 1.21), ("CGY", 0.81)])), Some(true), None),
    );
    let w = WeekWindow::new(tue, 7);
    assert_close!(opp_week_factor(&ctx, &memo, &a, Some(&w)), (1.1 + 0.9) / 2.0);
    assert_eq!(opp_week_factor(&ctx, &memo, &g, Some(&w)), 1.0);
    assert_eq!(opp_week_factor(&ctx, &memo, &a, Some(&WeekWindow::new(d(2026, 12, 1), 7))), 1.0);
    // per-game: skaters scaled, goalies neutral
    assert_close!(skater_opp_game(&ctx, &memo, &a, tue).mult, 1.1);
    assert_eq!(skater_opp_game(&ctx, &memo, &g, tue), OppGame::default());
    // the memo hands back the installed table for the same context
    assert!(Arc::ptr_eq(&memo.opponent_adjust(&ctx), &opponent_adjust(&ctx, &memo)));
}

#[test]
fn odds_text_and_default_source_without_data_dir() {
    let (_, tue, _) = mon_tue_wed();
    let ctx = opp_ctx(vec![], vec![player(json!({"cid": "b", "team": "BOS"}))]);
    let adj = OpponentAdjust::new(Some(odds_table(vec![(tue, vec![("BOS", 3.9), ("EDM", 2.7)])])), Some(Map::new()), Some(true), None);
    let g = adj.game(&ctx, Some("BOS"), tue);
    assert!(g.mult > 1.0);
    assert!(g.text("b").contains("b: today @EDM - market implies 3.90 goals for his team"));
    // no data dir: no odds; empty GA -> neutral
    let n = OpponentAdjust::new(None, Some(Map::new()), Some(true), None);
    assert_eq!(n.game(&ctx, Some("BOS"), tue).source, "neutral");
    assert!(odds_fn(None)(tue).is_empty());
}

// ============================================================================ test_roles.py

fn role_d() -> NaiveDate {
    d(2026, 10, 20)
}

fn vet(cid: &str, extra: Value) -> Player {
    let stats = json!({"G": 20.0, "A": 30.0, "SOG": 200.0, "HIT": 50.0});
    let mut v = json!({"cid": cid, "name": format!("{cid} Vet"), "team": "EDM", "positions": ["C", "F"],
                       "career_gp": 600,
                       "lines": {"prior": {"split": "prior", "gp": 80, "stats": stats},
                                 "prior2": {"split": "prior2", "gp": 80, "stats": stats},
                                 "prior3": {"split": "prior3", "gp": 80, "stats": stats}}});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    player(v)
}

fn role_signal(kind: &str, direction: i64, days_ago: i64, conf: f64, quote: &str) -> RoleSignal {
    let day = role_d() - Duration::days(days_ago);
    RoleSignal {
        player_name: None,
        cid: None,
        kind: kind.into(),
        direction,
        confidence: conf,
        quote: quote.into(),
        published: Some(PyDateTime::from_utc(Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0).unwrap()))),
        source: "rotowire".into(),
        item_id: None,
        origin: "rules".into(),
        ambiguous: false,
    }
}

fn reason(code: &str, text: &str, value: f64, baseline: f64) -> Reason {
    Reason::new(code, text).with_value(Some(value)).with_baseline(Some(baseline))
}

const NO_NEWS: &[RoleSignal] = &[];

#[test]
fn veteran_promoted_to_pp1_and_already_on_pp1() {
    let p = vet("a", json!({"line": "f2", "pp_unit": "pp1", "usual_line": "f2", "usual_pp_unit": "pp2"}));
    let rc = role_change(&p, NO_NEWS, Some(role_d()), true, true).unwrap();
    let r = ROLE_BOOST / ROLE_MINOR_BOOST;
    assert_close!(rc.mult, r);
    assert_close!(rc.season_mult, 1.0 + (r - 1.0) * ROLE_SEASON_WEIGHT);
    let codes: Vec<&str> = rc.reasons.iter().map(|r| r.code.as_str()).collect();
    assert_eq!(codes, vec!["ROLE_DFO", "ROLE_DFO", "ROLE_BOOST"]); // line evidence, then PP evidence
    assert!(rc.reasons[2].text.contains("usually F2, PP2, now F2, PP1"));
    let same = vet("a", json!({"line": "f1", "pp_unit": "pp1", "usual_line": "f1", "usual_pp_unit": "pp1"}));
    assert!(role_change(&same, NO_NEWS, Some(role_d()), true, true).is_none());
}

#[test]
fn veteran_demoted_goldens() {
    // Python goldens (role_change on the test_roles players)
    let p = vet("a", json!({"line": "f4", "usual_line": "f2", "usual_pp_unit": "pp2"}));
    let rc = role_change(&p, NO_NEWS, Some(role_d()), true, true).unwrap();
    assert_eq!(
        rc.reasons,
        vec![
            reason("ROLE_DFO", "Daily Faceoff: F4", -1.0, 0.7),
            reason(
                "ROLE_DEPTH",
                "Role change: usually F2, PP2, now F4, no PP: x0.88 on scoring rates this week, x0.94 rest of season",
                0.8762,
                1.0
            ),
        ]
    );
    assert_eq!((rc.mult, rc.season_mult), (0.8761904761904762, 0.9380952380952381));
    // without PP units listed for his team, the missing PP unit is not read as a demotion off the PP
    // (usual PP2 kept: x1.05 beats the depth line, so nothing moves)
    assert!(role_change(&p, NO_NEWS, Some(role_d()), false, true).is_none());
}

#[test]
fn pp_share_is_the_usual_role_without_snapshots() {
    let p = vet("a", json!({"line": "f1", "pp_unit": "pp1", "pp_share": 0.7}));
    assert!(role_change(&p, NO_NEWS, Some(role_d()), true, true).is_none());
    let q = vet("b", json!({"line": "f1", "pp_unit": "pp1", "pp_share": 0.05}));
    let rc = role_change(&q, NO_NEWS, Some(role_d()), true, true).unwrap();
    assert_eq!(
        rc.reasons,
        vec![
            reason("ROLE_DFO", "Daily Faceoff: first power-play unit (PP1)", 1.0, 0.9),
            reason(
                "ROLE_BOOST",
                "Role change: usually no PP (5% of team PP time), now PP1: x1.15 on scoring rates this week, x1.07 rest of season",
                1.15,
                1.0
            ),
        ]
    );
    assert_eq!((rc.mult, rc.season_mult), (1.15, 1.075));
}

#[test]
fn news_fills_in_when_daily_faceoff_does_not_list_him() {
    let p = vet("a", json!({"usual_line": "f2", "usual_pp_unit": "pp2"}));
    let news = [role_signal("pp1", 1, 1, 0.8, "moved to the top power-play unit")];
    let rc = role_change(&p, &news, Some(role_d()), false, false).unwrap();
    assert_eq!(
        rc.reasons,
        vec![
            reason("ROLE_NEWS", "pp1 +: \"moved to the top power-play unit\" (rotowire, 2026-10-19)", 1.0, 0.8),
            reason(
                "ROLE_BOOST",
                "Role change: usually F2, PP2, now PP1 (news): x1.10 on scoring rates this week, x1.05 rest of season",
                1.0952,
                1.0
            ),
        ]
    );
    assert_eq!((rc.mult, rc.season_mult), (1.0952380952380951, 1.0476190476190474));
    // stale news is ignored
    let stale = [role_signal("pp1", 1, 15, 0.8, "news")];
    assert!(role_change(&p, &stale, Some(role_d()), false, false).is_none());
}

#[test]
fn scratched_veteran_loses_week_value_only() {
    let p = vet("a", json!({"line_change": "scratched"}));
    let rc = role_change(&p, NO_NEWS, Some(role_d()), false, true).unwrap();
    assert_eq!((rc.mult, rc.week_extra), (1.0, SCRATCH_MULT));
    assert_eq!(
        rc.reasons,
        vec![reason(
            "ROLE_SCRATCH",
            "Daily Faceoff: dropped out of his team's lineup; not in the lineup now: week value x0.85",
            0.85,
            1.0
        )]
    );
    // no lineup listed for his team: nothing
    assert!(role_change(&p, NO_NEWS, Some(role_d()), false, false).is_none());
    // healthy-scratch news within 7 days
    let q = vet("b", json!({}));
    let news = [role_signal("scratched", -1, 2, 0.8, "healthy scratch")];
    let rc = role_change(&q, &news, Some(role_d()), false, true).unwrap();
    assert!(rc.reasons[0].text.starts_with("scratched: \"healthy scratch\" (rotowire, 2026-10-18)"));
}

#[test]
fn goalie_job_news_moves_the_share() {
    let won = [role_signal("goalie_role", 1, 1, 0.8, "has won the starting job")];
    let (s, r) = goalie_news_share(0.48, &won, Some(role_d()));
    assert_eq!(s, 0.568);
    assert_eq!(
        r.unwrap(),
        reason(
            "START_NEWS",
            "Goalie news (won the job): \"has won the starting job\" (rotowire, 2026-10-19); start share 48% -> 57% (40% of the way to 70%)",
            0.568,
            0.48
        )
    );
    let base = 0.48;
    let lost = [role_signal("goalie_role", -1, 1, 0.8, "lost the starting job")];
    assert!(goalie_news_share(base, &lost, Some(role_d())).0 < base);
    // already past the target in the news' direction
    assert_eq!(goalie_news_share(0.75, &won, Some(role_d())), (0.75, None));
    // a tandem pulls toward 0.5 from either side
    let split = [role_signal("goalie_split", 1, 1, 1.0, "split")];
    assert_close!(goalie_news_share(0.8, &split, Some(role_d())).0, 0.65);
    assert!(goalie_news_share(0.48, NO_NEWS, Some(role_d())).1.is_none());
}

#[test]
fn role_multiplier_tiers() {
    let s = |k: &str, d_: i64| Signal::kd(k, d_);
    assert_eq!(role_multiplier(&[s("pp1", 1), s("top_line", 1)]).0, ROLE_BOOST);
    assert_eq!(role_multiplier(&[s("pp2", 1)]).0, ROLE_MINOR_BOOST);
    assert_eq!(role_multiplier(&[s("top_line", -1), s("extended_role", 1)]).0, ROLE_MINOR_BOOST);
    assert_eq!(role_multiplier(&[s("top_line", -1)]).0, DEPTH_PENALTY);
    assert_eq!(role_multiplier(&[]), (1.0, vec![]));
}

// ============================================================================ test_rookie.py (model parts)

fn landing_history(pid: i64) -> Vec<LeagueSeason> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../tests/fixtures/nhl/landing_history_{pid}.json"));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let birth = v["birthDate"].as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    let rows = fm_sources::nhl::parse_season_totals(v["seasonTotals"].as_array().unwrap(), birth, 2);
    serde_json::from_value(serde_json::to_value(rows).unwrap()).unwrap()
}
const STENBERG: i64 = 8486103;
const NYGARD: i64 = 8484794;

fn rookie_as_of() -> NaiveDate {
    d(2026, 9, 29)
}

fn sk(cid: &str, extra: Value) -> Player {
    let mut v = json!({"cid": cid, "team": "SJS", "positions": ["LW"], "career_gp": 0,
                       "birth_date": "2007-09-30"});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    if v.get("draft_overall").is_some_and(|x| !x.is_null()) && v.get("draft_year").is_none() {
        v["draft_year"] = json!(2026);
    }
    player(v)
}

fn base_rates() -> Map<String, f64> {
    [("G", 0.2), ("A", 0.3), ("PTS", 0.5), ("SOG", 2.0), ("PPP", 0.12), ("HIT", 0.8)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

#[test]
fn age_multiplier_table() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    for (league, age, mult) in [
        ("OHL", Some(17.0), 1.16),
        ("OHL", Some(18.5), 1.04),
        ("OHL", Some(19.5), 1.0),
        ("OHL", Some(15.0), 1.24),
        ("SHL", Some(18.0), 1.24),
        ("AHL", Some(21.0), 1.08),
        ("AHL", Some(23.0), 1.0),
        ("NCAA", Some(20.0), 1.08),
        ("WJC-20", Some(17.0), 1.0),
        ("SHL", None, 1.0),
    ] {
        assert_close!(age_multiplier(league, age, None, "F"), mult);
    }
    assert_close!(age_multiplier("NHL", Some(20.3), None, "F"), vp::age_factor("F", Some(20.3), None, None));
}

#[test]
fn stenberg_nhle_by_hand_and_prior_golden() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let h = landing_history(STENBERG);
    let est = nhle_estimate(&h, None, "F", true, None).unwrap();
    let shl25 = 33.0 / 43.0 * 0.575 * 1.24;
    let j20 = 53.0 / 27.0 * 0.1 * 1.16;
    let shl24 = 3.0 / 25.0 * 0.575 * 1.24;
    let expected = (2.0 * 43.0 * shl25 + 27.0 * j20 + 25.0 * shl24) / (2.0 * 43.0 + 27.0 + 25.0);
    assert!((est.pts_pg - expected).abs() < 1e-6 * expected);
    assert_eq!(est.gp, 95);
    // golden: the whole prior as Python dumps it
    let pr = rookie_prior(&sk("s", json!({"draft_overall": 2})), &h, None, Some(19.0), true, None);
    let got = serde_json::to_value(&pr).unwrap();
    let want: Value = serde_json::from_str(GOLD_PRIOR).unwrap();
    assert_eq!(got, want);
}

const GOLD_PRIOR: &str = r##"{"pts_pg_nhle": 0.4010507246376812, "nhle_confidence": 0.7578947368421053, "nhle_gp": 95,
 "nhle_rows": [
  {"season": 20252026, "league": "SHL", "team": "Frölunda HC", "gp": 43, "g": 11, "a": 22, "pts": 33, "pts_pg": 0.767, "factor": 0.575, "age": 18.0, "age_mult": 1.24, "nhle_pts_pg": 0.547, "weight": 2.0},
  {"season": 20242025, "league": "J20 Nationell", "team": "Frölunda HC U20", "gp": 27, "g": 26, "a": 27, "pts": 53, "pts_pg": 1.963, "factor": 0.1, "age": 17.0, "age_mult": 1.16, "nhle_pts_pg": 0.228, "weight": 1.0},
  {"season": 20242025, "league": "SHL", "team": "Frölunda HC", "gp": 25, "g": 1, "a": 2, "pts": 3, "pts_pg": 0.12, "factor": 0.575, "age": 17.0, "age_mult": 1.24, "nhle_pts_pg": 0.086, "weight": 1.0}],
 "g_share": 0.4186046511627907, "sog_pg_est": 1.5261972607776453, "pedigree_pts_pg": 0.75, "pedigree_weight": 15.0,
 "pedigree_label": "#2 overall 2026 (top-3 pick)", "gp_expectation": 1.0, "confidence": 0.531,
 "reasons": [
  {"code": "NHLE", "text": "NHL equivalency 0.40 PTS/GP from 2025-26 SHL 43 GP 33 PTS x0.575 x1.24 age; 2024-25 J20 Nationell 27 GP 53 PTS x0.1 x1.16 age + SHL 25 GP 3 PTS x0.575 x1.24 age (seasons weighted 2:1 by GP; approximate league factors, 76% confidence)", "value": 0.4011, "baseline": 0.758},
  {"code": "PEDIGREE_PRIOR", "text": "Draft pedigree #2 overall 2026 (top-3 pick): prior 0.75 PTS/GP (approximate draft-tier rookie rates), vote weight 15 games", "value": 0.75, "baseline": 15.0}]}"##;

#[test]
fn nhle_nhl_rows_and_current_season() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let h = landing_history(NYGARD);
    let leagues = |e: &NhleEstimate| -> HashSet<String> {
        e.rows.iter().map(|r| r["league"].as_str().unwrap().to_string()).collect()
    };
    let with_nhl = nhle_estimate(&h, None, "F", true, None).unwrap();
    let no_nhl = nhle_estimate(&h, None, "F", false, None).unwrap();
    assert!(leagues(&with_nhl).contains("NHL") && !leagues(&no_nhl).contains("NHL"));
    assert!(no_nhl.pts_pg > with_nhl.pts_pg);
    let cur = nhle_estimate(&h, None, "F", true, Some(20252026)).unwrap();
    assert!(!cur.rows.iter().any(|r| r["league"] == "NHL" && r["season"] == 20252026));
    let row = |league: &str, gp: i64, pts: i64| LeagueSeason {
        season: 20252026,
        league: league.into(),
        team: None,
        gp,
        g: 0,
        a: 0,
        pts,
        age_at_season: None,
    };
    assert!(nhle_estimate(&[row("OHL", 5, 9)], None, "F", true, None).is_none());
    assert!(nhle_estimate(&[row("WJC-20", 7, 10)], None, "F", true, None).is_none());
}

#[test]
fn pedigree_tiers_and_fading() {
    for (pick, pos, pts) in [
        (1, "C", 0.75),
        (3, "LW", 0.75),
        (4, "C", 0.6),
        (10, "RW", 0.6),
        (23, "RW", 0.45),
        (50, "C", 0.35),
        (120, "C", 0.3),
        (2, "D", 0.75 * 0.7),
    ] {
        let got = pedigree_prior(&sk("p", json!({"draft_overall": pick, "positions": [pos]})), Some(19.0)).unwrap();
        assert_close!(got.0, pts);
        assert_close!(got.1, W_PEDIGREE);
    }
    assert!(pedigree_prior(&sk("p", json!({})), Some(19.0)).is_none());
    assert_close!(pedigree_prior(&sk("p", json!({"draft_overall": 2})), Some(24.0)).unwrap().1, W_PEDIGREE / 2.0);
    assert_close!(
        pedigree_prior(&sk("p", json!({"draft_overall": 2, "career_gp": 100})), Some(19.0)).unwrap().1,
        W_PEDIGREE / 2.0
    );
    assert!(pedigree_prior(&sk("p", json!({"draft_overall": 2})), Some(25.0)).is_none());
    assert!(pedigree_prior(&sk("p", json!({"draft_overall": 2, "career_gp": 250})), Some(19.0)).is_none());
}

fn rsig(kind: &str, direction: i64, days_ago: i64, conf: f64, quote: Option<&str>) -> RoleSignal {
    let now = Utc.with_ymd_and_hms(2026, 9, 28, 18, 0, 0).unwrap() - Duration::days(days_ago);
    RoleSignal {
        player_name: Some("s".into()),
        cid: None,
        kind: kind.into(),
        direction,
        confidence: conf,
        quote: quote.map(str::to_string).unwrap_or_else(|| format!("{kind} quote")),
        published: Some(PyDateTime::from_utc(now)),
        source: "rotowire".into(),
        item_id: None,
        origin: "rules".into(),
        ambiguous: false,
    }
}

fn rv(p: Option<Player>, signals: &[RoleSignal], proj_gp: Option<i64>, season_gp: i64) -> RookieEstimate {
    let p = p.unwrap_or_else(|| sk("s", json!({"draft_overall": 30})));
    let base = base_rates();
    rookie_value(
        &p,
        None,
        signals,
        RookieInputs {
            provider_projection: Some(&base),
            deployment: Some(&p),
            projection_gp: proj_gp,
            season_gp,
            as_of: Some(rookie_as_of()),
            ..Default::default()
        },
    )
}
use fm_core::values::RookieEstimate;

#[test]
fn projection_only_is_unchanged_and_votes_blend() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let p = sk("p", json!({}));
    let base = base_rates();
    let pr = rookie_prior(&p, &[], None, Some(19.0), true, None);
    let est = rookie_value(
        &p,
        Some(&pr),
        NO_NEWS,
        RookieInputs { provider_projection: Some(&base), as_of: Some(rookie_as_of()), ..Default::default() },
    );
    assert_eq!(est.rates, base);
    assert_close!(est.pts_pg.unwrap(), 0.5);
    assert_close!(est.confidence, 0.5);
    assert!(!est.reasons.iter().any(|r| r.code == "ROOKIE_BLEND"));
    // golden: NHLe + pedigree votes on the Stenberg prior
    let s = sk("s", json!({"draft_overall": 2}));
    let pr = rookie_prior(&s, &landing_history(STENBERG), None, Some(19.0), true, None);
    let est = rookie_value(
        &s,
        Some(&pr),
        NO_NEWS,
        RookieInputs { provider_projection: Some(&base), as_of: Some(rookie_as_of()), ..Default::default() },
    );
    assert_eq!(est.pts_pg, Some(0.5087940884585468));
    assert_eq!(est.rates["G"], 0.20351763538341872);
    assert_eq!(est.rates["PPP"], 0.12211058123005122);
    assert_eq!(est.rates["HIT"], 0.8);
    assert_eq!((est.confidence, est.gp_expectation, est.week_share), (0.681, 0.9, 0.9));
    assert_eq!(
        serde_json::to_value(&est.votes).unwrap(),
        json!([{"name": "baseline", "pts_pg": 0.5, "weight": 40.0}, {"name": "NHLe", "pts_pg": 0.4011, "weight": 30.32},
               {"name": "pedigree", "pts_pg": 0.75, "weight": 15.0}])
    );
    let tail: Vec<&Reason> = est.reasons.iter().skip(2).collect();
    assert_eq!(
        tail,
        vec![
            &reason(
                "ROOKIE_BLEND",
                "Rookie blend: PTS/GP 0.50 -> 0.51 from baseline 0.50 (w 40) + NHLe 0.40 (w 30) + pedigree 0.75 (w 15)",
                0.5088,
                0.5
            ),
            &reason("GP_EXPECTATION", "Expected games share 90%: on SJS as a top-10 pick, roster spot not confirmed 90%", 0.9, 1.0),
        ]
    );
}

#[test]
fn no_projection_builds_rates_from_the_priors() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let p = sk("s", json!({"draft_overall": 2}));
    let prior = rookie_prior(&p, &landing_history(STENBERG), None, Some(19.0), true, None);
    let mean: Map<String, f64> = [("G", 0.25), ("A", 0.35), ("PTS", 0.6), ("SOG", 2.2), ("PPP", 0.18), ("HIT", 1.1), ("BLK", 0.5)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let est = rookie_value(
        &p,
        Some(&prior),
        NO_NEWS,
        RookieInputs { mean: Some(&mean), as_of: Some(rookie_as_of()), ..Default::default() },
    );
    let wn = W_NHLE * prior.nhle_confidence;
    let post = (wn * prior.pts_pg_nhle.unwrap() + W_PEDIGREE * 0.75) / (wn + W_PEDIGREE);
    let r = &est.rates;
    assert_close!(r["PTS"], post);
    assert_close!(r["G"], post * prior.g_share.unwrap());
    assert_close!(r["G"] + r["A"], post);
    assert_close!(r["SOG"], r["G"] / 0.11);
    assert_close!(r["PPP"], post * 0.18 / 0.6);
    assert_eq!((r["HIT"], r["BLK"]), (1.1, 0.5));
    assert_eq!(r.keys().collect::<Vec<_>>(), vec!["HIT", "BLK", "PTS", "G", "A", "PPP", "PPG", "PPA", "SOG"]);
    let x = sk("x", json!({}));
    let nothing = rookie_value(
        &x,
        Some(&rookie_prior(&x, &[], None, Some(19.0), true, None)),
        NO_NEWS,
        RookieInputs { provider_projection: Some(&Map::new()), as_of: Some(rookie_as_of()), ..Default::default() },
    );
    assert!(nothing.rates.is_empty());
}

#[test]
fn incomparable_baseline_is_not_blended() {
    let _g = lock();
    vp::install(vp::load_packaged(None));
    let p = sk("s", json!({"draft_overall": 2}));
    let goals_only: Map<String, f64> = [("G".to_string(), 3.17)].into_iter().collect();
    let est = rookie_value(
        &p,
        Some(&rookie_prior(&p, &landing_history(STENBERG), None, Some(19.0), true, None)),
        NO_NEWS,
        RookieInputs { provider_projection: Some(&goals_only), as_of: Some(rookie_as_of()), ..Default::default() },
    );
    assert_eq!(est.rates, goals_only);
    assert!(est.reasons[0].text.contains("not blended"));
    assert!(!comparable_baseline(&goals_only) && comparable_baseline(&base_rates()));
}

#[test]
fn role_boost_from_news_and_dfo() {
    let plain = rv(None, NO_NEWS, None, 0);
    assert_eq!(plain.role_mult, 1.0);
    assert_close!(plain.gp_expectation, 0.75);
    let top = rv(None, &[rsig("top_line", 1, 1, 0.8, Some("skated on the top line with Celebrini"))], None, 0);
    assert_eq!(top.role_mult, ROLE_BOOST);
    assert_close!(top.rates["PTS"], 0.5 * ROLE_BOOST);
    assert_close!(top.gp_expectation, 0.95);
    let news = top.reasons.iter().find(|r| r.code == "ROLE_NEWS").unwrap();
    assert!(news.text.contains("skated on the top line with Celebrini") && news.text.contains("rotowire, 2026-09-27"));
    let dfo = rv(Some(sk("s", json!({"draft_overall": 30, "line": "f1", "pp_unit": "pp1"}))), NO_NEWS, None, 0);
    assert_eq!((dfo.role_mult, dfo.gp_expectation), (ROLE_BOOST, 1.0));
    let codes: HashSet<&str> = dfo.reasons.iter().map(|r| r.code.as_str()).collect();
    assert!(["ROLE_DFO", "ROLE_BOOST", "GP_EXPECTATION"].iter().all(|c| codes.contains(c)));
    assert_eq!(
        dfo.signals[0],
        serde_json::from_value::<Map<String, Value>>(json!({"source": "dfo", "origin": "dfo", "published": null,
            "confidence": 0.8, "kind": "nhl_roster", "direction": 1, "quote": "Daily Faceoff lineup: F1"}))
        .unwrap()
    );
    assert_eq!(
        rv(Some(sk("s", json!({"draft_overall": 30, "line": "f2", "pp_unit": "pp2"}))), NO_NEWS, None, 0).role_mult,
        ROLE_MINOR_BOOST
    );
    assert_close!(rv(Some(sk("s", json!({"draft_overall": 30, "line": "f4"}))), NO_NEWS, None, 0).role_mult, 0.92);
    assert_eq!(rv(None, &[rsig("pp1", 1, 1, 0.8, None), rsig("top_line", 1, 1, 0.8, None)], None, 0).role_mult, ROLE_BOOST);
}

#[test]
fn news_role_and_demotion_golden() {
    let est = rv(
        None,
        &[rsig("top_line", 1, 1, 0.8, Some("skated on the top line with Celebrini")), rsig("ahl_demotion", -1, 3, 0.8, None)],
        Some(45),
        0,
    );
    let want: Value = json!({
      "rates": {"G": 0.22999999999999998, "A": 0.345, "PTS": 0.575, "SOG": 2.3, "PPP": 0.13799999999999998, "HIT": 0.8},
      "gp_expectation": 0.05, "week_share": 0.05, "confidence": 0.5, "pts_pg": 0.5, "role_mult": 1.15,
      "baseline_pts_pg": 0.5, "votes": [{"name": "baseline", "pts_pg": 0.5, "weight": 40.0}],
      "signals": [
       {"kind": "top_line", "direction": 1, "confidence": 0.8, "quote": "skated on the top line with Celebrini",
        "published": "2026-09-27T18:00:00+00:00", "source": "rotowire", "origin": "rules", "ambiguous": false},
       {"kind": "ahl_demotion", "direction": -1, "confidence": 0.8, "quote": "ahl_demotion quote",
        "published": "2026-09-25T18:00:00+00:00", "source": "rotowire", "origin": "rules", "ambiguous": false}],
      "prior": null, "history": [],
      "reasons": [
       {"code": "ROLE_NEWS", "text": "top line +: \"skated on the top line with Celebrini\" (rotowire, 2026-09-27)", "value": 1.0, "baseline": 0.8},
       {"code": "ROLE_NEWS", "text": "ahl demotion -: \"ahl_demotion quote\" (rotowire, 2026-09-25)", "value": -1.0, "baseline": 0.8},
       {"code": "ROLE_BOOST", "text": "Role x1.15 on scoring rates (top line from rotowire)", "value": 1.15, "baseline": 1.0},
       {"code": "GP_EXPECTATION", "text": "Expected games share 5%: on SJS, roster spot not confirmed 75%; 50% weight on the projection's 45 GP (60%); latest news \"ahl_demotion quote\" (rotowire, 2026-09-25) -> 5%", "value": 0.05, "baseline": 1.0}]
    });
    assert_eq!(serde_json::to_value(&est).unwrap(), want);
}

#[test]
fn demotion_recall_scratch_stale_ambiguous() {
    assert_eq!(rv(None, &[rsig("junior_return", -1, 1, 0.8, None)], None, 0).gp_expectation, GP_JUNIOR);
    let back = rv(None, &[rsig("ahl_demotion", -1, 5, 0.8, None), rsig("nhl_roster", 1, 1, 0.8, None)], None, 0);
    assert_eq!(back.gp_expectation, 1.0);
    let again = rv(None, &[rsig("nhl_roster", 1, 5, 0.8, None), rsig("ahl_demotion", -1, 1, 0.8, None)], None, 0);
    assert_eq!(again.gp_expectation, GP_AHL);
    let sc = rv(None, &[rsig("nhl_roster", 1, 3, 0.8, None), rsig("scratched", -1, 1, 0.8, None)], None, 0);
    assert_close!(sc.gp_expectation, 0.85);
    assert_close!(sc.week_share, 0.85);
    assert_eq!(rv(None, &[rsig("top_line", 1, 30, 0.8, None)], None, 0).role_mult, 1.0);
    let mut amb = rsig("top_line", 1, 1, 0.8, None);
    amb.ambiguous = true;
    assert_eq!(rv(None, &[amb], None, 0).role_mult, 1.0);
    assert_eq!(rv(None, &[rsig("top_line", 1, 1, 0.5, None)], None, 0).role_mult, 1.0);
    let mut llm = rsig("top_line", 1, 1, 0.65, None);
    llm.origin = "llm".into();
    assert_eq!(rv(None, &[llm.clone()], None, 0).role_mult, 1.0);
    llm.confidence = 0.8;
    assert_eq!(rv(None, &[llm], None, 0).role_mult, ROLE_BOOST);
}

#[test]
fn games_share_from_projection_team_and_season_games() {
    assert_close!(rv(Some(sk("s", json!({"draft_overall": 5}))), NO_NEWS, None, 0).gp_expectation, 0.9);
    assert_close!(rv(Some(sk("s", json!({"draft_overall": 30, "team": null}))), NO_NEWS, None, 0).gp_expectation, 0.3);
    assert_close!(rv(None, NO_NEWS, Some(45), 0).gp_expectation, 0.5 * 0.75 + 0.5 * 45.0 / 75.0);
    assert_eq!(rv(None, NO_NEWS, None, 10).gp_expectation, 1.0);
    let confirmed = rv(Some(sk("s", json!({"draft_overall": 30, "line": "f2"}))), NO_NEWS, Some(45), 0);
    assert_close!(confirmed.gp_expectation, 0.75 * 1.0 + 0.25 * 45.0 / 75.0);
}

#[test]
fn rookie_evidence_and_unproven() {
    let bare = player(json!({"cid": "b"}));
    assert!(!has_rookie_evidence::<RoleSignal>(&bare, &[], &[]));
    assert!(has_rookie_evidence::<RoleSignal>(&sk("x", json!({})), &[], &[])); // career_gp known
    let mut lined = bare.clone();
    lined.pp_unit = Some("pp2".into());
    assert!(has_rookie_evidence::<RoleSignal>(&lined, &[], &[]));
    assert!(rookie::is_unproven(&sk("x", json!({}))));
    assert_eq!(position_group(&player(json!({"cid": "d", "positions": ["D"]}))), "D");
    assert_eq!(position_group(&player(json!({"cid": "d", "positions": ["D", "RW"]}))), "F");
}

// ============================================================================ snapshot parity

fn fixtures_dir() -> PathBuf {
    match std::env::var("FM_PARITY_FIXTURES") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures"),
    }
}

fn load(league: &str) -> Option<(Snapshot, Map<String, Value>)> {
    let dir = fixtures_dir().join(league);
    let snap = Snapshot::load(dir.join("snapshot.json")).ok()?;
    let values: Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("values.json")).ok()?).ok()?;
    Some((snap, values))
}

fn reasons_of(v: &Value) -> Vec<Reason> {
    serde_json::from_value(v["reasons"].clone()).unwrap()
}

fn season_age(p: &Player, as_of: NaiveDate) -> Option<f64> {
    let y = if as_of.month() >= 9 { as_of.year() } else { as_of.year() - 1 };
    let b = p.birth_date?;
    Some(fm_pycompat::num::round_to((d(y, 10, 1) - b).num_days() as f64 / 365.25, 3))
}

/// Compare JSON with a relative float tolerance.
fn json_close(a: &Value, b: &Value, path: &str) {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            assert!(close(x, y), "{path}: {x} != {y}");
        }
        (Value::Array(x), Value::Array(y)) => {
            assert_eq!(x.len(), y.len(), "{path}: length");
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                json_close(p, q, &format!("{path}[{i}]"));
            }
        }
        (Value::Object(x), Value::Object(y)) => {
            assert_eq!(x.keys().collect::<Vec<_>>(), y.keys().collect::<Vec<_>>(), "{path}: keys");
            for (k, p) in x {
                json_close(p, &y[k], &format!("{path}.{k}"));
            }
        }
        _ => assert_eq!(a, b, "{path}"),
    }
}

fn parity_league(league: &str) {
    let Some((snap, values)) = load(league) else {
        eprintln!("SKIP parity {league}: fixtures not found under {}", fixtures_dir().display());
        return;
    };
    let _g = lock();
    vp::install(snap.params.merged.clone());
    let ctx = &snap.ctx;
    let players: Map<String, &Player> = ctx.all_players().into_iter().map(|p| (p.cid.clone(), p)).collect();
    let memo = ScheduleMemo::new(None); // the dump ran on a data dir without an odds archive
    let window = context_window(ctx, WEEK_DAYS).filter(|w| w.avg_team_games > 0.0);
    let pp_teams: HashSet<Option<String>> = players
        .values()
        .filter(|p| p.pp_unit.as_deref().is_some_and(|s| !s.is_empty()))
        .map(|p| normalize_team(p.team.as_deref()))
        .collect();
    let lineup_teams: HashSet<Option<String>> = players
        .values()
        .filter(|p| p.line.as_deref().is_some_and(|s| !s.is_empty()))
        .map(|p| normalize_team(p.team.as_deref()))
        .collect();
    let role_codes = ["ROLE_DFO", "ROLE_NEWS", "ROLE_BOOST", "ROLE_DEPTH", "ROLE_SCRATCH"];
    let (mut n_games, mut n_opp, mut n_b2b, mut n_role, mut n_role_changes, mut n_rookie) = (0, 0, 0, 0, 0, 0);
    for (cid, v) in &values {
        let p = players[cid];
        let reasons = reasons_of(v);
        let team = normalize_team(p.team.as_deref());
        // ---- games_next7 / offnight_next7 / OPP_ADJ_WEEK
        let sf = window.as_ref().and_then(|w| {
            schedule_factor(p, &ctx.schedule, &ctx.games_per_day, w, OFFNIGHT_THRESHOLD, Some(1.0), None)
        });
        if let Some(sf) = &sf {
            assert_eq!(v["games_next7"].as_i64(), Some(sf.games), "{league} {cid} games_next7");
            assert_eq!(v["offnight_next7"].as_i64(), Some(sf.offnight), "{league} {cid} offnight_next7");
            n_games += 1;
            let opp = if sf.games > 0 { opp_week_factor(ctx, &memo, p, window.as_ref()) } else { 1.0 };
            match reasons.iter().find(|r| r.code == OPP_WEEK_REASON) {
                Some(r) => {
                    assert_eq!(r.value, Some(opp), "{league} {cid} OPP_ADJ_WEEK");
                    n_opp += 1;
                }
                None => assert!((opp - 1.0).abs() <= 1e-4 || v["proj_week"].is_null(), "{league} {cid} opp {opp}"),
            }
        }
        // ---- B2B week start share
        if let Some(r) = reasons.iter().find(|r| r.code == "B2B") {
            let b2b = ctx.b2b_second_night[team.as_deref().unwrap()].0;
            let got = schedule_factor(
                p,
                &ctx.schedule,
                &ctx.games_per_day,
                window.as_ref().unwrap(),
                OFFNIGHT_THRESHOLD,
                r.baseline,
                Some(b2b),
            )
            .unwrap();
            assert_eq!(Some(got.start_share), r.value, "{league} {cid} B2B");
            n_b2b += 1;
        }
        // ---- established-skater role change
        let rates_empty = v["rates"].as_object().is_none_or(|m| m.is_empty());
        let news = ctx.enrich.signals_for(p);
        if !p.is_goalie() && !rates_empty && !rookie::is_unproven(p) {
            let rc = role_change(p, news, Some(ctx.as_of), pp_teams.contains(&team), lineup_teams.contains(&team));
            let want: Vec<Reason> = reasons.iter().filter(|r| role_codes.contains(&r.code.as_str())).cloned().collect();
            let got = rc.map(|r| r.reasons).unwrap_or_default();
            assert_eq!(got, want, "{league} {cid} role change");
            n_role += 1;
            n_role_changes += usize::from(!want.is_empty());
        }
        // ---- goalie job news (only when the share is not from ledger starts / teammates)
        // ---- rookie prior + estimate
        if let Some(rk) = v.get("rookie").filter(|r| !r.is_null()) {
            let hist = ctx.enrich.history_for(p);
            let age = season_age(p, ctx.as_of);
            let had_nhl = ["prior", "prior2", "prior3"].iter().any(|s| p.gp(s) > 0);
            let start = if ctx.as_of.month() >= 9 { ctx.as_of.year() } else { ctx.as_of.year() - 1 } as i64;
            let before = start * 10000 + start + 1;
            let prior = rookie_prior(p, hist, None, age, !had_nhl, Some(before));
            json_close(&serde_json::to_value(&prior).unwrap(), &rk["prior"], &format!("{league} {cid} prior"));
            let rk_reasons: Vec<Reason> = serde_json::from_value(rk["reasons"].clone()).unwrap();
            if rk_reasons.iter().any(|r| r.text.contains("not blended")) {
                continue;
            }
            // the estimate's inputs that are W4's (baseline rates) are rebuilt from what it recorded
            let base: Option<Map<String, f64>> =
                rk["baseline_pts_pg"].as_f64().map(|b| [("PTS".to_string(), b)].into_iter().collect());
            let votes = rk["votes"].as_array().unwrap();
            let baseline_gp = votes
                .iter()
                .find(|x| x["name"] == "baseline")
                .map(|x| (x["weight"].as_f64().unwrap() - W_BASELINE).round())
                .unwrap_or(0.0);
            let proj_gp = p.lines.get("projected").map(|l| l.gp).filter(|g| *g > 0);
            let est = rookie_value(
                p,
                Some(&prior),
                news,
                RookieInputs {
                    provider_projection: base.as_ref(),
                    deployment: Some(p),
                    baseline_gp,
                    projection_gp: proj_gp,
                    season_gp: p.gp("season"),
                    as_of: Some(ctx.as_of),
                    ..Default::default()
                },
            );
            let tag = format!("{league} {cid} rookie");
            for k in ["gp_expectation", "week_share", "confidence", "pts_pg", "role_mult", "baseline_pts_pg"] {
                json_close(&serde_json::to_value(&est).unwrap()[k], &rk[k], &format!("{tag}.{k}"));
            }
            json_close(&serde_json::to_value(&est.votes).unwrap(), &rk["votes"], &format!("{tag}.votes"));
            assert_eq!(serde_json::to_value(&est.signals).unwrap(), rk["signals"], "{tag}.signals");
            assert_eq!(est.reasons.len(), rk_reasons.len(), "{tag} reasons");
            for (g, w) in est.reasons.iter().zip(&rk_reasons) {
                assert_eq!((g.code.as_str(), g.value, g.baseline), (w.code.as_str(), w.value, w.baseline), "{tag}");
                if g.code == "ROOKIE_BLEND" {
                    assert!(w.text.starts_with(&g.text), "{tag}: {} vs {}", g.text, w.text); // + FPG part
                } else {
                    assert_eq!(g.text, w.text, "{tag}");
                }
            }
            n_rookie += 1;
        }
    }
    eprintln!(
        "parity {league}: games {n_games}, OPP_ADJ_WEEK {n_opp}, B2B {n_b2b}, role checks {n_role} \
         ({n_role_changes} with changes), rookies {n_rookie}"
    );
    assert!(n_games > 100 && n_opp > 50 && n_role > 100 && n_rookie > 10, "{league}: too few checks");
    vp::install(vp::load_packaged(None));
}

#[test]
fn parity_espn() {
    parity_league("espn");
}

#[test]
fn parity_fantrax() {
    parity_league("fantrax");
}

#[test]
fn parity_team_factors_feed_the_opponent_table() {
    let Some((snap, _)) = load("espn") else { return };
    let _g = lock();
    vp::install(snap.params.merged.clone());
    let ctx = &snap.ctx;
    let ga = team_ga_factors(ctx);
    let gf = team_gf_factors(ctx);
    assert!(!ga.is_empty() && !gf.is_empty());
    for (_, (f, n)) in ga.iter().chain(gf.iter()) {
        assert!(*f > 0.5 && *f < 1.5 && *n >= GF_MIN_GAMES);
    }
    vp::install(vp::load_packaged(None));
}
