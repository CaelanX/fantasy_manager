//! Ports of tests/test_harness_ledger.py (schema, rec_key, upsert) and tests/test_harness_match.py
//! (all matching rules + an end-to-end run), plus a parity run against a copy of the real
//! harness.db (Python `match_episodes` vs Rust on the same file).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

use chrono::NaiveDate;
use fm_harness::episode_match::{
    alert_category, first_lockable_day, match_episodes, match_rows, set_my_team_id, window_end, EpisodeRow,
    LineupRow, MatchResult, TxRow,
};
use fm_harness::ledger::{rec_key, Ledger, Row, ADDED_COLUMNS, DEPLOYMENT_TABLES, MATCHUP_TABLES, TABLES};
use serde_json::{json, Value};

const ME: &str = "1";

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn no_subj() -> Vec<String> {
    Vec::new()
}

// --------------------------------------------------------------------------- ledger

#[test]
fn schema_has_every_table() {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    let names: BTreeSet<String> = led
        .query("SELECT name FROM sqlite_master WHERE type='table'", &[])
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    for t in TABLES.iter().chain(DEPLOYMENT_TABLES).chain(MATCHUP_TABLES) {
        assert!(names.contains(*t), "missing table {t}");
    }
    assert_eq!(led.path, tmp.path().join("harness.db"));
    let v = led.query("SELECT value FROM meta WHERE key='schema_version'", &[]).unwrap();
    assert_eq!(v[0]["value"], json!("2"));
    drop(led);
    Ledger::open(tmp.path()).unwrap(); // re-opening an existing db is fine
}

#[test]
fn rec_key_is_order_insensitive_and_distinguishes_subjects() {
    let a = rec_key("espn", "waiver", ["x", "y"], ["d"], None, no_subj());
    assert_eq!(a, rec_key("espn", "waiver", ["y", "x"], ["d"], Some(""), no_subj()));
    assert_ne!(a, rec_key("fantrax", "waiver", ["x", "y"], ["d"], None, no_subj()));
    let e: [&str; 0] = [];
    assert_ne!(rec_key("espn", "injury", e, e, None, ["h1"]), rec_key("espn", "injury", e, e, None, ["h2"]));
    assert_ne!(
        rec_key("espn", "trade", ["x"], ["m"], Some("Opp A"), no_subj()),
        rec_key("espn", "trade", ["x"], ["m"], Some("Opp B"), no_subj())
    );
}

#[test]
fn rec_key_matches_python_golden() {
    // values printed by fantasy_manager.harness.ledger.rec_key
    let e: [&str; 0] = [];
    assert_eq!(
        rec_key("espn", "waiver", ["y", "x"], ["d"], None, no_subj()),
        "b1cf14b9d1b431932a1c208f31174acc709bc99a"
    );
    assert_eq!(
        rec_key("espn", "trade", ["x"], ["m"], Some("Opp A"), no_subj()),
        "daad784be7e5fb13eb4edbe2b6467f4fc6421189"
    );
    assert_eq!(rec_key("espn", "injury", e, e, None, ["h2", "h1"]), "ca31a149944efdec43ccdcf503b8860bad7a4f42");
    assert_eq!(
        rec_key("fantrax", "alert", ["é"], e, Some("FA"), ["ü"]),
        "8d7b8d9e3cd2536cac9ec2a23442e1eccbddfe92"
    );
}

