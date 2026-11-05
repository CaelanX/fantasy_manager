//! Tests for the W1-core-runtime modules: config, prefs, normalize, calendar, activity,
//! unproven. Ports tests/test_config.py, tests/test_prefs.py and the normalize cases of
//! tests/test_matching.py; expected values for the Python-parity cases were produced by the
//! Python implementation.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use fm_core::activity::{count_adds, recent_adds_from, RECENT_ADDS_WINDOW};
use fm_core::calendar::calendar_weeks;
use fm_core::config::{default_espn_year, format_points, parse_points, Settings};
use fm_core::models::{ActivityItem, DynastyMode};
use fm_core::normalize::*;
use fm_core::prefs::*;
use fm_core::snapshot::Snapshot;
use fm_core::unproven::is_unproven;
use serde_json::{json, Value};

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn fixtures() -> PathBuf {
    std::env::var_os("FM_PARITY_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures"))
}

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn load(pairs: &[(&str, &str)]) -> anyhow::Result<Settings> {
    Settings::load_from(d(2026, 11, 4), None, env(pairs))
}

// ------------------------------------------------------------------------------------ config

#[test]
fn parse_points_cases() {
    let p = parse_points("G=3, a=2,SOG=0.4").unwrap();
    assert_eq!(p.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(), vec![("G", 3.0), ("A", 2.0), ("SOG", 0.4)]);
    assert!(parse_points("").unwrap().is_empty());
    assert!(parse_points("G3").is_err());
    let g = parse_points("FIGHTS=1,goalie.g=20").unwrap();
    assert_eq!(g.get("FT"), Some(&1.0));
    assert_eq!(g.get("goalie.G"), Some(&20.0));
    assert_eq!(format_points(&g, None), "FT=1,goalie.G=20");
}

#[test]
fn default_year() {
    assert_eq!(default_espn_year(d(2026, 9, 28)), 2027);
    assert_eq!(default_espn_year(d(2027, 3, 1)), 2027);
}

#[test]
fn settings_from_env() {
    let s = load(&[
        ("ESPN_LEAGUE_ID", "4242"),
        ("ESPN_YEAR", ""),
        ("FANTRAX_POINTS", "G=3,A=2"),
        ("FANTRAX_DYNASTY", "true"),
        ("FM_OFFLINE", "1"),
        ("FANTRAX_KEEPERS", "3"),
        ("unrelated_var", "x"),
    ])
    .unwrap();
    assert_eq!(s.espn_league_id, Some(4242));
    assert_eq!(s.espn_year, Some(2027)); // empty ignored -> default from today (2026-11-04)
    assert_eq!(s.fantrax_points.get("G"), Some(&3.0));
    assert!(s.fantrax_dynasty && s.fm_offline);
    assert_eq!(s.fm_llm_model, "openrouter/free");
    assert_eq!(s.fantrax_keepers, Some(3));
    assert_eq!(s.fantrax_keeper_horizon_years, 3);
    // pathlib: Path("./data") is "data" (W11b live: `harness params` printed ".\data\harness\params")
    assert_eq!(s.fm_data_dir.to_string_lossy(), "data");
    assert_eq!(fm_core::config::py_path("./data/").to_string_lossy(), "data");
    assert_eq!(fm_core::config::py_path(".").to_string_lossy(), ".");
    if cfg!(windows) {
        assert_eq!(fm_core::config::py_path("C:/x/./y").to_string_lossy(), r"C:\x\y");
    }
    assert_eq!(s.fm_web_trusted_proxies, "127.0.0.1,::1");
    assert!(!s.fields_set.contains("fantrax_mode"));
}

