//! Tests of the official Fantrax API client against trimmed real responses for our league, recorded on
//! 2026-11-10 (`tests/fixtures/fantrax/fxea_*.json`), through a tape-backed `HttpCache` (no network).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::NaiveDate;
use fm_http::tape::{TapeData, TapeHttp};
use fm_http::{cache_key, CachedResponse, HttpCache, ParamValue, Params, Tape};
use fm_providers::fantrax_official::*;
use serde_json::Value;

const LEAGUE: &str = "oq3cj6dkmufzrxl9";
const INFO: &str = include_str!("../../../tests/fixtures/fantrax/fxea_league_info.json");
const PICKS: &str = include_str!("../../../tests/fixtures/fantrax/fxea_draft_picks.json");
const STANDINGS: &str = include_str!("../../../tests/fixtures/fantrax/fxea_standings.json");
const SCORES6: &str = include_str!("../../../tests/fixtures/fantrax/fxea_matchup_scores_p6.json");
const ROSTERS42: &str = include_str!("../../../tests/fixtures/fantrax/fxea_rosters_p42.json");
const ADP: &str = include_str!("../../../tests/fixtures/fantrax/fxea_adp.json");
const ERROR_BODY: &str = include_str!("../../../tests/fixtures/fantrax/fxea_error.json");

fn p(items: &[(&str, &str)]) -> Params {
    items.iter().map(|(k, v)| ((*k).to_string(), ParamValue::Str((*v).to_string()))).collect()
}

fn url(endpoint: &str) -> String {
    format!("{BASE_URL}/{endpoint}")
}

/// A tape holding the given (endpoint, params, status, body) responses.
fn tape_cache(rows: &[(&str, Params, u16, &str)]) -> Arc<HttpCache> {
    let mut http = HashMap::new();
    for (endpoint, params, status, body) in rows {
        let u = url(endpoint);
        http.insert(
            cache_key("GET", &u, params, &[]),
            TapeHttp {
                url: u.clone(),
                params: Value::Null,
                key_headers: HashMap::new(),
                status: *status,
                final_url: u,
                body: (*body).to_string(),
            },
        );
    }
    Arc::new(HttpCache::from_tape(Tape(Arc::new(TapeData { http, fxpa: HashMap::new() }))))
}

fn info_params() -> Params {
    p(&[("leagueId", LEAGUE), ("excludePlayerInfo", "true")])
}