#[test]
fn migrates_a_v1_database_and_upserts_idempotently() {
    let tmp = tempfile::tempdir().unwrap();
    {
        // a schema-v1 file: outcomes without the added columns
        let conn = rusqlite::Connection::open(tmp.path().join("harness.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE outcomes (outcome_id TEXT PRIMARY KEY, league TEXT, episode_id TEXT, decision_id TEXT, \
             window TEXT NOT NULL, basis TEXT, predicted_gain REAL, realized_gain REAL, gain_units TEXT, \
             hit INTEGER, partial INTEGER, graded_at TEXT, detail_json TEXT);
             INSERT INTO outcomes(outcome_id, window) VALUES ('o1', '7d');",
        )
        .unwrap();
    }
    let led = Ledger::open(tmp.path()).unwrap();
    let cols: Vec<String> = led
        .query("PRAGMA table_info(outcomes)", &[])
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    for (_, added) in ADDED_COLUMNS {
        for (name, _) in added.iter() {
            assert!(cols.iter().any(|c| c == name), "column {name} not added");
        }
    }
    let row = led.query("SELECT complete, kind FROM outcomes", &[]).unwrap();
    assert_eq!(row[0]["complete"], json!(0));
    assert_eq!(row[0]["kind"], Value::Null);

    let mk = |kind: &str| -> Row {
        match json!({"decision_id": "d1", "league": "espn", "day": "2026-10-01", "origin": "followed",
                     "kind": kind, "episode_id": null, "group_id": null, "adds_json": "[]",
                     "drops_json": "[]", "created_at": "t"}) {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    };
    assert_eq!(led.upsert("decisions", &[mk("waiver")], &["decision_id"]).unwrap(), 1);
    assert_eq!(led.upsert("decisions", &[mk("trade")], &["decision_id"]).unwrap(), 1);
    assert_eq!(led.count("decisions", "", &[]).unwrap(), 1);
    let r = led.query("SELECT kind FROM decisions", &[]).unwrap();
    assert_eq!(r[0]["kind"], json!("trade"));
    // key-only rows: ON CONFLICT DO NOTHING (the existing row keeps its value)
    let only_key: Row = match json!({"key": "k1"}) {
        Value::Object(m) => m,
        _ => unreachable!(),
    };
    led.execute("INSERT INTO meta(key, value) VALUES ('k1', 'kept')", &[]).unwrap();
    led.commit().unwrap();
    assert_eq!(led.upsert("meta", &[only_key], &["key"]).unwrap(), 1);
    assert_eq!(led.query("SELECT value FROM meta WHERE key='k1'", &[]).unwrap()[0]["value"], json!("kept"));
    assert_eq!(led.upsert("decisions", &[], &["decision_id"]).unwrap(), 0);
}

#[test]
fn runs_count_reset_and_dml_transaction_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    let id = led.start_run("daily", Some("espn"), Some("2026-10-01"), None, None).unwrap();
    assert_eq!(id, 1);
    led.finish_run(id, "ok", Some(&json!({"b": 1, "a": [1.5, "é"]}))).unwrap();
    let r = led.query("SELECT status, summary_json, finished_at FROM runs", &[]).unwrap();
    assert_eq!(r[0]["status"], json!("ok"));
    assert_eq!(r[0]["summary_json"], json!("{\"b\": 1, \"a\": [1.5, \"\\u00e9\"]}"));
    assert_eq!(led.count("runs", "WHERE league=?", &[&"espn"]).unwrap(), 1);
    // uncommitted DML is invisible to a second connection until commit
    led.execute("INSERT INTO meta(key, value) VALUES ('k', 'v')", &[]).unwrap();
    let other = rusqlite::Connection::open(tmp.path().join("harness.db")).unwrap();
    let seen = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT COUNT(*) FROM meta WHERE key='k'", [], |r| r.get(0)).unwrap()
    };
    assert_eq!(seen(&other), 0);
    led.commit().unwrap();
    assert_eq!(seen(&other), 1);
    led.reset().unwrap();
    assert_eq!(led.count("runs", "", &[]).unwrap(), 0);
    // meta is not a TABLES member: it survives a reset
    assert_eq!(led.count("meta", "", &[]).unwrap(), 2);
}

// --------------------------------------------------------------------------- match rules

fn ep(kind: &str, first: NaiveDate, last: Option<NaiveDate>, adds: &[&str], drops: &[&str], subjects: &[&str], title: &str, eid: &str) -> EpisodeRow {
    let mut e = EpisodeRow::new(eid, kind, first, last.unwrap_or(first));
    e.title = title.to_string();
    e.adds = adds.iter().map(|s| s.to_string()).collect();
    e.drops = drops.iter().map(|s| s.to_string()).collect();
    e.subjects = subjects.iter().map(|s| s.to_string()).collect();
    e
}

fn epk(kind: &str, first: NaiveDate, adds: &[&str], drops: &[&str]) -> EpisodeRow {
    ep(kind, first, None, adds, drops, &[], "", "e1")
}

fn tx(action: &str, cid: &str, day: NaiveDate, group: &str, team: &str, other: Option<&str>) -> TxRow {
    let mut t = TxRow::new(group, action, cid, team, day);
    t.group_id = Some(group.to_string());
    t.counterparty_id = other.map(|s| s.to_string());
    t
}