#[test]
fn bool_and_int_parsing() {
    for (v, want) in [("Yes", true), ("off", false), ("T", true), ("0", false), ("ON", true), ("n", false)] {
        assert_eq!(load(&[("FM_OFFLINE", v)]).unwrap().fm_offline, want, "{v}");
    }
    assert!(load(&[("FM_OFFLINE", "maybe")]).is_err());
    assert_eq!(load(&[("FM_WEB_SESSION_DAYS", " 7 ")]).unwrap().fm_web_session_days, 7);
    assert!(load(&[("ESPN_LEAGUE_ID", "abc")]).is_err());
    assert_eq!(load(&[]).unwrap().fantrax_keepers, None);
}

#[test]
fn secrets_are_masked() {
    let s = load(&[("FANTRAX_COOKIE", "supersecret"), ("OPENROUTER_API_KEY", "k-123")]).unwrap();
    let dbg = format!("{s:?}");
    assert!(!dbg.contains("supersecret") && !dbg.contains("k-123"), "{dbg}");
    assert_eq!(s.fantrax_cookie.as_ref().unwrap().expose(), "supersecret");
}

#[test]
fn fantrax_mode_parsing() {
    assert_eq!(load(&[]).unwrap().fantrax_mode, DynastyMode::Balanced);
    assert_eq!(load(&[("FANTRAX_MODE", "Rebuild")]).unwrap().fantrax_mode, DynastyMode::Rebuild);
    assert!(load(&[("FANTRAX_MODE", "tank")]).is_err());
}

#[test]
fn env_file_and_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join(".env");
    std::fs::write(&f, "FANTRAX_MODE=rebuild\nfm_llm_model=from-file\nODDS_REGION=\nFM_OFFLINE=true\nUNKNOWN=1\n").unwrap();
    let s = Settings::load_from(d(2026, 11, 4), Some(&f), env(&[("FM_LLM_MODEL", "from-env"), ("FM_OFFLINE", "")])).unwrap();
    assert_eq!(s.fantrax_mode, DynastyMode::Rebuild);
    assert_eq!(s.fm_llm_model, "from-env"); // env overrides the file
    assert!(s.fm_offline); // empty env value ignored, so the file's value stays
    assert_eq!(s.odds_region, "us"); // empty file value ignored
    // a missing file is fine
    Settings::load_from(d(2026, 11, 4), Some(&tmp.path().join("nope.env")), env(&[])).unwrap();
}

// ------------------------------------------------------------------------------------ prefs

fn data(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().join("data")
}

#[test]
fn prefs_roundtrip_and_atomic() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = data(&tmp);
    assert_eq!(get_pref("x", &dir), None); // missing file and dir
    set_pref("x", Some(json!(3)), &dir).unwrap();
    set_pref("dynasty_mode", Some(json!("rebuild")), &dir).unwrap();
    assert_eq!(get_pref("x", &dir), Some(json!(3)));
    let all = load_prefs(&dir);
    assert_eq!(all.len(), 2);
    let text = std::fs::read_to_string(dir.join("prefs.json")).unwrap();
    assert_eq!(text, "{\n  \"dynasty_mode\": \"rebuild\",\n  \"x\": 3\n}\n"); // sort_keys, indent=2
    let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, vec![std::ffi::OsString::from("prefs.json")]); // no temp files left
    set_pref("x", None, &dir).unwrap();
    assert_eq!(serde_json::to_value(load_prefs(&dir)).unwrap(), json!({"dynasty_mode": "rebuild"}));
}

#[test]
fn prefs_non_ascii_is_escaped_like_python() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = data(&tmp);
    set_pref("name", Some(json!("Nečas \u{1F600}")), &dir).unwrap();
    let text = std::fs::read_to_string(dir.join("prefs.json")).unwrap();
    assert_eq!(text, "{\n  \"name\": \"Ne\\u010das \\ud83d\\ude00\"\n}\n");
    assert_eq!(get_pref("name", &dir), Some(json!("Nečas \u{1F600}")));
}