fn client(rows: &[(&str, Params, u16, &str)]) -> FantraxOfficial {
    FantraxOfficial::new(tape_cache(rows), LEAGUE)
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

#[test]
fn periods_use_the_end_date_minus_one_day() {
    let info = client(&[("getLeagueInfo", info_params(), 200, INFO)]).league_info().unwrap();
    assert_eq!(info.scoring_periods.len(), 27);
    let by = |n: i64| info.scoring_periods.iter().find(|x| x.number == n).unwrap();
    // endDate 2026-10-05T18:59:59-0400 -> last day 10-04, a 6-day first period
    assert_eq!((by(1).first, by(1).last, by(1).days()), (d(2026, 9, 29), d(2026, 10, 4), 6));
    // endDate 2026-11-09T19:59:59-0500 -> 11-08, the Monday-Sunday week the private path gave
    assert_eq!((by(6).first, by(6).last, by(6).days()), (d(2026, 11, 2), d(2026, 11, 8), 7));
    // 2027-02-01 to endDate 2027-02-15 minus a day: the two-week period before the playoffs
    assert_eq!((by(19).first, by(19).last, by(19).days()), (d(2027, 2, 1), d(2027, 2, 14), 14));
    assert_eq!(by(27).last, d(2027, 4, 10));
    // consecutive periods never overlap and leave no gap
    for w in info.scoring_periods.windows(2) {
        assert_eq!(w[0].last + chrono::Duration::days(1), w[1].first, "periods {} and {}", w[0].number, w[1].number);
    }
}

#[test]
fn roster_period_for_a_date() {
    let info = client(&[("getLeagueInfo", info_params(), 200, INFO)]).league_info().unwrap();
    // the trimmed sample keeps roster periods 1, 42 and 194; period 42 is the day of 2026-11-09
    assert_eq!(period_for_date(&info.roster_periods, d(2026, 11, 9)), Some(42));
    assert_eq!(period_for_date(&info.roster_periods, d(2026, 11, 12)), None);
}

#[test]
fn league_info_playoffs_teams_scoring_and_limits() {
    let info = client(&[("getLeagueInfo", info_params(), 200, INFO)]).league_info().unwrap();
    let po = info.playoffs.unwrap();
    assert_eq!((po.last_regular_period, po.first_playoff_period, po.num_playoff_teams), (24, 25, 6));
    assert!(po.used && !po.merge_playoff_periods);
    assert!(!po.is_playoff_period(24) && po.is_playoff_period(25) && po.is_playoff_period(27));
    assert_eq!(info.teams.len(), 12);
    assert_eq!(info.teams[0], OfficialTeam { id: "qvljl1xumuhhm8sk".into(), name: "Gwinzzz".into() });
    assert_eq!((info.season_year, info.start_date, info.end_date), (2026, Some(d(2026, 9, 29)), Some(d(2027, 4, 10))));
    let sc = info.scoring.unwrap();
    assert_eq!(sc.kind, "HEAD_TO_HEAD_POINTS_BASED");
    assert_eq!((sc.skater["G"], sc.skater["A"], sc.skater["SOG"], sc.skater["Hit"], sc.skater["Blk"]), (4.0, 2.0, 0.5, 0.3, 0.5));
    // a goalie assist is worth 3, not the skater's 2
    assert_eq!((sc.goalie["A"], sc.goalie["W"], sc.goalie["GA"], sc.goalie["SV"], sc.goalie["SHO"]), (3.0, 5.0, -1.0, 0.25, 7.5));
    let lim = info.roster_limits.unwrap();
    assert_eq!((lim.max_total, lim.max_active, lim.max_reserve), (16, 12, 6));
    assert_eq!((lim.max_active_by_position["F"], lim.max_active_by_position["D"], lim.max_active_by_position["G"]), (6, 4, 2));
}

#[test]
fn matchups_skip_playoff_placeholders() {
    let info = client(&[("getLeagueInfo", info_params(), 200, INFO)]).league_info().unwrap();
    let period = |n: i64| info.matchups.iter().find(|m| m.period == n).unwrap();
    assert_eq!(period(7).pairings.len(), 6);
    let first = &period(7).pairings[0];
    assert_eq!((first.away.name.as_str(), first.home.name.as_str()), ("Hesham", "3-0"));
    assert_eq!(first.home.short_name.as_deref(), Some("Zoommba"));
    // period 24 still has real teams; 25 and 27 hold {seed} / {bye} sides only
    assert_eq!(period(24).pairings.len(), 6);
    assert!(period(25).pairings.is_empty() && period(27).pairings.is_empty());
}

/// The league info with a period-6 pairing added (the sample holds the matchup list for periods 1, 7, 24, 25
/// and 27 only): Nermeen LouLou (away) against Jaydentrapp (home), the game the saved period-6 scores hold.
fn info_with_period_6() -> OfficialLeagueInfo {
    let mut info = client(&[("getLeagueInfo", info_params(), 200, INFO)]).league_info().unwrap();
    let side = |id: &str, name: &str| OfficialSide { id: id.to_string(), name: name.to_string(), short_name: None };
    info.matchups.push(OfficialPeriodMatchups {
        period: 6,
        pairings: vec![OfficialPairing { away: side("jswcd1a0muhm7zpo", "Nermeen LouLou"), home: side("ss08endfmuhlze0p", "Jaydentrapp") }],
    });
    info
}

#[test]
fn my_matchup_pairs_the_listed_opponent_with_the_period_scores() {
    let info = info_with_period_6();
    let board = client(&[("getMatchupScores", p(&[("leagueId", LEAGUE), ("period", "6")]), 200, SCORES6)]).matchup_scores(Some(6)).unwrap();
    // the finished period 6, from either side (the final the private schedule view stored was 191.6 to 197.55)
    assert_eq!(my_matchup(&info, &board, "jswcd1a0muhm7zpo", 6), Ok(Some(("ss08endfmuhlze0p".to_string(), 191.6, 197.55))));
    assert_eq!(my_matchup(&info, &board, "ss08endfmuhlze0p", 6), Ok(Some(("jswcd1a0muhm7zpo".to_string(), 197.55, 191.6))));
    // a team with no pairing in the list, and a playoff period of seed placeholders
    assert_eq!(my_matchup(&info, &board, "nobody", 6), Ok(None));
    assert_eq!(my_matchup(&info, &board, "jswcd1a0muhm7zpo", 25), Ok(None));
}

#[test]
fn my_matchup_reports_a_list_and_scores_that_disagree() {
    let mut info = info_with_period_6();
    // the list says Nermeen plays Hesham, the scores say Jaydentrapp
    info.matchups.last_mut().unwrap().pairings[0].home = OfficialSide { id: "kk4uhki2muhp99yv".into(), name: "Hesham".into(), short_name: None };
    let board = client(&[("getMatchupScores", p(&[("leagueId", LEAGUE), ("period", "6")]), 200, SCORES6)]).matchup_scores(Some(6)).unwrap();
    assert!(my_matchup(&info, &board, "jswcd1a0muhm7zpo", 6).unwrap_err().contains("disagree"));
}

#[test]
fn draft_picks_cover_every_team() {
    let picks = client(&[("getDraftPicks", p(&[("leagueId", LEAGUE)]), 200, PICKS)]).draft_picks().unwrap();
    assert_eq!(picks.future.len(), 360);
    assert!(picks.current.is_empty());
    let mut ids: Vec<&str> = picks.future.iter().map(|x| x.original_owner_team_id.as_str()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 12);
    let first = &picks.future[0];
    assert_eq!((first.year, first.round), (2027, 1));
    assert_eq!(first.owner_team_id, first.original_owner_team_id);
}

#[test]
fn standings_split_the_record() {
    let st = client(&[("getStandings", p(&[("leagueId", LEAGUE)]), 200, STANDINGS)]).standings().unwrap();
    assert_eq!(st.len(), 12);
    assert_eq!(st[0].rank, 1);
    assert_eq!(st[0].team_name, "Miff Bucannons V4");
    assert_eq!(st[0].record, Some((6, 0, 0)));
    assert_eq!(st[0].points_for, 1367.45);
    assert_eq!(parse_standings(&serde_json::json!([{"teamId": "x", "rank": 2, "points": "4-1-1"}])).unwrap()[0].record, Some((4, 1, 1)));
    // not a W-L-T text (a roto or points league): no record, not an error
    assert_eq!(parse_standings(&serde_json::json!([{"teamId": "x", "rank": 2, "points": "44.5"}])).unwrap()[0].record, None);
}

#[test]
fn finished_period_scores() {
    let c = client(&[("getMatchupScores", p(&[("leagueId", LEAGUE), ("period", "6")]), 200, SCORES6)]);
    let sb = c.matchup_scores(Some(6)).unwrap();
    assert_eq!((sb.period, sb.matchups.len()), (6, 6));
    let (away, home) = &sb.matchups[0];
    assert_eq!((away.team_name.as_str(), away.score, away.games_played), ("3-0", Some(255.9), Some(42)));
    assert_eq!((home.team_name.as_str(), home.score), ("Dee_snuts69", Some(228.3)));
    // the current period (no parameter) is a different request: not on this tape, so it is not cached
    assert_eq!(c.matchup_scores(None).unwrap_err(), OfficialError::NotCached);
}

#[test]
fn rosters_for_a_period() {
    let c = client(&[("getTeamRosters", p(&[("leagueId", LEAGUE), ("period", "42")]), 200, ROSTERS42)]);
    let r = c.team_rosters(Some(42)).unwrap();
    assert_eq!((r.period, r.teams.len()), (42, 12));
    let t = r.teams.iter().find(|t| t.team_id == "qvljl1xumuhhm8sk").unwrap();
    assert_eq!((t.team_name.as_str(), t.items.len()), ("Gwinzzz", 17));
    assert_eq!(t.items[0], OfficialRosterItem { player_id: "042ye".into(), position: "F".into(), status: OfficialRosterStatus::Reserve });
    assert_eq!(t.items[1].status, OfficialRosterStatus::Active);
    assert!(r.teams.iter().flat_map(|t| &t.items).any(|i| i.status == OfficialRosterStatus::InjuredReserve));
}

#[test]
fn adp_drops_the_sentinel() {
    let adp = client(&[("getAdp", p(&[("sport", "NHL")]), 200, ADP)]).adp().unwrap();
    assert_eq!(adp.len(), 25);
    assert_eq!(adp["02f9l"], 1.6);
    assert!(!adp.contains_key("05yji"), "a 10000.0 row means no ADP");
}

#[test]
fn player_ids_flip_names_and_drop_pseudo_players() {
    // 208 rostered players of period 42, Ryan O'Reilly, and two team-total pseudo-players ("30010#...").
    let body = include_str!("../../../tests/fixtures/fantrax/fxea_player_ids.json");
    let ids = client(&[("getPlayerIds", p(&[("sport", "NHL")]), 200, body)]).player_ids().unwrap();
    assert_eq!(ids.len(), 209);
    assert!(ids.keys().all(|k| !k.contains('#')));
    assert_eq!(ids["042ye"], OfficialPlayerName { name: "Jesper Bratt".into(), team: "NJD".into(), position: "LW".into() });
    // The API strips apostrophes; that is not undone.
    assert_eq!(ids["04qz4"].name, "Ryan OReilly");
    assert_eq!(ids["04qz4"].team, "(N/A)");
    assert_eq!(flip_name("Smith Jr., John"), "John Smith Jr.");
    assert_eq!(flip_name("Team"), "Team");
}

#[test]
fn error_body_is_a_failure_with_a_safe_warning() {
    let c = client(&[("getLeagueInfo", info_params(), 200, ERROR_BODY)]);
    let err = c.league_info().unwrap_err();
    let OfficialError::Failed(detail) = &err else { panic!("{err:?}") };
    assert!(detail.contains("not found"), "{detail}");
    let w = err.warning("league info").unwrap();
    assert!(w.starts_with("Fantrax official API league info unavailable"), "{w}");
    assert!(w.ends_with("using the private data instead"), "{w}");
}

#[test]
fn error_body_leaves_no_cached_row() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(HttpCache::open(dir.path(), false).unwrap());
    let u = url("getLeagueInfo");
    let key = cache_key("GET", &u, &info_params(), &[]);
    let resp = CachedResponse { status_code: 200, text: ERROR_BODY.to_string(), url: u.clone(), from_cache: false };
    cache.store(&key, &u, &resp).unwrap();
    assert!(cache.lookup(&key).is_some());
    // the stored error body is fresh, so it is what the call reads; the wrapper must drop it
    let err = FantraxOfficial::new(cache.clone(), LEAGUE).league_info().unwrap_err();
    assert!(matches!(err, OfficialError::Failed(_)));
    assert!(cache.lookup(&key).is_none(), "an error body must not stay cached");
    // and a body that is not JSON is dropped the same way
    let junk = CachedResponse { status_code: 200, text: "<html>maintenance</html>".into(), url: u.clone(), from_cache: false };
    cache.store(&key, &u, &junk).unwrap();
    assert!(FantraxOfficial::new(cache.clone(), LEAGUE).league_info().is_err());
    assert!(cache.lookup(&key).is_none());
}