fn tm(action: &str, cid: &str, day: NaiveDate) -> TxRow {
    tx(action, cid, day, "g1", ME, None)
}

fn lr(team: &str, day: NaiveDate, cid: &str, slot: &str, starting: bool) -> LineupRow {
    LineupRow { team_id: team.into(), day, cid: cid.into(), slot: slot.into(), starting }
}

struct Run {
    res: MatchResult,
}
impl Run {
    fn status(&self, id: &str) -> &str {
        &self.res.episodes.iter().find(|e| e.episode_id == id).unwrap().status
    }
    fn acted(&self, id: &str) -> Option<&str> {
        self.res.episodes.iter().find(|e| e.episode_id == id).unwrap().acted_on.as_deref()
    }
    fn detail(&self, id: &str) -> Value {
        let m = self.res.episodes.iter().find(|e| e.episode_id == id).unwrap().match_json.as_deref().unwrap();
        serde_json::from_str(m).unwrap()
    }
    fn origins(&self) -> Vec<String> {
        self.res.decisions.iter().map(|r| r["origin"].as_str().unwrap().to_string()).collect()
    }
}

fn run_with(eps: Vec<EpisodeRow>, txs: Vec<TxRow>, lineups: Vec<LineupRow>, today: NaiveDate, provider: &str) -> Run {
    Run { res: match_rows(&eps, &txs, &lineups, Some(ME), provider, today, "espn") }
}

fn run(eps: Vec<EpisodeRow>, txs: Vec<TxRow>, lineups: Vec<LineupRow>) -> Run {
    run_with(eps, txs, lineups, d(2026, 10, 20), "espn")
}

#[test]
fn exact_waiver_is_followed() {
    let r = run(
        vec![epk("waiver", d(2026, 10, 5), &["a"], &["d"])],
        vec![tm("ADD", "a", d(2026, 10, 6)), tm("DROP", "d", d(2026, 10, 6))],
        vec![],
    );
    assert_eq!(r.status("e1"), "followed");
    assert_eq!(r.acted("e1"), Some("2026-10-06"));
    assert_eq!(r.origins(), ["followed"]); // the group is consumed: no user_only
}

#[test]
fn waiver_with_a_different_drop_is_partial() {
    let r = run(
        vec![epk("waiver", d(2026, 10, 5), &["a"], &["d"])],
        vec![tm("ADD", "a", d(2026, 10, 5)), tm("DROP", "x", d(2026, 10, 5))],
        vec![],
    );
    assert_eq!(r.status("e1"), "partial");
    assert_eq!(r.detail("e1")["dropped"], json!(["x"]));
    assert_eq!(r.origins(), ["partial"]);
}

#[test]
fn add_outside_window_is_user_only_and_episode_expires() {
    let e = epk("waiver", d(2026, 10, 5), &["a"], &["d"]);
    assert_eq!(window_end(&e, "espn"), d(2026, 10, 8));
    let r = run(
        vec![e.clone()],
        vec![tm("ADD", "a", d(2026, 10, 10)), tm("DROP", "d", d(2026, 10, 10))],
        vec![],
    );
    assert_eq!(r.status("e1"), "expired");
    let dec = &r.res.decisions;
    assert_eq!(dec.len(), 1);
    assert_eq!(dec[0]["origin"], json!("user_only"));
    assert_eq!(dec[0]["kind"], json!("waiver"));
    assert_eq!(dec[0]["adds_json"], json!("[\"a\"]"));
    // before the window closes the unmatched episode stays open
    let r = run_with(vec![e], vec![], vec![], d(2026, 10, 8), "espn");
    assert_eq!(r.status("e1"), "open");
}

#[test]
fn other_teams_moves_are_not_mine() {
    let r = run(
        vec![epk("waiver", d(2026, 10, 5), &["a"], &[])],
        vec![tx("ADD", "a", d(2026, 10, 6), "g1", "2", None)],
        vec![],
    );
    assert_eq!(r.status("e1"), "expired");
    assert!(r.res.decisions.is_empty());
}