#[test]
fn corrupt_file_is_tolerated() {
    for content in ["{not json", "[1, 2]", "", "\u{0}\u{1}"] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = data(&tmp);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs.json"), content).unwrap();
        assert!(load_prefs(&dir).is_empty(), "{content:?}");
        assert_eq!(get_pref("dynasty_mode", &dir), None);
        let mut s = load(&[]).unwrap();
        s.fm_data_dir = dir.clone();
        assert_eq!(dynasty_mode_info(&s), (DynastyMode::Balanced, "default".to_string()));
        set_pref("dynasty_mode", Some(json!("contend")), &dir).unwrap(); // overwrites the corrupt file
        assert_eq!(get_pref("dynasty_mode", &dir), Some(json!("contend")));
    }
}

#[test]
fn mode_precedence_pref_over_env_over_default() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = data(&tmp);
    let dir_s = dir.to_str().unwrap().to_string();
    let s = load(&[("FM_DATA_DIR", &dir_s)]).unwrap();
    assert_eq!(dynasty_mode_info(&s), (DynastyMode::Balanced, "default".to_string()));
    let s = load(&[("FM_DATA_DIR", &dir_s), ("FANTRAX_MODE", "Contend")]).unwrap();
    assert_eq!(dynasty_mode_info(&s), (DynastyMode::Contend, "env".to_string()));
    set_pref(DYNASTY_MODE_KEY, Some(json!("rebuild")), &s.fm_data_dir).unwrap();
    assert_eq!(dynasty_mode_info(&s), (DynastyMode::Rebuild, "prefs".to_string()));
    assert_eq!(effective_dynasty_mode(&s), DynastyMode::Rebuild);
    set_pref(DYNASTY_MODE_KEY, Some(json!("tank")), &s.fm_data_dir).unwrap(); // invalid pref ignored
    assert_eq!(dynasty_mode_info(&s), (DynastyMode::Contend, "env".to_string()));
    assert_eq!(mode_source_label("prefs"), "dashboard/prefs");
    assert_eq!(mode_source_label("env"), "FANTRAX_MODE");
    assert_eq!(mode_source_label("option"), "--mode, this run only");
    assert_eq!(mode_source_label("weird"), "weird");
    assert_eq!(normalize_mode(" Rebuild "), Some(DynastyMode::Rebuild));
    assert_eq!(normalize_mode("tank"), None);
}

#[test]
fn env_file_counts_as_env() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("mode.env");
    std::fs::write(&f, "FANTRAX_MODE=rebuild\n").unwrap();
    let mut s = Settings::load_from(d(2026, 11, 4), Some(&f), env(&[])).unwrap();
    s.fm_data_dir = data(&tmp);
    assert_eq!(dynasty_mode_info(&s), (DynastyMode::Rebuild, "env".to_string()));
}

#[test]
fn harness_auto_apply_values() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = data(&tmp);
    assert!(harness_auto_apply(&dir));
    for (v, want) in [(json!("Off"), false), (json!("yes"), true), (json!(false), false), (json!(true), true), (json!(0), false)] {
        set_pref(HARNESS_AUTO_APPLY_KEY, Some(v.clone()), &dir).unwrap();
        assert_eq!(harness_auto_apply(&dir), want, "{v}");
    }
}

// --------------------------------------------------------------------------------- normalize

#[test]
fn normalize_equivalences() {
    let pairs = [
        ("Chris Tanev", "Christopher Tanev"),
        ("Mike Matheson", "Michael Matheson"),
        ("Will Smith", "William Smith"),
        ("Tim Stützle", "Tim Stutzle"),
        ("Jesperi Kotkaniemi", "JESPERI  KOTKANIEMI"),
        ("Ryan O'Reilly", "Ryan OReilly"),
        ("Ryan O’Reilly", "Ryan O'Reilly"),
        ("Jacob Bernard-Docker", "Jacob Bernard Docker"),
        ("Lukas Dostál", "Lukas Dostal"),
        ("Oliver Ekman-Larsson", "Oliver Ekman Larsson"),
        ("Mads Søgaard", "Mads Sogaard"),
        ("Tim Stützle Jr.", "Tim Stutzle"),
    ];
    for (a, b) in pairs {
        assert_eq!(normalize_name(Some(a)), normalize_name(Some(b)), "{a} vs {b}");
    }
}

