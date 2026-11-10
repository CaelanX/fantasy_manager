//! W10-web-app: the dashboard server.
//!
//! * HTML parity: every page of `parity/dump_web_html.py` (Python FastAPI app over the parity
//!   snapshot, pinned clock, seeded matchup) requested from the Rust router with the same
//!   LoadResult; bodies compared after whitespace normalization (JSON APIs as parsed JSON). Goldens:
//!   `FM_WEB_HTML_GOLDEN` or `$FM_PARITY_FIXTURES/_w10_golden`; skipped when absent.
//! * Unit / behaviour tests ported from tests/test_web.py, test_web_refresh.py,
//!   test_web_trade_eval.py and test_schedule_web_cli.py (web parts): redirects, cache, refresh,
//!   mode toggle, explain, wording helpers, 404/405/422.
//! * Latency: cold first page vs warm page (printed).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use fm_core::config::Settings;
use fm_core::models::Recommendation;
use fm_core::PyDateTime;
use fm_engine::recommend::injuries::StatusSnapshot;
use fm_providers::pipeline::from_snapshot;
use fm_web::app::{self, App, AppConfig, Loader, Page};
use fm_web::views::LoadResult;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn fixtures() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var("FM_PARITY_FIXTURES").ok()?);
    p.join("espn").join("snapshot.json").exists().then_some(p)
}

fn golden_dir() -> Option<PathBuf> {
    let d = std::env::var("FM_WEB_HTML_GOLDEN").map(PathBuf::from).ok().or_else(|| fixtures().map(|f| f.join("_w10_golden")))?;
    d.join("espn.json").exists().then_some(d)
}

fn read(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let t = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &t);
        } else {
            std::fs::copy(e.path(), t).unwrap();
        }
    }
}

fn attach<T: serde::de::DeserializeOwned>(raw: Value, ctx: &fm_core::models::LeagueContext) -> Option<fm_core::Map<String, T>> {
    let obj = raw.as_object()?;
    let players: std::collections::HashMap<&str, &fm_core::models::Player> =
        ctx.all_players().into_iter().map(|p| (p.cid.as_str(), p)).collect();
    let mut out = fm_core::Map::new();
    for (cid, v) in obj {
        let mut v = v.clone();
        let o = v.as_object_mut().unwrap();
        o.remove("cid");
        o.insert("player".into(), serde_json::to_value(players[cid.as_str()]).unwrap());
        out.insert(cid.clone(), serde_json::from_value(v).unwrap_or_else(|e| panic!("{cid}: {e}")));
    }
    Some(out)
}

/// The LoadResult dump_web_html.py builds, plus the snapshot provider.
fn snapshot_page(league: &str, fx: &Path, settings: &Settings) -> (Page, PyDateTime) {
    let dir = fx.join(league);
    let data_dir = &settings.fm_data_dir;
    let loaded = from_snapshot(&dir, Some(data_dir)).unwrap();
    let snap = read(&dir.join("snapshot.json"));
    let now = PyDateTime::parse(snap["now"].as_str().unwrap()).unwrap();
    let recs: Vec<Recommendation> = serde_json::from_value(read(&dir.join("engines").join("advise_merged.json"))).unwrap();
    let values = attach(read(&dir.join("values.json")), &loaded.lc).unwrap();
    let dynv = attach(read(&dir.join("dynasty.json")), &loaded.lc);
    let mut lc = loaded.lc;
    let (mode, source) = app::current_mode(settings);
    lc.dynasty_mode = fm_core::prefs::normalize_mode(&mode).unwrap();
    lc.dynasty_mode_source = Some(source.clone());
    let warnings = lc.warnings.clone();
    let mut res = LoadResult::new(lc, values, dynv, recs, now.clone(), loaded.clock.today, data_dir.clone());
    res.matchup_seed = Some(7);
    res.mode = Some(mode);
    res.mode_source = Some(source);
    *res.warnings.lock().unwrap() = warnings;
    let mine = res.ctx.my_team().unwrap().clone();
    let first = mine.players_iter().next().unwrap().clone();
    let t0 = (now.to_utc().unwrap().timestamp() as f64) - 3.0 * 86400.0;
    res.status_log.insert(
        first.cid.clone(),
        vec![
            StatusSnapshot { cid: first.cid.clone(), status: "healthy".into(), note: None, seen_at: t0 },
            StatusSnapshot { cid: first.cid.clone(), status: "dtd".into(), note: Some("upper body".into()), seen_at: t0 + 86400.0 },
        ],
    );
    let items = fm_sources::news::store_news(Some(data_dir), &[], None, now.to_utc().unwrap());
    let news = fm_sources::news_match::match_news_to_players(&items, &res.ctx.all_players());
    res.news_by_cid = news.into_iter().map(|(k, v)| (k, v.iter().map(|n| serde_json::to_value(n).unwrap()).collect())).collect();
    let provider: Arc<dyn fm_core::provider::LeagueProvider> = Arc::from(loaded.provider);
    (Page { res: Arc::new(res), provider: Some(provider) }, now)
}

