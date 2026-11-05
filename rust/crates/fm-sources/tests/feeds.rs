//! Ports of tests/test_moneypuck.py (parsing / client / ttl / norms), tests/test_injuries.py and
//! the parser + client parts of tests/test_dailyfaceoff.py. Fixtures are read from the Python
//! test dir (`tests/fixtures`).

use std::path::PathBuf;
use std::sync::Mutex;

use chrono::NaiveDate;
use fm_core::PyDateTime;
use fm_http::ParamValue;
use fm_http::{Params, FetchJson};
use fm_sources::dailyfaceoff as dfo;
use fm_sources::injuries as inj;
use fm_sources::moneypuck as mp;
use serde_json::{json, Value};

fn fixture(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tests/fixtures").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn approx(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9 * (1.0 + b.abs()), "{a} != {b}");
}

fn dt(s: &str) -> PyDateTime {
    PyDateTime::parse(s).unwrap()
}

// --------------------------------------------------------------------------- MoneyPuck

const LAFFERTY: i64 = 8478043;
const MITCHELL: i64 = 8484262;
const GUDBRANSON: i64 = 8475790;

#[test]
fn mp_parse_all_situation() {
    let rows = mp::parse_skaters(&fixture("moneypuck/skaters_2025_head.csv"), Some("all"), false).unwrap();
    assert_eq!(rows.iter().map(|r| r.nhl_id).collect::<Vec<_>>(), vec![LAFFERTY, MITCHELL, GUDBRANSON]);
    assert!(rows.iter().all(|r| r.situation == "all"));
    let r = &rows[0];
    assert_eq!(
        (r.name.as_str(), r.team.as_deref(), r.position.as_deref(), r.season, r.gp),
        ("Sam Lafferty", Some("CHI"), Some("C"), 2025, 29)
    );
    approx(r.toi_min, 14959.0 / 60.0);
    assert_eq!((r.ixg, r.goals, r.sog, r.ixg_adj, r.points), (0.85, 1.0, 9.0, 0.83, 2.0));
    assert_eq!(r.assists(), 1.0);
    assert_eq!(r.onice_xg_pct, Some(0.39));
    approx(r.onice_sh_pct.unwrap(), 7.0 / 78.0);
    approx(r.onice_sv_pct.unwrap(), 1.0 - 9.0 / 94.0);
    approx(r.goals_minus_ixg(), 0.15);
    approx(r.ixg_per_game().unwrap(), 0.85 / 29.0);
    assert!(r.pp_toi_min.is_none());
    assert!(rows[1].position.as_deref() == Some("D") && rows[1].group() == "D" && r.group() == "F");
}

#[test]
fn mp_situation_filtering_and_pp() {
    let text = fixture("moneypuck/skaters_2025_head.csv");
    assert_eq!(mp::parse_skaters(&text, None, false).unwrap().len(), 15);
    let five = mp::parse_skaters(&text, Some("5on5"), false).unwrap();
    assert!(five.iter().all(|r| r.situation == "5on5") && five.len() == 3);
    approx(five[2].onice_sh_pct.unwrap(), 19.0 / 203.0);
    let pk = mp::parse_skaters(&text, Some("4on5"), false).unwrap();
    assert!((pk[2].toi_min - 100.267).abs() < 1e-3);
    let rows = mp::parse_skaters(&text, Some("all"), true).unwrap();
    assert!((rows[0].pp_toi_min.unwrap() - 0.3).abs() < 1e-9);
    assert_eq!((rows[0].pp_ixg, rows[0].pp_goals), (Some(0.0), Some(0.0)));
}

#[test]
fn mp_goalies() {
    let text = fixture("moneypuck/goalies_2025_head.csv");
    let gs = mp::parse_goalies(&text, Some("all")).unwrap();
    assert_eq!(gs.len(), 1);
    let g = &gs[0];
    assert_eq!(
        (g.name.as_str(), g.team.as_deref(), g.gp, g.xga, g.ga, g.sa),
        ("Igor Shesterkin", Some("NYR"), 51, 147.25, 126.0, 1425.0)
    );
    approx(g.sv_pct().unwrap(), 1.0 - 126.0 / 1425.0);
    approx(g.gsax(), 21.25);
    assert!(g.xsv_pct().unwrap() < g.sv_pct().unwrap());
    let sits: Vec<String> = mp::parse_goalies(&text, None).unwrap().into_iter().map(|g| g.situation).collect();
    assert_eq!(sits, vec!["other", "all", "5on5", "4on5", "5on4"]);
}

