//! Ports of tests/test_notify.py (transport faked), test_digest.py / test_pregame.py (intent) and
//! the byte-for-byte parity check against `parity/dump_report.py` goldens.
//!
//! Parity needs `$FM_PARITY_FIXTURES/<league>/snapshot.json` and
//! `$FM_PARITY_FIXTURES/_w9_golden/report.json`
//! (`python parity/dump_report.py <worktree> <fixtures> <fixtures>/_w9_golden`); it skips with
//! a message when they are absent. Nothing here posts to a real webhook.

use std::cell::RefCell;
use std::path::PathBuf;

use fm_core::models::{LeagueContext, Recommendation, Status};
use fm_core::snapshot::Snapshot;
use fm_core::values::PlayerValue;
use fm_core::{Cid, Map, PyDateTime};
use fm_report::digest::{build_digest, write_digest};
use fm_report::notify::*;
use fm_report::pregame::*;
use serde_json::{json, Value};

// --------------------------------------------------------------------------- fake transport

enum Resp {
    Ok(u16, &'static str, Option<&'static str>),
    Timeout,
    Connect,
}

struct Fake {
    script: RefCell<Vec<Resp>>,
    calls: RefCell<Vec<(String, Value)>>,
}

impl Fake {
    fn new(script: Vec<Resp>) -> Fake {
        Fake { script: RefCell::new(script.into_iter().rev().collect()), calls: RefCell::new(vec![]) }
    }
}

impl Sender for Fake {
    fn post(&self, url: &str, payload: &Value) -> Result<HttpResponse, SendError> {
        self.calls.borrow_mut().push((url.to_string(), payload.clone()));
        match self.script.borrow_mut().pop().unwrap_or(Resp::Ok(204, "", None)) {
            Resp::Ok(status, text, ra) => Ok(HttpResponse { status, text: text.to_string(), retry_after: ra.map(str::to_string) }),
            Resp::Timeout => Err(SendError::Timeout),
            Resp::Connect => Err(SendError::Http("ConnectError".into())),
        }
    }
    fn sleep(&self, _s: f64) {}
}

const DISCORD: &str = "https://discord.com/api/webhooks/123/SECRET-TOKEN";
const SLACK: &str = "https://hooks.slack.com/services/T0/B0/SECRET";

#[test]
fn chunk_text_respects_limit_and_lines() {
    let text = (0..60).map(|i| format!("line {i} {}", "x".repeat(90))).collect::<Vec<_>>().join("\n");
    let chunks = chunk_text(&text, 1900);
    assert!(chunks.len() > 1 && chunks.iter().all(|c| c.chars().count() <= 1900));
    assert_eq!(chunks.join("\n"), text);
    let parts = chunk_text(&"word ".repeat(1000), 1900);
    assert!(parts.iter().all(|c| c.chars().count() <= 1900) && parts.len() == 3);
    assert!(chunk_text("   ", 100).is_empty());
}

#[test]
fn send_discord_chunks_and_payload() {
    let f = Fake::new(vec![]);
    let text = (0..50).map(|_| "y".repeat(100)).collect::<Vec<_>>().join("\n");
    assert_eq!(send_discord(Some(DISCORD), &text, Some(&f)), "discord: sent 3 message(s)");
    let calls = f.calls.borrow();
    assert!(calls.iter().all(|(_, p)| p["content"].as_str().unwrap().chars().count() <= 1900));
    assert_eq!(calls[0].1["allowed_mentions"], json!({"parse": []}));
    assert_eq!(calls[0].0, DISCORD);
}

#[test]
fn send_slack_payload() {
    let f = Fake::new(vec![Resp::Ok(200, "ok", None)]);
    assert_eq!(send_slack(Some(SLACK), "hello", Some(&f)), "slack: sent 1 message(s)");
    assert_eq!(*f.calls.borrow(), vec![(SLACK.to_string(), json!({"text": "hello"}))]);
}

#[test]
fn http_error_and_transport_errors_never_leak_secret() {
    let f = Fake::new(vec![Resp::Ok(404, "{\"message\": \"Unknown Webhook\"}", None)]);
    let res = send_discord(Some(DISCORD), "hi", Some(&f));
    assert!(res.starts_with("discord: error HTTP 404") && res.contains("Unknown Webhook") && !res.contains("SECRET"));
    let res = send_slack(Some(SLACK), "hi", Some(&Fake::new(vec![Resp::Connect])));
    assert!(res.starts_with("slack: error ConnectError") && !res.contains("SECRET"));
    let res = send_discord(Some(DISCORD), "hi", Some(&Fake::new(vec![Resp::Timeout])));
    assert_eq!(res, "discord: error timed out after 0/1 message(s)");
}