/// Documented non-determinism (ARCHITECTURE 4.3): the plan's win probabilities come from a
/// Monte Carlo fed in set order (abs 2 percentage points here), the plan day lines list goalies
/// in set order (sorted here), and the "loaded HH:MM" stamp is the load time. Returns the masked
/// body and the masked numbers.
fn normalize_set_order(s: &str) -> (String, Vec<i64>) {
    use regex::Regex;
    let mut nums = Vec::new();
    let pats = [
        r"([+-]?\d+)(% win chance)",
        r"(Win chance )(\d+)(% with quick lineup, )(\d+)(% with the plan)",
        r#"(class="rink-(?:fill|pt pt-set|pt pt-plan)" style="(?:width|left): )(\d+)(%)"#,
        r"(<dt>With (?:quick lineup|the plan)</dt><dd>)(\d+)(%</dd>)",
    ];
    let mut out = s.to_string();
    for p in pats {
        let re = Regex::new(p).unwrap();
        out = re
            .replace_all(&out, |c: &regex::Captures<'_>| {
                let mut r = String::new();
                for i in 1..c.len() {
                    let g = &c[i];
                    if let Ok(n) = g.trim_start_matches('+').parse::<i64>() {
                        nums.push(n);
                        r.push('#');
                    } else {
                        r.push_str(g);
                    }
                }
                r
            })
            .into_owned();
    }
    let goalies = Regex::new(r#"(<p class="d-k">Goalies</p><ul>)(.*?)(</ul>)"#).unwrap();
    out = goalies
        .replace_all(&out, |c: &regex::Captures<'_>| {
            let mut items: Vec<&str> = c[2].split("</li>").filter(|x| !x.is_empty()).collect();
            items.sort();
            format!("{}{}{}", &c[1], items.iter().map(|x| format!("{x}</li>")).collect::<String>(), &c[3])
        })
        .into_owned();
    let stamp = Regex::new(r#"<time datetime="[0-9T:\-]+">\d\d:\d\d</time>"#).unwrap();
    out = stamp.replace_all(&out, "<time>#</time>").into_owned();
    (out, nums)
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First differing region of two (whitespace-normalized) bodies.
fn first_diff(a: &str, b: &str) -> String {
    let (ac, bc): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let i = ac.iter().zip(&bc).position(|(x, y)| x != y).unwrap_or(ac.len().min(bc.len()));
    let lo = i.saturating_sub(160);
    let s = |v: &[char]| v[lo.min(v.len())..(i + 160).min(v.len())].iter().collect::<String>();
    format!("@{i} (len {} vs {})\n  python: {}\n  rust:   {}", ac.len(), bc.len(), s(&ac), s(&bc))
}

fn json_diff(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                match y.get(k) {
                    Some(w) => json_diff(&format!("{path}.{k}"), v, w, out),
                    None => out.push(format!("{path}.{k}: missing in rust")),
                }
            }
            for k in y.keys().filter(|k| !x.contains_key(*k)) {
                out.push(format!("{path}.{k}: extra in rust"));
            }
            let (kx, ky): (Vec<_>, Vec<_>) = (x.keys().collect(), y.keys().filter(|k| x.contains_key(*k)).collect());
            if kx.len() == ky.len() && kx != ky {
                out.push(format!("{path}: key order differs"));
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                out.push(format!("{path}: len {} vs {}", x.len(), y.len()));
            }
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                json_diff(&format!("{path}[{i}]"), v, w, out);
            }
        }
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            if (x - y).abs() > 1e-9 * (1.0 + x.abs().max(y.abs())) {
                out.push(format!("{path}: {x} vs {y}"));
            }
        }
        _ if a != b => out.push(format!("{path}: {} vs {}", a.to_string().chars().take(100).collect::<String>(), b.to_string().chars().take(100).collect::<String>())),
        _ => {}
    }
}

async fn send(router: &axum::Router, method: &str, url: &str, body: Option<&str>) -> (u16, String, String, String) {
    let mut b = Request::builder().method(method).uri(url);
    if body.is_some() {
        b = b.header("content-type", "application/x-www-form-urlencoded");
    }
    let req = b.body(Body::from(body.unwrap_or("").to_string())).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let ctype = resp.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    let loc = resp.headers().get("location").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, ctype, loc, String::from_utf8_lossy(&bytes).into_owned())
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
}