#[test]
fn mp_header_only_file_is_empty_and_bom_is_stripped() {
    let text = fixture("moneypuck/skaters_2026_empty.csv");
    assert!(mp::parse_skaters(&text, Some("all"), false).unwrap().is_empty());
    assert!(mp::parse_skaters(&text, None, false).unwrap().is_empty());
    let with_bom = format!("\u{feff}{}", fixture("moneypuck/skaters_2025_head.csv"));
    assert_eq!(mp::parse_skaters(&with_bom, Some("all"), false).unwrap().len(), 3);
    assert!(mp::csv_columns("").is_none() && mp::csv_columns("  \n").is_none());
    assert!(mp::missing_columns("", &["x"]).is_empty());
    assert_eq!(mp::missing_columns(&text, &["playerId", "nope"]), vec!["nope".to_string()]);
}

#[test]
fn mp_empty_invalid_values_and_team_fixes() {
    let csv = "playerId,season,name,team,position,situation,games_played,icetime,I_F_xGoals,onIce_xGoalsPercentage\n\
               1,2025, Bob ,T.B,L,all,x,,,\n\
               2,2025,Al,L.A,R,all,3,60,1.5,0.5\n";
    let rows = mp::parse_skaters(csv, Some("all"), false).unwrap();
    assert_eq!(rows[0].team.as_deref(), Some("TBL"));
    assert_eq!(rows[0].name, "Bob");
    assert_eq!(rows[0].position.as_deref(), Some("LW"));
    assert_eq!((rows[0].gp, rows[0].toi_min, rows[0].ixg, rows[0].onice_xg_pct), (0, 0.0, 0.0, None));
    assert_eq!(rows[1].team.as_deref(), Some("LAK"));
    assert_eq!(rows[1].onice_xg_pct, Some(0.5));
}

struct FakeFetch {
    calls: Mutex<Vec<String>>,
}

#[test]
fn mp_client_urls_memo_and_situation_check() {
    let fake = FakeFetch { calls: Mutex::new(Vec::new()) };
    let sk = fixture("moneypuck/skaters_2025_head.csv");
    let go = fixture("moneypuck/goalies_2025_head.csv");
    let fetch = |url: &str, _p: &Params, _ttl: f64| -> anyhow::Result<String> {
        fake.calls.lock().unwrap().push(url.to_string());
        if url.ends_with("/2025/regular/skaters.csv") {
            Ok(sk.clone())
        } else if url.ends_with("/2025/regular/goalies.csv") {
            Ok(go.clone())
        } else {
            anyhow::bail!("unexpected url {url}")
        }
    };
    let c = mp::MoneyPuckClient::new(Box::new(fetch));
    assert_eq!(c.skaters(2025, Some("all"), false).unwrap().len(), 3);
    assert_eq!(c.skaters(20252026, Some("5on5"), false).unwrap().len(), 3); // NHL season ids accepted
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec!["https://moneypuck.com/moneypuck/playerData/seasonSummary/2025/regular/skaters.csv".to_string()]
    );
    assert_eq!(c.goalies(2025, Some("all")).unwrap()[0].nhl_id, 8478048);
    let by = c.skaters_by_id(2025, "all", false).unwrap();
    assert_eq!(by.keys().copied().collect::<Vec<_>>(), vec![LAFFERTY, MITCHELL, GUDBRANSON]);
    assert!(c.skaters(2025, Some("5v5"), false).is_err());
}