#[test]
fn rate_limit_retried_once_and_notify_all() {
    let f = Fake::new(vec![Resp::Ok(429, "", Some("0")), Resp::Ok(204, "", None)]);
    assert_eq!(send_discord(Some(DISCORD), "hi", Some(&f)), "discord: sent 1 message(s)");
    assert_eq!(f.calls.borrow().len(), 2);
    let g = Fake::new(vec![Resp::Ok(200, "", None), Resp::Ok(200, "", None)]);
    let res = notify_all_urls(Some(DISCORD), Some(SLACK), "digest", Some(&g));
    assert_eq!(res, vec!["discord: sent 1 message(s)", "slack: sent 1 message(s)"]);
    assert!(!any_failed(&res));
    assert_eq!(notify_all_urls(None, None, "x", None), vec![NO_WEBHOOKS.to_string()]);
    assert_eq!(send_discord(None, "x", None), "discord: not configured");
}

#[test]
fn notify_changes_formats_bullets_and_chunks() {
    let lines: Vec<String> = (0..30).map(|i| format!("line {i} {}", "x".repeat(90))).collect();
    let f = Fake::new(vec![]);
    let res = notify_changes_urls(Some("https://discord.invalid/hook"), Some("https://slack.invalid/x"), "Pre-game 5:02 PM - ESPN", &lines, Some(&f));
    assert!(res[0].starts_with("discord: sent") && res[1].starts_with("slack: sent"));
    let calls = f.calls.borrow();
    let discord: Vec<&str> = calls.iter().filter(|(u, _)| u.contains("discord")).map(|(_, p)| p["content"].as_str().unwrap()).collect();
    let slack: Vec<&str> = calls.iter().filter(|(u, _)| u.contains("slack")).map(|(_, p)| p["text"].as_str().unwrap()).collect();
    assert!(discord.len() >= 2 && discord.iter().chain(&slack).all(|c| c.chars().count() <= 1500));
    assert!(discord[0].starts_with("**Pre-game 5:02 PM - ESPN**\n\u{2022} line 0") && slack[0].starts_with("*Pre-game"));
}

// --------------------------------------------------------------------------- pregame (synthetic)

fn dt(s: &str) -> PyDateTime {
    PyDateTime::parse(s).unwrap()
}

#[test]
fn summarize_caps_length_and_no_change_lines() {
    let changes: Vec<Change> = (0..80)
        .map(|i| {
            let mut c = Change::new(ChangeKind::NewAlert, format!("Player number {i} moved to the first line tonight after a long practice report"));
            c.priority = 3;
            c
        })
        .collect();
    let ran = dt("2026-09-29T17:02:00");
    let lineup = json!({"text": "Lineup: start A; bench B", "set": false});
    let text = summarize(&changes, Some(&lineup), &ran, SUMMARY_LIMIT);
    assert!(text.chars().count() <= SUMMARY_LIMIT);
    assert!(text.starts_with("Pre-game 5:02 PM:") && text.contains("more)") && text.ends_with("Lineup: start A; bench B."));
    assert_eq!(summarize(&[], None, &ran, SUMMARY_LIMIT), NO_CHANGES);
    assert_eq!(summarize(&[], Some(&json!({"text": "Lineup: start A", "set": false})), &ran, SUMMARY_LIMIT), "Pre-game: no changes. Lineup: start A.");
}

#[test]
fn last_names_and_clock() {
    assert_eq!(last(Some("Pyotr Kochetkov")), "Kochetkov");
    assert_eq!(last(Some("James van Riemsdyk")), "van Riemsdyk");
    assert_eq!(last(None), "?");
    assert_eq!(clock(&dt("2026-09-29T00:05:00")), "12:05 AM");
    assert_eq!(clock(&dt("2026-09-29T17:02:00")), "5:02 PM");
}