#[test]
fn normalize_name_matches_python() {
    // [input, Python's output]
    let cases: Vec<(&str, &str)> = vec![
        ("Martin Nečas", "martin necas"),
        ("T.J. Oshie", "tj oshie"),
        ("T. J. Oshie", "tj oshie"),
        ("t.j.oshie", "tj oshie"),
        ("Alex Ovechkin", "alexander ovechkin"),
        ("Jacob Bernard-Docker", "jacob bernard docker"),
        ("Ryan O\u{2019}Reilly", "ryan oreilly"),
        ("Łukasz Øtest ßeta", "lukasz otest sseta"),
        ("Mike", "mike"),
        ("Chris Jr.", "chris"),
        ("Jr Sr", "jr sr"),
        ("J. T. Miller", "jt miller"),
        ("P.K. Subban Jr.", "pk subban"),
        ("A B C D Smith", "abcd smith"),
        ("Ørn Ðan Þor", "orn an or"),
        ("Nick Alex", "nicholas alex"),
        ("O'Brien-Smith III", "obrien smith"),
        ("a.b", "a b"),
        ("..x", "x"),
        ("x..", "x"),
        ("Zoë \u{ff26}\u{ff55}\u{ff4c}\u{ff4c}", "zoe full"),
        ("Élie 3rd", "elie 3rd"),
        (" \u{ff34} \u{ff2a}  Oshie ", "tj oshie"),
        ("Matt  Boldy", "matthew boldy"),
        ("\u{1c5} test", "dz test"),
        ("\u{ff21}.\u{ff22}. Foo", "ab foo"),
        ("\u{130}stanbul Dan", "istanbul dan"),
    ];
    for (input, want) in cases {
        assert_eq!(normalize_name(Some(input)), want, "{input:?}");
    }
}

#[test]
fn normalize_details() {
    assert_eq!(normalize_name(Some("Martin Nečas")), "martin necas");
    assert_eq!(normalize_name(Some("  Juraj   Slafkovský ")), "juraj slafkovsky");
    assert_eq!(normalize_name(Some("")), "");
    assert_eq!(normalize_name(None), "");
    assert_eq!(normalize_name(Some("Jake Guentzel")), "jake guentzel");
    assert_eq!(strip_accents("Nečas ø"), "Necas o");
}

#[test]
fn team_and_position_normalization() {
    assert_eq!(normalize_team(Some("NJ")).as_deref(), Some("NJD"));
    assert_eq!(normalize_team(Some("tb")).as_deref(), Some("TBL"));
    assert_eq!(normalize_team(Some("Utah")).as_deref(), Some("UTA"));
    assert_eq!(normalize_team(Some("EDM")).as_deref(), Some("EDM"));
    assert_eq!(normalize_team(Some("ARI")).as_deref(), Some("UTA"));
    assert_eq!(normalize_team(None), None);
    assert_eq!(normalize_team(Some("")), None);
    let set = |s: &str| -> Vec<String> {
        let mut v: Vec<String> = normalize_positions(Some(s)).into_iter().collect();
        v.sort();
        v
    };
    assert_eq!(set("C/LW"), vec!["C", "LW"]);
    assert_eq!(set("L"), vec!["LW"]);
    assert_eq!(set("F"), vec!["C", "LW", "RW"]);
    assert_eq!(set("W, d"), vec!["D", "LW", "RW"]);
    assert!(set("X").is_empty() && normalize_positions(None).is_empty());
    assert!(positions_agree(Some("C"), Some("LW")));
    assert!(!positions_agree(Some("C"), Some("D")));
    assert!(!positions_agree(Some("G"), None));
    assert!(positions_agree(Some("D"), Some("D/LW")));
}

// ------------------------------------------------------------------- activity / unproven

fn item(action: &str, team: Option<&str>, cid: Option<&str>, ts: &str) -> ActivityItem {
    serde_json::from_value(json!({
        "source": "espn", "tx_id": "t", "ts": ts, "team_id": team, "action": action, "cid": cid,
    }))
    .unwrap()
}

