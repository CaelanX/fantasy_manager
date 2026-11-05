//! Tests for `W2-nhl-matching`: NHL client/parsers, matcher (rapidfuzz parity), crosswalk, news
//! match. Ports of tests/test_nhl.py, test_matching.py, test_crosswalk.py, test_news_match.py plus
//! goldens generated with rapidfuzz 3.14 (`tests/golden/*.json`, see the generator notes in the
//! report) and a fresh-crosswalk resolve over the parity tape (skipped when the tape is absent).

use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use fm_core::{Map, Player, PyDateTime};
use fm_http::cache::ParamValue;
use fm_http::Params;
use fm_sources::matching::crosswalk::{
    candidates_of, nhl_candidates, parse_confirm, split_cid, Crosswalk, ResolveStats, SourceItem,
};
use fm_sources::matching::matcher::{
    build_index, candidates_from, match_player_in, token_sort_ratio, Candidate, Confidence, Key, PlayerIndex,
};
use fm_sources::news::NewsItem;
use fm_sources::news_match::{match_news_to_players, name_keys, player_news_summary};
use fm_sources::nhl::{self, *};
use serde_json::{json, Value};

fn root() -> PathBuf {
    std::env::var("FM_REPO_ROOT").map(PathBuf::from).unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."))
}
fn fix_dir() -> PathBuf {
    root().join("tests/fixtures/nhl")
}
fn golden(name: &str) -> Value {
    let p = root().join("rust/crates/fm-sources/tests/golden").join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}
fn load(name: &str) -> Value {
    let p = fix_dir().join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}
fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

const ROUTES: [(&str, &str); 12] = [
    ("/skater/summary", "skater_summary.json"),
    ("/skater/realtime", "skater_realtime.json"),
    ("/skater/faceoffwins", "skater_faceoffwins.json"),
    ("/goalie/summary", "goalie_summary.json"),
    ("/player/8478402/landing", "player_landing_8478402.json"),
    ("/player/8478402/game-log/20252026/2", "game_log_8478402.json"),
    ("/player/8476883/game-log/20252026/2", "game_log_goalie_8476883.json"),
    ("/club-schedule-season/EDM/20262027", "club_schedule_EDM.json"),
    ("/club-schedule-season/CGY/20262027", "club_schedule_CGY.json"),
    ("/roster/EDM/20262027", "roster_EDM.json"),
    ("/schedule/2026-10-07", "schedule_2026-10-07.json"),
    ("/search/player", "search_sebastian_aho.json"),
];

type Calls = Arc<Mutex<Vec<(String, Params)>>>;

/// A client over `route(url, params)` that records every (url, params).
fn recording_client(season: i64, route: impl Fn(&str, &Params) -> anyhow::Result<Value> + Send + Sync + 'static) -> (NhlClient, Calls) {
    let calls: Calls = Arc::new(Mutex::new(vec![]));
    let c2 = calls.clone();
    let fetch = move |url: &str, params: &Params, _ttl: f64| -> anyhow::Result<Value> {
        c2.lock().unwrap().push((url.to_string(), params.clone()));
        route(url, params)
    };
    (NhlClient::new(Box::new(fetch), Some(season), d(2026, 9, 28)), calls)
}

/// A client over the recorded fixtures.
struct Fixture {
    client: NhlClient,
    calls: Calls,
}

fn fixture_client() -> Fixture {
    let cache: HashMap<String, Value> = ROUTES.iter().map(|(s, f)| (s.to_string(), load(f))).collect();
    let (client, calls) = recording_client(20262027, move |url, _| {
        for (suffix, _) in ROUTES.iter() {
            if url.ends_with(suffix) {
                return Ok(cache[*suffix].clone());
            }
        }
        anyhow::bail!("unexpected url {url}")
    });
    Fixture { client, calls }
}

impl Fixture {
    fn calls(&self) -> Vec<(String, Params)> {
        self.calls.lock().unwrap().clone()
    }
}

fn pstr<'a>(p: &'a Params, k: &str) -> Option<&'a str> {
    p.iter().find(|(kk, _)| kk == k).and_then(|(_, v)| if let ParamValue::Str(s) = v { Some(s.as_str()) } else { None })
}

// --------------------------------------------------------------------------- test_nhl.py

#[test]
fn current_season_rollover() {
    assert_eq!(current_season(d(2026, 9, 28)), 20262027);
    assert_eq!(current_season(d(2026, 9, 1)), 20262027);
    assert_eq!(current_season(d(2026, 8, 31)), 20252026);
    assert_eq!(current_season(d(2027, 3, 1)), 20262027);
    assert_eq!(prior_season(20262027), 20252026);
    let c = NhlClient::new(Box::new(|_, _, _| Ok(Value::Null)), None, d(2026, 11, 4));
    assert_eq!(c.season, 20262027);
}

#[test]
fn helpers() {
    assert_eq!(parse_toi_str("24:49"), Some(1489.0));
    assert_eq!(parse_toi(Some(&json!(1234.5))), Some(1234.5));
    assert_eq!(parse_toi(None), None);
    assert_eq!(parse_toi_str("1:02:03"), Some(3723.0));
    assert_eq!(parse_toi_str("x:3"), None);
    assert_eq!(split_teams(Some("SJS, TOR")), vec!["SJS", "TOR"]);
    assert!(split_teams(None).is_empty());
}