#[test]
fn good_body_stays_cached() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(HttpCache::open(dir.path(), false).unwrap());
    let u = url("getLeagueInfo");
    let key = cache_key("GET", &u, &info_params(), &[]);
    let resp = CachedResponse { status_code: 200, text: INFO.to_string(), url: u.clone(), from_cache: false };
    cache.store(&key, &u, &resp).unwrap();
    assert!(FantraxOfficial::new(cache.clone(), LEAGUE).league_info().is_ok());
    assert!(cache.lookup(&key).is_some());
}

#[test]
fn http_failures_never_mention_status_url_or_login() {
    // 403 is what a missing User-Agent gets; it must read as a rejected request, not a bad login
    let forbidden = client(&[("getLeagueInfo", info_params(), 403, "<html>Just a moment...</html>")]).league_info().unwrap_err();
    let w = forbidden.warning("league info").unwrap();
    assert!(w.contains("rejected the request (User-Agent)"), "{w}");
    let server = client(&[("getLeagueInfo", info_params(), 500, "oops")]).league_info().unwrap_err();
    for e in [forbidden, server] {
        let w = e.warning("league info").unwrap().to_lowercase();
        for bad in ["login", "cookie", "session", "403", "500", "http", "fantrax.com"] {
            assert!(!w.contains(bad), "{bad} in {w}");
        }
    }
}