#[test]
fn count_adds_and_recent_adds() {
    let items = vec![
        item("ADD", Some("13"), Some("espn:1"), "2026-10-20T12:00:00"),
        item("ADD", Some("13"), Some("espn:2"), "2026-10-30T12:00:00"),
        item("ADD", Some("13"), Some("espn:1"), "2026-11-02T12:00:00"), // later add of espn:1
        item("DROP", Some("13"), Some("espn:3"), "2026-11-02T12:00:00"),
        item("ADD", Some("7"), Some("espn:4"), "2026-11-02T12:00:00"),
        item("ADD", Some("13"), Some("espn:5"), "2026-09-01T12:00:00"), // outside window
        item("ADD", Some("13"), None, "2026-11-01T12:00:00"),           // no cid
    ];
    assert_eq!(count_adds(&items, "13", None, None), 5);
    assert_eq!(count_adds(&items, "13", Some(d(2026, 10, 30)), Some(d(2026, 11, 1))), 2);
    assert_eq!(count_adds(&items, "13", Some(d(2026, 11, 2)), None), 1);
    assert_eq!(count_adds(&[], "13", None, None), 0);
    let r = recent_adds_from(&items, "13", d(2026, 11, 4), RECENT_ADDS_WINDOW);
    assert_eq!(r.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(), vec![("espn:2", d(2026, 10, 30)), ("espn:1", d(2026, 11, 2))]); // 10-20 add is before the window
}

fn snapshot(lg: &str) -> Option<Snapshot> {
    let p = fixtures().join(lg).join("snapshot.json");
    p.exists().then(|| Snapshot::load(&p).unwrap())
}

#[test]
fn unproven_matches_python_on_fixtures() {
    // Python: number of is_unproven players among ctx.all_players()
    for (lg, total, unproven) in [("espn", 652usize, 118usize), ("fantrax", 941, 193)] {
        let Some(snap) = snapshot(lg) else { continue };
        let ps = snap.ctx.all_players();
        assert_eq!(ps.len(), total, "{lg}");
        assert_eq!(ps.iter().filter(|p| is_unproven(p)).count(), unproven, "{lg}");
    }
}

#[test]
fn calendar_weeks_matches_python_on_fixtures() {
    for lg in ["espn", "fantrax"] {
        let Some(snap) = snapshot(lg) else { continue };
        let ctx = &snap.ctx;
        let w = calendar_weeks(ctx, None, None, None);
        assert_eq!(w.len(), 27, "{lg}");
        assert_eq!(w[0], (d(2026, 9, 29), d(2026, 10, 4)));
        assert_eq!(*w.last().unwrap(), (d(2027, 4, 5), d(2027, 4, 10)));
        let w20 = calendar_weeks(ctx, Some(20), None, None);
        assert_eq!(w20.len(), 20);
        assert_eq!(w20[..3], [(d(2026, 9, 29), d(2026, 11, 22)), (d(2026, 11, 23), d(2026, 11, 29)), (d(2026, 11, 30), d(2026, 12, 6))]);
        let from_today = calendar_weeks(ctx, None, Some(ctx.as_of), None);
        assert_eq!(from_today.len(), 22);
        assert_eq!(from_today[..2], [(d(2026, 11, 4), d(2026, 11, 8)), (d(2026, 11, 9), d(2026, 11, 15))]);
    }
}

#[test]
fn calendar_weeks_empty_cases() {
    let Some(snap) = snapshot("espn") else { return };
    let mut ctx = snap.ctx.clone();
    assert!(calendar_weeks(&ctx, None, Some(d(2027, 1, 2)), Some(d(2027, 1, 1))).is_empty());
    ctx.schedule.clear();
    assert!(calendar_weeks(&ctx, None, None, None).is_empty());
}

#[allow(dead_code)]
fn _unused(_: &Path, _: Value) {}