#[test]
fn mp_ttl_and_cached_fetch() {
    let today = NaiveDate::from_ymd_opt(2026, 9, 29);
    assert_eq!(mp::ttl_for_url(&mp::season_url(2026, mp::Kind::Skaters), today), mp::TTL_CURRENT);
    assert_eq!(mp::ttl_for_url(&mp::season_url(2025, mp::Kind::Skaters), today), mp::TTL_PAST);
    assert_eq!(
        mp::ttl_for_url(&mp::season_url(2025, mp::Kind::Skaters), NaiveDate::from_ymd_opt(2026, 3, 1)),
        mp::TTL_CURRENT
    );
    assert_eq!(mp::start_year(20252026), 2025);
    // the client asks its fetcher with the season-dependent ttl
    let seen = Mutex::new(Vec::new());
    let sk = fixture("moneypuck/skaters_2025_head.csv");
    let c = mp::MoneyPuckClient::new(Box::new(|url: &str, _p: &Params, ttl: f64| {
        seen.lock().unwrap().push((url.to_string(), ttl));
        Ok(sk.clone())
    }));
    c.skaters(2018, Some("all"), false).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![(mp::season_url(2018, mp::Kind::Skaters), mp::TTL_PAST)]);
}

#[test]
fn mp_norms_and_calibration() {
    let text = fixture("moneypuck/skaters_2025_head.csv");
    let rows = mp::parse_skaters(&text, Some("5on5"), false).unwrap();
    let n = mp::onice_sh_norms(&rows, 0.0);
    approx(n["F"], 7.0 / 78.0);
    approx(n["D"], (3.0 + 19.0) / (31.0 + 203.0));
    let n2 = mp::onice_sh_norms(&rows, 200.0);
    assert_eq!(n2.len(), 2);
    approx(n2["F"], 7.0 / 78.0);
    approx(n2["D"], 19.0 / 203.0);
    let cal = mp::xg_calibration(&mp::parse_skaters(&text, Some("all"), false).unwrap());
    approx(cal["F"], 1.0 / 0.85);
    approx(cal["D"], 2.0 / (0.38 + 1.11));
}

// --------------------------------------------------------------------------- injuries

fn reports() -> Vec<inj::InjuryReport> {
    let payload: Value = serde_json::from_str(&fixture("injuries/espn_injuries.json")).unwrap();
    let calls = Mutex::new(Vec::new());
    let fetch = |url: &str, _p: &Params, _ttl: f64| -> anyhow::Result<Value> {
        calls.lock().unwrap().push(url.to_string());
        Ok(payload.clone())
    };
    let f: &FetchJson = &fetch;
    let out = inj::fetch_injuries(Some(f), 3600.0).unwrap();
    assert_eq!(*calls.lock().unwrap(), vec![inj::INJURIES_URL.to_string()]);
    out
}

fn rep<'a>(rs: &'a [inj::InjuryReport], name: &str) -> &'a inj::InjuryReport {
    rs.iter().find(|r| r.player_name == name).unwrap_or_else(|| panic!("no {name}"))
}

#[test]
fn inj_parse_basic_fields_and_statuses() {
    let rs = reports();
    let greer = rep(&rs, "A.J. Greer");
    assert_eq!(greer.espn_player_id, Some(3648015));
    assert_eq!((greer.team_name.as_deref(), greer.team.as_deref()), (Some("Anaheim Ducks"), Some("ANA")));
    assert_eq!(greer.position.as_deref(), Some("LW"));
    assert_eq!((greer.status_raw.as_str(), greer.status), ("Day-To-Day", inj::InjuryStatus::Dtd));
    assert_eq!(greer.injury_type.as_deref(), Some("Upper Body"));
    assert_eq!(greer.return_date, NaiveDate::from_ymd_opt(2026, 10, 2));
    assert_eq!(greer.updated, Some(dt("2026-09-27T13:59:00Z")));
    assert!(greer.comment.is_some() && !greer.is_out());
    assert_eq!(rep(&rs, "Troy Terry").status, inj::InjuryStatus::Out);
    assert_eq!(rep(&rs, "Matthew Poitras").status, inj::InjuryStatus::Ir);
    assert_eq!(rep(&rs, "Charlie McAvoy").status, inj::InjuryStatus::Suspended);
    assert!(rep(&rs, "Connor Hellebuyck").is_out());
    assert_eq!(rep(&rs, "Seamus Casey").team.as_deref(), Some("NJD"));
    let lam = rep(&rs, "Maveric Lamoureux");
    assert_eq!((lam.team_name.as_deref(), lam.team.as_deref()), (Some("Utah Mammoth"), Some("UTA")));
}