#[test]
fn failure_messages() {
    assert_eq!(failure_message("fantrax", "Fantrax says you are not logged in: ..."), "Fantrax login expired: refresh FANTRAX_COOKIE (or run `fm auth fantrax --login`)");
    assert_eq!(failure_message("espn", "ESPN denied access: 401"), "ESPN login expired: refresh ESPN_S2 / ESPN_SWID in .env");
    assert_eq!(failure_message("espn", "ESPN request failed: boom"), "ESPN not loaded: ESPN request failed: boom");
}

fn state(goalies: Value, status: &str, line: &str) -> Value {
    json!({"version": 1, "source": "daily", "taken_at": "2026-09-29T08:05:00", "goalies": goalies,
           "my_players": {"espn:1": {"name": "Seth Jarvis", "team": "CAR", "goalie": false, "status": status, "note": null,
                                     "line": line, "pp_unit": "pp1", "slot": "C", "plays_today": true, "start": null},
                          "espn:3": {"name": "Brandon Bussi", "team": "CAR", "goalie": true, "status": "healthy", "note": null,
                                     "line": "g", "pp_unit": null, "slot": "G", "plays_today": true, "start": null}},
           "lineup_today": null, "alerts": [], "fa_goalies": []})
}

#[test]
fn diffs_cover_goalie_status_and_line_moves() {
    let g = |s: &str| json!({"CAR": {"goalie": "Brandon Bussi", "strength": s, "game": "FLA@CAR", "opponent": "FLA", "home": true, "depth": 1}});
    let prev = state(g("Likely"), "healthy", "f1");
    let cur = state(g("Confirmed"), "out", "f2");
    let ch = diff_states(Some(&prev), &cur, None);
    let texts: Vec<&str> = ch.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, vec!["Bussi CONFIRMED (CAR vs FLA)", "Jarvis now OUT: bench him", "Jarvis moved to F2 (was F1)"]);
    assert_eq!(ch[0].scope.as_deref(), Some("mine"));
    // nothing changed -> nothing reported; baseline statuses stand in for a missing snapshot
    assert!(diff_states(Some(&cur), &cur, None).is_empty());
    let base: Map<String, String> = [("espn:1".to_string(), "healthy".to_string())].into_iter().collect();
    let ch = diff_states(None, &cur, Some(&base));
    assert_eq!(ch.iter().filter(|c| c.kind == ChangeKind::InjuryNew).count(), 1);
    // a goalie who sits
    let mut cur2 = cur.clone();
    cur2["goalies"] = json!({"CAR": {"goalie": "Frederik Andersen", "strength": "Confirmed", "game": "FLA@CAR", "opponent": "FLA", "home": true, "depth": 1}});
    let ch = diff_goalies(Some(&prev), &cur2);
    assert_eq!(ch[0].text, "Bussi sits: Andersen CONFIRMED for CAR vs FLA; was Bussi".replace("; was Bussi", ""));
}

#[test]
fn opponent_goalies_merge_into_one_line() {
    let mk = |short: &str| {
        let mut c = Change::new(ChangeKind::GoalieConfirmed, format!("Your x skaters face {short}"));
        c.scope = Some("opponent".into());
        c.priority = 4;
        c.short = Some(short.into());
        c
    };
    let out = display_changes(&[mk("Swayman (BOS)"), mk("Bobrovsky (FLA)")], None);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, "Confirmed vs your skaters: Swayman (BOS), Bobrovsky (FLA)");
}

#[test]
fn snapshot_roundtrip_and_old_version_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
    let st = state(json!({}), "healthy", "f1");
    let p = save_snapshot(dir.path(), "espn", day, &st).unwrap();
    assert_eq!(p, dir.path().join("pregame").join("espn-2026-09-29.json"));
    assert_eq!(load_snapshot(dir.path(), "espn", day).unwrap()["source"], "daily");
    std::fs::write(&p, "{\"version\": 0}").unwrap();
    assert!(load_snapshot(dir.path(), "espn", day).is_none());
}

#[test]
fn notify_lines_lead_with_plan_actions() {
    let mut r = PregameResult::new("espn", chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap(), dt("2026-09-29T17:02:00"));
    r.summary_text = NO_CHANGES.into();
    assert_eq!(r.notify_lines(), vec![NO_CHANGES.to_string()]);
    r.today_actions = vec!["Add X".into()];
    assert_eq!(r.notify_lines()[0], CHECKLIST_HEAD);
    assert_eq!(r.title(), "Pre-game 5:02 PM - ESPN");
}

// --------------------------------------------------------------------------- parity vs Python