#[test]
fn html_parity_with_python() {
    let _g = lock();
    let (Some(fx), Some(golden)) = (fixtures(), golden_dir()) else {
        eprintln!("SKIP: FM_PARITY_FIXTURES / _w10_golden not available (run parity/dump_web_html.py)");
        return;
    };
    let mut failures: Vec<String> = Vec::new();
    let mut exact = 0;
    let mut total = 0;
    for league in ["espn", "fantrax"] {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        copy_dir(&fx.join("_datadir"), &data_dir);
        let settings = Settings { fm_data_dir: data_dir.clone(), fm_offline: true, ..Settings::default() };
        let (page, now) = snapshot_page(league, &fx, &settings);
        let mut cfg = AppConfig::new(settings.clone());
        cfg.default_league = league.into();
        cfg.home = "plan".into();
        cfg.llm = Some(Arc::new(|_| None));
        let clock = now.clone();
        cfg.clock = Arc::new(move || clock.clone());
        let p2 = page.clone();
        let loader: Arc<dyn Loader> = Arc::new(move |_lg: &str| -> anyhow::Result<Page> { Ok(p2.clone()) });
        let appx = App::new(loader, cfg);
        let router = appx.router();
        let g = read(&golden.join(format!("{league}.json")));
        let rt = rt();
        for (name, pg) in g["pages"].as_object().unwrap() {
            total += 1;
            let url = pg["url"].as_str().unwrap();
            let t0 = Instant::now();
            let (status, ctype, _, body) = rt.block_on(send(&router, "GET", url, None));
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            let want = pg["body"].as_str().unwrap();
            if let Ok(d) = std::env::var("FM_W10_DUMP") {
                let _ = std::fs::create_dir_all(&d);
                let _ = std::fs::write(Path::new(&d).join(format!("{league}_{name}.rust")), &body);
                let _ = std::fs::write(Path::new(&d).join(format!("{league}_{name}.py")), want);
            }
            let wstatus = pg["status"].as_u64().unwrap() as u16;
            if status != wstatus {
                failures.push(format!("{league}/{name}: status {wstatus} vs {status}\n{}", body.chars().take(600).collect::<String>()));
                continue;
            }
            if pg["content_type"].as_str().unwrap().starts_with("application/json") {
                let (mut a, mut b): (Value, Value) = (serde_json::from_str(want).unwrap(), serde_json::from_str(&body).unwrap_or(Value::Null));
                // the load time
                for v in [&mut a, &mut b] {
                    if let Some(m) = v.get_mut("meta").and_then(|m| m.as_object_mut()) {
                        m.insert("loaded_at".into(), json!("#"));
                    }
                }
                let mut d = Vec::new();
                json_diff("", &a, &b, &mut d);
                if d.is_empty() {
                    if want == body {
                        exact += 1;
                    }
                } else {
                    failures.push(format!("{league}/{name} ({url}): {} json diffs:\n  {}", d.len(), d.iter().take(12).cloned().collect::<Vec<_>>().join("\n  ")));
                }
                continue;
            }
            assert!(ctype.starts_with("text/html"), "{league}/{name}: {ctype}");
            if want == body {
                exact += 1;
                eprintln!("{league}/{name}: exact ({ms:.0} ms)");
                continue;
            }
            let ((a, na), (b, nb)) = (normalize_set_order(want), normalize_set_order(&body));
            if na.len() != nb.len() || na.iter().zip(&nb).any(|(x, y)| (x - y).abs() > 2) {
                failures.push(format!("{league}/{name}: win-probability numbers beyond tolerance: {na:?} vs {nb:?}"));
                continue;
            }
            if a == b {
                eprintln!("{league}/{name}: equal after set-order / timestamp masking ({ms:.0} ms)");
                continue;
            }
            let (a, b) = (normalize_ws(&a), normalize_ws(&b));
            if a != b {
                failures.push(format!("{league}/{name} ({url}): {}", first_diff(&a, &b)));
            } else {
                eprintln!("{league}/{name}: equal modulo whitespace ({ms:.0} ms)");
            }
        }
    }
    eprintln!("{exact}/{total} pages byte-identical");
    if !failures.is_empty() {
        panic!("{} of {total} pages differ:\n{}", failures.len(), failures.join("\n\n"));
    }
}

// --------------------------------------------------------------------------- latency