#[test]
fn an_error_message_with_forbidden_words_is_replaced() {
    let body = r#"{"error":{"message":"Please log in again: session expired (HTTP 401)"}}"#;
    let err = client(&[("getLeagueInfo", info_params(), 200, body)]).league_info().unwrap_err();
    let w = err.warning("league info").unwrap().to_lowercase();
    assert!(w.contains("the api returned an error"), "{w}");
    for bad in ["log in", "login", "session", "401", "http"] {
        assert!(!w.contains(bad), "{bad} in {w}");
    }
}

#[test]
fn offline_miss_is_silent() {
    let err = client(&[]).league_info().unwrap_err();
    assert_eq!(err, OfficialError::NotCached);
    assert_eq!(err.warning("league info"), None);
}

#[test]
fn unexpected_shape_is_a_failure() {
    let err = client(&[("getStandings", p(&[("leagueId", LEAGUE)]), 200, r#"{"standings": []}"#)]).standings().unwrap_err();
    let w = err.warning("standings").unwrap();
    assert!(w.contains("unexpected response"), "{w}");
    // a period that ends before it starts is rejected rather than guessed
    let bad = serde_json::json!([{"number": 1, "startDate": "2026-09-29T17:00:00.0-0400", "endDate": "2026-09-29T18:00:00.0-0400"}]);
    assert!(parse_periods(&bad).unwrap_err().contains("ends before it starts"));
}