#[test]
fn inj_map_status_and_team_table() {
    use inj::InjuryStatus::*;
    for (raw, want) in [
        (Some("Day-To-Day"), Dtd),
        (Some("day-to-day"), Dtd),
        (Some("Out"), Out),
        (Some("Injured Reserve"), Ir),
        (Some("IR"), Ir),
        (Some("Long-Term Injured Reserve"), Ltir),
        (Some("LTIR"), Ltir),
        (Some("Suspension"), Suspended),
        (Some("Suspended"), Suspended),
        (Some("Questionable"), Dtd),
        (Some(""), Unknown),
        (None, Unknown),
        (Some("Active"), Unknown),
    ] {
        assert_eq!(inj::map_status(raw, None), want, "{raw:?}");
    }
    assert_eq!(inj::map_status(None, Some("INJURY_STATUS_IR")), Ir);
    assert_eq!(inj::map_status(None, Some("INJURY_STATUS_DAYTODAY")), Dtd);
    assert_eq!(inj::map_status(None, Some("INJURY_STATUS_SUSPENSION")), Suspended);
    let uniq: std::collections::BTreeSet<_> = inj::ESPN_TEAM_TO_NHL.iter().map(|(_, v)| *v).collect();
    assert_eq!(uniq.len(), 32);
    assert_eq!(inj::team_to_nhl(Some("Utah Hockey Club"), None).as_deref(), Some("UTA"));
    assert_eq!(inj::team_to_nhl(None, Some("TB")).as_deref(), Some("TBL"));
    assert_eq!(inj::team_to_nhl(None, Some("EDM")).as_deref(), Some("EDM"));
    assert_eq!(inj::team_to_nhl(Some("Nowhere Nobodies"), None), None);
}

// --------------------------------------------------------------------------- Daily Faceoff

fn wrap(data: &Value) -> String {
    format!("<html><body><script id=\"__NEXT_DATA__\" type=\"application/json\">{}</script></body></html>", data)
}

#[test]
fn dfo_parse_team_lines_dallas() {
    let tl = dfo::parse_team_lines(&fixture("dailyfaceoff/dallas-stars.html"), None).unwrap();
    assert_eq!((tl.team.as_str(), tl.slug.as_deref()), ("DAL", Some("dallas-stars")));
    assert_eq!(tl.updated_at, Some(dt("2026-09-28T18:13:31.333Z")));
    assert_eq!(tl.source.as_deref(), Some("Sam Nestler"));
    assert!(tl.source_url.as_deref().unwrap().starts_with("https://x.com/"));
    assert_eq!(tl.players.len(), 20);
    let by = |n: &str| tl.players.iter().find(|p| p.name == n).unwrap();
    let r = by("Mikko Rantanen");
    assert_eq!(
        (r.line.as_deref(), r.pp_unit.as_deref(), r.pk_unit.as_deref(), r.position.as_deref()),
        (Some("f1"), Some("pp1"), None, Some("RW"))
    );
    assert_eq!(r.groups, vec!["f1", "pp1"]);
    assert_eq!(r.dfo_id, Some(2477));
    assert!((r.toi_last5.unwrap() - (108.0 + 25.0 / 60.0) / 5.0).abs() < 0.01);
    assert_eq!((r.ppp_last5, r.gp_last10), (Some(3), Some(10)));
    let h = by("Miro Heiskanen");
    assert_eq!(
        (h.line.as_deref(), h.pp_unit.as_deref(), h.pk_unit.as_deref(), h.position.as_deref()),
        (Some("d1"), Some("pp1"), Some("pk1"), Some("D"))
    );
    assert!(by("Jake Oettinger").line.as_deref() == Some("g") && by("Jake Oettinger").goalie_depth == Some(1));
    assert!(by("Casey DeSmith").goalie_depth == Some(2) && by("Casey DeSmith").position.as_deref() == Some("G"));
    let pp2: Vec<&str> = tl.by_group("pp2").iter().take(2).map(|p| p.name.as_str()).collect();
    assert_eq!(pp2, vec!["Justin Hryckowian", "Roope Hintz"]);
    assert!(tl.players.iter().all(|p| p.injury_status.is_none() && !p.gtd));
}

