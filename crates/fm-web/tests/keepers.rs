//! Keeper planner: the Roster page's keeper panel and K badges, the /keepers page and
//! `keeper_status`, over a small synthetic keeper league (and the same league without keepers).

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use fm_core::config::Settings;
use fm_core::models::LeagueContext;
use fm_core::values::PlayerValue;
use fm_core::{Map, PyDateTime};
use fm_engine::valuation::dynasty::apply_dynasty;
use fm_web::app::{App, AppConfig, Loader, Page};
use fm_web::keepers_view::{keeper_status, keepers_page};
use fm_web::views::LoadResult;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// (cid, age, fpg)
const MINE: [(&str, i32, f64); 4] = [("m1", 24, 3.0), ("m2", 26, 2.6), ("m3", 30, 2.0), ("m4", 35, 1.0)];
const THEIRS: [(&str, i32, f64); 3] = [("t1", 23, 4.0), ("t2", 25, 3.5), ("t3", 33, 1.2)];
const FAS: [(&str, i32, f64); 2] = [("fa1", 27, 1.5), ("fa2", 29, 0.8)];

fn player(cid: &str, age: i32) -> Value {
    json!({"cid": cid, "name": format!("Player {cid}"), "name_norm": format!("player {cid}"), "ids": {}, "team": "BOS",
           "positions": ["C"], "status": "healthy", "birth_date": format!("{}-10-01", 2026 - age), "career_gp": 400,
           "lines": {"prior": {"split": "prior", "gp": 70, "stats": {"GP": 70}}}})
}

fn slots(rows: &[(&str, i32, f64)]) -> Vec<Value> {
    rows.iter().enumerate().map(|(i, (cid, age, _))| json!({"slot": if i == 0 { "C" } else { "BN" }, "starting": i == 0, "player": player(cid, *age)})).collect()
}

fn result(keepers: Option<i64>, dir: &Path) -> LoadResult {
    let ctx: LeagueContext = serde_json::from_value(json!({
        "provider": "fantrax", "league_id": "1", "name": "Keeper League", "season": 2027,
        "scoring": {"kind": "points", "weights": {"G": 3.0, "A": 2.0}},
        "roster_shape": {"C": 1, "BN": 4},
        "teams": [{"team_id": "a", "name": "Mine", "owner_is_me": true, "slots": slots(&MINE)},
                  {"team_id": "b", "name": "Theirs", "owner_is_me": false, "slots": slots(&THEIRS)}],
        "free_agents": FAS.iter().map(|(c, a, _)| player(c, *a)).collect::<Vec<_>>(),
        "as_of": "2026-11-04", "season_start": "2026-10-07", "dynasty": true, "keeper_horizon_years": 3,
        "dynasty_mode": "balanced", "keepers_per_team": keepers,
    }))
    .unwrap();
    let all: Vec<&fm_core::models::Player> = ctx.all_players().into_iter().chain(ctx.free_agents.iter()).collect();
    let fpg: Map<&str, f64> = MINE.iter().chain(&THEIRS).chain(&FAS).map(|(c, _, f)| (*c, *f)).collect();
    let values: Map<String, PlayerValue> = all
        .iter()
        .map(|p| {
            let f = fpg[p.cid.as_str()];
            let v = json!({"player": p, "fpg": f, "fpg_season": f, "fpg_week": f, "vorp": f - 1.5});
            (p.cid.clone(), serde_json::from_value(v).unwrap())
        })
        .collect();
    let dynv = apply_dynasty(&values, &ctx, None, None);
    let now = PyDateTime::parse("2026-11-04T17:00:00+00:00").unwrap();
    LoadResult::new(ctx, values, Some(dynv), vec![], now, chrono::NaiveDate::from_ymd_opt(2026, 11, 4).unwrap(), dir.into())
}

struct Fixed(Page);

impl Loader for Fixed {
    fn load(&self, _league: &str, _s: &Settings, _now: &PyDateTime) -> anyhow::Result<Page> {
        Ok(self.0.clone())
    }
}

fn get(res: LoadResult, dir: &Path, urls: &[&str]) -> Vec<(u16, String)> {
    let page = Page { res: Arc::new(res), provider: None };
    let mut cfg = AppConfig::new(Settings { fm_data_dir: dir.into(), fm_offline: true, ..Settings::default() });
    cfg.llm = Some(Arc::new(|_| None));
    cfg.clock = Arc::new(|| PyDateTime::parse("2026-11-04T17:00:00+00:00").unwrap());
    let router = App::new(Arc::new(Fixed(page)), cfg).router();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    urls.iter()
        .map(|u| {
            rt.block_on(async {
                let resp = router.clone().oneshot(Request::builder().uri(*u).body(Body::empty()).unwrap()).await.unwrap();
                let status = resp.status().as_u16();
                let bytes = resp.into_body().collect().await.unwrap().to_bytes();
                (status, String::from_utf8_lossy(&bytes).into_owned())
            })
        })
        .collect()
}

