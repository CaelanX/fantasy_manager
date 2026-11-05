//! ESPN provider tests (work package W3-espn): the intent of `tests/test_espn_parsing.py` plus
//! the provider parity test (`EspnProvider::load()` replayed from `fixtures/espn/tape.json.gz`
//! == `fixtures/espn/raw_ctx.json`, and the provider extras == `snapshot.provider`).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Local, NaiveDate, TimeZone};
use fm_core::config::Settings;
use fm_core::models::{ActivityAction, Status};
use fm_core::provider::LeagueProvider;
use fm_core::Map;
use fm_http::{cache_key, ParamValue};
use fm_http::tape::{TapeData, TapeHttp};
use fm_http::{HttpCache, Params, Tape};
use fm_providers::espn::api::{self, EspnFantasyRequests, League};
use fm_providers::espn::*;
use serde_json::{json, Value};

const PLAYER_STATS: &str = include_str!("../../../tests/fixtures/espn/player_stats_sample.json");
const ACTIVITY: &str = include_str!("../../../tests/fixtures/espn/activity.json");
const TRENDING: &str = include_str!("../../../tests/fixtures/espn/trending.json");

fn fixture() -> Value {
    serde_json::from_str(PLAYER_STATS).unwrap()
}

fn year() -> i64 {
    fixture()["year"].as_i64().unwrap()
}

