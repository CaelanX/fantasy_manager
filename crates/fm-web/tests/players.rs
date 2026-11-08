//! Player tools routes: /players (search, filters, watchlist), POST /watch and /watch/seen, the
//! This week strip, /compare and the player card's action bar. A tiny synthetic league whose
//! data the test can change between requests (no result cache: TTL 0).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use fm_core::config::Settings;
use fm_core::PyDateTime;
use fm_web::app::{App, AppConfig, Loader, Page};
use fm_web::views::LoadResult;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn send(router: &axum::Router, method: &str, url: &str, body: Option<&str>) -> (u16, String, String) {
    let mut b = Request::builder().method(method).uri(url);
    if body.is_some() {
        b = b.header("content-type", "application/x-www-form-urlencoded");
    }
    let req = b.body(Body::from(body.unwrap_or("").to_string())).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let loc = resp.headers().get("location").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, loc, String::from_utf8_lossy(&bytes).into_owned())
}

fn player(cid: &str, name: &str, team: &str, pos: &str, status: &str) -> Value {
    json!({"cid": cid, "name": name, "name_norm": name.to_lowercase(), "ids": {}, "team": team, "positions": [pos], "status": status})
}

/// Crosby on my team, Kane (out) on "Rivals", Pospíšil a free agent; `moved`: Pospíšil signed by
/// Rivals and Kane healthy again.
fn page(data_dir: &Path, moved: bool) -> Page {
    let crosby = player("espn:1", "Sidney Crosby", "PIT", "C", "healthy");
    let kane = player("espn:2", "Patrick Kane", "DET", "RW", if moved { "healthy" } else { "out" });
    let pospisil = player("espn:3", "Martin Pospíšil", "CGY", "C", "healthy");
    let mut rivals = vec![json!({"slot": "RW", "starting": true, "player": kane})];
    if moved {
        rivals.push(json!({"slot": "BN", "starting": false, "player": pospisil}));
    }
    let ctx: fm_core::models::LeagueContext = serde_json::from_value(json!({
        "provider": "espn", "league_id": "1", "name": "Test League", "season": 2027,
        "scoring": {"kind": "points", "weights": {"G": 3.0, "A": 2.0}},
        "roster_shape": {"C": 1, "RW": 1, "BN": 1},
        "teams": [{"team_id": "1", "name": "Mine", "owner_is_me": true, "slots": [{"slot": "C", "starting": true, "player": crosby}]},
                  {"team_id": "2", "name": "Rivals", "owner_is_me": false, "slots": rivals}],
        "free_agents": if moved { json!([]) } else { json!([pospisil]) }, "as_of": "2026-11-04"
    }))
    .unwrap();
    let now = PyDateTime::parse("2026-11-04T17:00:00+00:00").unwrap();
    let res = LoadResult::new(ctx, Default::default(), None, vec![], now, chrono::NaiveDate::from_ymd_opt(2026, 11, 4).unwrap(), data_dir.into());
    Page { res: Arc::new(res), provider: None }
}

struct SwapLoader(Mutex<Page>);

impl Loader for SwapLoader {
    fn load(&self, _league: &str, _s: &Settings, _now: &PyDateTime) -> anyhow::Result<Page> {
        Ok(self.0.lock().unwrap().clone())
    }
}

fn app(dir: &Path) -> (Arc<App>, Arc<SwapLoader>) {
    let loader = Arc::new(SwapLoader(Mutex::new(page(dir, false))));
    let mut cfg = AppConfig::new(Settings { fm_data_dir: dir.into(), fm_offline: true, ..Settings::default() });
    cfg.llm = Some(Arc::new(|_| None));
    cfg.cache_ttl = Duration::ZERO;
    cfg.clock = Arc::new(|| PyDateTime::parse("2026-11-04T17:00:00+00:00").unwrap());
    let l: Arc<dyn Loader> = loader.clone();
    (App::new(l, cfg), loader)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
}

#[test]
fn players_search_and_filters() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = app(dir.path());
    let r = a.router();
    let rt = rt();
    let get = |url: &str| {
        let (s, _, b) = rt.block_on(send(&r, "GET", url, None));
        assert_eq!(s, 200, "{url}: {b}");
        b
    };
    let b = get("/players?league=espn");
    assert!(b.contains("Sidney Crosby") && b.contains("Patrick Kane") && b.contains("Martin Pospíšil"));
    assert!(b.contains("id=\"watchlist\"") && b.contains("Nobody on your watchlist yet"));
    assert!(b.contains("/static/players.js"));
    let b = get("/players?league=espn&q=POSPISIL");
    assert!(b.contains("Martin Pospíšil") && !b.contains("Patrick Kane"), "accent- and case-insensitive name search");
    let only = |b: &str, name: &str| ["Sidney Crosby", "Patrick Kane", "Martin Pospíšil"].iter().all(|n| b.contains(n) == (*n == name));
    assert!(only(&get("/players?league=espn&own=mine"), "Sidney Crosby"));
    assert!(only(&get("/players?league=espn&own=fa"), "Martin Pospíšil"));
    assert!(only(&get("/players?league=espn&own=others"), "Patrick Kane"));
    assert!(only(&get("/players?league=espn&pos=RW"), "Patrick Kane"));
    assert!(get("/players?league=espn&q=nobody").contains("No players match."));
    assert_eq!(rt.block_on(send(&r, "GET", "/players?league=yahoo", None)).0, 422);
}