fn fixtures() -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var("FM_PARITY_FIXTURES").ok()?);
    d.join("_w9_golden/report.json").exists().then_some(d)
}

fn read(p: PathBuf) -> Value {
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}

fn approx(a: &Value, b: &Value, path: &str) {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            assert!((x - y).abs() <= 1e-9 * (1.0 + y.abs()), "{path}: {x} vs {y}");
        }
        (Value::Object(x), Value::Object(y)) => {
            assert_eq!(x.keys().collect::<Vec<_>>().len(), y.len(), "{path}: key count {:?} vs {:?}", x.keys().collect::<Vec<_>>(), y.keys().collect::<Vec<_>>());
            for (k, v) in y {
                approx(x.get(k).unwrap_or_else(|| panic!("{path}.{k} missing")), v, &format!("{path}.{k}"));
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            assert_eq!(x.len(), y.len(), "{path}: len");
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                approx(p, q, &format!("{path}[{i}]"));
            }
        }
        _ => assert_eq!(a, b, "{path}"),
    }
}

fn load_league(fx: &std::path::Path, league: &str) -> (Snapshot, Map<Cid, PlayerValue>, Vec<Recommendation>, Vec<Recommendation>) {
    let snap = Snapshot::load(fx.join(league).join("snapshot.json")).unwrap();
    let players: Map<String, fm_core::models::Player> = snap.ctx.all_players().into_iter().map(|p| (p.cid.clone(), p.clone())).collect();
    let mut vals: Map<Cid, PlayerValue> = Map::new();
    for (cid, v) in read(fx.join(league).join("values.json")).as_object().unwrap() {
        let mut v = v.clone();
        v["player"] = serde_json::to_value(&players[cid]).unwrap();
        vals.insert(cid.clone(), serde_json::from_value(v).unwrap());
    }
    let recs = |name: &str| -> Vec<Recommendation> { serde_json::from_value(read(fx.join(league).join("engines").join(name))).unwrap() };
    (snap, vals, recs("advise_merged.json"), recs("line_alerts.json"))
}

fn digest_json(d: &fm_report::digest::Digest) -> Value {
    json!({"markdown": d.markdown, "html": d.html, "summary": d.summary, "alert": d.alert,
           "overall": d.health.as_ref().map(|h| h.overall.as_str()), "league": d.league,
           "day": d.day().map(|d| d.format("%Y-%m-%d").to_string())})
}

#[test]
fn digest_matches_python() {
    let Some(fx) = fixtures() else { return eprintln!("skip: no _w9_golden/report.json") };
    let gold = read(fx.join("_w9_golden/report.json"));
    for league in ["espn", "fantrax"] {
        let (snap, vals, recs, _) = load_league(&fx, league);
        let ctx = &snap.ctx;
        let gen = snap.local_now.unwrap();
        let mine = ctx.my_team().unwrap().players();
        let mut news: Map<String, Vec<fm_sources::news::NewsItem>> = Map::new();
        for (i, p) in mine.iter().take(4).enumerate() {
            let item = json!({"source": "rotowire", "player_name": p.name, "headline": format!("{} update {i}", p.name),
                              "blurb": "Skated in full and is expected back.", "url": format!("https://example.com/{i}"),
                              "published": "2026-11-03T12:00:00+00:00"});
            news.insert(p.cid.clone(), vec![serde_json::from_value(item).unwrap()]);
        }
        let g = &gold[league]["digests"];
        let check = |name: &str, d: fm_report::digest::Digest| {
            let got = digest_json(&d);
            for k in ["markdown", "html", "summary", "alert", "overall", "league", "day"] {
                assert!(got[k] == g[name][k], "{league}/{name}/{k} differs\n--- rust ---\n{}\n--- python ---\n{}", got[k], g[name][k]);
            }
        };
        check("plain", build_digest(ctx, &vals, &recs, Some(&news), &gen, None, None, None));
        let plan: Vec<String> = ["This week vs Rival: win 66% -> 71% (moves left 2)", "Quick lineup each day; also do:", "Today: add X"].map(String::from).to_vec();
        let health: Vec<String> = ["Projection MAE: 0.55", "Hit rate: 60%", "Params: packaged (hash abc)"].map(String::from).to_vec();
        check("full", build_digest(ctx, &vals, &recs, Some(&news), &gen, Some("forwards beat season-to-date by 12%"), Some(&health), Some(&plan)));
        let mut bad: LeagueContext = ctx.clone();
        bad.sources.push(serde_json::from_value(json!({"name": "Fantrax", "ok": false, "severity": "fail", "feeds_valuation": true,
                                                       "detail": "login expired - refresh FANTRAX_COOKIE"})).unwrap());
        bad.sources.push(serde_json::from_value(json!({"name": "Odds", "ok": false, "severity": "fail", "feeds_valuation": false,
                                                       "detail": "HTTP 500 https://x.com/a?b=1"})).unwrap());
        check("failing", build_digest(&bad, &vals, &recs, Some(&news), &gen, None, None, None));
        check("empty", build_digest(ctx, &vals, &[], None, &gen, None, None, None));
        let long: Vec<Recommendation> = (0..20)
            .flat_map(|_| recs.iter().take(10).cloned().map(|mut r| { r.narrative = Some("x ".repeat(400)); r }))
            .collect();
        check("long", build_digest(ctx, &vals, &long, Some(&news), &gen, None, None, None));
    }
}