#[test]
fn dfo_parse_team_lines_injured_group() {
    let tl = dfo::parse_team_lines(&fixture("dailyfaceoff/edmonton-oilers.html"), None).unwrap();
    assert_eq!((tl.team.as_str(), tl.source.as_deref()), ("EDM", Some("Projected")));
    let by = |n: &str| tl.players.iter().find(|p| p.name == n).unwrap();
    let rnh = by("Ryan Nugent-Hopkins");
    assert!(rnh.group == "ir" && rnh.line.is_none() && !rnh.in_lineup() && rnh.injury_status.as_deref() == Some("dtd"));
    assert_eq!(by("Frederik Andersen").injury_status.as_deref(), Some("out"));
    assert!(by("Tristan Jarry").dfo_id == Some(2626) && by("Tristan Jarry").goalie_depth == Some(1));
}

#[test]
fn dfo_parser_walks_json_instead_of_fixed_path() {
    let data = dfo::extract_next_data(&fixture("dailyfaceoff/dallas-stars.html")).unwrap();
    let comb = data["props"]["pageProps"]["combinations"].clone();
    let moved = json!({"props": {"pageProps": {"page": {"sections": [{"x": 1}, {"lineup": comb}]}}}});
    let tl = dfo::parse_team_lines(&wrap(&moved), None).unwrap();
    assert!(tl.team == "DAL" && tl.players.len() == 20);
    let e1 = dfo::parse_team_lines("<html>no data</html>", None).unwrap_err();
    assert!(e1.downcast_ref::<dfo::DfoError>().is_some());
    let e2 = dfo::parse_team_lines(&wrap(&json!({"props": {"pageProps": {"combinations": {"players": []}}}})), None)
        .unwrap_err();
    assert!(e2.downcast_ref::<dfo::DfoError>().is_some());
}

#[test]
fn dfo_parse_starting_goalies() {
    let starts = dfo::parse_starting_goalies(&fixture("dailyfaceoff/starting-goalies-2026-09-29.html")).unwrap();
    assert_eq!(starts.len(), 10);
    let games: std::collections::BTreeSet<&str> = starts.iter().map(|s| s.game.as_str()).collect();
    assert_eq!(games, ["CHI@VGK", "FLA@CAR", "MTL@TOR", "NYR@BOS", "VAN@EDM"].into_iter().collect());
    let sway = starts.iter().find(|s| s.goalie_name == "Jeremy Swayman").unwrap();
    assert_eq!(
        (sway.team.as_str(), sway.opponent.as_deref(), sway.home, sway.dfo_id),
        ("BOS", Some("NYR"), true, Some(28098))
    );
    assert!(sway.strength.as_deref() == Some("Confirmed") && sway.is_start() && sway.is_confirmed());
    assert_eq!(sway.source.as_deref(), Some("Jack Studley"));
    assert_eq!(sway.created_at, Some(dt("2026-09-29T15:02:24.985Z")));
    assert_eq!(sway.game_time, Some(dt("2026-09-30T00:00:00Z")));
    let shes = starts.iter().find(|s| s.goalie_name == "Igor Shesterkin").unwrap();
    assert!(shes.strength.is_none() && !shes.is_start());
    assert_eq!(starts.iter().filter(|s| s.is_start()).count(), 2);
    assert!(dfo::parse_starting_goalies(&wrap(&json!({"props": {"pageProps": {"data": []}}}))).unwrap().is_empty());
}