#[test]
fn cold_and_warm_page_latency() {
    let _g = lock();
    let Some(fx) = fixtures() else {
        eprintln!("SKIP: FM_PARITY_FIXTURES not set");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    copy_dir(&fx.join("_datadir"), &data_dir);
    let settings = Settings { fm_data_dir: data_dir, fm_offline: true, ..Settings::default() };
    // a loader that runs the offline snapshot pipeline (provider from the snapshot + tape,
    // valuation, dynasty) and advise, i.e. a cold load without network
    struct SnapLoader {
        fx: PathBuf,
    }
    impl Loader for SnapLoader {
        fn load(&self, league: &str, settings: &Settings, _now: &PyDateTime) -> anyhow::Result<Page> {
            let l = from_snapshot(&self.fx.join(league), Some(&settings.fm_data_dir))?;
            let snap = read(&self.fx.join(league).join("snapshot.json"));
            let now = PyDateTime::parse(snap["now"].as_str().unwrap()).unwrap();
            let base = app::Base {
                ctx: l.lc,
                values: l.values,
                provider: Arc::from(l.provider),
                news_by_cid: Default::default(),
                loaded_at: chrono::Local::now().naive_local(),
                seconds: 0.0,
                cache: None,
            };
            let (mode, src) = app::current_mode(settings);
            let res = app::finish(&base, &mode, &src, false, settings, &now);
            Ok(Page { res: Arc::new(res), provider: Some(base.provider.clone()) })
        }
    }
    let snap = read(&fx.join("espn").join("snapshot.json"));
    let now = PyDateTime::parse(snap["now"].as_str().unwrap()).unwrap();
    let mut cfg = AppConfig::new(settings);
    cfg.home = "plan".into();
    cfg.llm = Some(Arc::new(|_| None));
    cfg.clock = Arc::new(move || now.clone());
    let appx = App::new(Arc::new(SnapLoader { fx: fx.clone() }), cfg);
    let router = appx.router();
    let rt = rt();
    for league in ["espn", "fantrax"] {
        let t0 = Instant::now();
        let (s, ..) = rt.block_on(send(&router, "GET", &format!("/overview?league={league}"), None));
        let cold = t0.elapsed();
        assert_eq!(s, 200);
        let mut warm = Vec::new();
        for path in ["/overview", "/plan", "/roster", "/recommendations", "/matchup", "/schedule", "/trade-eval", "/news"] {
            let t = Instant::now();
            let (s, ..) = rt.block_on(send(&router, "GET", &format!("{path}?league={league}"), None));
            assert_eq!(s, 200, "{path}");
            warm.push((path, t.elapsed()));
        }
        let t = Instant::now();
        let (s, ..) = rt.block_on(send(&router, "GET", &format!("/overview?league={league}"), None));
        assert_eq!(s, 200);
        let again = t.elapsed();
        eprintln!(
            "LATENCY {league}: cold first page {:.0} ms; warm overview {:.1} ms; first view of each page: {}",
            cold.as_secs_f64() * 1e3,
            again.as_secs_f64() * 1e3,
            warm.iter().map(|(p, d)| format!("{p} {:.0} ms", d.as_secs_f64() * 1e3)).collect::<Vec<_>>().join(", ")
        );
    }
}

// --------------------------------------------------------------------------- behaviour (synthetic)

/// A tiny league built from the espn snapshot is too heavy for unit tests; these use a loader
/// over the snapshot when available, else a minimal context.
struct CountingLoader {
    page: Mutex<Option<Page>>,
    calls: AtomicUsize,
    fail: Option<&'static str>,
}

impl Loader for CountingLoader {
    fn load(&self, _league: &str, _s: &Settings, _now: &PyDateTime) -> anyhow::Result<Page> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.fail {
            Some("provider") => Err(fm_core::ProviderError::Message("ESPN_LEAGUE_ID is not set".into()).into()),
            Some(m) => Err(anyhow::anyhow!("{m}")),
            None => Ok(self.page.lock().unwrap().clone().expect("page")),
        }
    }
}

fn minimal_page(data_dir: &Path) -> Page {
    let ctx: fm_core::models::LeagueContext = serde_json::from_value(json!({
        "provider": "espn", "league_id": "1", "name": "Test League", "season": 2027,
        "scoring": {"kind": "points", "weights": {"G": 3.0, "A": 2.0}},
        "roster_shape": {"C": 1, "BN": 1},
        "teams": [{"team_id": "1", "name": "Mine", "owner_is_me": true, "slots": [
            {"slot": "C", "starting": true, "player": {"cid": "espn:1", "name": "Sidney <Crosby>", "name_norm": "sidney crosby",
             "ids": {}, "team": "PIT", "positions": ["C"], "status": "ir"}},
            {"slot": "BN", "starting": false, "player": null}]},
                  {"team_id": "2", "name": "Theirs", "owner_is_me": false, "slots": []}],
        "free_agents": [], "as_of": "2026-11-04"
    }))
    .unwrap();
    let now = PyDateTime::parse("2026-11-04T17:00:00+00:00").unwrap();
    let rec: Recommendation = serde_json::from_value(json!({
        "kind": "injury", "score": 5.0, "title": "Move Sidney <Crosby> to IR", "add": [], "drop": [],
        "subjects": [ctx.teams[0].slots[0].player], "reasons": [{"code": "INJURY", "text": "on IR", "data": {}}]
    }))
    .unwrap();
    let res = LoadResult::new(ctx, Default::default(), None, vec![rec], now, chrono::NaiveDate::from_ymd_opt(2026, 11, 4).unwrap(), data_dir.into());
    Page { res: Arc::new(res), provider: None }
}

fn test_app(dir: &Path, fail: Option<&'static str>) -> (Arc<App>, Arc<CountingLoader>) {
    let page = if fail.is_none() { Some(minimal_page(dir)) } else { None };
    let loader = Arc::new(CountingLoader { page: Mutex::new(page), calls: AtomicUsize::new(0), fail });
    let mut cfg = AppConfig::new(Settings { fm_data_dir: dir.into(), fm_offline: true, ..Settings::default() });
    cfg.llm = Some(Arc::new(|_| None));
    cfg.clock = Arc::new(|| PyDateTime::parse("2026-11-04T17:00:00+00:00").unwrap());
    let l2: Arc<dyn Loader> = loader.clone();
    (App::new(l2, cfg), loader)
}