#[test]
fn write_digest_names_files_by_league_and_day() {
    let Some(fx) = fixtures() else { return eprintln!("skip: no fixtures") };
    let (snap, vals, recs, _) = load_league(&fx, "espn");
    let d = build_digest(&snap.ctx, &vals, &recs, None, &snap.local_now.unwrap(), None, None, None);
    let dir = tempfile::tempdir().unwrap();
    let (md, html) = write_digest(&d, dir.path().join("reports")).unwrap();
    let day = d.day().unwrap().format("%Y-%m-%d");
    assert_eq!(md.file_name().unwrap().to_string_lossy(), format!("digest-espn-{day}.md"));
    assert_eq!(std::fs::read_to_string(&md).unwrap(), d.markdown);
    assert!(std::fs::read_to_string(&html).unwrap().starts_with("<!doctype html>"));
}

#[test]
fn chunking_and_formatting_match_python() {
    let Some(fx) = fixtures() else { return eprintln!("skip: no fixtures") };
    let n = &read(fx.join("_w9_golden/report.json"))["notify"];
    for (name, text) in n["text_cases"].as_object().unwrap() {
        for (i, size) in [1900usize, 1500, 100].iter().enumerate() {
            assert_eq!(json!(chunk_text(text.as_str().unwrap(), *size)), n["chunks"][name][i], "{name}/{size}");
        }
    }
    let lines: Vec<String> = ["  a   b  ", "", "   ", "second\nline", "xxxxx"].map(String::from).to_vec();
    for style in ["discord", "slack"] {
        assert_eq!(json!([format_changes("  Title ", &lines, style), format_changes("", &lines, style)]), n["format"][style], "{style}");
    }
}

#[test]
fn webhook_scenarios_match_python() {
    let Some(fx) = fixtures() else { return eprintln!("skip: no fixtures") };
    let n = &read(fx.join("_w9_golden/report.json"))["notify"];
    let big = n["text_cases"]["multi"].as_str().unwrap().to_string();
    let (d, s) = ("https://discord.com/api/webhooks/1/SECRET", "https://hooks.slack.com/services/T/B/SECRET");
    for sc in n["scenarios"].as_array().unwrap() {
        let name = sc["name"].as_str().unwrap();
        let script = || -> Fake {
            Fake::new(
                sc["responses"].as_array().unwrap().iter().map(|r| match r["error"].as_str() {
                    Some("timeout") => Resp::Timeout,
                    Some(_) => Resp::Connect,
                    None => {
                        let text: &'static str = Box::leak(r["text"].as_str().unwrap_or("").to_string().into_boxed_str());
                        let ra: Option<&'static str> = r["retry_after"].as_str().map(|x| &*Box::leak(x.to_string().into_boxed_str()));
                        Resp::Ok(r["status"].as_u64().unwrap() as u16, text, ra)
                    }
                }).collect(),
            )
        };
        let calls = |f: &Fake| -> Value { Value::Array(f.calls.borrow().iter().map(|(u, p)| json!([u, p])).collect()) };
        let f = script();
        let text = if name == "ok_multi" || name == "late_error" { big.as_str() } else { "hi" };
        assert_eq!(json!(send_discord(Some(d), text, Some(&f))), sc["discord"]["result"], "{name} discord");
        assert_eq!(calls(&f), sc["discord"]["calls"], "{name} discord calls");
        let f = script();
        assert_eq!(json!(send_slack(Some(s), "hello", Some(&f))), sc["slack"]["result"], "{name} slack");
        assert_eq!(calls(&f), sc["slack"]["calls"], "{name} slack calls");
        let f = script();
        let r = notify_changes_urls(Some(d), None, "Pre-game 5:02 PM - ESPN", &["a".to_string(), "b".to_string()], Some(&f));
        assert_eq!(json!(r), sc["changes"]["result"], "{name} changes");
        assert_eq!(calls(&f), sc["changes"]["calls"], "{name} changes calls");
    }
}