#[test]
fn report_params() {
    let f = fixture_client();
    f.client.skater_summary(Some(20252026), 2).unwrap();
    let (url, params) = f.calls().last().unwrap().clone();
    assert_eq!(url, "https://api.nhle.com/stats/rest/en/skater/summary");
    assert_eq!(pstr(&params, "cayenneExp"), Some("seasonId=20252026 and gameTypeId=2"));
    assert_eq!(pstr(&params, "limit"), Some("-1"));
    // json.dumps default spacing goes into the URL and the cache key
    assert_eq!(pstr(&params, "sort"), Some(r#"[{"property": "playerId", "direction": "ASC"}]"#));
    assert_eq!(pstr(&params, "isGame"), Some("false"));
    assert_eq!(pstr(&params, "start"), Some("0"));
}

#[test]
fn all_skaters_merge() {
    let f = fixture_client();
    let skaters = f.client.all_skaters(Some(20252026), 2, true, true).unwrap();
    assert_eq!(skaters.len(), 3);
    let mcd = skaters.iter().find(|s| s.player_id == 8478402).unwrap();
    assert_eq!(mcd.name, "Connor McDavid");
    assert_eq!(mcd.team.as_deref(), Some("EDM"));
    assert_eq!(mcd.position.as_deref(), Some("C"));
    for k in SKATER_STAT_KEYS {
        assert!(mcd.stats.contains_key(k), "{k}");
    }
    let row = &load("skater_summary.json")["data"][0];
    let rt = load("skater_realtime.json")["data"].as_array().unwrap().iter().find(|r| r["playerId"] == 8478402).unwrap().clone();
    let fo = load("skater_faceoffwins.json")["data"].as_array().unwrap().iter().find(|r| r["playerId"] == 8478402).unwrap().clone();
    assert_eq!(mcd.stats["PTS"], row["points"].as_f64().unwrap());
    assert_eq!(mcd.stats["PPP"], row["ppPoints"].as_f64().unwrap());
    assert_eq!(mcd.stats["SOG"], row["shots"].as_f64().unwrap());
    assert_eq!(mcd.stats["HIT"], rt["hits"].as_f64().unwrap());
    assert_eq!(mcd.stats["BLK"], rt["blockedShots"].as_f64().unwrap());
    assert_eq!(mcd.stats["FOW"], fo["totalFaceoffWins"].as_f64().unwrap());
    assert_eq!(mcd.toi_per_game, row["timeOnIcePerGame"].as_f64());
    assert_eq!(mcd.games_played() as f64, row["gamesPlayed"].as_f64().unwrap());
    let pg = mcd.per_game()["PTS"];
    assert!((pg - row["points"].as_f64().unwrap() / row["gamesPlayed"].as_f64().unwrap()).abs() < 1e-12);
    assert_eq!(skaters.iter().find(|s| s.player_id == 8476453).unwrap().position.as_deref(), Some("RW"));
}

#[test]
fn report_paginates_when_capped() {
    let rows: Vec<Value> = (0..5).map(|i| json!({"playerId": i})).collect();
    let rows2 = rows.clone();
    let fetch = move |_url: &str, params: &Params, _ttl: f64| -> anyhow::Result<Value> {
        let start: usize = pstr(params, "start").unwrap().parse().unwrap();
        if pstr(params, "limit") == Some("-1") {
            return Ok(json!({"data": rows2[..2], "total": 5}));
        }
        let end = (start + 2).min(5);
        Ok(json!({"data": rows2[start..end], "total": 5}))
    };
    let c = NhlClient::new(Box::new(fetch), Some(20252026), d(2026, 9, 28));
    let got = c.report("skater", "summary", None, 2, None).unwrap();
    let ids: Vec<i64> = got.iter().map(|r| r["playerId"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![0, 1, 2, 3, 4]);
}

#[test]
fn all_skaters_without_optional_reports() {
    let f = fixture_client();
    let skaters = f.client.all_skaters(Some(20252026), 2, false, false).unwrap();
    assert!(skaters.iter().all(|s| !s.stats.contains_key("FOW") && !s.stats.contains_key("HIT")));
    assert!(!f.calls().iter().any(|(u, _)| u.contains("realtime") || u.contains("faceoff")));
}

#[test]
fn traded_player_takes_last_team() {
    let mut row = load("skater_summary.json")["data"][0].clone();
    row["teamAbbrevs"] = json!("SJS, TOR");
    row["positionCode"] = json!("L");
    let s = nhl::skater_from_rows(&row, None, None, 2).unwrap();
    assert_eq!(s.team.as_deref(), Some("TOR"));
    assert_eq!(s.teams, vec!["SJS", "TOR"]);
    assert_eq!(s.position.as_deref(), Some("LW"));
    assert!(!s.stats.contains_key("HIT"));
}

#[test]
fn goalie_summary() {
    let f = fixture_client();
    let goalies = f.client.goalie_summary(Some(20252026), 2).unwrap();
    let g = &goalies[0];
    assert_eq!((g.name.as_str(), g.team.as_deref(), g.position.as_str()), ("Andrei Vasilevskiy", Some("TBL"), "G"));
    for k in GOALIE_STAT_KEYS {
        assert!(g.stats.contains_key(k), "{k}");
    }
    assert_eq!((g.stats["W"], g.stats["SV"], g.stats["SA"]), (39.0, 1353.0, 1483.0));
    assert!((g.stats["SVPCT"] - 0.91233).abs() < 1e-9);
}

#[test]
fn game_log_skater() {
    let f = fixture_client();
    let log = f.client.game_log(8478402, Some(20252026), 2).unwrap();
    assert_eq!(log.len(), 4);
    let e = &log[0];
    assert_eq!(e.date, d(2026, 4, 16));
    assert_eq!((e.opponent.as_deref(), e.home, e.team.as_deref()), (Some("VAN"), Some(true), Some("EDM")));
    assert_eq!((e.stats["A"], e.stats["PTS"], e.stats["PPP"], e.stats["SOG"]), (4.0, 4.0, 2.0, 3.0));
    assert_eq!(e.toi_seconds, Some((18 * 60 + 16) as f64));
    assert!(!e.is_goalie);
    assert_eq!(e.game_type, Some(2));
}

#[test]
fn game_log_goalie() {
    let f = fixture_client();
    let e = &f.client.game_log(8476883, Some(20252026), 2).unwrap()[0];
    assert!(e.is_goalie);
    assert_eq!(e.decision.as_deref(), Some("W"));
    assert_eq!((e.stats["W"], e.stats["L"]), (1.0, 0.0));
    assert_eq!((e.stats["SA"], e.stats["GA"], e.stats["SV"]), (30.0, 3.0, 27.0));
    assert!((e.stats["SVPCT"] - 0.9).abs() < 1e-9);
}

#[test]
fn player_landing() {
    let f = fixture_client();
    let p = f.client.player_landing(8478402).unwrap();
    assert_eq!(p.name(), "Connor McDavid");
    assert_eq!((p.team.as_deref(), p.position.as_deref()), (Some("EDM"), Some("C")));
    assert_eq!(p.birth_date, Some(d(1997, 1, 13)));
    assert_eq!(p.last5.len(), 5);
    assert_eq!(p.last5[0].toi_seconds, Some((24 * 60 + 49) as f64));
    assert_eq!(p.last5[0].home, Some(false));
    assert!(!p.featured_regular_season.is_empty());
}

#[test]
fn team_schedule_filters_regular_season() {
    let f = fixture_client();
    let games = f.client.team_schedule("EDM", None, Some(2)).unwrap();
    assert_eq!(games.len(), 10);
    assert!(games.iter().all(|g| g.game_type == 2));
    assert_eq!(games[0].date, d(2026, 9, 29));
    assert_eq!(games[0].game_id, 2026020004);
    assert!(games[0].teams().0 == "EDM" || games[0].teams().1 == "EDM");
    assert!(games.windows(2).all(|w| w[0].date <= w[1].date));
    assert_eq!(f.client.team_schedule("EDM", None, None).unwrap().len(), 12);
}

#[test]
fn league_schedule_and_window() {
    let f = fixture_client();
    let sched = f.client.league_schedule(None, &["EDM", "CGY"], Some(2)).unwrap();
    assert_eq!(sched.keys().cloned().collect::<Vec<_>>(), vec!["EDM", "CGY"]);
    let edm = &sched["EDM"];
    assert_eq!(games_in_window(edm, edm[0], edm[0]), 1);
    let cutoff = d(2026, 10, 13);
    assert_eq!(
        games_in_window(edm, d(2026, 10, 1), cutoff),
        edm.iter().filter(|x| d(2026, 10, 1) <= **x && **x <= cutoff).count()
    );
    let per_day = games_per_day(&sched);
    let mut want: Vec<NaiveDate> = edm.iter().chain(sched["CGY"].iter()).copied().collect();
    want.sort();
    want.dedup();
    assert_eq!(per_day.keys().copied().collect::<Vec<_>>(), want);
}

#[test]
fn games_per_day_off_nights_back_to_backs() {
    let mut sched: Map<String, Vec<NaiveDate>> = Map::new();
    sched.insert("AAA".into(), vec![d(2026, 10, 7)]);
    sched.insert("BBB".into(), vec![d(2026, 10, 7)]);
    for i in 0..16 {
        sched.insert(format!("T{i}"), vec![d(2026, 10, 8)]);
    }
    let per_day = games_per_day(&sched);
    let want: Map<NaiveDate, i64> = [(d(2026, 10, 7), 1), (d(2026, 10, 8), 8)].into_iter().collect();
    assert_eq!(per_day, want);
    assert_eq!(off_nights(&per_day, 8), [d(2026, 10, 7)].into_iter().collect());
    let b2b = back_to_backs(&[d(2026, 10, 7), d(2026, 10, 8), d(2026, 10, 10), d(2026, 10, 11)]);
    assert_eq!(b2b, vec![(d(2026, 10, 7), d(2026, 10, 8)), (d(2026, 10, 10), d(2026, 10, 11))]);
}

#[test]
fn team_roster_and_age() {
    let f = fixture_client();
    let roster = f.client.team_roster("EDM", None).unwrap();
    assert_eq!(roster.len(), 9);
    let dach = roster.iter().find(|p| p.last_name == "Dach").unwrap();
    assert_eq!((dach.player_id, dach.position.as_deref(), dach.team.as_str()), (8482703, Some("C"), "EDM"));
    assert_eq!(dach.name(), "Colton Dach");
    assert_eq!(dach.birth_date, Some(d(2003, 1, 4)));
    assert!((dach.age_on(d(2026, 1, 4)).unwrap() - 23.0).abs() < 1e-9);
    let a = dach.age_on(d(2026, 9, 28)).unwrap();
    assert!(23.7 < a && a < 23.8);
    let pos: std::collections::HashSet<_> = roster.iter().filter_map(|p| p.position.clone()).collect();
    assert!(pos.contains("D") && pos.contains("G"));
    assert_eq!(f.client.league_rosters(None, &["EDM"]).unwrap().keys().cloned().collect::<Vec<_>>(), vec!["EDM"]);
}

#[test]
fn week_schedule() {
    let f = fixture_client();
    let wk = f.client.week_schedule(Some(d(2026, 10, 7))).unwrap();
    assert!(wk.days.contains_key(&d(2026, 10, 7)));
    let g = &wk.days[&d(2026, 10, 7)][0];
    assert_eq!((g.away.as_str(), g.home.as_str(), g.game_type), ("PIT", "WSH", 2));
    assert!(g.start_time_utc.unwrap().isoformat().starts_with("2026-10-07T23:30"));
    assert!(wk.games_per_team()["PIT"] >= 1);
}

#[test]
fn search_player() {
    let f = fixture_client();
    let aho = &f.client.search_player("sebastian aho", Some(true), 20).unwrap()[0];
    assert_eq!((aho.player_id, aho.team.as_deref(), aho.position.as_deref()), (8478427, Some("CAR"), Some("C")));
    let (_, params) = f.calls().last().unwrap().clone();
    assert_eq!(pstr(&params, "q"), Some("sebastian aho"));
    assert_eq!(pstr(&params, "active"), Some("true"));
}

#[test]
fn nhl_teams_complete() {
    let set: std::collections::HashSet<_> = NHL_TEAMS.iter().collect();
    assert_eq!(NHL_TEAMS.len(), 32);
    assert_eq!(set.len(), 32);
}

#[test]
fn player_landing_draft_and_career() {
    let body = json!({"playerId": 8486103, "firstName": {"default": "Ivar"}, "lastName": {"default": "Stenberg"},
        "birthDate": "2007-09-30", "position": "C",
        "draftDetails": {"year": 2026, "teamAbbrev": "SJS", "round": 1, "pickInRound": 2, "overallPick": 2}});
    let b = body.clone();
    let c = NhlClient::new(Box::new(move |_, _, _| Ok(b.clone())), None, d(2026, 11, 4));
    let p = c.player_landing(8486103).unwrap();
    assert_eq!((p.draft_year, p.draft_round, p.draft_overall, p.draft_team.as_deref()), (Some(2026), Some(1), Some(2), Some("SJS")));
    assert_eq!(p.career_gp, Some(0));
    let body = json!({"playerId": 1, "careerTotals": {"regularSeason": {"gamesPlayed": 311}}});
    let c = NhlClient::new(Box::new(move |_, _, _| Ok(body.clone())), None, d(2026, 11, 4));
    let p = c.player_landing(1).unwrap();
    assert_eq!((p.draft_overall, p.career_gp), (None, Some(311)));
}

#[test]
fn ttl_policy() {
    let today = d(2026, 11, 4);
    let mk = |cay: &str| -> Params {
        vec![("isGame".into(), "true".into()), ("cayenneExp".into(), ParamValue::Str(cay.to_string()))]
    };
    assert_eq!(game_window_ttl(&mk(r#"seasonId=1 and gameDate<="2026-04-01""#), today), Some(TTL_GAME_WINDOW_CLOSED));
    assert_eq!(game_window_ttl(&mk(r#"seasonId=1 and gameDate<="2026-11-04""#), today), Some(TTL_GAME_WINDOW_OPEN));
    assert_eq!(game_window_ttl(&mk("seasonId=1"), today), Some(TTL_GAME_WINDOW_OPEN));
    assert_eq!(game_window_ttl(&vec![("isGame".into(), "false".into())], today), None);
    assert_eq!(
        game_window_filter(Some(d(2026, 4, 1)), Some(d(2026, 4, 2))).as_deref(),
        Some(r#"gameDate>="2026-04-01" and gameDate<="2026-04-02""#)
    );
}

// --------------------------------------------------------------------------- test_deployment.py (provider part)

const DAY: (i32, u32, u32) = (2026, 4, 1);

fn deploy_client(fail: &'static [&'static str]) -> (NhlClient, Calls) {
    let routes: Vec<((&str, &str), Value)> = vec![
        (("skater/timeonice", "true"), load("deploy_toi_games_2026-04-01.json")),
        (("skater/powerplay", "true"), load("deploy_pp_games_2026-04-01.json")),
        (("goalie/summary", "true"), load("deploy_goalie_games_2026-04-01.json")),
        (("team/powerplay", "true"), load("deploy_team_pp_games_2026-04-01.json")),
        (("skater/timeonice", "false"), load("deploy_season_timeonice_20252026.json")),
        (("skater/powerplay", "false"), load("deploy_season_powerplay_20252026.json")),
    ];
    recording_client(20252026, move |url, params| {
        let path = url.rsplit("/stats/rest/en/").next().unwrap().to_string();
        if fail.contains(&path.as_str()) {
            anyhow::bail!("boom");
        }
        let is_game = pstr(params, "isGame").unwrap_or("");
        let Some((_, body)) = routes.iter().find(|((p, g), _)| *p == path && *g == is_game) else {
            anyhow::bail!("unexpected {url}");
        };
        let cay = pstr(params, "cayenneExp").unwrap();
        if (is_game == "true" && !cay.contains("\"2026-04-01\"")) || (is_game == "false" && !cay.contains("seasonId=20252026")) {
            return Ok(json!({"data": [], "total": 0}));
        }
        Ok(body.clone())
    })
}

#[test]
fn skater_toi_games_parse_and_params() {
    let (c, calls) = deploy_client(&[]);
    let day = d(DAY.0, DAY.1, DAY.2);
    let rows = c.skater_toi_games(Some(20252026), Some(day), Some(day), 2).unwrap();
    let (url, params) = calls.lock().unwrap().last().unwrap().clone();
    assert!(url.ends_with("/skater/timeonice"));
    assert_eq!((pstr(&params, "isGame"), pstr(&params, "limit")), (Some("true"), Some("-1")));
    assert_eq!(
        pstr(&params, "cayenneExp"),
        Some(r#"seasonId=20252026 and gameTypeId=2 and gameDate>="2026-04-01" and gameDate<="2026-04-01""#)
    );
    let sort: Value = serde_json::from_str(pstr(&params, "sort").unwrap()).unwrap();
    let props: Vec<&str> = sort.as_array().unwrap().iter().map(|s| s["property"].as_str().unwrap()).collect();
    assert_eq!(props, vec!["gameDate", "gameId", "playerId"]);
    assert_eq!(
        pstr(&params, "sort").unwrap(),
        r#"[{"property": "gameDate", "direction": "ASC"}, {"property": "gameId", "direction": "ASC"}, {"property": "playerId", "direction": "ASC"}]"#
    );
    assert_eq!(rows.len(), 12);
    let mack = rows.iter().find(|r| r.player_id == 8477492).unwrap();
    assert_eq!((mack.team.as_deref(), mack.opponent.as_deref(), mack.home, mack.date, mack.game_id), (Some("COL"), Some("VAN"), Some(true), day, Some(2025021188)));
    assert_eq!((mack.toi, mack.ev_toi, mack.pp_toi, mack.sh_toi, mack.shifts), (Some(1461.0), Some(1141.0), Some(320.0), Some(0.0), Some(21)));
    assert_eq!((mack.name.as_deref(), mack.position.as_deref()), (Some("Nathan MacKinnon"), Some("C")));
}

#[test]
fn pp_goalie_team_games_parse() {
    let (c, calls) = deploy_client(&[]);
    let day = d(DAY.0, DAY.1, DAY.2);
    let (s, e) = (Some(day), Some(day));
    let pp: HashMap<i64, NhlSkaterPpGame> = c.skater_pp_games(Some(20252026), s, e, 2).unwrap().into_iter().map(|r| (r.player_id, r)).collect();
    assert_eq!(pp[&8477492].pp_toi, Some(320.0));
    assert!((pp[&8477492].pp_share.unwrap() - 0.888).abs() < 1e-9);
    assert_eq!((pp[&8481641].pp_toi, pp[&8481641].pp_share), (Some(0.0), Some(0.0)));
    let goalies: HashMap<i64, NhlGoalieGame> = c.goalie_games(Some(20252026), s, e, 2).unwrap().into_iter().map(|r| (r.player_id, r)).collect();
    let (b, w, lank) = (&goalies[&8478406], &goalies[&8475809], &goalies[&8480947]);
    assert!(b.started);
    assert_eq!((b.sa, b.sv, b.ga, b.toi), (19.0, 13.0, 6.0, Some(2121.0)));
    assert!(!w.started && w.decision.as_deref() == Some("L"));
    assert!(lank.started && lank.decision.as_deref() == Some("W") && lank.team.as_deref() == Some("VAN") && lank.home == Some(false));
    let teams: HashMap<String, NhlTeamPpGame> =
        c.team_pp_toi_games(Some(20252026), s, e, 2).unwrap().into_iter().map(|t| (t.team.clone().unwrap(), t)).collect();
    assert_eq!((teams["COL"].team_name.as_deref(), teams["COL"].pp_toi), (Some("Colorado Avalanche"), Some(360.0)));
    assert_eq!((teams["VAN"].pp_toi, teams["VAN"].pp_opportunities), (Some(254.0), 3.0));
    let (_, params) = calls.lock().unwrap().last().unwrap().clone();
    assert!(pstr(&params, "sort").unwrap().contains(r#""property": "teamId""#));
}

#[test]
fn game_rows_paginate_on_game_and_player() {
    let rows: Vec<Value> = [1, 2].iter().flat_map(|g| [10, 11].map(|p| json!({"gameDate": "2026-04-01", "gameId": g, "playerId": p}))).collect();
    let (c, _) = recording_client(20252026, move |_, params| {
        if pstr(params, "limit") == Some("-1") {
            return Ok(json!({"data": rows[..3], "total": 4}));
        }
        let start: usize = pstr(params, "start").unwrap().parse().unwrap();
        Ok(json!({"data": rows[start..], "total": 4}))
    });
    let day = d(2026, 4, 1);
    let got = c.skater_toi_games(Some(20252026), Some(day), Some(day), 2).unwrap();
    assert_eq!(got.iter().map(|r| (r.game_id.unwrap(), r.player_id)).collect::<Vec<_>>(), vec![(1, 10), (1, 11), (2, 10), (2, 11)]);
}

#[test]
fn skater_deployment_season() {
    let (c, _) = deploy_client(&[]);
    let rep = c.skater_deployment_season(Some(20252026), 2).unwrap();
    let mcd = &rep[&8478402];
    assert_eq!((mcd.team.as_deref(), mcd.gp), (Some("EDM"), 82));
    assert!((mcd.toi_per_game.unwrap() - 1379.12195).abs() < 1e-6 && (mcd.pp_share.unwrap() - 0.864).abs() < 1e-9);
    assert!(c.skater_deployment_season(Some(20262027), 2).unwrap().is_empty());
}

// --------------------------------------------------------------------------- test_preseason.py (provider part)

const STENBERG: i64 = 8486103;
const GAMES: [i64; 3] = [2026010032, 2026010039, 2026010063];

fn preseason_client(fetch_json: Option<Box<dyn Fn(&str) -> anyhow::Result<Value> + Send + Sync>>) -> (NhlClient, Calls) {
    recording_client(20262027, move |url, _| {
        if let Some(f) = &fetch_json {
            return f(url);
        }
        if url.contains("/club-schedule-season/SJS/") {
            return Ok(load("preseason_club_schedule_SJS.json"));
        }
        for gid in GAMES {
            if url.contains(&format!("/gamecenter/{gid}/boxscore")) {
                return Ok(load(&format!("preseason_boxscore_{gid}.json")));
            }
            if url.contains(&format!("/gamecenter/{gid}/landing")) {
                return Ok(load(&format!("preseason_landing_{gid}.json")));
            }
        }
        anyhow::bail!("KeyError {url}")
    })
}

#[test]
fn boxscore_lines_and_goal_strength() {
    let mut lines = boxscore_skater_lines(&load("preseason_boxscore_2026010039.json")).unwrap();
    let st = lines.iter().find(|l| l.player_id == STENBERG).unwrap().clone();
    assert_eq!((st.team.as_deref(), st.opponent.as_deref(), st.home, st.position.as_deref(), st.game_type), (Some("SJS"), Some("ANA"), Some(true), Some("LW"), Some(1)));
    assert_eq!((st.toi, st.date), (Some(923.0), d(2026, 9, 24)));
    for (k, v) in [("GP", 1.0), ("G", 3.0), ("A", 1.0), ("PTS", 4.0), ("SOG", 3.0), ("PPG", 1.0)] {
        assert_eq!(st.stats[k], v, "{k}");
    }
    assert!(!st.stats.contains_key("PPP") && !st.has_strength);
    let teams: std::collections::HashSet<_> = lines.iter().filter_map(|l| l.team.clone()).collect();
    assert_eq!(teams, ["SJS".to_string(), "ANA".to_string()].into_iter().collect());
    apply_goal_strength(&mut lines, goal_strength_points(&load("preseason_landing_2026010039.json")).as_ref());
    let st = lines.iter().find(|l| l.player_id == STENBERG).unwrap();
    assert_eq!((st.stats["PPG"], st.stats["PPA"], st.stats["PPP"], st.has_strength), (1.0, 0.0, 1.0, true));
}

#[test]
fn pp_assist_comes_from_the_scoring_summary() {
    let (c, _) = preseason_client(None);
    let lines = c.game_skater_lines(2026010032, true).unwrap();
    let st = lines.iter().find(|l| l.player_id == STENBERG).unwrap();
    assert_eq!((st.stats["A"], st.stats["PPG"], st.stats["PPA"], st.stats["PPP"]), (1.0, 0.0, 1.0, 1.0));
}

#[test]
fn unfinished_or_limited_box_scores_are_empty() {
    let mut b = load("preseason_boxscore_2026010039.json");
    let orig = b.clone();
    b["gameState"] = json!("LIVE");
    assert!(boxscore_skater_lines(&b).unwrap().is_empty());
    let mut b = orig;
    b.as_object_mut().unwrap().remove("playerByGameStats");
    assert!(boxscore_skater_lines(&b).unwrap().is_empty());
    assert!(goal_strength_points(&json!({"summary": {}})).is_none());
}

#[test]
fn preseason_games_dedupe_and_stop_before_as_of() {
    let (c, _) = preseason_client(None);
    let games = c.preseason_games(Some(20262027), &["SJS", "SJS"], Some(d(2026, 9, 26))).unwrap();
    assert_eq!(games.iter().map(|g| g.game_id).collect::<Vec<_>>(), vec![2026010009, 2026010032, 2026010039]);
    assert!(c.preseason_games(Some(20262027), &["SJS"], None).unwrap().iter().all(|g| g.game_type == 1));
}

#[test]
fn preseason_game_logs_use_game_type_1() {
    let (c, calls) = preseason_client(Some(Box::new(|_| Ok(json!({"seasonId": 20262027, "gameTypeId": 1, "playerStatsSeasons": []})))));
    assert!(c.preseason_game_logs(STENBERG, None).unwrap().is_empty());
    let urls: Vec<String> = calls.lock().unwrap().iter().map(|(u, _)| u.clone()).collect();
    assert_eq!(urls, vec![format!("https://api-web.nhle.com/v1/player/{STENBERG}/game-log/20262027/1")]);
}

#[test]
fn preseason_skater_games_skip_failing_games() {
    let (c, _) = preseason_client(None);
    let lines = c.preseason_skater_games(Some(20262027), &["SJS"], None).unwrap();
    let got: std::collections::HashSet<i64> = lines.iter().map(|l| l.game_id).collect();
    assert_eq!(got, GAMES.into_iter().collect());
}

#[test]
fn season_totals_and_nhle() {
    for (pid, name) in [(8486103, "landing_history_8486103.json"), (8484984, "landing_history_8484984.json"), (8484794, "landing_history_8484794.json")] {
        let v = load(name);
        let rows = v["seasonTotals"].as_array().unwrap();
        let birth = v["birthDate"].as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
        let hist = parse_season_totals(rows, birth, 2);
        assert!(!hist.is_empty(), "{pid}");
        assert!(hist.windows(2).all(|w| (w[0].season, w[0].league != "NHL") <= (w[1].season, w[1].league != "NHL")));
        assert!(hist.iter().all(|h| h.gp > 0));
    }
    let f = nhle_factors();
    assert!(f["leagues"]["NHL"]["factor"].as_f64().is_some());
}

// --------------------------------------------------------------------------- goldens vs rapidfuzz 3.14

#[test]
fn token_sort_ratio_goldens() {
    let pairs = golden("token_sort.json");
    let pairs = pairs.as_array().unwrap();
    assert!(pairs.len() >= 4000);
    for p in pairs {
        let (a, b, want) = (p[0].as_str().unwrap(), p[1].as_str().unwrap(), p[2].as_f64().unwrap());
        let got = token_sort_ratio(a, b);
        assert_eq!(got, want, "token_sort_ratio({a:?}, {b:?})");
    }
}

fn candidates_golden(v: &Value) -> Vec<Candidate> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|c| Candidate {
            key: Key::Int(c["key"].as_i64().unwrap()),
            name: c["name"].as_str().unwrap().to_string(),
            team: c["team"].as_str().map(str::to_string),
            position: c["position"].as_str().map(str::to_string),
        })
        .collect()
}

#[test]
fn matcher_goldens() {
    let g = golden("matcher.json");
    let index = PlayerIndex::new(candidates_golden(&g["candidates"]));
    let (qs, rs) = (g["queries"].as_array().unwrap(), g["results"].as_array().unwrap());
    assert_eq!(qs.len(), rs.len());
    assert!(qs.len() > 1500);
    let mut by_conf: HashMap<&str, usize> = HashMap::new();
    for (q, r) in qs.iter().zip(rs) {
        let m = index.find(q[0].as_str().unwrap(), q[1].as_str(), q[2].as_str());
        let want_conf = r["confidence"].as_str().unwrap();
        assert_eq!(m.confidence.as_str(), want_conf, "{q}");
        assert_eq!(m.key.as_ref().and_then(Key::as_i64), r["key"].as_i64(), "{q}");
        assert_eq!(m.name.as_deref(), r["name"].as_str(), "{q}");
        assert_eq!(m.score, r["score"].as_f64().unwrap(), "{q}");
        *by_conf.entry(want_conf).or_default() += 1;
    }
    // the golden exercises every outcome
    for k in ["exact", "high", "pending", "none"] {
        assert!(by_conf.get(k).copied().unwrap_or(0) > 0, "no golden case for {k}");
    }
}

// --------------------------------------------------------------------------- test_matching.py

fn pool() -> Vec<Candidate> {
    vec![
        Candidate::new(8478427, "Sebastian Aho", Some("CAR"), Some("C")),
        Candidate::new(8480222, "Sebastian Aho", Some("NYI"), Some("D")),
        Candidate::new(8471214, "Alexander Ovechkin", Some("WSH"), Some("L")),
        Candidate::new(8478483, "Mitchell Marner", Some("VGK"), Some("R")),
        Candidate::new(8480039, "Martin Necas", Some("COL"), Some("C")),
        Candidate::new(8471698, "T.J. Oshie", Some("WSH"), Some("R")),
        Candidate::new(8478402, "Connor McDavid", Some("EDM"), Some("C")),
        Candidate::new(8477934, "Leon Draisaitl", Some("EDM"), Some("C")),
        Candidate::new(8479318, "Auston Matthews", Some("TOR"), Some("C")),
        Candidate::new(8481559, "Jack Hughes", Some("NJD"), Some("C")),
        Candidate::new(8480800, "Quinn Hughes", Some("MIN"), Some("D")),
    ]
}

fn key_of(r: &fm_sources::matching::matcher::MatchResult) -> Option<i64> {
    r.key.as_ref().and_then(Key::as_i64)
}

#[test]
fn exact_matches() {
    let idx = build_index(pool());
    let r = idx.find("Nečas", Some("COL"), None);
    assert!(matches!(r.confidence, Confidence::Pending | Confidence::None));
    let r = idx.find("Martin Nečas", None, None);
    assert_eq!((key_of(&r), r.confidence, r.score), (Some(8480039), Confidence::Exact, 100.0));
    assert_eq!(key_of(&idx.find("TJ Oshie", None, None)), Some(8471698));
    assert_eq!(idx.find("TJ Oshie", None, None).confidence, Confidence::Exact);
    assert_eq!(key_of(&idx.find("Alex Ovechkin", Some("WSH"), None)), Some(8471214));
    assert_eq!(idx.find("Mitch Marner", None, None).confidence, Confidence::Exact);
    assert!(idx.find("Mitch Marner", None, None).matched());
}

#[test]
fn duplicate_name_tie_breaks() {
    let idx = build_index(pool());
    let car = idx.find("Sebastian Aho", Some("CAR"), None);
    let nyi = idx.find("Sebastian Aho", Some("NYI"), None);
    assert_eq!((key_of(&car), car.confidence), (Some(8478427), Confidence::Exact));
    assert_eq!((key_of(&nyi), nyi.confidence), (Some(8480222), Confidence::Exact));
    let dd = idx.find("Sebastian Aho", None, Some("D"));
    let f = idx.find("Sebastian Aho", None, Some("C/LW"));
    assert_eq!((key_of(&dd), dd.confidence), (Some(8480222), Confidence::High));
    assert_eq!((key_of(&f), f.confidence), (Some(8478427), Confidence::High));
    let r = match_player_in("Sebastian Aho", &pool(), None, None);
    assert_eq!(r.confidence, Confidence::Pending);
    assert!(!r.matched());
}

#[test]
fn fuzzy_bands() {
    let p = pool();
    let r = match_player_in("Leon Draisaitel", &p, Some("EDM"), None);
    assert_eq!((key_of(&r), r.confidence), (Some(8477934), Confidence::High));
    assert!(r.score >= 92.0);
    let r = match_player_in("Leon Draisaitel", &p, None, None);
    assert_eq!((key_of(&r), r.confidence), (Some(8477934), Confidence::Pending));
    let r = match_player_in("Austin Mathews", &p, Some("TOR"), None);
    assert_eq!(key_of(&r), Some(8479318));
    assert!(matches!(r.confidence, Confidence::Pending | Confidence::High));
    assert!(r.score >= 85.0);
    let r = match_player_in("Wayne Gretzky", &p, None, None);
    assert!(r.confidence == Confidence::None && r.key.is_none());
    assert_eq!(match_player_in("", &p, None, None).confidence, Confidence::None);
    assert_eq!(match_player_in("Connor McDavid", &[], None, None).confidence, Confidence::None);
    let r = match_player_in("Jak Hughes", &p, Some("NJD"), None);
    assert_eq!(key_of(&r), Some(8481559));
}

#[test]
fn candidates_from_dicts() {
    let items = vec![
        json!({"player_id": 1, "name": "Nikita Kucherov", "team": "TBL", "position": "RW"}),
        json!({"player_id": 2, "name": "Brayden Point", "team": "TBL", "position": "C"}),
    ];
    let c = candidates_from(&items, "player_id", "name", "team", "position");
    assert_eq!(c.iter().map(|x| x.key.as_i64().unwrap()).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(key_of(&match_player_in("Brayden Point", &c, None, None)), Some(2));
}

// --------------------------------------------------------------------------- test_crosswalk.py

fn pl(cid: &str, name: &str, team: Option<&str>, pos: &[&str]) -> Player {
    let (_, sid) = split_cid(cid);
    serde_json::from_value(json!({
        "cid": cid, "name": name, "name_norm": fm_core::normalize::normalize_name(Some(name)),
        "ids": {"espn": sid}, "team": team, "positions": pos,
    }))
    .unwrap()
}

fn fixture_candidates() -> Vec<Candidate> {
    let f = fixture_client();
    let sk = f.client.all_skaters(Some(20252026), 2, true, true).unwrap();
    let go = f.client.goalie_summary(Some(20252026), 2).unwrap();
    let ro = f.client.team_roster("EDM", None).unwrap();
    nhl_candidates(&[
        candidates_of(&sk),
        candidates_of(&go),
        candidates_of(&ro),
        vec![
            Candidate::new(8478427, "Sebastian Aho", Some("CAR"), Some("C")),
            Candidate::new(8480222, "Sebastian Aho", Some("NYI"), Some("D")),
        ],
    ])
}

#[test]
fn nhl_candidates_merge_rows_and_rosters() {
    let c = fixture_candidates();
    let by_id: HashMap<i64, &Candidate> = c.iter().map(|x| (x.key.as_i64().unwrap(), x)).collect();
    assert_eq!(by_id[&8478402].name, "Connor McDavid");
    assert_eq!(by_id[&8478402].team.as_deref(), Some("EDM"));
    assert_eq!(by_id[&8477934].name, "Leon Draisaitl");
    assert_eq!(by_id[&8476883].position.as_deref(), Some("G"));
}

#[test]
fn resolve_and_persist() {
    let tmp = tempfile::tempdir().unwrap();
    let cands = fixture_candidates();
    let mut players = vec![
        pl("espn:1", "Connor McDavid", Some("EDM"), &["C", "F"]),
        pl("espn:2", "Leon Draisaitl", Some("EDM"), &["C", "F"]),
        pl("espn:3", "Nathan McKinnon", Some("COL"), &["C", "F"]),
        pl("espn:4", "Sebastian Aho", None, &["C", "F"]),
        pl("espn:5", "Nobody Atall", Some("EDM"), &["D"]),
    ];
    let xw = Crosswalk::new(tmp.path()).unwrap();
    let stats = xw.resolve(&mut players, &cands).unwrap();
    assert_eq!(players.iter().map(|p| p.cid.clone()).collect::<Vec<_>>(), (1..=5).map(|i| format!("espn:{i}")).collect::<Vec<_>>());
    assert_eq!(players[0].nhl_id(), Some(8478402));
    assert_eq!(players[0].ids["nhl"], "8478402");
    assert_eq!(players[1].nhl_id(), Some(8477934));
    assert_eq!(players[2].nhl_id(), Some(8477492));
    assert_eq!(players[4].nhl_id(), None);
    assert_eq!(players[3].nhl_id(), Some(8478427));
    assert_eq!((stats.resolved, stats.new, stats.unmatched), (4, 4, 1));
    assert_eq!(xw.get("espn", "1").unwrap().unwrap().confidence, "exact");
    assert_eq!(xw.get("espn", "5").unwrap().unwrap().confidence, "none");
    xw.close();

    // a fresh instance reuses stored matches without an index
    let xw2 = Crosswalk::new(tmp.path()).unwrap();
    let mut again = vec![pl("espn:1", "C. McDavid", Some("EDM"), &["C"])];
    let empty: Vec<Candidate> = vec![];
    assert_eq!(xw2.resolve(&mut again, &empty).unwrap().resolved, 1);
    assert_eq!(again[0].nhl_id(), Some(8478402));
}

#[test]
fn pending_review_and_confirm() {
    let tmp = tempfile::tempdir().unwrap();
    let cands = vec![
        Candidate::new(1, "Elias Pettersson", Some("VAN"), Some("C")),
        Candidate::new(2, "Elias Pettersson", Some("VAN"), Some("C")),
    ];
    let xw = Crosswalk::new(tmp.path()).unwrap();
    let mut ps = vec![pl("espn:77", "Elias Pettersson", Some("VAN"), &["C"])];
    let stats = xw.resolve(&mut ps, &cands).unwrap();
    assert_eq!(stats.pending, 1);
    assert_eq!(ps[0].nhl_id(), None);
    let pend = xw.pending(false).unwrap();
    assert_eq!(pend.iter().map(|r| (r.source.as_str(), r.source_id.as_str(), r.confidence.as_str())).collect::<Vec<_>>(), vec![("espn", "77", "pending")]);
    let row = xw.confirm("espn", "77", 2).unwrap();
    assert_eq!((row.confidence.as_str(), row.name.as_str()), ("confirmed", "Elias Pettersson"));
    assert!(xw.pending(false).unwrap().is_empty());
    let mut p2 = vec![pl("espn:77", "Elias Pettersson", Some("VAN"), &["C"])];
    xw.resolve(&mut p2, &cands).unwrap();
    assert_eq!(p2[0].nhl_id(), Some(2));
}

#[test]
fn provider_supplied_nhl_id_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let mut p = pl("fantrax:abc", "Someone", Some("EDM"), &["C"]);
    p.ids.insert("nhl".into(), "123".into());
    let xw = Crosswalk::new(tmp.path()).unwrap();
    let empty: Vec<Candidate> = vec![];
    assert_eq!(xw.resolve(std::slice::from_mut(&mut p), &empty).unwrap().resolved, 1);
    assert_eq!(xw.get("fantrax", "abc").unwrap().unwrap().nhl_id, Some(123));
}

#[test]
fn resolve_source_dfo() {
    let tmp = tempfile::tempdir().unwrap();
    let xw = Crosswalk::new(tmp.path()).unwrap();
    let items = vec![
        SourceItem { source_id: "10".into(), name: "Connor McDavid".into(), team: Some("EDM".into()), position: Some("C".into()) },
        SourceItem { source_id: "11".into(), name: "Wayne Gretzky".into(), team: None, position: None },
        SourceItem { source_id: "10".into(), name: "Dup".into(), team: None, position: None },
    ];
    let (out, stats) = xw.resolve_source("dfo", &items, &pool(), false).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out["10"], 8478402);
    assert_eq!(stats, ResolveStats { resolved: 1, new: 1, pending: 0, unmatched: 1 });
    assert!(xw.get("dfo", "11").unwrap().is_none());
}

#[test]
fn parse_confirm_and_split() {
    assert_eq!(parse_confirm("espn:123=8478402").unwrap(), ("espn".to_string(), "123".to_string(), 8478402));
    assert_eq!(split_cid("fantrax:x:y"), ("fantrax".to_string(), "x:y".to_string()));
    for bad in ["espn123=1", "espn:1=abc", "espn:1"] {
        assert!(parse_confirm(bad).is_err(), "{bad}");
    }
}

// --------------------------------------------------------------------------- crosswalk over the parity tape

fn fixtures_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FM_PARITY_FIXTURES") {
        let p = PathBuf::from(p);
        if p.join("fantrax/tape.json.gz").exists() {
            return Some(p);
        }
    }
    let p = root().join("rust/parity/fixtures");
    p.join("fantrax/tape.json.gz").exists().then_some(p)
}

#[test]
fn fresh_crosswalk_over_tape_matches_python() {
    let Some(fx) = fixtures_root() else {
        eprintln!("SKIP: no fantrax/tape.json.gz (set FM_PARITY_FIXTURES)");
        return;
    };
    // tape: (url, cayenneExp) -> body
    let raw = flate2::read::GzDecoder::new(std::fs::File::open(fx.join("fantrax/tape.json.gz")).unwrap());
    let tape: Value = serde_json::from_reader(raw).unwrap();
    let mut by: HashMap<(String, Option<String>), Value> = HashMap::new();
    for (_, v) in tape["http"].as_object().unwrap() {
        let Ok(body) = serde_json::from_str::<Value>(v["body"].as_str().unwrap_or("")) else { continue };
        let cay = v["params"].get("cayenneExp").and_then(|c| c.as_str()).map(str::to_string);
        by.insert((v["url"].as_str().unwrap().to_string(), cay), body);
    }
    let fetch = move |url: &str, params: &Params, _ttl: f64| -> anyhow::Result<Value> {
        let cay = pstr(params, "cayenneExp").map(str::to_string);
        by.get(&(url.to_string(), cay)).cloned().ok_or_else(|| anyhow::anyhow!("tape miss {url}"))
    };
    let c = NhlClient::new(Box::new(fetch), Some(20262027), d(2026, 11, 4));
    let pr_sk = c.all_skaters(Some(20252026), 2, true, true).unwrap();
    let pr_g = c.goalie_summary(Some(20252026), 2).unwrap();
    let cur_sk = c.all_skaters(Some(20262027), 2, true, true).unwrap();
    let cur_g = c.goalie_summary(Some(20262027), 2).unwrap();
    let mut roster = vec![];
    for t in NHL_TEAMS {
        roster.extend(c.team_roster(t, Some(20262027)).unwrap());
    }
    let cands = nhl_candidates(&[candidates_of(&pr_sk), candidates_of(&pr_g), candidates_of(&cur_sk), candidates_of(&cur_g), candidates_of(&roster)]);

    let want = golden("crosswalk_fantrax.json");
    let snap: Value = serde_json::from_reader(std::fs::File::open(fx.join("fantrax/snapshot.json")).unwrap()).unwrap();
    let ctx = &snap["ctx"];
    let mut vals: Vec<Value> = vec![];
    for t in ctx["teams"].as_array().unwrap() {
        for s in t["slots"].as_array().unwrap() {
            if !s["player"].is_null() {
                vals.push(s["player"].clone());
            }
        }
    }
    vals.extend(ctx["free_agents"].as_array().unwrap().iter().cloned());
    let mut players: Vec<Player> = vals
        .into_iter()
        .map(|mut v| {
            v["ids"].as_object_mut().unwrap().remove("nhl");
            v["lines"] = json!({});
            serde_json::from_value(v).unwrap()
        })
        .collect();
    let tmp = tempfile::tempdir().unwrap();
    let xw = Crosswalk::new(tmp.path()).unwrap();
    let stats = xw.resolve(&mut players, &cands).unwrap();
    assert_eq!(stats.summary(), want["summary"].as_str().unwrap());
    let rows = xw.all().unwrap();
    let want_rows = want["rows"].as_array().unwrap();
    assert_eq!(rows.len(), want_rows.len());
    for (r, w) in rows.iter().zip(want_rows) {
        assert_eq!(r.source, w[0].as_str().unwrap());
        assert_eq!(r.source_id, w[1].as_str().unwrap());
        assert_eq!(r.nhl_id, w[2].as_i64(), "{}", r.source_id);
        assert_eq!(r.confidence, w[3].as_str().unwrap(), "{}", r.source_id);
        assert_eq!(r.candidate_name.as_deref(), w[4].as_str(), "{}", r.source_id);
        assert_eq!(r.score, w[5].as_f64(), "{}", r.source_id);
    }
}

// --------------------------------------------------------------------------- test_news_match.py

fn news(headline: &str, player_name: Option<&str>, blurb: &str, source: &str, day: u32, id: Option<&str>) -> NewsItem {
    let p = PyDateTime::parse(&format!("2026-09-{day:02}T12:00:00+00:00")).unwrap();
    NewsItem {
        source: source.to_string(),
        id: id.map(str::to_string),
        player_name: player_name.map(str::to_string),
        headline: headline.to_string(),
        blurb: blurb.to_string(),
        url: None,
        published: Some(p),
        tags: vec![],
    }
}

fn news_players() -> Vec<Player> {
    let mk = |cid: &str, name: &str, team: &str, pos: &str| -> Player {
        serde_json::from_value(json!({"cid": cid, "name": name,
            "name_norm": fm_core::normalize::normalize_name(Some(name)), "ids": {}, "team": team, "positions": [pos]}))
        .unwrap()
    };
    vec![
        mk("stutzle", "Tim Stützle", "OTT", "C"),
        mk("necas", "Martin Nečas", "COL", "C"),
        mk("ovi", "Alex Ovechkin", "WSH", "LW"),
        mk("aho-car", "Sebastian Aho", "CAR", "C"),
        mk("aho-nyi", "Sebastian Aho", "NYI", "D"),
        mk("hughes", "Jack Hughes", "NJD", "C"),
    ]
}

fn ids(v: &[NewsItem]) -> Vec<String> {
    v.iter().map(|n| n.id.clone().unwrap_or_default()).collect()
}

#[test]
fn news_rotowire_player_name_with_accents() {
    let ps = news_players();
    let refs: Vec<&Player> = ps.iter().collect();
    let items = vec![
        news("Scores twice", Some("Tim Stutzle"), "", "rotowire", 1, Some("a")),
        news("Out week-to-week", Some("Martin Necas"), "", "rotowire", 1, Some("b")),
    ];
    let out = match_news_to_players(&items, &refs);
    assert_eq!(ids(&out["stutzle"]), vec!["a"]);
    assert_eq!(ids(&out["necas"]), vec!["b"]);
}

#[test]
fn news_unmatched_and_ambiguous_are_dropped() {
    let ps = news_players();
    let refs: Vec<&Player> = ps.iter().collect();
    let items = vec![
        news("Nothing", Some("Connor McDavid"), "", "rotowire", 1, Some("x")),
        news("Two goals", Some("Sebastian Aho"), "", "rotowire", 1, Some("y")),
    ];
    assert!(match_news_to_players(&items, &refs).is_empty());
}

#[test]
fn news_espn_headline_scan() {
    let ps = news_players();
    let refs: Vec<&Player> = ps.iter().collect();
    let items = vec![
        news("Stützle, Nečas lead the way in preseason win", None, "Tim Stützle scored and Martin Necas added two assists.", "espn", 1, Some("e1")),
        news("Hughes brothers reunite", None, "Quinn and Luke talk Jack.", "espn", 1, Some("e2")),
        news("Alex Ovechkin chases another record", None, "", "espn", 1, Some("e3")),
        news("Sebastian Aho signs extension", None, "", "espn", 1, Some("e4")),
        news("Jack Hughesman is not a player", None, "", "espn", 1, Some("e5")),
    ];
    let out = match_news_to_players(&items, &refs);
    assert_eq!(ids(&out["stutzle"]), vec!["e1"]);
    assert_eq!(ids(&out["necas"]), vec!["e1"]);
    assert_eq!(ids(&out["ovi"]), vec!["e3"]);
    assert!(!out.contains_key("hughes"));
    assert!(!out.contains_key("aho-car") && !out.contains_key("aho-nyi"));
}

#[test]
fn news_dedupe_and_newest_first() {
    let ps = news_players();
    let refs: Vec<&Player> = ps.iter().collect();
    let items = vec![
        news("Old", Some("Jack Hughes"), "", "rotowire", 1, Some("1")),
        news("New", Some("Jack Hughes"), "", "rotowire", 5, Some("2")),
        news("New", Some("Jack Hughes"), "", "rotowire", 5, Some("2")),
    ];
    let out = match_news_to_players(&items, &refs);
    assert_eq!(ids(&out["hughes"]), vec!["2", "1"]);
}

#[test]
fn news_name_keys_include_nickname_forms() {
    let keys = name_keys("Alexander Ovechkin");
    assert!(keys.contains("alex ovechkin") && keys.contains("alexander ovechkin"));
    assert!(name_keys("Madonna").is_empty());
}

#[test]
fn news_summary() {
    let items = vec![
        news("Old news", None, &"x".repeat(500), "rotowire", 1, None),
        news("Fresh news", None, "Short.", "rotowire", 9, None),
    ];
    let lines = player_news_summary(&items, 1, 200);
    assert_eq!(lines, vec!["2026-09-09 (rotowire): Fresh news -- Short."]);
    let long = player_news_summary(&items, 3, 200);
    assert!(long.len() == 2 && long[1].ends_with('\u{2026}') && long[1].chars().count() < 260);
}