#[test]
fn two_for_one_trade_subset_is_partial_and_exact_is_followed() {
    let e = epk("trade", d(2026, 10, 5), &["x", "y"], &["m"]);
    assert_eq!(window_end(&e, "espn"), d(2026, 10, 12));
    let part = vec![
        tx("TRADE_IN", "x", d(2026, 10, 9), "T", ME, None),
        tx("TRADE_OUT", "m", d(2026, 10, 9), "T", ME, None),
    ];
    let r = run(vec![e.clone()], part.clone(), vec![]);
    assert_eq!(r.status("e1"), "partial");
    assert_eq!(r.origins(), ["partial"]);
    let mut full = part;
    full.push(tx("TRADE_IN", "y", d(2026, 10, 9), "T", ME, None));
    assert_eq!(run(vec![e.clone()], full, vec![]).status("e1"), "followed");
    // an unrelated trade sharing only a get is neither
    let other = vec![
        tx("TRADE_IN", "x", d(2026, 10, 9), "U", ME, None),
        tx("TRADE_OUT", "z", d(2026, 10, 9), "U", ME, None),
    ];
    let r = run(vec![e], other, vec![]);
    assert_eq!(r.status("e1"), "expired");
    assert_eq!(r.res.decisions[0]["origin"], json!("user_only"));
    assert_eq!(r.res.decisions[0]["kind"], json!("trade"));
}

#[test]
fn trade_proposal_only_is_proposed() {
    let e = epk("trade", d(2026, 10, 5), &["x"], &["m"]);
    let props = vec![
        tx("PROPOSED", "x", d(2026, 10, 6), "P", ME, Some("2")), // I receive x from team 2
        tx("PROPOSED", "m", d(2026, 10, 6), "P", "2", Some(ME)), // I give m to team 2
    ];
    let r = run_with(vec![e.clone()], props.clone(), vec![], d(2026, 10, 7), "espn");
    assert_eq!(r.status("e1"), "proposed");
    assert_eq!(r.origins(), ["proposed"]);
    // window passed: flagged in the detail
    let r = run_with(vec![e], props, vec![], d(2026, 10, 20), "espn");
    assert_eq!(r.detail("e1")["window_passed"], json!(true));
}

#[test]
fn fantrax_lineup_rec_issued_tuesday_is_graded_the_following_monday() {
    let (tue, mon) = (d(2026, 10, 6), d(2026, 10, 12));
    assert_eq!(first_lockable_day("fantrax", tue), mon);
    assert_eq!(first_lockable_day("fantrax", mon), mon);
    assert_eq!(first_lockable_day("espn", tue), tue);
    let e = ep("lineup", tue, None, &["in"], &["out"], &[], "Start in over out for this week's lineup (locks Monday)", "e1");
    let tue2 = d(2026, 10, 13);
    let before = vec![
        lr(ME, d(2026, 10, 7), "in", "BN", false),
        lr(ME, d(2026, 10, 7), "out", "C", true),
        lr(ME, mon, "in", "BN", false),
        lr(ME, mon, "out", "C", true),
    ]; // Monday 8am: pre-lock
    let after = vec![lr(ME, tue2, "in", "C", true), lr(ME, tue2, "out", "BN", false)];
    let both: Vec<LineupRow> = before.iter().chain(after.iter()).cloned().collect();
    let r = run_with(vec![e.clone()], vec![], both, d(2026, 10, 14), "fantrax");
    assert_eq!(r.status("e1"), "followed");
    assert_eq!(r.acted("e1"), Some("2026-10-12"));
    assert_eq!(r.detail("e1")["graded_day"], json!("2026-10-13"));
    // before a post-lock snapshot exists the episode stays open
    let r = run_with(vec![e.clone()], vec![], before.clone(), d(2026, 10, 12), "fantrax");
    assert_eq!(r.status("e1"), "open");
    // post-lock snapshot unchanged -> expired (graded as ignored)
    let same = vec![lr(ME, tue2, "in", "BN", false), lr(ME, tue2, "out", "C", true)];
    let r = run_with(vec![e], vec![], same, d(2026, 10, 14), "fantrax");
    assert_eq!(r.status("e1"), "expired");
    assert_eq!(r.detail("e1")["reason"], json!("lineup not changed"));
    // a rec issued on the Monday itself is graded against that Monday's lock
    let r = run_with(vec![epk("lineup", mon, &["in"], &["out"])], vec![], before, mon, "fantrax");
    assert_eq!(r.status("e1"), "open");
}