#[test]
fn healthz_does_not_load_and_cache_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let (a, loader) = test_app(dir.path(), None);
    let r = a.router();
    let rt = rt();
    let (s, ct, _, body) = rt.block_on(send(&r, "GET", "/healthz", None));
    assert_eq!((s, ct.as_str()), (200, "application/json"));
    assert_eq!(body, r#"{"status":"ok","cached":{"espn":false,"fantrax":false}}"#);
    assert_eq!(loader.calls.load(Ordering::SeqCst), 0);
    let (s, _, _, body) = rt.block_on(send(&r, "GET", "/overview?league=espn", None));
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("Sidney &lt;Crosby&gt;"), "names are escaped");
    rt.block_on(send(&r, "GET", "/roster?league=espn", None));
    assert_eq!(loader.calls.load(Ordering::SeqCst), 1, "cached within the TTL");
    let (s, _, loc, _) = rt.block_on(send(&r, "POST", "/refresh?league=espn&next=/roster", None));
    assert_eq!((s, loc.as_str()), (303, "/roster?league=espn"));
    rt.block_on(send(&r, "GET", "/overview?league=espn", None));
    assert_eq!(loader.calls.load(Ordering::SeqCst), 2, "refresh drops the cache");
}

#[test]
fn errors_404_405_422_setup_and_friendly() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = test_app(dir.path(), None);
    let r = a.router();
    let rt = rt();
    let (s, _, _, b) = rt.block_on(send(&r, "GET", "/overview?league=yahoo", None));
    assert_eq!(s, 422);
    assert!(b.contains("string_pattern_mismatch"));
    assert_eq!(rt.block_on(send(&r, "GET", "/nope", None)).0, 404);
    assert_eq!(rt.block_on(send(&r, "GET", "/refresh", None)).0, 405);
    let (s, ct, _, css) = rt.block_on(send(&r, "GET", "/static/style.css", None));
    assert_eq!(s, 200);
    assert!(ct.starts_with("text/css") && !css.is_empty(), "{ct}");
    let (s, _, _, b) = rt.block_on(send(&r, "GET", "/player/espn%3A999?league=espn", None));
    assert_eq!(s, 404);
    assert!(b.contains("No player with id &#39;espn:999&#39; in this league."), "{b}");

    let (a, _) = test_app(dir.path(), Some("provider"));
    let (s, _, _, b) = rt.block_on(send(&a.router(), "GET", "/overview?league=espn", None));
    assert_eq!(s, 200);
    assert!(b.contains("ESPN_LEAGUE_ID is not set") && b.contains("ESPN_LEAGUE_ID"), "setup page");
    let (s, _, _, b) = rt.block_on(send(&a.router(), "GET", "/api/recs.json?league=espn", None));
    assert_eq!(s, 503);
    assert!(b.contains("\"setup\""));
    let (a, _) = test_app(dir.path(), Some("boom"));
    let (s, _, _, b) = rt.block_on(send(&a.router(), "GET", "/roster?league=espn", None));
    assert_eq!(s, 500);
    assert!(b.contains("Error: boom"));
}

#[test]
fn post_mode_sets_pref_and_redirects_same_site_only() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = test_app(dir.path(), None);
    let r = a.router();
    let rt = rt();
    for (nxt, want) in [
        ("//evil.example/x", "/?league=fantrax&msg=mode-contend"),
        ("https://evil.example/", "/?league=fantrax&msg=mode-contend"),
        ("/%5Cevil.example", "/?league=fantrax&msg=mode-contend"),
        ("/%09/evil.example", "/?league=fantrax&msg=mode-contend"),
        ("/news?msg=old", "/news?league=fantrax&msg=mode-contend"),
        ("", "/?league=fantrax&msg=mode-contend"),
    ] {
        let body = format!("mode=contend&league=fantrax&next={}", nxt.replace('?', "%3F").replace('=', "%3D").replace('&', "%26"));
        let (s, _, loc, _) = rt.block_on(send(&r, "POST", "/mode", Some(&body)));
        assert_eq!((s, loc.as_str()), (303, want), "{nxt}");
    }
    let prefs = std::fs::read_to_string(dir.path().join("prefs.json")).unwrap();
    assert!(prefs.contains("contend"));
    assert_eq!(rt.block_on(send(&r, "POST", "/mode", Some("mode=wild&league=espn"))).0, 422);
    assert_eq!(rt.block_on(send(&r, "POST", "/mode", Some("mode=rebuild&league=yahoo"))).0, 422);
    let (_, _, _, b) = rt.block_on(send(&r, "GET", "/api/mode.json", None));
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["mode"], "contend");
    assert_eq!(v["source"], "prefs");
}

#[test]
fn explain_without_key_leaves_inline_note() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = test_app(dir.path(), None);
    let r = a.router();
    let rt = rt();
    let page = a.get_result("espn").unwrap();
    let key = app::rec_key("espn", &page.res.recs[0]);
    let (s, _, loc, _) = rt.block_on(send(&r, "POST", "/explain", Some(&format!("league=espn&rec_key={key}&next=/recommendations"))));
    assert_eq!(s, 303);
    assert_eq!(loc, format!("/recommendations?league=espn#rec-{key}"));
    let ex = page.res.explanations.lock().unwrap().get(&key).cloned().unwrap();
    assert!(ex["error"].as_str().unwrap().contains("OPENROUTER_API_KEY"));
    let (_, _, loc, _) = rt.block_on(send(&r, "POST", "/explain", Some("league=espn&rec_key=nope&next=/x")));
    assert_eq!(loc, "/x?league=espn");
}