#[test]
fn keeper_league_pages_and_status() {
    let dir = tempfile::tempdir().unwrap();
    let res = result(Some(2), dir.path());
    let page = keepers_page(&res);
    let plan = &page["plan"];
    assert_eq!(plan["k"], 2);
    let mine = &plan["teams"][0];
    assert_eq!((mine["name"].as_str(), mine["me"].as_bool()), (Some("Mine"), Some(true)));
    let kept: Vec<&str> = mine["keepers"].as_array().unwrap().iter().map(|c| c["cid"].as_str().unwrap()).collect();
    let flagged: Vec<&str> = MINE.iter().map(|r| r.0).filter(|c| res.dynasty.as_ref().unwrap()[*c].keeper).collect();
    assert_eq!(kept.len(), 2);
    assert_eq!({ let mut k = kept.clone(); k.sort(); k }, { let mut f = flagged.clone(); f.sort(); f });
    let bubble = mine["bubble"].as_array().unwrap();
    assert_eq!(bubble.len(), 2);
    let last = mine["keepers"][1]["surplus"].as_f64().unwrap();
    assert!((bubble[0]["gap"].as_f64().unwrap() - (last - bubble[0]["surplus"].as_f64().unwrap())).abs() < 1e-9);
    assert_eq!(bubble[0]["behind"], mine["keepers"][1]["name"]);
    let pool = plan["pool"].as_array().unwrap();
    assert_eq!(pool[0]["overall"], 1);
    let all_kept: Vec<&str> = plan["teams"].as_array().unwrap().iter().flat_map(|t| t["keepers"].as_array().unwrap()).map(|c| c["cid"].as_str().unwrap()).collect();
    assert!(pool.iter().all(|x| !all_kept.contains(&x["cid"].as_str().unwrap())));
    assert!(pool.iter().any(|x| x["team"].is_null()), "free agents are in the pool");

    let st = keeper_status(&res, kept[0]).unwrap();
    assert_eq!(st, json!({"keeper": true, "rank": 1, "of": 2, "team": "Mine", "surplus": st["surplus"]}));
    let b = keeper_status(&res, bubble[0]["cid"].as_str().unwrap()).unwrap();
    assert_eq!((b["keeper"].as_bool(), b["rank"].as_u64()), (Some(false), Some(3)));
    assert!(keeper_status(&res, "fa1").unwrap()["team"].is_null());
    assert!(keeper_status(&res, "nobody").is_none());

    let r = get(res, dir.path(), &["/roster?league=fantrax", "/keepers?league=fantrax", "/roster?league=fantrax&team=b"]);
    let (s, roster) = &r[0];
    assert_eq!(*s, 200, "{roster}");
    assert!(roster.contains("Your keepers") && roster.contains("On the bubble:") && roster.contains("/keepers?league=fantrax"));
    assert!(roster.contains(&format!("/player/{}?league=fantrax", kept[0])));
    assert!(roster.contains(" behind Player "), "gap to my last keeper");
    assert_eq!(roster.matches("class=\"ub kb\"").count(), 2, "K badge on each predicted keeper");
    let (s, keepers) = &r[1];
    assert_eq!(*s, 200, "{keepers}");
    assert!(keepers.contains("Predicted keepers") && keepers.contains("Draft pool preview") && keepers.contains("Theirs"));
    assert!(keepers.contains("Free agent"));
    let (s, other) = &r[2];
    assert_eq!(*s, 200);
    assert!(other.contains("Predicted keepers") && !other.contains("Your keepers"));
}

#[test]
fn non_keeper_league_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let res = result(None, dir.path());
    assert_eq!(keepers_page(&res), json!({"plan": null}));
    assert!(keeper_status(&res, "m1").is_none());
    let r = get(res, dir.path(), &["/keepers?league=fantrax", "/roster?league=fantrax"]);
    assert_eq!(r[0].0, 200);
    assert!(r[0].1.contains("is not a keeper league") && !r[0].1.contains("Draft pool"));
    assert!(!r[1].1.contains("Your keepers") && !r[1].1.contains("class=\"ub kb\""));
}