#[test]
fn espn_lineup_partial_when_only_one_side_changes() {
    let e = epk("lineup", d(2026, 10, 10), &["in"], &["out"]);
    let rows = vec![lr(ME, d(2026, 10, 10), "in", "C", true), lr(ME, d(2026, 10, 10), "out", "UTIL", true)];
    assert_eq!(run(vec![e], vec![], rows).status("e1"), "partial");
}

#[test]
fn injury_ir_within_two_days() {
    let e = ep("injury", d(2026, 10, 6), None, &["fa"], &[], &["hurt"], "Move hurt to IR and add fa", "e1");
    let ok = vec![lr(ME, d(2026, 10, 8), "hurt", "IR", false)];
    let r = run(vec![e.clone()], vec![tm("ADD", "fa", d(2026, 10, 7))], ok);
    assert_eq!(r.status("e1"), "followed");
    assert_eq!(r.origins(), ["followed"]);
    let late = vec![lr(ME, d(2026, 10, 9), "hurt", "IR", false)]; // 3 days later: outside the window
    assert_eq!(run(vec![e.clone()], vec![], late).status("e1"), "expired");
    // IR move only, no add
    assert_eq!(run(vec![e], vec![tm("IR", "hurt", d(2026, 10, 6))], vec![]).status("e1"), "partial");
}

#[test]
fn activation_rec_followed_when_player_leaves_ir() {
    let e = ep("injury", d(2026, 10, 6), None, &["back"], &[], &[], "Activate back from IR", "e1");
    let rows = vec![lr(ME, d(2026, 10, 6), "back", "IR", false), lr(ME, d(2026, 10, 7), "back", "BN", false)];
    assert_eq!(run(vec![e], vec![], rows).status("e1"), "followed");
}

#[test]
fn flags_trade_within_14_days() {
    let sell = ep("sell_high", d(2026, 10, 1), None, &[], &["hot"], &[], "", "s");
    let buy = ep("buy_low", d(2026, 10, 1), None, &["cold"], &[], &[], "", "b");
    let r = run_with(
        vec![sell, buy],
        vec![tx("TRADE_OUT", "hot", d(2026, 10, 14), "T", ME, None), tx("TRADE_IN", "cold", d(2026, 10, 20), "U", ME, None)],
        vec![],
        d(2026, 10, 30),
        "espn",
    );
    assert_eq!(r.status("s"), "followed");
    assert_eq!(r.status("b"), "expired");
}

// --------------------------------------------------------------------------- alerts

fn alert(first: NaiveDate, subjects: &[&str], title: &str, counterparty: Option<&str>, negative: bool) -> EpisodeRow {
    let mut e = ep("alert", first, None, &[], &[], subjects, title, "a1");
    e.counterparty = counterparty.map(|s| s.to_string());
    e.negative = negative;
    e
}

#[test]
fn free_agent_alert_followed_by_my_add_is_followed_like_a_waiver() {
    let e = alert(
        d(2026, 10, 5),
        &["misa"],
        "Preseason standout: Michael Misa (4 GP, 1.25 PTS/GP; free agent)",
        Some("FA"),
        false,
    );
    assert_eq!(window_end(&e, "fantrax"), d(2026, 10, 8));
    let r = run_with(
        vec![e],
        vec![tm("ADD", "misa", d(2026, 10, 7)), tm("DROP", "old", d(2026, 10, 7))],
        vec![],
        d(2026, 10, 20),
        "fantrax",
    );
    assert_eq!(r.status("a1"), "followed");
    assert_eq!(r.acted("a1"), Some("2026-10-07"));
    let m = r.detail("a1");
    assert_eq!((&m["added"], &m["dropped"], &m["alert"]), (&json!(["misa"]), &json!(["old"]), &json!("fa")));
    let dec = &r.res.decisions;
    assert_eq!(dec.len(), 1); // the group is consumed: no user_only
    assert_eq!(dec[0]["kind"], json!("alert"));
    assert_eq!((&dec[0]["adds_json"], &dec[0]["drops_json"]), (&json!("[\"misa\"]"), &json!("[\"old\"]")));
}

#[test]
fn free_agent_alert_add_after_the_window_expires() {
    let e = alert(d(2026, 10, 5), &["x"], "X (free agent) promoted to PP1 (PP2 -> PP1): waiver watch", None, false);
    let r = run(vec![e], vec![tm("ADD", "x", d(2026, 10, 9))], vec![]);
    assert_eq!(r.status("a1"), "expired");
    assert_eq!(r.res.decisions.len(), 1);
    assert_eq!((&r.res.decisions[0]["origin"], &r.res.decisions[0]["kind"]), (&json!("user_only"), &json!("waiver")));
}