#[test]
fn dfo_team_slug_map_complete_and_matches_site() {
    assert_eq!(dfo::TEAM_SLUGS.len(), 32);
    let slugs: std::collections::BTreeSet<_> = dfo::TEAM_SLUGS.iter().map(|(_, s)| *s).collect();
    assert_eq!(slugs.len(), 32);
    let parsed = dfo::parse_team_slugs(&fixture("dailyfaceoff/dallas-stars.html")).unwrap();
    let want: Vec<(String, String)> =
        dfo::TEAM_SLUGS.iter().map(|(t, s)| (t.to_string(), s.to_string())).collect();
    let mut got: Vec<(String, String)> = parsed.into_iter().collect();
    got.sort();
    let mut want_sorted = want;
    want_sorted.sort();
    assert_eq!(got, want_sorted); // sortedTeams on the real page
    assert_eq!(
        dfo::team_url("VEG").unwrap(),
        "https://www.dailyfaceoff.com/teams/vegas-golden-knights/line-combinations"
    );
    assert_eq!(
        dfo::goalies_url(NaiveDate::from_ymd_opt(2026, 9, 29).unwrap()),
        "https://www.dailyfaceoff.com/starting-goalies/2026-09-29"
    );
    assert!(dfo::team_url("XXX").unwrap_err().downcast_ref::<dfo::DfoError>().is_some());
}

#[test]
fn dfo_client_degrades_per_team() {
    let dal = fixture("dailyfaceoff/dallas-stars.html");
    let calls = Mutex::new(0usize);
    let fetch = |url: &str, _p: &Params, ttl: f64| -> anyhow::Result<String> {
        *calls.lock().unwrap() += 1;
        assert_eq!(ttl, dfo::TTL_LINES);
        if url.contains("dallas-stars") {
            Ok(dal.clone())
        } else if url.contains("edmonton-oilers") {
            Ok("<html>maintenance</html>".to_string())
        } else {
            anyhow::bail!("HTTP 503")
        }
    };
    let mut c = dfo::DailyFaceoffClient::new(Box::new(fetch));
    let out = c.all_lines(Some(&["DAL".to_string(), "EDM".to_string(), "TB".to_string()]));
    assert_eq!(out.keys().cloned().collect::<Vec<_>>(), vec!["DAL".to_string()]);
    assert_eq!(*calls.lock().unwrap(), 3);
    assert!(c.warnings.len() == 1 && c.warnings[0].contains("2 team(s)") && c.warnings[0].contains("EDM"));
    assert_eq!(dfo::ttl_for("https://www.dailyfaceoff.com/starting-goalies/2026-09-29"), 3.0 * 3600.0);
    assert!(dfo::USER_AGENT.contains("personal use"));
}

#[test]
fn dfo_cached_fetch_uses_cache_ttls_offline() {
    // through a real (offline, empty) cache: a miss proves the call went through HttpCache
    let dir = tempfile::tempdir().unwrap();
    let cache = fm_http::HttpCache::open(dir.path(), true).unwrap();
    let f = dfo::make_cached_fetch_text(&cache, None, None);
    let url = dfo::team_url("DAL").unwrap();
    assert!(f(&url, &Vec::new(), 0.0).unwrap_err().downcast_ref::<fm_http::CacheMiss>().is_some());
    let _ = ParamValue::None;
}

/// One-off parity helper: with FM_DUMP_DIR set, write what Rust parsed so it can be diffed with
/// the Python `model_dump(mode="json")` output.
#[test]
fn dump_parse_outputs_for_python_diff() {
    let Ok(dir) = std::env::var("FM_DUMP_DIR") else { return };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, html) in [("dal", "dallas-stars"), ("edm", "edmonton-oilers")] {
        let tl = dfo::parse_team_lines(&fixture(&format!("dailyfaceoff/{html}.html")), None).unwrap();
        std::fs::write(dir.join(format!("{name}.json")), serde_json::to_string(&tl).unwrap()).unwrap();
    }
    let g = dfo::parse_starting_goalies(&fixture("dailyfaceoff/starting-goalies-2026-09-29.html")).unwrap();
    std::fs::write(dir.join("goalies.json"), serde_json::to_string(&g).unwrap()).unwrap();
    std::fs::write(dir.join("injuries.json"), serde_json::to_string(&reports()).unwrap()).unwrap();
}