/// Answers with what it was sent, so the page test can see the prompt.
struct EchoLlm;

impl fm_sources::llm::SupportsComplete for EchoLlm {
    fn available(&self) -> bool {
        true
    }
    fn model(&self) -> Option<String> {
        Some("stub-model".into())
    }
    fn complete(&self, system: &str, user: &str, _: fm_sources::llm::CompleteOpts) -> anyhow::Result<String> {
        if user.contains("QUESTION: fail") {
            return Err(fm_sources::llm::LLMError::new("Could not reach the LLM server at http://x. Is it running?").into());
        }
        Ok(format!(
            "<b>focus {}</b> paste {} history {}",
            user.contains("## PLAYERS THIS CHAT IS ABOUT") && user.contains("  owner: MY TEAM (slot C)"),
            system.contains("PASTED BY THE MANAGER") && user.contains("Makar for Tkachuk"),
            user.contains("MANAGER: Take it?\nASSISTANT: <b>focus true</b>")
        ))
    }
}

#[test]
fn ask_chats_answer_follow_up_and_are_managed() {
    let dir = tempfile::tempdir().unwrap();
    let rt = rt();
    // no model configured: a setup note, no form, and no Ask boxes on other pages
    let (a, _) = test_app(dir.path(), None);
    let (s, _, _, body) = rt.block_on(send(&a.router(), "GET", "/ask?league=espn", None));
    assert!(s == 200 && body.contains("FM_LLM_BASE_URL") && !body.contains("<textarea"), "{body}");
    assert!(!rt.block_on(send(&a.router(), "GET", "/recommendations?league=espn", None)).3.contains("ask-box"));

    let loader: Arc<dyn Loader> = Arc::new(CountingLoader { page: Mutex::new(Some(minimal_page(dir.path()))), calls: AtomicUsize::new(0), fail: None });
    let mut cfg = AppConfig::new(Settings { fm_data_dir: dir.path().into(), fm_offline: true, ..Settings::default() });
    cfg.llm = Some(Arc::new(|_| Some(Box::new(EchoLlm))));
    cfg.clock = Arc::new(|| PyDateTime::parse("2026-11-04T17:00:00+00:00").unwrap());
    let r = App::new(loader, cfg).router();
    let (s, _, _, body) = rt.block_on(send(&r, "GET", "/ask?league=espn", None));
    assert!(s == 200 && body.contains(r#"<form method="post" action="/ask""#) && body.contains("Nothing saved yet"), "{body}");
    assert!(body.contains(r#"<a href="/ask?league=espn" aria-current="page">Ask</a>"#));
    // the moves page hands a move over to a new chat
    let (_, _, _, body) = rt.block_on(send(&r, "GET", "/recommendations?league=espn", None));
    assert!(body.contains(r#"class="ask-box""#) && body.contains("Suggested move: Move Sidney &lt;Crosby&gt; to IR. Reasons: on IR"), "{body}");

    let chat_id = |loc: &str| loc.strip_prefix("/ask?league=espn&chat=").and_then(|x| x.strip_suffix("#latest")).unwrap_or_else(|| panic!("{loc}")).to_string();
    let stream = |id: &str| rt.block_on(send(&r, "GET", &format!("/ask/stream?league=espn&chat={id}"), None));

    // a question is saved as pending; the page then streams the answer. A failure leaves it
    // pending until it is dropped, which removes a chat that never got an answer.
    let (s, _, loc, _) = rt.block_on(send(&r, "POST", "/ask", Some("league=espn&question=fail")));
    assert_eq!(s, 303);
    let bad = chat_id(&loc);
    let (_, _, _, body) = rt.block_on(send(&r, "GET", &format!("/ask?league=espn&chat={bad}"), None));
    assert!(body.contains(&format!(r#"data-ask-stream="/ask/stream?league=espn&amp;chat={bad}""#)) && !body.contains("<textarea"), "{body}");
    let (s, ct, _, body) = stream(&bad);
    assert_eq!((s, ct.as_str()), (200, "text/event-stream"));
    assert_eq!(body, "event: fail\ndata: \"Could not reach the LLM server at http://x. Is it running?\"\n\n");
    assert_eq!(rt.block_on(send(&r, "POST", "/ask/discard", Some(&format!("league=espn&chat={bad}")))).0, 303);
    assert!(rt.block_on(send(&r, "GET", "/ask?league=espn", None)).3.contains("Nothing saved yet"));

    // a new chat about a player, with pasted text; then a follow-up in the same chat
    let (s, _, loc, _) = rt.block_on(send(&r, "POST", "/ask", Some("league=espn&question=Take+it%3F&context=Makar+for+Tkachuk&cids=espn:1,")));
    assert_eq!(s, 303);
    let id = chat_id(&loc);
    assert_eq!(stream(&id).3, "event: answer\ndata: \"<b>focus true</b> paste true history false\"\n\nevent: done\ndata: \"\"\n\n");
    assert_eq!(stream(&id).3, "event: done\ndata: \"\"\n\n", "nothing pending: no second run");
    let (s, _, loc2, _) = rt.block_on(send(&r, "POST", "/ask", Some(&format!("league=espn&chat={id}&question=And+next+week%3F&context=ignored"))));
    assert_eq!((s, loc2.as_str()), (303, loc.as_str()));
    assert!(stream(&id).3.contains("history true"));
    let (_, _, _, body) = rt.block_on(send(&r, "GET", &format!("/ask?league=espn&chat={id}"), None));
    assert!(!body.contains("data-ask-stream") && body.contains("<textarea"), "answered: the follow-up box is back: {body}");
    assert!(body.contains("&lt;b&gt;focus true&lt;/b&gt; paste true history false"), "first turn, escaped: {body}");
    assert!(body.contains("&lt;b&gt;focus true&lt;/b&gt; paste true history true"), "the follow-up saw the first turn: {body}");
    assert!(body.contains("Players: Sidney &lt;Crosby&gt;") && body.contains("<pre>Makar for Tkachuk</pre>") && body.contains("stub-model &middot; 2026-11-04"));
    assert!(body.contains(&format!(r#"chat={id}" aria-current="true">Take it?</a>"#)) && body.contains("2 questions"), "{body}");

    let (s, _, loc, _) = rt.block_on(send(&r, "POST", "/ask/rename", Some(&format!("league=espn&chat={id}&title=Makar+deal"))));
    assert_eq!((s, loc), (303, format!("/ask?league=espn&chat={id}")));
    assert!(rt.block_on(send(&r, "GET", "/ask?league=espn", None)).3.contains(">Makar deal</a>"));
    let (s, _, loc, _) = rt.block_on(send(&r, "POST", "/ask/delete", Some(&format!("league=espn&chat={id}"))));
    assert_eq!((s, loc.as_str()), (303, "/ask?league=espn"));
    assert!(rt.block_on(send(&r, "GET", &format!("/ask?league=espn&chat={id}"), None)).3.contains("Nothing saved yet"));

    assert_eq!(rt.block_on(send(&r, "POST", "/ask", Some("league=yahoo&question=x"))).0, 422);
    assert_eq!(rt.block_on(send(&r, "POST", "/ask/delete", Some("league=espn"))).0, 422);
    assert_eq!(rt.block_on(send(&r, "GET", "/ask/delete", None)).0, 405);
}

#[test]
fn moved_redirects_to_plan_with_banner_params() {
    let dir = tempfile::tempdir().unwrap();
    let (a, loader) = test_app(dir.path(), None);
    let r = a.router();
    let rt = rt();
    rt.block_on(send(&r, "GET", "/roster?league=espn", None));
    let (s, _, loc, _) = rt.block_on(send(&r, "POST", "/moved", Some("league=espn&since=2026-11-04T10:00:00&label=Add%20X&retry=1")));
    assert_eq!(s, 303);
    assert_eq!(loc, "/plan?league=espn&msg=moved&since=2026-11-04T10%3A00%3A00&label=Add+X&retry=1");
    rt.block_on(send(&r, "GET", "/roster?league=espn", None));
    assert_eq!(loader.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn trades_page_modes_shop_and_get() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = test_app(dir.path(), None);
    let r = a.router();
    let rt = rt();
    let (s, _, _, body) = rt.block_on(send(&r, "GET", "/trade-eval?league=espn", None));
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("<h1>Trades</h1>") && body.contains("Evaluate an offer") && body.contains(r#"name="give""#));
    let (s, _, _, body) = rt.block_on(send(&r, "GET", "/trade-eval?league=espn&mode=shop", None));
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("Pick one of your players to see what teams would give") && body.contains(r#"value="espn:1""#));
    let (s, _, _, body) = rt.block_on(send(&r, "GET", "/trade-eval?league=espn&mode=shop&cid=espn:1", None));
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("What teams would give for") && body.contains("No team would give a fair package"), "{body}");
    assert!(body.contains(r#"href="/player/espn:1?league=espn""#));
    let (s, _, _, body) = rt.block_on(send(&r, "GET", "/trade-eval?league=espn&mode=get&cid=espn:1", None));
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("not on another team"), "{body}");
    assert!(body.contains(r#"<optgroup label="Theirs">"#));
}

#[test]
fn rollback_requires_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = test_app(dir.path(), None);
    let r = a.router();
    let rt = rt();
    let (s, _, loc, _) = rt.block_on(send(&r, "POST", "/params/rollback", Some("league=espn&version=v0001")));
    assert_eq!((s, loc.as_str()), (303, "/health?league=espn&msg=rollback-unconfirmed"));
    let (_, _, loc, _) = rt.block_on(send(&r, "POST", "/params/rollback", Some("league=espn&version=bad&confirm=1")));
    assert_eq!(loc, "/health?league=espn&msg=rollback-unknown");
    let (_, _, loc, _) = rt.block_on(send(&r, "POST", "/params/rollback", Some("league=espn&confirm=on")));
    assert_eq!(loc, "/health?league=espn&msg=rollback-none");
    let (s, _, _, b) = rt.block_on(send(&r, "GET", "/health?league=espn&msg=rollback-none", None));
    assert_eq!(s, 200, "{b}");
    assert!(b.contains("Nothing to roll back: the packaged parameters are active."));
}

#[test]
fn cache_ttl_expires_and_one_load_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let loader = Arc::new(CountingLoader { page: Mutex::new(Some(minimal_page(dir.path()))), calls: AtomicUsize::new(0), fail: None });
    let s = Settings { fm_data_dir: dir.path().into(), ..Settings::default() };
    let now = PyDateTime::parse("2026-11-04T17:00:00+00:00").unwrap();
    let l2: Arc<dyn Loader> = loader.clone();
    let cache = Arc::new(app::ResultCache::new(l2, Duration::from_millis(150)));
    let hs: Vec<_> = (0..6)
        .map(|_| {
            let (c, s, n) = (cache.clone(), s.clone(), now.clone());
            std::thread::spawn(move || c.get("espn", &s, &n).unwrap())
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(loader.calls.load(Ordering::SeqCst), 1, "concurrent requests share one load");
    std::thread::sleep(Duration::from_millis(200));
    cache.get("espn", &s, &now).unwrap();
    assert_eq!(loader.calls.load(Ordering::SeqCst), 2, "expired");
    cache.refresh_ahead("espn", &s, &now).unwrap();
    assert_eq!(loader.calls.load(Ordering::SeqCst), 3);
    assert!(cache.age("espn").unwrap() < 0.1);
}

#[test]
fn wording_helpers() {
    assert_eq!(app::safe_next(Some("/roster"), "espn", &[]), "/roster?league=espn");
    assert_eq!(app::safe_next(Some("/roster?league=fantrax"), "espn", &[]), "/roster?league=fantrax");
    assert_eq!(app::safe_next(Some("//x.example"), "espn", &[]), "/?league=espn");
    assert_eq!(app::safe_next(None, "espn", &[("msg", "mode-balanced".into())]), "/?league=espn&msg=mode-balanced");
    assert_eq!(app::last_name("Martin St. Louis Jr."), "Louis");
    assert_eq!(app::ago_text(30.0), "just now");
    assert_eq!(app::ago_text(150.0), "2 min ago");
    assert_eq!(app::ago_text(7300.0), "2 h ago");

    let p = |cid: &str, name: &str, status: &str| -> fm_core::models::Player {
        serde_json::from_value(json!({"cid": cid, "name": name, "name_norm": name.to_lowercase(), "ids": {}, "team": "TOR",
                                      "positions": ["C"], "status": status}))
        .unwrap()
    };
    let rec = |kind: &str, title: &str, add: Vec<fm_core::models::Player>, drop: Vec<fm_core::models::Player>| -> Recommendation {
        serde_json::from_value(json!({"kind": kind, "score": 1.0, "title": title, "add": add, "drop": drop, "reasons": []})).unwrap()
    };
    assert_eq!(app::action_line(&rec("injury", "Move Seth Jarvis to IR and add Eeli Tolvanen", vec![], vec![])), "Move Seth Jarvis to IR, add Eeli Tolvanen");
    let (one, tk) = (p("f1", "Free Agent One", "healthy"), p("b", "Matthew Tkachuk", "healthy"));
    assert_eq!(
        app::action_line(&rec("lineup", "Start X over Y for this week's lineup (locks Monday)", vec![one.clone()], vec![tk.clone()])),
        "Start One over Tkachuk"
    );
    let (mc, ma, st) = (p("x", "Connor McDavid", "healthy"), p("y", "Cale Makar", "healthy"), p("a", "Tim Stützle", "healthy"));
    assert_eq!(app::action_line(&rec("trade", "t", vec![mc, ma], vec![st])), "Propose: Stützle for McDavid + Makar");
    assert_eq!(
        app::action_note(&rec("lineup", "Start X over Y for this week's lineup (locks Monday)", vec![], vec![])).unwrap(),
        "For this week's lineup: Fantrax locks it on Monday."
    );

    // overview lede
    assert_eq!(app::overview_summary(&[], &[], 0), "No moves to make right now. Nobody on your roster is hurt.");
    let hy = p("hy", "Zach Hyman", "out");
    let he = p("he", "Connor Hellebuyck", "healthy");
    let ek = p("ek", "Aaron Ekblad", "out");
    let ma2 = p("ma", "Brad Marchand", "dtd");
    let bench = rec("lineup", "Bench Aaron Ekblad: out but in a starting slot (D)", vec![], vec![ek.clone()]);
    let out = app::overview_summary(&[bench], &[("IR".into(), &hy), ("IR".into(), &he), ("D".into(), &ek), ("F".into(), &ma2)], 0);
    assert!(out.contains("One player needs attention: <b>Aaron Ekblad</b> is out."), "{out}");
    assert!(out.ends_with("Hurt, nothing to do right now: Zach Hyman (IR slot), Connor Hellebuyck (IR slot), and Brad Marchand (day-to-day)."), "{out}");
    let a = p("q", "A<b>", "dtd");
    let out = app::overview_summary(&[], &[("C".into(), &a)], 2);
    assert!(out.contains("Hurt, nothing to do right now: A&lt;b&gt; (day-to-day).") && out.ends_with("You carry two prospects."), "{out}");
}