#[test]
fn alert_on_a_player_on_no_roster_counts_as_free_agent() {
    let e = alert(d(2026, 10, 5), &["x"], "Rookie role signal: X - skating on the top line (RotoWire, 2026-10-05)", None, false);
    assert_eq!(alert_category(&e), Some("info"));
    let rostered = vec![lr("2", d(2026, 10, 5), "y", "C", true)];
    let r = run(vec![e.clone()], vec![tm("ADD", "x", d(2026, 10, 6))], rostered.clone());
    assert_eq!(r.status("a1"), "followed");
    // on another team's roster that day: informational, my later add does not follow it
    let mut on_other = rostered;
    on_other.push(lr("2", d(2026, 10, 5), "x", "C", true));
    let r = run(vec![e], vec![tm("ADD", "x", d(2026, 10, 6))], on_other);
    assert_eq!(r.status("a1"), "expired");
}

#[test]
fn negative_alert_on_my_player_followed_by_drop_or_trade() {
    let e = alert(d(2026, 10, 5), &["m"], "Role loss: M (TOI -3.1, off PP1; my team)", Some("Me"), false);
    assert_eq!(window_end(&e, "espn"), d(2026, 10, 12));
    let r = run(vec![e.clone()], vec![tm("DROP", "m", d(2026, 10, 11)), tm("ADD", "n", d(2026, 10, 11))], vec![]);
    assert_eq!(r.status("a1"), "followed");
    let m = r.detail("a1");
    assert_eq!((&m["via"], &m["dropped"], &m["added"]), (&json!("drop"), &json!(["m"]), &json!(["n"])));
    assert_eq!(r.origins(), ["followed"]);
    let r = run(
        vec![e],
        vec![tx("TRADE_OUT", "m", d(2026, 10, 9), "T", ME, None), tx("TRADE_IN", "t", d(2026, 10, 9), "T", ME, None)],
        vec![],
    );
    assert_eq!(r.status("a1"), "followed");
    assert_eq!(r.detail("a1")["via"], json!("trade"));
    // scratched / off PP1 titles and negative rookie news count as negative too
    let s = alert(d(2026, 10, 5), &["m"], "M out of the lineup (scratched): bench caution", None, false);
    assert_eq!(run(vec![s], vec![tm("DROP", "m", d(2026, 10, 6))], vec![]).status("a1"), "followed");
    let n = alert(d(2026, 10, 5), &["m"], "Rookie role signal: M - sent down", None, true);
    assert_eq!(run(vec![n], vec![tm("DROP", "m", d(2026, 10, 6))], vec![]).status("a1"), "followed");
}

#[test]
fn positive_alert_on_my_player_then_drop_is_not_followed() {
    let e = alert(d(2026, 10, 5), &["m"], "M promoted to PP1 (PP2 -> PP1): start him / hold", None, false);
    let r = run(vec![e], vec![tm("DROP", "m", d(2026, 10, 6))], vec![]);
    assert_eq!(r.status("a1"), "expired");
    assert_eq!(r.origins(), ["user_only"]);
}

// --------------------------------------------------------------------------- end to end

fn exec(led: &Ledger, sql: &str, params: &[&dyn rusqlite::ToSql]) {
    led.execute(sql, params).unwrap();
}

fn seed_episode(led: &Ledger, league: &str, eid: &str, kind: &str, title: &str, first: &str, last: &str, adds: &[&str], drops: &[&str]) {
    let key = format!("k-{eid}");
    exec(
        led,
        "INSERT INTO rec_episodes(episode_id, league, rec_key, kind, title, first_seen, last_seen, n_days) \
         VALUES (?,?,?,?,?,?,?,3)",
        &[&eid, &league, &key, &kind, &title, &first, &last],
    );
    for (side, cids) in [("add", adds), ("drop", drops)] {
        for c in cids {
            exec(
                led,
                "INSERT INTO rec_players(league, as_of, rec_key, side, cid) VALUES (?,?,?,?,?)",
                &[&league, &first, &key, &side, c],
            );
        }
    }
    led.commit().unwrap();
}