#[test]
fn watch_toggle_changes_strip_and_mark_seen() {
    let dir = tempfile::tempdir().unwrap();
    let (a, loader) = app(dir.path());
    let r = a.router();
    let rt = rt();
    let post = |url: &str, body: &str| rt.block_on(send(&r, "POST", url, Some(body)));
    let get = |url: &str| rt.block_on(send(&r, "GET", url, None)).2;

    let (s, loc, _) = post("/watch", "league=espn&cid=espn%3A3&on=1&next=%2Fplayers%3Fq%3Dmar");
    assert_eq!((s, loc.as_str()), (303, "/players?q=mar&league=espn"));
    let (s, _, b) = post("/watch", "league=espn&cid=espn%3A2&on=1&format=json");
    assert_eq!(s, 200);
    assert_eq!(serde_json::from_str::<Value>(&b).unwrap(), json!({"cid": "espn:2", "on": true}));
    let file: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("watchlist.json")).unwrap()).unwrap();
    assert_eq!(file["espn"]["espn:3"]["seen"]["owner"], Value::Null);
    assert_eq!(file["espn"]["espn:2"]["seen"], json!({"owner": "2", "status": "out", "return_date": null, "line": null, "pp": null}));
    assert!(file["espn"]["espn:2"]["added"].as_str().unwrap().starts_with("2026-11-04T"), "local time of the request clock");

    let b = get("/players?league=espn");
    assert!(b.contains("2 players") && !b.contains("Mark all seen"), "nothing changed yet");
    assert!(!get("/plan?league=espn").contains("watch-strip"), "no strip without changes");

    *loader.0.lock().unwrap() = page(dir.path(), true);
    let b = get("/players?league=espn");
    assert!(b.contains("Now on Rivals") && b.contains("Out \u{2192} Healthy") && b.contains("Mark all seen"), "{b}");
    let plan = get("/plan?league=espn");
    assert!(plan.contains("Martin Pospíšil is now on Rivals") && plan.contains("Patrick Kane went from Out to Healthy"), "plan strip");
    assert!(plan.contains("href=\"/players?league=espn#watchlist\""));

    let (s, loc, _) = post("/watch/seen", "league=espn&next=%2Fplayers");
    assert_eq!((s, loc.as_str()), (303, "/players?league=espn#watchlist"));
    let b = get("/players?league=espn");
    assert!(!b.contains("Now on Rivals") && !b.contains("Mark all seen"));
    assert!(!get("/plan?league=espn").contains("watch-strip"));

    let (s, _, b) = post("/watch", "league=espn&cid=espn%3A2&on=0&format=json");
    assert_eq!((s, b.as_str()), (200, r#"{"cid":"espn:2","on":false}"#));
    let file: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("watchlist.json")).unwrap()).unwrap();
    assert!(file["espn"].get("espn:2").is_none() && file["espn"].get("espn:3").is_some());

    assert_eq!(post("/watch", "league=espn&cid=espn%3A999&on=1").0, 404);
    assert_eq!(post("/watch", "league=yahoo&cid=espn%3A1&on=1").0, 422);
    assert_eq!(post("/watch", "league=espn&on=1").0, 422);
    assert_eq!(rt.block_on(send(&r, "GET", "/watch", None)).0, 405);
    let (s, loc, _) = post("/watch", "league=espn&cid=espn%3A3&on=0&next=%2F%2Fevil.example");
    assert_eq!((s, loc.as_str()), (303, "/?league=espn"), "same-site redirects only");
}

#[test]
fn compare_page_and_add_search() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = app(dir.path());
    let r = a.router();
    let rt = rt();
    let get = |url: &str| rt.block_on(send(&r, "GET", url, None));
    let (s, _, b) = get("/compare?league=espn&p=espn%3A1&p=espn%3A2&p=espn%3A999&p=espn%3A1");
    assert_eq!(s, 200, "{b}");
    assert_eq!(b.matches("data-compare-col").count(), 2, "unknown and repeated players dropped");
    assert!(b.contains("Games next 14") && b.contains("Rivals") && b.contains("aria-label=\"Remove Patrick Kane\""));
    assert!(b.contains("href=\"/compare?league=espn&amp;p=espn%3A1\""), "remove link keeps the others");
    let (_, _, b) = get("/compare?league=espn&p=espn%3A1");
    assert!(b.contains("Add up to three more players"));
    let (_, _, b) = get("/compare?league=espn");
    assert!(b.contains("Nobody to compare yet"));
    let (s, loc, _) = get("/compare?league=espn&p=espn%3A1&q=kane");
    assert_eq!((s, loc.as_str()), (303, "/compare?league=espn&p=espn%3A1&p=espn%3A2"));
    let (_, _, b) = get("/compare?league=espn&p=espn%3A1&q=zzz");
    assert!(b.contains("No player matches"));
    let (_, _, b) = get("/compare?league=espn&q=i");
    assert!(b.contains("More than one player matches"), "an ambiguous name lists candidates");
}

#[test]
fn card_action_bar() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = app(dir.path());
    let r = a.router();
    let rt = rt();
    let card = |cid: &str| {
        let (s, _, b) = rt.block_on(send(&r, "GET", &format!("/player/{cid}?league=espn&card=1"), None));
        assert_eq!(s, 200, "{b}");
        b
    };
    let mine = card("espn%3A1");
    assert!(mine.contains("href=\"/trade-eval?league=espn&amp;mode=shop&amp;cid=espn:1\"") && mine.contains(">Shop him<"));
    let theirs = card("espn%3A2");
    assert!(theirs.contains("mode=get&amp;cid=espn:2") && theirs.contains(">Get him<"));
    let fa = card("espn%3A3");
    assert!(!fa.contains("trade-eval") && fa.contains("data-compare=\"espn:3\"") && fa.contains("<span class=\"w-txt\">Watch</span>"));
    assert!(fa.contains("Full player page"));
    rt.block_on(send(&r, "POST", "/watch", Some("league=espn&cid=espn%3A3&on=1")));
    assert!(card("espn%3A3").contains("<span class=\"w-txt\">Watching</span>"));
}