#[test]
fn pregame_state_diff_summary_match_python() {
    let Some(fx) = fixtures() else { return eprintln!("skip: no fixtures") };
    let gold = read(fx.join("_w9_golden/report.json"));
    for league in ["espn", "fantrax"] {
        let (snap, vals, _, alerts) = load_league(&fx, league);
        let ctx = &snap.ctx;
        let now = snap.local_now.unwrap();
        let later = PyDateTime::naive(now.local + chrono::Duration::hours(9));
        let team = ctx.my_team().unwrap();
        let goalies: Vec<&fm_core::models::Player> = team.players().into_iter().filter(|p| p.is_goalie()).collect();
        let fa: Vec<&fm_core::models::Player> = ctx.free_agents.iter().filter(|p| p.is_goalie()).take(6).collect();
        let starts = |strength: &str| -> Vec<fm_sources::dailyfaceoff::GoalieStart> {
            goalies.iter().chain(fa.iter()).enumerate().map(|(i, p)| {
                let t = fm_core::normalize::normalize_team(p.team.as_deref()).unwrap_or_else(|| "NYR".into());
                serde_json::from_value(json!({"game": format!("OPP{i}@{t}"), "game_time": null, "team": t, "opponent": format!("OPP{i}"),
                    "home": true, "goalie_name": p.name, "dfo_id": null, "strength": if i % 2 == 0 { strength } else { "Likely" },
                    "source": null, "created_at": null})).unwrap()
            }).collect()
        };
        let morning = build_state(league, ctx, &vals, &starts("Likely"), Some(&alerts[..3.min(alerts.len())]), "daily", &now, None, None).unwrap();
        let mut ctx2: LeagueContext = ctx.clone();
        let mut n = 0;
        for s in ctx2.teams.iter_mut().find(|t| t.owner_is_me).unwrap().slots.iter_mut() {
            if let Some(p) = s.player.as_mut().filter(|p| !p.is_goalie()) {
                if n == 0 {
                    p.status = Status::Out;
                } else if n == 1 {
                    p.line = Some("f4".into());
                    p.pp_unit = Some("pp2".into());
                }
                n += 1;
                if n == 2 {
                    break;
                }
            }
        }
        let cur = build_state(league, &ctx2, &vals, &starts("Confirmed"), Some(&alerts[..8.min(alerts.len())]), "pregame", &later, None, None).unwrap();
        let changes = diff_states(Some(&morning), &cur, None);
        let lineup = today_lineup(&ctx2, &vals).unwrap();
        let g = &gold[league]["pregame"];
        approx(&morning, &g["morning"], &format!("{league}.morning"));
        approx(&cur, &g["state"], &format!("{league}.state"));
        approx(&lineup.clone().unwrap_or(Value::Null), &g["lineup"], &format!("{league}.lineup"));
        let ch: Value = serde_json::to_value(&changes).unwrap();
        assert_eq!(ch, g["changes"], "{league} changes");
        assert_eq!(json!(summarize(&changes, lineup.as_ref(), &later, SUMMARY_LIMIT)), g["summary"], "{league} summary");
        assert_eq!(json!(summarize(&changes, None, &later, SUMMARY_LIMIT)), g["summary_nolineup"], "{league} summary_nolineup");
        assert_eq!(json!(summarize(&[], lineup.as_ref(), &now, SUMMARY_LIMIT)), g["summary_none"], "{league} summary_none");
        let dir = tempfile::tempdir().unwrap();
        let day = ctx.as_of;
        let p = save_snapshot(dir.path(), league, day, &cur).unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap(), g["snapshot_json"].as_str().unwrap(), "{league} snapshot file");
        assert_eq!(goalies_state(&starts("Confirmed"), None), g["goalies"], "{league} goalies_state");
    }
}