fn seed_tx(led: &Ledger, tx_id: &str, action: &str, cid: &str, team: &str, day: &str, group: &str) {
    let ts = format!("{day}T09:00:00");
    let is_me = (team == ME) as i64;
    exec(
        led,
        "INSERT INTO transactions(league, source, tx_id, action, cid, team_id, ts, day, group_id, is_me) \
         VALUES ('espn','espn',?,?,?,?,?,?,?,?)",
        &[&tx_id, &action, &cid, &team, &ts, &day, &group, &is_me],
    );
    led.commit().unwrap();
}

#[test]
fn end_to_end_match_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    set_my_team_id(&led, "espn", ME).unwrap();
    seed_episode(&led, "espn", "e-a", "waiver", "Add a, drop d", "2026-10-05", "2026-10-07", &["espn:a"], &["espn:d"]);
    seed_tx(&led, "t1", "ADD", "espn:a", ME, "2026-10-08", "t1");
    seed_tx(&led, "t1", "DROP", "espn:d", ME, "2026-10-08", "t1");
    seed_tx(&led, "t2", "ADD", "espn:z", ME, "2026-10-08", "t2");
    seed_tx(&led, "t3", "ADD", "espn:q", "4", "2026-10-08", "t3");
    let s1 = match_episodes(&led, "espn", Some(d(2026, 10, 20)), None).unwrap();
    assert_eq!(s1.by_status.get("followed"), Some(&1));
    assert_eq!(s1.decisions.get("followed"), Some(&1));
    assert_eq!(s1.decisions.get("user_only"), Some(&1));
    assert_eq!(s1.by_kind_status["waiver"]["followed"], 1);
    assert_eq!(s1.line(), "espn: 1 episodes (followed 1); decisions: followed 1, user_only 1");
    let row = &led.query("SELECT * FROM rec_episodes", &[]).unwrap()[0];
    assert_eq!(row["acted_on"], json!("2026-10-08"));
    assert_eq!(row["window_end"], json!("2026-10-10")); // last_seen + 3
    let strip = |rows: Vec<Row>| -> Vec<Row> {
        rows.into_iter()
            .map(|mut r| {
                for k in ["updated_at", "created_at"] {
                    r.remove(k);
                }
                r
            })
            .collect()
    };
    let snap = |l: &Ledger| -> Vec<Vec<Row>> {
        ["rec_episodes", "decisions"]
            .iter()
            .map(|t| strip(l.query(&format!("SELECT * FROM {t} ORDER BY 1, 2"), &[]).unwrap()))
            .collect()
    };
    let before = snap(&led);
    let s2 = match_episodes(&led, "espn", Some(d(2026, 10, 20)), None).unwrap();
    assert_eq!(snap(&led), before);
    assert_eq!(s2.by_status, s1.by_status);
}