/// The fixture's SimpleNamespace-style player dict -> an espn_api Player.
fn espn_player(d: &Value) -> api::Player {
    let stats = d["stats"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v["total"].as_object().map(|t| t.clone().into_iter().collect()).unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default();
    api::Player {
        name: d["name"].clone(),
        player_id: d["playerId"].clone(),
        position: d["position"].as_str().unwrap_or("").to_string(),
        lineup_slot: d["lineupSlot"].as_str().unwrap_or("").to_string(),
        eligible_slots: d["eligibleSlots"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect(),
        pro_team: d["proTeam"].as_str().unwrap_or("Unknown Team").to_string(),
        injury_status: d.get("injuryStatus").cloned().unwrap_or(Value::Null),
        injured: d.get("injured").and_then(Value::as_bool).unwrap_or(false),
        stats,
    }
}

fn total(player: usize, key: &str) -> Map<String, Value> {
    fixture()["players"][player]["stats"][key]["total"].as_object().unwrap().clone().into_iter().collect()
}

// ------------------------------------------------------------------ tape-backed fakes

struct TapeBuilder(HashMap<String, TapeHttp>);

impl TapeBuilder {
    fn new() -> Self {
        TapeBuilder(HashMap::new())
    }
    fn add(&mut self, url: &str, params: &Params, headers: &[(String, String)], status: u16, body: &Value) {
        let key = cache_key("GET", url, params, headers);
        self.0.insert(
            key,
            TapeHttp {
                url: url.to_string(),
                params: Value::Null,
                key_headers: HashMap::new(),
                status,
                final_url: url.to_string(),
                body: body.to_string(),
            },
        );
    }
    fn cache(self) -> Arc<HttpCache> {
        Arc::new(HttpCache::from_tape(Tape(Arc::new(TapeData { http: self.0, fxpa: HashMap::new() }))))
    }
}

const LEAGUE_URL: &str = "https://lm-api-reads.fantasy.espn.com/apis/v3/games/fhl/seasons/2027/segments/0/leagues/42";

fn view(v: &str) -> Params {
    vec![("view".to_string(), ParamValue::Str(v.to_string()))]
}

fn minimal_league_json(current_week: i64, teams: Value) -> Value {
    json!({
        "status": {"currentMatchupPeriod": 2, "firstScoringPeriod": 1, "finalScoringPeriod": 194, "previousSeasons": []},
        "scoringPeriodId": current_week,
        "seasonId": 2027,
        "settings": {
            "name": "Test League", "size": 2,
            "scheduleSettings": {"matchupPeriodCount": 20, "matchupPeriods": {"1": [1]}, "playoffTeamCount": 4,
                                 "playoffSeedingRule": "X"},
            "tradeSettings": {"vetoVotesRequired": 1}, "draftSettings": {"keeperCount": 0},
            "scoringSettings": {"matchupTieRule": "NONE", "playoffMatchupTieRule": "NONE", "scoringType": "H2H_POINTS"},
            "acquisitionSettings": {"isUsingAcquisitionBudget": false}
        },
        "schedule": [],
        "teams": teams,
    })
}

fn team_json(id: i64, name: &str) -> Value {
    json!({"id": id, "abbrev": "T", "name": name, "divisionId": 0,
           "record": {"overall": {"wins": 0, "losses": 0, "ties": 0}},
           "playoffSeed": 1, "rankCalculatedFinal": 0, "roster": {"entries": []}})
}

fn league_from(cache: Arc<HttpCache>, current_week: i64, teams: Value) -> League {
    let req = EspnFantasyRequests::new(2027, 42, None, None, cache, ESPN_TTL);
    League::from_data(req, &minimal_league_json(current_week, teams), Some(&json!([]))).unwrap()
}

fn settings(year: i64) -> Settings {
    Settings { espn_league_id: Some(42), espn_year: Some(year), ..Default::default() }
}

fn activity_filter(offset: i64) -> Vec<(String, String)> {
    let filters = json!({"topics": {"filterType": {"value": ["ACTIVITY_TRANSACTIONS"]}, "limit": 25,
                                    "limitPerMessageSet": {"value": 25}, "offset": offset,
                                    "sortMessageDate": {"sortPriority": 1, "sortAsc": false},
                                    "sortFor": {"sortPriority": 2, "sortAsc": false},
                                    "filterIncludeMessageTypeIds": {"value": [178, 180, 179, 239, 181, 244]}}});
    api::filter_header(&filters)
}

// ------------------------------------------------------------------ test_espn_parsing.py

#[test]
fn canonical_stats_maps_and_drops_placeholders() {
    let cs = canonical_stats(Some(&total(0, "Total 2027")));
    assert_eq!(cs["PM"], 3.0);
    assert!(!cs.contains_key("+/-"));
    assert_eq!(cs["PTS"], 13.0); // computed from G + A
    for junk in ["5", "12", "TTOI ?", "ATOI"] {
        assert!(!cs.contains_key(junk));
    }
    let g = canonical_stats(Some(&total(1, "Total 2027")));
    assert!((g["SVPCT"] - 0.917).abs() < 1e-9);
    assert!(!g.contains_key("MIN ?") && !g.contains_key("PTS"));
}

#[test]
fn parse_stat_lines_splits() {
    let f = fixture();
    let lines = parse_stat_lines(&espn_player(&f["players"][0]).stats, year());
    let mut keys: Vec<&str> = lines.keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, ["last7", "prior", "projected", "season"]);
    assert_eq!((lines["season"].gp, lines["prior"].gp, lines["projected"].gp), (10, 80, 82));
    assert!((lines["season"].per_game()["G"] - 0.5).abs() < 1e-12);
    let goalie = parse_stat_lines(&espn_player(&f["players"][1]).stats, year());
    assert!((goalie["season"].per_game()["GAA"] - 2.5).abs() < 1e-12); // rate stat not divided
}

#[test]
fn player_from_espn_fields() {
    let f = fixture();
    let mcd = player_from_espn(&espn_player(&f["players"][0]), year(), Some(99.9), None).unwrap();
    assert_eq!(mcd.cid, "espn:3895074");
    assert_eq!(mcd.ids.get("espn").map(String::as_str), Some("3895074"));
    assert_eq!(mcd.team.as_deref(), Some("EDM"));
    assert_eq!(mcd.positions, ["C", "F"]);
    assert_eq!(mcd.status, Status::Healthy);
    assert_eq!(mcd.name_norm, "connor mcdavid");
    assert_eq!(mcd.pct_owned, Some(99.9));
    let g = player_from_espn(&espn_player(&f["players"][1]), year(), None, None).unwrap();
    assert_eq!(g.team.as_deref(), Some("MTL"));
    assert_eq!(g.positions, ["G"]);
    assert!(g.is_goalie());
    let fa = player_from_espn(&espn_player(&f["players"][2]), year(), None, None).unwrap();
    assert_eq!(fa.team.as_deref(), Some("UTA"));
    assert_eq!(fa.status, Status::Dtd);
    assert_eq!(fa.status_note.as_deref(), Some("DAY_TO_DAY"));
    assert_eq!(fa.positions, ["RW", "F"]);
    assert!(!fa.lines.contains_key("season"));
}

#[test]
fn pro_team_abbrev_cases() {
    for (name, abbr) in [
        (Some("Montréal Canadiens"), Some("MTL")),
        (Some("Montr?al Canadiens"), Some("MTL")),
        (Some("Vegas Golden Knights"), Some("VGK")),
        (Some("Seattle Kraken"), Some("SEA")),
        (Some("Utah Hockey Club"), Some("UTA")),
        (Some("Utah Mammoth"), Some("UTA")),
        (Some("St. Louis Blues"), Some("STL")),
        (Some("Unknown Team"), None),
        (None, None),
    ] {
        assert_eq!(pro_team_abbrev(name), abbr, "{name:?}");
    }
}

#[test]
fn every_espn_constant_team_maps() {
    for name in api::pro_team_names() {
        assert!(pro_team_abbrev(Some(name)).is_some(), "{name}");
    }
}

#[test]
fn injury_status_mapping() {
    assert_eq!(map_injury_status(&json!("ACTIVE"), false), Status::Healthy);
    assert_eq!(map_injury_status(&json!("INJURY_RESERVE"), false), Status::Ir);
    assert_eq!(map_injury_status(&json!("SUSPENSION"), false), Status::Suspended);
    assert_eq!(map_injury_status(&json!("OUT"), false), Status::Out);
    assert_eq!(map_injury_status(&json!([]), false), Status::Healthy); // json_parsing's [] when absent
    assert_eq!(map_injury_status(&Value::Null, true), Status::Unknown);
}

#[test]
fn scoring_from_settings_points() {
    let f = fixture();
    let items = f["scoringItems"].as_array().unwrap();
    let cfg = scoring_from_settings(Some("H2H_POINTS"), items).unwrap();
    assert_eq!(cfg.kind.as_str(), "points");
    let want: Vec<(&str, f64)> = vec![("G", 2.0), ("A", 1.0), ("PM", 0.5), ("SOG", 0.1), ("HIT", 0.1), ("BLK", 0.5),
                                      ("PPP", 0.5), ("W", 4.0), ("GA", -2.0), ("SV", 0.2), ("SO", 3.0)];
    let mut got: Vec<(&str, f64)> = cfg.weights.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    let mut want_sorted = want.clone();
    got.sort_by(|a, b| a.0.cmp(b.0));
    want_sorted.sort_by(|a, b| a.0.cmp(b.0));
    assert_eq!(got, want_sorted); // statId 12 dropped
    // season fantasy points for the fixture skater from its totals
    let season = &parse_stat_lines(&espn_player(&f["players"][0]).stats, year())["season"];
    let pts: f64 = cfg.weights.iter().map(|(k, w)| w * season.stats.get(k).copied().unwrap_or(0.0)).sum();
    assert!((pts - (5.0 * 2.0 + 8.0 + 3.0 * 0.5 + 30.0 * 0.1 + 4.0 * 0.1 + 2.0 * 0.5 + 5.0 * 0.5)).abs() < 1e-9);
}

#[test]
fn scoring_from_settings_categories() {
    let f = fixture();
    let items = f["scoringItems"].as_array().unwrap();
    let cfg = scoring_from_settings(Some("H2H_CATEGORY"), items).unwrap();
    assert_eq!(cfg.kind.as_str(), "categories");
    assert!(cfg.categories.iter().any(|c| c == "G"));
    assert_eq!(cfg.weights["GA"], -1.0);
    assert_eq!(scoring_from_settings(Some("ROTO"), items).unwrap().kind.as_str(), "roto");
}

#[test]
fn roster_shape() {
    let f = fixture();
    let shape = roster_shape_from_counts(Some(&f["lineupSlotCounts"])).unwrap();
    let got: Vec<(&str, i64)> = shape.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    let mut got_sorted = got.clone();
    got_sorted.sort();
    let mut want = vec![("C", 2), ("LW", 2), ("RW", 2), ("D", 4), ("G", 2), ("UTIL", 1), ("BN", 5), ("IR", 3)];
    want.sort();
    assert_eq!(got_sorted, want);
}

#[test]
fn position_limits_keyed_by_default_position_id() {
    let f = fixture();
    let rs = &f["rosterSettings"];
    let pl = position_limits_from_settings(Some(rs));
    assert_eq!(pl.len(), 1);
    assert_eq!(pl["G"], 3);
    let pl = position_limits_from_settings(Some(&json!({"positionLimits": {"1": 6, "4": 8, "5": 2, "0": 0}})));
    assert_eq!(pl.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(), [("C", 6), ("D", 8), ("G", 2)]);
    assert!(position_limits_from_settings(Some(&json!({}))).is_empty());
    assert!(position_limits_from_settings(None).is_empty());
    // roster size: every non-IR lineup slot (F 9 + D 5 + G 2 + UTIL 1 + BN 5)
    assert_eq!(max_roster_from_counts(Some(&rs["lineupSlotCounts"])).unwrap(), Some(22));
    assert_eq!(max_roster_from_counts(Some(&json!({}))).unwrap(), None);
}

#[test]
fn primary_position_is_the_espn_default_position() {
    let f = fixture();
    let got: Vec<Option<String>> = f["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| player_from_espn(&espn_player(d), year(), None, None).unwrap().primary_position)
        .collect();
    assert_eq!(got, [Some("C".to_string()), Some("G".to_string()), Some("RW".to_string())]);
}

#[test]
fn find_my_team_cases() {
    let mk = |id: i64, name: &str, abbrev: &str, owner: &str| api::Team {
        team_id: id,
        team_abbrev: abbrev.into(),
        team_name: name.into(),
        division_id: json!(0),
        wins: 0,
        losses: 0,
        ties: 0,
        standing: json!(0),
        final_standing: json!(0),
        roster: vec![],
        owners: vec![json!({"id": owner})],
    };
    let teams = vec![mk(1, "Puck Luck", "PL", "{AAAA-1111}"), mk(2, "Ice Holes", "ICE", "{BBBB-2222}")];
    assert_eq!(teams[find_my_team(&teams, Some("{bbbb-2222}"), None).unwrap()].team_id, 2);
    assert_eq!(teams[find_my_team(&teams, None, Some("1")).unwrap()].team_id, 1);
    assert_eq!(teams[find_my_team(&teams, Some("{bbbb-2222}"), Some("puck")).unwrap()].team_id, 1); // ESPN_TEAM wins
    let err = find_my_team(&teams, Some("{nobody}"), None).unwrap_err().to_string();
    assert!(err.contains("1=Puck Luck"), "{err}");
}

#[test]
fn parse_espn_activity_maps_adds_drops_trades() {
    let data: Value = serde_json::from_str(ACTIVITY).unwrap();
    let names: HashMap<String, String> =
        [("1", "Mine"), ("2", "Rival"), ("3", "Other")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    let players: HashMap<i64, String> = [(101, "Added Guy".to_string())].into_iter().collect();
    let items = parse_espn_activity(data["topics"].as_array().unwrap(), &names, &players).unwrap();
    let got: Vec<(ActivityAction, &str, &str, &str, Option<&str>)> = items
        .iter()
        .map(|i| {
            (
                i.action,
                i.cid.as_deref().unwrap(),
                i.team_id.as_deref().unwrap(),
                i.group_id.as_deref().unwrap(),
                i.counterparty_id.as_deref(),
            )
        })
        .collect();
    use ActivityAction::*;
    assert_eq!(
        got,
        [
            (Add, "espn:101", "1", "t-add", None),
            (Drop, "espn:102", "1", "t-add", None),
            (TradeOut, "espn:103", "1", "t-trade", Some("2")),
            (TradeIn, "espn:103", "2", "t-trade", Some("1")),
            (TradeOut, "espn:104", "2", "t-trade", Some("1")),
            (TradeIn, "espn:104", "1", "t-trade", Some("2")),
            (Drop, "espn:105", "3", "t-drop", None),
            (Add, "espn:106", "3", "t-waiver", None),
        ]
    );
    assert_eq!(items[0].player_name.as_deref(), Some("Added Guy"));
    assert_eq!(items[0].team_name.as_deref(), Some("Mine"));
    assert_eq!(items[0].source, "espn");
    let expect = fromtimestamp_local(1791214200.0).unwrap().date();
    assert_eq!(items[0].ts.date(), expect);
}

fn topic(i: i64) -> Value {
    let base = Local.with_ymd_and_hms(2026, 10, 30, 12, 0, 0).single().unwrap() - chrono::Duration::days(i);
    json!({"id": format!("t{i}"), "date": base.timestamp() * 1000,
           "messages": [{"messageTypeId": 178, "targetId": i, "to": 1}]})
}

#[test]
fn espn_activity_pages_until_since() {
    let url = format!("{LEAGUE_URL}/communication/");
    let params = view("kona_league_communication");
    // pages 0 and 1 only are on the tape: a request for page 2 or later would be a miss (warning)
    let mut tb = TapeBuilder::new();
    for p in 0..2 {
        let topics: Vec<Value> = (p * 25..p * 25 + 25).map(topic).collect();
        tb.add(&url, &params, &activity_filter(p * 25), 200, &json!({"topics": topics}));
    }
    let league = league_from(tb.cache(), 37, json!([team_json(1, "Mine")]));
    let prov = EspnProvider::new(settings(2027), league.espn_request.cache.clone(), 75).with_league(league);
    let items = prov.activity(Some(NaiveDate::from_ymd_opt(2026, 9, 25).unwrap())); // 35 days back
    assert!(prov.warnings_now().is_empty(), "{:?}", prov.warnings_now());
    assert_eq!(items.len(), 36);
    assert_eq!(items.iter().map(|i| i.ts.date()).min(), NaiveDate::from_ymd_opt(2026, 9, 25));
    assert_eq!(items[0].team_name.as_deref(), Some("Mine"));

    // no since: capped at ESPN_MAX_PAGES pages (all 10 on the tape, the 11th would miss)
    let mut tb = TapeBuilder::new();
    let topics: Vec<Value> = (0..25).map(topic).collect();
    for p in 0..10 {
        tb.add(&url, &params, &activity_filter(p * 25), 200, &json!({"topics": topics}));
    }
    let league = league_from(tb.cache(), 37, json!([]));
    let prov = EspnProvider::new(settings(2027), league.espn_request.cache.clone(), 75).with_league(league);
    let items = prov.activity(None);
    assert!(prov.warnings_now().is_empty(), "{:?}", prov.warnings_now());
    assert_eq!(items.len(), 25); // the same 25 topics on every page, deduplicated
}

#[test]
fn espn_activity_failure_is_a_warning() {
    let league = league_from(TapeBuilder::new().cache(), 37, json!([]));
    let prov = EspnProvider::new(settings(2027), league.espn_request.cache.clone(), 75).with_league(league);
    assert!(prov.activity(None).is_empty());
    let w = prov.warnings_now();
    assert!(w[0].starts_with("ESPN activity page 1 unavailable: CacheMiss: offline and not cached"), "{w:?}");
}

fn box_entry(pid: i64, slot: i64, pts: Value) -> Value {
    json!({"lineupSlotId": slot, "playerId": pid,
           "playerPoolEntry": {"player": {"id": pid, "fullName": format!("P{pid}"), "defaultPositionId": 1,
                                          "eligibleSlots": [0], "proTeamId": 6,
                                          "stats": [{"appliedTotal": pts}]}}})
}

#[test]
fn espn_box_scores_to_lineup_days() {
    // one matchup period 2 box score, period 11 (week 12 is current)
    let matchup = json!({"winner": "UNDECIDED",
        "home": {"teamId": 1, "rosterForCurrentScoringPeriod": {"entries": [
            box_entry(1, 0, json!(4.5)), box_entry(2, 7, json!(1.0)), box_entry(3, 8, json!(0))]}},
        "away": {"teamId": 2, "rosterForCurrentScoringPeriod": {"entries": [box_entry(4, 5, json!(7.2))]}}});
    let mut tb = TapeBuilder::new();
    let params: Params = vec![
        ("view".into(), ParamValue::List(vec!["mMatchupScore".into(), "mScoreboard".into()])),
        ("scoringPeriodId".into(), ParamValue::Str("11".into())),
    ];
    let filters = json!({"schedule": {"filterMatchupPeriodIds": {"value": [2]}}});
    tb.add(LEAGUE_URL, &params, &api::filter_header(&filters), 200, &json!({"schedule": [matchup]}));
    tb.add("https://lm-api-reads.fantasy.espn.com/apis/v3/games/fhl/seasons/2027", &view("proTeamSchedules_wl"), &[],
           200, &json!({"settings": {"proTeams": []}}));
    let league = league_from(tb.cache(), 12, json!([team_json(1, "Mine"), team_json(2, "Rival")]));
    let boxes = league.box_scores(None, Some(11), false).unwrap();
    let day = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
    let rows = parse_espn_box_lineups(&boxes, day, &[1, 2]);
    let got: Vec<(&str, &str, &str, bool, Option<f64>)> =
        rows.iter().map(|r| (r.team_id.as_str(), r.cid.as_str(), r.slot.as_str(), r.starting, r.provider_pts)).collect();
    assert_eq!(
        got,
        [
            ("1", "espn:1", "C", true, Some(4.5)),
            ("1", "espn:2", "BN", false, Some(1.0)),
            ("1", "espn:3", "IR", false, Some(0.0)),
            ("2", "espn:4", "G", true, Some(7.2)),
        ]
    );
    let prov = EspnProvider::new(settings(2027), league.espn_request.cache.clone(), 75).with_league(league);
    assert_eq!(prov.scoring_period_for(NaiveDate::from_ymd_opt(2026, 10, 9).unwrap(), Some(day)), Some(11));
    assert_eq!(prov.box_scores(11, NaiveDate::from_ymd_opt(2026, 10, 9)).len(), 4);
    assert!(prov.box_scores(0, None).is_empty()); // before the first scoring period: no-op
    assert!(prov.warnings_now().is_empty(), "{:?}", prov.warnings_now());
}

#[test]
fn ownership_fields_from_espn_block() {
    let t: Value = serde_json::from_str(TRENDING).unwrap();
    let own = ownership_fields(Some(&t["players"][0]["player"])); // Sergei Murashov
    assert_eq!(
        own,
        Ownership { pct_owned: Some(73.17), pct_owned_change: Some(7.69), pct_started: Some(61.03), adp: Some(99.96),
                    adp_change: Some(0.98) }
    );
    // rostered players' league entries have no averageDraftPositionPercentChange; ADP 0 = none
    let o = ownership_fields(Some(&json!({"ownership": {"percentOwned": 99.9, "percentChange": 0.01,
                                                         "averageDraftPosition": 0}})));
    assert_eq!(o, Ownership { pct_owned: Some(99.9), pct_owned_change: Some(0.01), ..Default::default() });
    assert_eq!(ownership_fields(Some(&json!({}))), Ownership::default());
}

#[test]
fn ownership_reaches_free_agent_players() {
    let t: Value = serde_json::from_str(TRENDING).unwrap();
    let entries = t["players"].as_array().unwrap();
    let rostered_own = ownership_fields(Some(&entries[0]["player"]));
    let rostered = player_from_espn(&api::Player::new(&entries[0]).unwrap(), year(), None, Some(&rostered_own)).unwrap();
    assert_eq!(
        (rostered.pct_owned, rostered.pct_owned_change, rostered.pct_started, rostered.adp),
        (Some(73.17), Some(7.69), Some(61.03), Some(99.96))
    );
    let fa_raw = &entries[4]; // a faller, as a free-agent row
    let own = ownership_fields(Some(&fa_raw["player"]));
    let fa = player_from_espn(&api::Player::new(fa_raw).unwrap(), year(), None, Some(&own)).unwrap();
    assert_eq!(fa.name, "Filip Gustavsson");
    assert_eq!((fa.pct_owned_change, fa.adp_change), (Some(-3.82), Some(-1.19)));
    assert_eq!(fa.positions, ["G"]);
    assert_eq!(fa.team.as_deref(), Some("MIN"));
    // an explicit pct_owned wins over the block's
    let p = player_from_espn(&api::Player::new(fa_raw).unwrap(), year(), Some(1.0), Some(&own)).unwrap();
    assert_eq!(p.pct_owned, Some(1.0));
}

#[test]
fn parse_trending_risers_and_fallers() {
    let t: Value = serde_json::from_str(TRENDING).unwrap();
    let up = parse_trending(&t, year(), 3, false);
    let names: Vec<&str> = up.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Sergei Murashov", "Luke Evangelista", "Carter Hart"]);
    let first = &up[0];
    assert_eq!(first["cid"], json!("espn:5188366"));
    assert_eq!(first["espn_id"], json!("5188366"));
    assert_eq!(first["positions"], json!(["G"]));
    assert_eq!((first["pct_owned_change"].as_f64(), first["pct_owned"].as_f64(), first["adp"].as_f64()),
               (Some(7.69), Some(73.17), Some(99.96)));
    assert!(!up[1]["team"].is_null() && !up[1]["positions"].as_array().unwrap().is_empty());
    let down = parse_trending(&t, year(), 50, true);
    let got: Vec<(&str, f64)> =
        down.iter().map(|d| (d["name"].as_str().unwrap(), d["pct_owned_change"].as_f64().unwrap())).collect();
    assert_eq!(got, [("Filip Gustavsson", -3.82), ("Kevin Fiala", -2.82)]);
    assert_eq!(parse_trending(&t, year(), 50, false).len(), 4); // fallers never listed as risers
    assert!(parse_trending(&json!({"players": [{"player": {"id": 1, "fullName": "X"}}]}), year(), 50, false).is_empty());
}

#[test]
fn trending_goes_through_the_cache_with_the_sort_filter() {
    let t: Value = serde_json::from_str(TRENDING).unwrap();
    let mut tb = TapeBuilder::new();
    let url = trending_url(2027);
    assert!(url.contains("/seasons/2027/segments/0/leaguedefaults/1"));
    let params = view("kona_player_info");
    for fallers in [false, true] {
        tb.add(&url, &params, &api::filter_header(&trending_filter(2027, TRENDING_POOL, fallers)), 200, &t);
    }
    let prov = EspnProvider::new(settings(2027), tb.cache(), 75);
    let got = prov.trending(2, false);
    let names: Vec<&str> = got.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Sergei Murashov", "Luke Evangelista"]);
    assert_eq!(prov.trending(50, true).len(), 2);
    assert!(prov.warnings_now().is_empty());
    assert_eq!(
        api::py_json(&trending_filter(2027, TRENDING_POOL, false)),
        r#"{"players": {"sortPercChanged": {"sortPriority": 1, "sortAsc": false}, "limit": 300, "filterStatsForTopScoringPeriodIds": {"value": 1, "additionalValue": ["002027"]}}}"#
    );

    let bad = EspnProvider::new(settings(2027), TapeBuilder::new().cache(), 75);
    assert!(bad.trending(50, false).is_empty());
    assert!(bad.warnings_now()[0].contains("offline"), "{:?}", bad.warnings_now());
}

#[test]
fn stat_limits_from_settings_cases() {
    // the live league: lineupSlotStatLimits is {} (no games-played caps)
    assert!(stat_limits_from_settings(Some(&json!({"lineupSlotStatLimits": {}}))).is_empty());
    assert!(stat_limits_from_settings(None).is_empty());
    assert_eq!(
        stat_limits_from_settings(Some(&json!({"lineupSlotStatLimits": {"5": {"limitValue": 82, "statId": 32}}}))),
        ["G: limit 82 (stat 32)"]
    );
}

#[test]
fn acquisition_limits_and_counter() {
    let acq = json!({"matchupAcquisitionLimit": 0.2857142857142857, "matchupLimitPerScoringPeriod": true,
                     "acquisitionLimit": -1, "isUsingAcquisitionBudget": false, "acquisitionBudget": 100,
                     "waiverHours": 24, "acquisitionType": "WAIVERS_TRADITIONAL"});
    let l = acquisition_limits(Some(&acq), None);
    assert_eq!((l.per_period, l.season, l.faab_budget, l.waiver_hours), (Some(2), None, None, Some(24)));
    assert_eq!(l.per_scoring_period, Some(0.2857142857142857));
    assert_eq!(acquisition_limits(Some(&acq), Some(14)).per_period, Some(4));
    let team = json!({"transactionCounter": {"acquisitions": 6, "matchupAcquisitionTotals": {"5": 2},
                                             "acquisitionBudgetSpent": 3}});
    assert_eq!(transaction_counter(Some(&team), Some(5)), (Some(2), Some(6), Some(3.0)));
    assert_eq!(transaction_counter(Some(&team), Some(6)), (Some(0), Some(6), Some(3.0)));
    assert_eq!(transaction_counter(Some(&json!({})), Some(6)), (None, None, None));
}

// ------------------------------------------------------------------ parity (tape replay)

#[test]
fn parity_espn_raw_ctx_from_tape() {
    use fm_parity::{assert_parity, Fixtures, Tolerance};

    let fx = Fixtures::league("espn");
    let Some(tape_path) = fx.tape_path() else {
        fm_parity::skip("parity_espn_raw_ctx_from_tape", "fixtures/espn/tape.json.gz absent");
        return;
    };
    let raw = fx.raw_ctx();
    let snap = fx.snapshot();
    let tape = Tape::load(&tape_path).unwrap();
    let cache = Arc::new(HttpCache::from_tape(tape));

    let mine = raw["teams"].as_array().unwrap().iter().find(|t| t["owner_is_me"] == json!(true)).unwrap();
    let s = Settings {
        espn_league_id: Some(raw["league_id"].as_str().unwrap().parse().unwrap()),
        espn_year: Some(raw["season"].as_i64().unwrap()),
        espn_team: Some(mine["team_id"].as_str().unwrap().to_string()),
        ..Default::default()
    };

    let mut prov = EspnProvider::new(s, cache, 75).with_today(snap.today);
    let mut ctx = LeagueProvider::load(&mut prov).unwrap();
    // every request was on the tape: a miss would have become a warning (activity, schedule)
    assert!(ctx.warnings.is_empty(), "{:?}", ctx.warnings);
    assert!(prov.extras().warnings.is_empty(), "{:?}", prov.extras().warnings);
    // dynasty mode resolution (prefs > env > default) is the load pipeline's job
    ctx.dynasty_mode_source = raw["dynasty_mode_source"].as_str().map(str::to_string);
    let actual = serde_json::to_value(&ctx).unwrap();
    assert_parity!("espn_raw_ctx", raw, actual, Tolerance::exact());

    let ex = prov.extras();
    assert_eq!(ex.kind, snap.provider.kind);
    assert_eq!(ex.espn_league, snap.provider.espn_league);
    assert_eq!(ex.waiver_rank, snap.provider.waiver_rank);
    assert_eq!(ex.acquisition_settings, snap.provider.acquisition_settings);
    assert_eq!(ex.warnings, snap.provider.warnings);
}