#[test]
fn followed_alert_is_loaded_with_its_counterparty_and_reasons() {
    let tmp = tempfile::tempdir().unwrap();
    let led = Ledger::open(tmp.path()).unwrap();
    set_my_team_id(&led, "espn", ME).unwrap();
    // an alert episode whose rec row carries a negative ROLE_NEWS reason and an FA-less counterparty
    seed_episode(&led, "espn", "e-n", "alert", "Rookie role signal: M - sent down", "2026-10-05", "2026-10-05", &[], &[]);
    exec(
        &led,
        "INSERT INTO rec_players(league, as_of, rec_key, side, cid) VALUES ('espn','2026-10-05','k-e-n','subject','espn:m')",
        &[],
    );
    exec(
        &led,
        "INSERT INTO recs(league, as_of, rec_key, kind, counterparty, reasons_json) VALUES \
         ('espn','2026-10-05','k-e-n','alert','Me','[{\"code\": \"ROLE_NEWS\", \"value\": -1.0}]')",
        &[],
    );
    led.commit().unwrap();
    seed_tx(&led, "t1", "DROP", "espn:m", ME, "2026-10-06", "t1");
    let s = match_episodes(&led, "espn", Some(d(2026, 10, 20)), None).unwrap();
    assert_eq!(s.by_kind_status["alert"]["followed"], 1);
    let m: Value = serde_json::from_str(
        led.query("SELECT match_json FROM rec_episodes", &[]).unwrap()[0]["match_json"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(m["alert"], json!("mine"));
}

// --------------------------------------------------------------------------- parity with Python

const PY_SCRIPT: &str = r#"
import sys, json, datetime
sys.path.insert(0, sys.argv[1])
from pathlib import Path
from fantasy_manager.harness.ledger import Ledger
from fantasy_manager.harness.match import match_episodes
led = Ledger(Path(sys.argv[2]))
s = match_episodes(led, sys.argv[3], today=datetime.date.fromisoformat(sys.argv[4]))
out = {"summary": {"episodes": s.episodes, "by_status": s.by_status, "by_kind_status": s.by_kind_status,
                   "decisions": s.decisions, "line": s.line()},
       "episodes": led.query("SELECT * FROM rec_episodes ORDER BY episode_id"),
       "decisions": led.query("SELECT * FROM decisions ORDER BY decision_id"),
       "counts": {t: led.count(t) for t in ("recs", "rec_players", "transactions", "lineup_days", "outcomes", "runs")}}
print(json.dumps(out))
"#;

fn real_harness_db() -> Option<PathBuf> {
    let root = std::env::var("FM_PARITY_FIXTURES").ok()?;
    let p = PathBuf::from(root).join("_datadir").join("harness.db");
    p.exists().then_some(p)
}

fn python() -> Option<PathBuf> {
    let p = PathBuf::from("C:/code/fantasy_manager/.venv/Scripts/python.exe");
    p.exists().then_some(p)
}

fn strip_volatile(rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            let mut m = r.as_object().unwrap().clone();
            for k in ["updated_at", "created_at"] {
                m.remove(k);
            }
            Value::Object(m)
        })
        .collect()
}

#[test]
fn match_episodes_on_a_copy_of_the_real_harness_db_equals_python() {
    let (Some(db), Some(py)) = (real_harness_db(), python()) else {
        eprintln!("skipped: needs FM_PARITY_FIXTURES/_datadir/harness.db and the repo venv python");
        return;
    };
    let worktree = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let worktree = worktree
        .ancestors()
        .find(|p| p.join("fantasy_manager").is_dir())
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::var("FM_WORKTREE").ok().map(PathBuf::from))
        .expect("worktree root (set FM_WORKTREE when building outside the repo)");
    let today = "2026-10-27";
    for league in ["espn", "fantrax"] {
        let (rust_dir, py_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::copy(&db, rust_dir.path().join("harness.db")).unwrap();
        std::fs::copy(&db, py_dir.path().join("harness.db")).unwrap();
        let out = Command::new(&py)
            .args(["-I", "-c", PY_SCRIPT])
            .arg(&worktree)
            .arg(py_dir.path())
            .arg(league)
            .arg(today)
            .output()
            .unwrap();
        assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
        let golden: Value = serde_json::from_slice(&out.stdout).unwrap();

        let led = Ledger::open(rust_dir.path()).unwrap();
        let summary = match_episodes(&led, league, Some(NaiveDate::parse_from_str(today, "%Y-%m-%d").unwrap()), None).unwrap();
        assert_eq!(summary.line(), golden["summary"]["line"].as_str().unwrap(), "{league} summary line");
        assert_eq!(summary.episodes as u64, golden["summary"]["episodes"].as_u64().unwrap());
        assert_eq!(
            serde_json::to_value(&summary.by_kind_status).unwrap(),
            golden["summary"]["by_kind_status"],
            "{league} by_kind_status"
        );
        for t in ["recs", "rec_players", "transactions", "lineup_days", "outcomes", "runs"] {
            assert_eq!(led.count(t, "", &[]).unwrap(), golden["counts"][t].as_i64().unwrap(), "{t} count");
        }
        for (table, key) in [("rec_episodes", "episodes"), ("decisions", "decisions")] {
            let order = if table == "rec_episodes" { "episode_id" } else { "decision_id" };
            let rows: Vec<Value> = led
                .query(&format!("SELECT * FROM {table} ORDER BY {order}"), &[])
                .unwrap()
                .into_iter()
                .map(Value::Object)
                .collect();
            let want: Vec<Value> = golden[key].as_array().unwrap().clone();
            assert_eq!(rows.len(), want.len(), "{table} rows");
            let (rows, want) = (strip_volatile(&rows), strip_volatile(&want));
            for (a, b) in rows.iter().zip(&want) {
                assert_eq!(a, b, "{table} row differs");
            }
        }
    }
}
