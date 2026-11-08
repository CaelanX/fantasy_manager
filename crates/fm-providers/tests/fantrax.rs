//! W3-fantrax tests: parity against `parity/fixtures/fantrax` (tape replay) and the ported
//! intent of `tests/test_fantrax.py` / `tests/test_fantrax_auth.py`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::NaiveDate;
use serde_json::{json, Value};

use fm_core::config::{parse_points, Secret, Settings};
use fm_core::provider::LeagueProvider;
use fm_core::snapshot::ProviderExtras;
use fm_core::{Map, ProviderError};
use fm_http::Tape;
use fm_providers::fantrax::*;
use fm_providers::fantrax_auth::{self as fa, FantraxAuth, SESSION_FILE};

// ------------------------------------------------------------------ comparison helper

fn compare(path: &str, exp: &Value, act: &Value, out: &mut Vec<String>) {
    if out.len() > 40 {
        return;
    }
    match (exp, act) {
        (Value::Number(a), Value::Number(b)) => {
            let (x, y) = (a.as_f64().unwrap_or(f64::NAN), b.as_f64().unwrap_or(f64::NAN));
            let tol = 1e-9_f64.max(1e-9 * x.abs().max(y.abs()));
            if !((x - y).abs() <= tol) {
                out.push(format!("{path}: expected {x} got {y}"));
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            let ka: Vec<&String> = a.keys().collect();
            let kb: Vec<&String> = b.keys().collect();
            if ka != kb {
                let missing: Vec<_> = ka.iter().filter(|k| !b.contains_key(k.as_str())).take(5).collect();
                let extra: Vec<_> = kb.iter().filter(|k| !a.contains_key(k.as_str())).take(5).collect();
                out.push(format!("{path}: keys differ (missing {missing:?}, extra {extra:?}, or order)"));
            }
            for (k, v) in a {
                if let Some(w) = b.get(k) {
                    compare(&format!("{path}.{k}"), v, w, out);
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                out.push(format!("{path}: length {} vs {}", a.len(), b.len()));
            }
            for (i, (v, w)) in a.iter().zip(b).enumerate() {
                compare(&format!("{path}[{i}]"), v, w, out);
            }
        }
        _ => {
            if exp != act {
                out.push(format!("{path}: expected {} got {}", short(exp), short(act)));
            }
        }
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 200 {
        format!("{}...", &s[..200])
    } else {
        s
    }
}

fn assert_same(name: &str, exp: &Value, act: &Value) {
    let mut diffs = Vec::new();
    compare(name, exp, act, &mut diffs);
    assert!(diffs.is_empty(), "{name}: {} differences:\n{}", diffs.len(), diffs.join("\n"));
}

// ------------------------------------------------------------------ parity (tape replay)

fn fixtures_root() -> PathBuf {
    if let Ok(p) = std::env::var("FM_PARITY_FIXTURES") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures")
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).expect("json")
}

#[test]
fn parity_raw_ctx_from_tape() {
    let dir = fixtures_root().join("fantrax");
    let tape_path = dir.join("tape.json.gz");
    if !tape_path.is_file() {
        eprintln!("SKIP parity_raw_ctx_from_tape: {} not found (set FM_PARITY_FIXTURES)", tape_path.display());
        return;
    }
    let raw: Value = read_json(&dir.join("raw_ctx.json"));
    let snap: Value = read_json(&dir.join("snapshot.json"));
    let today = NaiveDate::parse_from_str(snap["today"].as_str().expect("today"), "%Y-%m-%d").expect("date");
    let tape = Tape::load(&tape_path).expect("tape");

    let tmp = tempfile::tempdir().expect("tmp");
    let mut s = Settings::default();
    s.fantrax_league_id = Some(raw["league_id"].as_str().expect("league_id").to_string());
    s.fm_data_dir = tmp.path().to_path_buf();
    s.fm_offline = true;
    s.fantrax_keepers = snap["env"]["FANTRAX_KEEPERS"].as_str().and_then(|v| v.parse().ok());
    // FANTRAX_POINTS as configured for the dump (config_mae == weights_mae: the league's rules)
    let w: Map<String, f64> = serde_json::from_value(raw["scoring"]["weights"].clone()).expect("weights");
    let g: Map<String, f64> = serde_json::from_value(raw["scoring"]["goalie_weights"].clone()).expect("goalie");
    s.fantrax_points = parse_points(&fm_core::config::format_points(&w, Some(&g))).expect("points");

    let mut prov = FantraxProvider::from_tape(s, tape, today);
    let t0 = std::time::Instant::now();
    let mut ctx = prov.load().expect("load");
    eprintln!("fantrax load from tape: {:?}", t0.elapsed());
    ctx.warnings.extend(prov.extras().warnings.iter().cloned()); // cli._load_context
    let mut act = serde_json::to_value(&ctx).expect("ctx json");
    let mut exp = raw.clone();
    for v in [&mut act, &mut exp] {
        let o = v.as_object_mut().expect("obj");
        o.remove("dynasty_mode");
        o.remove("dynasty_mode_source");
    }
    assert_same("raw_ctx", &exp, &act);

    let exp_extras: ProviderExtras = serde_json::from_value(snap["provider"].clone()).expect("extras");
    assert_same(
        "provider",
        &serde_json::to_value(&exp_extras).expect("json"),
        &serde_json::to_value(prov.extras()).expect("json"),
    );
}

#[test]
fn parity_rules_page_from_tape() {
    // the Rules page HTML parser on the real cached page, compared field by field
    let dir = fixtures_root().join("fantrax");
    let tape_path = dir.join("tape.json.gz");
    if !tape_path.is_file() {
        eprintln!("SKIP parity_rules_page_from_tape: tape not found");
        return;
    }
    let tape = Tape::load(&tape_path).expect("tape");
    let snap: Value = read_json(&dir.join("snapshot.json"));
    let rec = tape
        .0
        .fxpa
        .values()
        .find(|r| r.msgs.len() == 1 && r.msgs[0]["method"] == "getLeagueRulesOld")
        .expect("rules page on tape");
    let content = rec.body["responses"][0]["data"]["content"].as_str().expect("content");
    let rules = parse_league_rules(content);
    assert_same("rules", &snap["provider"]["rules"], &serde_json::to_value(&rules).expect("json"));
    assert_eq!(rules_max_roster(Some(&rules)), Some(16));
    assert_eq!(parse_lineup_lock(rules.get(LINEUP_RULE)), "daily");
    assert_eq!(rules_claim_limits(Some(&rules)).1, Some(3));
}

#[test]
fn parity_raw_ctx_from_fantrax_cache_dir() {
    // offline replay from the shared fxpa file cache (`<data_dir>/fantrax_cache`), like the dump
    let root = fixtures_root();
    let cache_src = root.join("_datadir").join("fantrax_cache");
    if !cache_src.is_dir() {
        eprintln!("SKIP parity_raw_ctx_from_fantrax_cache_dir: {} not found", cache_src.display());
        return;
    }
    let dir = root.join("fantrax");
    let raw: Value = read_json(&dir.join("raw_ctx.json"));
    let snap: Value = read_json(&dir.join("snapshot.json"));
    let today = NaiveDate::parse_from_str(snap["today"].as_str().expect("today"), "%Y-%m-%d").expect("date");
    let tmp = tempfile::tempdir().expect("tmp");
    let dst = tmp.path().join("fantrax_cache");
    std::fs::create_dir_all(&dst).expect("mkdir");
    for e in std::fs::read_dir(&cache_src).expect("read_dir").flatten() {
        std::fs::copy(e.path(), dst.join(e.file_name())).expect("copy");
    }
    let mut s = Settings::default();
    s.fantrax_league_id = Some(raw["league_id"].as_str().expect("league_id").to_string());
    s.fm_data_dir = tmp.path().to_path_buf();
    s.fm_offline = true;
    s.fantrax_keepers = Some(3);
    let w: Map<String, f64> = serde_json::from_value(raw["scoring"]["weights"].clone()).expect("weights");
    let g: Map<String, f64> = serde_json::from_value(raw["scoring"]["goalie_weights"].clone()).expect("goalie");
    s.fantrax_points = parse_points(&fm_core::config::format_points(&w, Some(&g))).expect("points");
    let mut prov = FantraxProvider::new(s, true).with_today(today);
    let ctx = prov.load().expect("load");
    assert!(prov.extras().warnings.is_empty(), "{:?}", prov.extras().warnings);
    let mut act = serde_json::to_value(&ctx).expect("ctx json");
    let mut exp = raw.clone();
    for v in [&mut act, &mut exp] {
        let o = v.as_object_mut().expect("obj");
        o.remove("dynasty_mode");
        o.remove("dynasty_mode_source");
    }
    assert_same("raw_ctx(cache)", &exp, &act);
    assert_eq!(prov.requests_made(), 0);
}

// ------------------------------------------------------------------ fake fxpa transport

const POINTS: &str = "G=3,A=2,SOG=0.4,HIT=0.3,BLK=0.5,PPP=0.5,W=4,SV=0.2,GA=-1,SO=3";
const PROJ: &str = "PROJECTION_0_31n_SEASON";
const YTD: &str = "SEASON_31n_YEAR_TO_DATE";
const USER: &str = "user-sentinel-8841";
const PASSWORD: &str = "pw-sentinel-5512";

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 9, 28).expect("date")
}

fn fx(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fantrax").join(format!("{name}.json"));
    read_json(&p)
}

fn positions() -> Map<String, String> {
    fx("league_info")["positionMap"]
        .as_object()
        .expect("map")
        .iter()
        .map(|(k, v)| (k.clone(), v["shortName"].as_str().expect("s").to_string()))
        .collect()
}

fn live_positions() -> Map<String, String> {
    [("207", "F"), ("202", "D"), ("201", "G")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

fn rule_weights() -> Map<String, f64> {
    [
        ("A", 2.0),
        ("BLK", 0.5),
        ("ENG", 2.0),
        ("G", 4.0),
        ("HIT", 0.3),
        ("FT", 3.0),
        ("PIM", 0.5),
        ("SHG", 2.0),
        ("SOG", 0.5),
        ("PPP", 1.0),
        ("GA", -1.0),
        ("SV", 0.25),
        ("SO", 7.5),
        ("W", 5.0),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), *v))
    .collect()
}

fn fmap(pairs: &[(&str, f64)]) -> Map<String, f64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

type Override = Arc<dyn Fn(&str, &Value) -> Option<Value> + Send + Sync>;

/// Every POST body's `msgs` (and login payloads), shared with the test.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<(String, Value)>>>);

impl Log {
    fn push(&self, kind: &str, v: Value) {
        self.0.lock().unwrap().push((kind.to_string(), v));
    }
    fn of(&self, kind: &str) -> Vec<Value> {
        self.0.lock().unwrap().iter().filter(|(k, _)| k == kind).map(|(_, v)| v.clone()).collect()
    }
    /// data dicts of every message with `method` in data POSTs
    fn data_of(&self, method: &str) -> Vec<Value> {
        self.of("post")
            .iter()
            .flat_map(|msgs| msgs.as_array().cloned().unwrap_or_default())
            .filter(|m| m["method"] == method)
            .map(|m| m["data"].clone())
            .collect()
    }
}

#[derive(Clone)]
struct LoginCfg {
    data: Value,
    errors: Option<Value>,
    body: Option<Value>,
}

#[derive(Clone, Default)]
struct Fake {
    log: Log,
    overrides: Option<Override>,
    page_errors: HashMap<String, Value>,
    raise_for: HashSet<String>,
    live: bool,
    html: bool,
    boom: bool,
    login: Option<LoginCfg>,
    headers: Vec<(String, String)>,
    jar: Vec<Cookie>,
}

const NOT_LOGGED_IN: &str = "WARNING_NOT_LOGGED_IN";

impl Fake {
    fn new() -> Self {
        Fake::default()
    }
    fn live() -> Self {
        Fake { live: true, ..Fake::default() }
    }
    fn page_error(mut self, method: &str, code: &str, title: Option<&str>) -> Self {
        let mut e = json!({"code": code});
        if let Some(t) = title {
            e["title"] = json!(t);
        }
        self.page_errors.insert(method.to_string(), e);
        self
    }
    fn not_logged_in(self, methods: &[&str]) -> Self {
        methods.iter().fold(self, |s, m| s.page_error(m, NOT_LOGGED_IN, None))
    }
    fn with_override(mut self, f: impl Fn(&str, &Value) -> Option<Value> + Send + Sync + 'static) -> Self {
        self.overrides = Some(Arc::new(f));
        self
    }
    fn login_session(log: Log, cfg: LoginCfg, still_logged_out: bool) -> Self {
        let mut f = Fake { log, login: Some(cfg), ..Fake::default() };
        if still_logged_out {
            f = f.not_logged_in(&["getFantasyLeagueInfo", "getTeamRosterInfo"]);
        }
        f
    }

    fn data_for(&self, method: &str, data: &Value) -> Value {
        if let Some(o) = &self.overrides {
            if let Some(v) = o(method, data) {
                return v;
            }
        }
        if self.live {
            if let Some(v) = self.live_data_for(method, data) {
                return v;
            }
        }
        match method {
            "getFantasyLeagueInfo" => fx("league_info"),
            "getTeamRosterInfo" => fx(&format!("roster_{}", data["teamId"].as_str().unwrap_or("t1"))),
            "getLeagueRulesOld" => json!({"newJoin": false, "content": ""}),
            "getStandings" => fx("standings"),
            "getTradeBlocks" => fx("trade_blocks"),
            "getPendingTransactions" => fx("pending_trades"),
            "getTransactionDetailsHistory" => fx("transactions"),
            "getPlayerStats" => fx("player_pool"),
            other => panic!("unexpected method {other}"),
        }
    }

    fn live_data_for(&self, method: &str, data: &Value) -> Option<Value> {
        let tf = data["seasonOrProjection"].as_str();
        match method {
            "getFantasyLeagueInfo" => {
                let mut info = fx("league_info");
                info["positionMap"] = json!({"207": {"id": "207", "shortName": "F"}, "202": {"id": "202", "shortName": "D"},
                                             "201": {"id": "201", "shortName": "G"}});
                Some(info)
            }
            "getTeamRosterInfo" => {
                let name = match tf {
                    None => "roster_period",
                    Some(PROJ) => "roster_projected",
                    Some(YTD) => "roster_ytd",
                    Some(other) => panic!("unexpected timeframe {other}"),
                };
                let mut d = fx(name);
                if data["teamId"] == "t2" {
                    for t in d["tables"].as_array_mut().expect("tables") {
                        let rows: Vec<Value> =
                            t["rows"].as_array().expect("rows").iter().filter(|r| r.get("scorer").is_none()).cloned().collect();
                        t["rows"] = Value::Array(rows);
                    }
                }
                Some(d)
            }
            "getPlayerStats" => {
                if data["statusOrTeamFilter"] == "ALL_TAKEN" {
                    return Some(fx("pool_taken"));
                }
                let goalies = data["positionOrGroup"] == "POS_201";
                if tf == Some(PROJ) {
                    return Some(fx(if goalies { "pool_goalies_projected" } else { "pool_skaters_projected" }));
                }
                let mut empty = fx("pool_goalies_projected");
                empty["statsTable"] = json!([]);
                empty["displayedSelections"]["displayedSeasonOrProjection"] =
                    json!({"code": YTD, "timeframeTypeCode": "YEAR_TO_DATE"});
                Some(empty)
            }
            "getLeagueRulesOld" => Some(fx("league_rules")),
            _ => None,
        }
    }
}

impl FxSession for Fake {
    fn post(&mut self, url: &str, query: &[(String, String)], payload: &Value) -> Result<FxResponse, RequestError> {
        assert_eq!(url, FXPA_URL);
        if query == [("lgnu".to_string(), "1".to_string())] {
            let cfg = self.login.clone().expect("login session");
            self.log.push("login", payload.clone());
            if let Some(b) = cfg.body {
                return Ok(FxResponse { status: 200, body: Ok(b) });
            }
            let mut resp = json!({"data": cfg.data.clone()});
            if let Some(e) = cfg.errors {
                resp["errors"] = e;
            }
            if cfg.data.get("userInfo").is_some() {
                self.set_cookie(Cookie {
                    name: "FX_RM".into(),
                    value: "fresh-remember-me".into(),
                    domain: ".fantrax.com".into(),
                    path: "/".into(),
                    expires: None,
                });
            }
            return Ok(FxResponse { status: 200, body: Ok(json!({"responses": [resp]})) });
        }
        assert_eq!(query, [("leagueId".to_string(), "lg1".to_string())]);
        if self.boom {
            panic!("network used");
        }
        if self.html {
            return Ok(FxResponse { status: 200, body: Err("html".into()) });
        }
        let msgs = payload["msgs"].as_array().expect("msgs").clone();
        self.log.push("post", Value::Array(msgs.clone()));
        for m in &msgs {
            assert!(m["data"].as_object().expect("data").values().all(|v| v.is_string()), "non-string data {m}");
            let method = m["method"].as_str().expect("method");
            if self.raise_for.contains(method) {
                return Err(RequestError { kind: "ConnectionError".into(), message: "down".into() });
            }
            if let Some(pe) = self.page_errors.get(method) {
                return Ok(FxResponse { status: 200, body: Ok(json!({"pageError": pe})) });
            }
        }
        let responses: Vec<Value> =
            msgs.iter().map(|m| json!({"data": self.data_for(m["method"].as_str().expect("m"), &m["data"])})).collect();
        Ok(FxResponse { status: 200, body: Ok(json!({"responses": responses})) })
    }
    fn get(&mut self, url: &str) -> Result<FxResponse, RequestError> {
        self.log.push("get", json!(url));
        Ok(FxResponse { status: 200, body: Ok(json!({})) })
    }
    fn set_header(&mut self, name: &str, value: &str) {
        self.headers.push((name.to_string(), value.to_string()));
    }
    fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }
    fn cookies(&self) -> Vec<Cookie> {
        self.jar.clone()
    }
    fn set_cookie(&mut self, cookie: Cookie) {
        self.jar.retain(|c| c.name != cookie.name);
        self.jar.push(cookie);
    }
}

fn settings(dir: &Path) -> Settings {
    let mut s = Settings::default();
    s.fantrax_league_id = Some("lg1".into());
    s.fantrax_cookie = Some(Secret::new("FX_RM=secret; JSESSIONID=abc"));
    s.fantrax_points = parse_points(POINTS).expect("points");
    s.fantrax_dynasty = true;
    s.fm_data_dir = dir.to_path_buf();
    s
}

fn provider_with(s: Settings, session: Fake) -> FantraxProvider {
    FantraxProvider::new(s, false).with_session(Box::new(session)).with_today(today())
}

fn err_text(e: &anyhow::Error) -> String {
    e.to_string()
}

fn is_provider_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ProviderError>().is_some()
}

// ------------------------------------------------------------------ pure helpers

#[test]
fn cookie_header_and_text() {
    let h = parse_cookie_header("Cookie: FX_RM=a=b; JSESSIONID=xyz ; empty=");
    assert_eq!(h.into_iter().collect::<Vec<_>>(), vec![
        ("FX_RM".to_string(), "a=b".to_string()),
        ("JSESSIONID".into(), "xyz".into()),
        ("empty".into(), "".into())
    ]);
    assert_eq!(parse_cookie_header("a=\"quoted\"")["a"], "quoted");
    let txt = "# Netscape HTTP Cookie File\n.fantrax.com\tTRUE\t/\tTRUE\t1893456000\tFX_RM\tremember\n\
               #HttpOnly_www.fantrax.com\tFALSE\t/\tTRUE\t0\tJSESSIONID\tsess\n.espn.com\tTRUE\t/\tFALSE\t0\tSWID\t{nope}\n";
    let got = parse_cookie_text(txt);
    assert_eq!(got.len(), 2);
    assert_eq!((got["FX_RM"].as_str(), got["JSESSIONID"].as_str()), ("remember", "sess"));
    let got = parse_cookie_text("FX_RM=x; b=y\n");
    assert_eq!((got["FX_RM"].as_str(), got["b"].as_str()), ("x", "y"));
    let js = r#"[{"name": "FX_RM", "value": "v", "domain": ".fantrax.com"}, {"name": "other", "value": "o", "domain": ".example.com"}]"#;
    let got = parse_cookie_text(js);
    assert_eq!(got.len(), 1);
    assert_eq!(got["FX_RM"], "v");
}

#[test]
fn load_cookies_from_file() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("cookies.txt");
    std::fs::write(&f, ".fantrax.com\tTRUE\t/\tTRUE\t0\tFX_RM\tfromfile\n").unwrap();
    let mut s = settings(tmp.path());
    s.fantrax_cookie = None;
    s.fantrax_cookie_file = Some(f);
    assert_eq!(load_cookies(&s).unwrap()["FX_RM"], "fromfile");
    s.fantrax_cookie_file = Some(tmp.path().join("nope.txt"));
    let e = load_cookies(&s).unwrap_err();
    assert!(is_provider_error(&e) && err_text(&e).contains("does not exist"));
}

#[test]
fn canonical_slots() {
    for (raw, slot) in
        [("C", "C"), ("Util", "UTIL"), ("Skt", "UTIL"), ("Res", "BN"), ("IR", "IR"), ("Min", "MIN"), ("Taxi", "TAXI"), ("<b>D</b>", "D")]
    {
        assert_eq!(canonical_slot(raw), slot, "{raw}");
    }
}

#[test]
fn positions_and_status_helpers() {
    assert_eq!(player_positions("<b>C</b>,RW"), ["C", "RW", "F"]);
    assert_eq!(player_positions("D"), ["D"]);
    assert_eq!(player_positions("G"), ["G"]);
    assert_eq!(player_positions("C,D"), ["C", "F", "D"]);
    use fm_core::models::Status;
    let icons = json!([{"typeId": "1", "tooltip": "Day-to-Day"}, {"typeId": "2", "tooltip": "IR"}]);
    assert_eq!(status_from_icons(Some(&icons)), (Status::Ir, Some("IR".into())));
    assert_eq!(status_from_icons(Some(&json!([{"typeId": "30"}]))), (Status::Out, None));
    assert_eq!(status_from_icons(Some(&json!([{"typeId": "6", "tooltip": "Suspended"}]))), (Status::Suspended, Some("Suspended".into())));
    assert_eq!(status_from_icons(Some(&json!([{"typeId": "9", "tooltip": "news"}]))), (Status::Healthy, None));
}

#[test]
fn season_from_settings_cases() {
    assert_eq!(season_from_settings(&fx("league_info")["fantasySettings"], today()), 2027);
    assert_eq!(season_from_settings(&json!({"subtitle": "2026-27 Season"}), today()), 2027);
    assert_eq!(season_from_settings(&json!({}), today()), 2027);
}

#[test]
fn parse_roster_slots_statuses_and_stats() {
    let r = parse_roster(&fx("roster_t1"), &positions(), None, Some(today()));
    let by_slot: Vec<(String, Option<String>, bool)> =
        r.slots.iter().map(|s| (s.slot.clone(), s.player.as_ref().map(|p| p.name.clone()), s.starting)).collect();
    let want = [
        ("C", Some("Young Star"), true),
        ("LW", Some("Veteran Winger"), true),
        ("D", Some("Steady Dman"), true),
        ("UTIL", None, true),
        ("BN", Some("Bench Guy"), false),
        ("IR", Some("Hurt Center"), false),
        ("MIN", Some("Prospect Kid"), false),
        ("G", Some("Top Goalie"), true),
    ];
    assert_eq!(by_slot, want.iter().map(|(a, b, c)| (a.to_string(), b.map(str::to_string), *c)).collect::<Vec<_>>());
    let players: HashMap<String, _> = r.slots.iter().filter_map(|s| s.player.clone()).map(|p| (p.cid.clone(), p)).collect();
    let star = &players["fantrax:p1"];
    assert_eq!(star.ids["fantrax"], "p1");
    assert_eq!(star.team.as_deref(), Some("NJD"));
    assert_eq!(star.positions, ["C", "RW", "F"]);
    let season = &star.lines["season"];
    assert_eq!(season.gp, 10);
    assert_eq!((season.stats["G"], season.stats["PTS"], season.stats["PM"]), (5.0, 12.0, 3.0));
    assert_eq!((season.stats["HIT"], season.stats["BLK"], season.stats[FPTS_KEY]), (5.0, 3.0, 46.0));
    use fm_core::models::Status;
    assert_eq!(players["fantrax:p2"].status, Status::Dtd);
    assert_eq!(players["fantrax:p2"].status_note.as_deref(), Some("Lower Body - Day-to-Day"));
    assert_eq!(players["fantrax:p2"].team.as_deref(), Some("TBL"));
    assert_eq!(players["fantrax:p3"].team.as_deref(), Some("LAK"));
    assert_eq!(players["fantrax:p4"].team.as_deref(), Some("WSH"));
    let hurt = &players["fantrax:p5"];
    assert!(hurt.status == Status::Ir && hurt.team.as_deref() == Some("MTL") && !hurt.lines.contains_key("season"));
    assert!(!players["fantrax:p6"].lines.contains_key("season"));
    let goalie = &players["fantrax:p7"];
    assert!(goalie.team.as_deref() == Some("VGK") && goalie.is_goalie());
    let g = &goalie.lines["season"];
    assert!((g.stats["SVPCT"] - 0.927).abs() < 1e-9 && (g.stats["GAA"] - 2.2).abs() < 1e-9);
    assert!(!g.stats.contains_key("PTS") && (g.per_game()["SVPCT"] - 0.927).abs() < 1e-9);
    assert_eq!(r.fantasy_points["fantrax:p1"], (Some(46.0), Some(4.6)));
    assert_eq!(r.ages["fantrax:p6"], 19.0);
    assert_eq!(
        r.slot_counts.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(),
        [("C", 1), ("LW", 1), ("D", 1), ("UTIL", 1), ("G", 1)]
    );
    assert_eq!(r.limits["Reserve"], (1, 6));
    assert_eq!(r.limits["Minors"], (1, 5));
}

#[test]
fn parse_roster_derives_gp_from_fpts() {
    let r = parse_roster(&fx("roster_t2"), &positions(), None, Some(today()));
    let players: HashMap<String, _> = r.slots.iter().filter_map(|s| s.player.clone()).map(|p| (p.cid.clone(), p)).collect();
    let l = &players["fantrax:r1"].lines["season"];
    assert_eq!(l.gp, 10);
    assert_eq!(l.stats, fmap(&[("GP", 10.0), (FPTS_KEY, 50.0)]));
    assert!(!players["fantrax:r3"].lines.contains_key("season"));
    assert_eq!(players["fantrax:r3"].status, fm_core::models::Status::Suspended);
    assert_eq!(r.slots.len(), 3); // empty reserve row dropped
}

#[test]
fn parse_player_pool_skips_rostered_and_percent_columns() {
    let (players, rows, pages) = parse_player_pool(&fx("player_pool"), None, Some(today()));
    assert_eq!(pages, 1);
    assert_eq!(players.iter().map(|p| p.cid.as_str()).collect::<Vec<_>>(), ["fantrax:f1", "fantrax:f2", "fantrax:p4"]);
    let f1 = &players[0];
    assert_eq!(f1.pct_owned, Some(12.0));
    assert_eq!(rows[&f1.cid].age, Some(23.0));
    assert_eq!(f1.positions, ["C", "LW", "F"]);
    assert!(!f1.lines["season"].stats.contains_key("PM"));
    assert_eq!(f1.lines["season"].gp, 10);
}

fn synthetic_player(cid: &str, gp: i64, stats: &[(&str, f64)], fpts: Option<f64>, goalie: bool, split: &str) -> fm_core::Player {
    use fm_core::models::{Split, StatLine};
    let mut s: Map<String, f64> = stats.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    s.insert("GP".into(), gp as f64);
    if let Some(f) = fpts {
        s.insert(FPTS_KEY.into(), f);
    }
    let sp = if split == "season" { Split::Season } else { Split::Projected };
    let mut lines = Map::new();
    lines.insert(split.to_string(), StatLine { split: sp, gp, stats: s });
    let mut p: fm_core::Player = serde_json::from_value(json!({
        "cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": null,
        "positions": if goalie { vec!["G"] } else { vec!["C"] }
    }))
    .expect("player");
    p.lines = lines;
    p
}

#[test]
fn validate_weights_error_math() {
    let w = fmap(&[("G", 3.0), ("A", 2.0)]);
    let players = vec![
        synthetic_player("a", 10, &[("G", 5.0), ("A", 5.0)], Some(25.0), false, "season"),
        synthetic_player("b", 10, &[("G", 2.0), ("A", 0.0)], Some(8.0), false, "season"),
        synthetic_player("c", 0, &[], Some(0.0), false, "season"),
        synthetic_player("d", 5, &[("SOG", 10.0)], Some(4.0), false, "season"),
        synthetic_player("e", 5, &[("G", 1.0)], None, false, "season"),
    ];
    assert!((validate_weights(&w, players.iter()) - 0.1).abs() < 1e-12);
    assert!(validate_weights(&w, players[2..].iter()).is_nan());
}

// ------------------------------------------------------------------ provider end to end

#[test]
fn provider_load() {
    let tmp = tempfile::tempdir().unwrap();
    let mut prov = provider_with(settings(tmp.path()), Fake::new());
    let ctx = prov.load().unwrap();
    assert_eq!((ctx.provider.as_str(), ctx.league_id.as_str(), ctx.season), ("fantrax", "lg1", 2027));
    assert!(ctx.name == "Test Dynasty League" && ctx.dynasty && ctx.keeper_horizon_years == 3);
    assert_eq!(ctx.as_of, today());
    let me = ctx.my_team().unwrap();
    assert_eq!((me.team_id.as_str(), me.name.as_str(), me.record), ("t1", "Caelan's Crushers", Some((2, 2, 0))));
    assert_eq!(ctx.team("t2").unwrap().record, Some((3, 1, 0)));
    assert_eq!(
        ctx.roster_shape.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(),
        [("C", 1), ("LW", 1), ("D", 1), ("UTIL", 1), ("G", 1), ("BN", 6), ("IR", 3), ("MIN", 5)]
    );
    assert_eq!(ctx.scoring.weights["G"], 3.0);
    assert_eq!(ctx.free_agents.iter().map(|p| p.cid.as_str()).collect::<Vec<_>>(), ["fantrax:f1", "fantrax:f2"]);
    assert!(prov.weights_mae.abs() < 1e-9);
    assert_eq!(prov.fantasy_points["fantrax:f1"], (Some(30.0), Some(3.0)));
    assert_eq!(prov.ages["fantrax:r2"], 35.0);
    let tb = &prov.trade_blocks;
    assert_eq!(tb.len(), 1);
    assert_eq!((tb[0].team_name.as_str(), tb[0].note.as_str()), ("Rival Team", "Looking for D help"));
    assert_eq!(tb[0].players_offered, ["fantrax:r1"]);
    assert_eq!((tb[0].positions_wanted.clone(), tb[0].stats_wanted.clone()), (vec!["D".to_string()], vec!["Blk".to_string()]));
    assert_eq!(prov.pending_trades[0].moves[1]["pick"], json!({"year": 2027, "round": 1, "original_owner": "t1"}));
    assert_eq!(
        prov.transactions.iter().map(|t| (t.kind.as_str(), t.player_cid.as_deref())).collect::<Vec<_>>(),
        [("FA", Some("fantrax:p4")), ("DROP", Some("fantrax:x9"))]
    );
    let w = prov.extras().warnings.clone();
    assert!(w.len() == 1 && w[0].contains("free-agent list has no per-stat columns"), "{w:?}");
    assert_eq!(prov.weights_checked, 5);
}

#[test]
fn describe_settings_and_raw() {
    let tmp = tempfile::tempdir().unwrap();
    let mut prov = provider_with(settings(tmp.path()), Fake::new());
    let text = prov.describe_settings_load().unwrap().join("\n");
    for s in [
        "Test Dynasty League",
        "My team: Caelan's Crushers (t1)",
        "Minors 1/5",
        "mean abs error 0.000 FP/G vs Fantrax FPts over 5 players",
        "fantasySettings.rosterSettings.maxMinorsPlayers: 5",
        "fantasySettings.draftSettings.futureDraftPicksTradeable: True",
    ] {
        assert!(text.contains(s), "{s}\n{text}");
    }
    assert!(!text.contains("secret"));
    let raw = prov.raw_settings_load().unwrap();
    assert_eq!(raw["roster_limits"]["Inj Res"], json!({"total": 1, "max": 3}));
    assert_eq!(raw["rules"]["fantasySettings.rosterSettings.keeperLimit"], "ALL");
}

#[test]
fn free_agent_failure_sets_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Fake::new().page_error("getPlayerStats", "UNEXPECTED_ERROR", Some("boom"));
    let log = fake.log.clone();
    let mut prov = provider_with(settings(tmp.path()), fake);
    let ctx = prov.load().unwrap();
    assert!(ctx.free_agents.is_empty() && ctx.teams.len() == 2);
    assert!(prov.extras().warnings.iter().any(|w| w.contains("free-agent pool unavailable")));
    let n = log.data_of("getPlayerStats").iter().filter(|d| d["statusOrTeamFilter"] == "ALL_AVAILABLE").count();
    assert_eq!(n, 2);
}

#[test]
fn free_agent_pool_queries_goalies_separately() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Fake::new().with_override(|m, d| {
        (m == "getPlayerStats" && d["positionOrGroup"] == "POS_705").then(|| json!({"statsTable": []}))
    });
    let log = fake.log.clone();
    let mut prov = provider_with(settings(tmp.path()), fake);
    let ctx = prov.load().unwrap();
    assert_eq!(ctx.free_agents.iter().map(|p| p.cid.as_str()).collect::<Vec<_>>(), ["fantrax:f1", "fantrax:f2"]);
    let groups: Vec<String> = log
        .data_of("getPlayerStats")
        .iter()
        .filter(|d| d["statusOrTeamFilter"] == "ALL_AVAILABLE")
        .map(|d| d["positionOrGroup"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(groups, ["HOCKEY_SKATING", "POS_705"]);
    assert!(!prov.extras().warnings.iter().any(|w| w.contains("unavailable")));
}

#[test]
fn extras_failures_are_warnings() {
    let tmp = tempfile::tempdir().unwrap();
    let mut fake = Fake::new().page_error("getStandings", "UNEXPECTED_ERROR", Some("nope"));
    fake.raise_for.insert("getTradeBlocks".into());
    let mut prov = provider_with(settings(tmp.path()), fake);
    let ctx = prov.load().unwrap();
    assert!(ctx.my_team().unwrap().record.is_none() && prov.trade_blocks.is_empty());
    let w = &prov.extras().warnings;
    assert!(w.iter().any(|w| w.starts_with("Standings unavailable")));
    assert!(w.iter().any(|w| w.starts_with("Trade blocks unavailable: Could not reach Fantrax: ConnectionError")), "{w:?}");
}

#[test]
fn not_logged_in_is_provider_error() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Fake::new().not_logged_in(&["getFantasyLeagueInfo"]);
    let e = provider_with(settings(tmp.path()), fake).load().unwrap_err();
    assert!(is_provider_error(&e) && err_text(&e).contains("FANTRAX_COOKIE") && !err_text(&e).contains("secret"));
}

#[test]
fn not_member_and_non_json() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Fake::new().page_error("getFantasyLeagueInfo", "NOT_MEMBER_OF_LEAGUE", None);
    let e = provider_with(settings(tmp.path()), fake).load().unwrap_err();
    assert!(err_text(&e).contains("not a member of league lg1"));
    let fake = Fake { html: true, ..Fake::new() };
    let e = provider_with(settings(&tmp.path().join("x")), fake).load().unwrap_err();
    assert!(is_provider_error(&e) && err_text(&e).contains("non-JSON"), "{e}");
}

#[test]
fn missing_config_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = settings(tmp.path());
    s.fantrax_league_id = None;
    let e = FantraxProvider::new(s, false).load().unwrap_err();
    assert!(err_text(&e).contains("FANTRAX_LEAGUE_ID"));
    let mut s = settings(tmp.path());
    s.fantrax_cookie = None;
    let e = FantraxProvider::new(s, false).load().unwrap_err();
    assert!(is_provider_error(&e) && err_text(&e).contains("FANTRAX_COOKIE"));
}

#[test]
fn team_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = settings(tmp.path());
    s.fantrax_team = Some("rival".into());
    assert_eq!(provider_with(s, Fake::new()).load().unwrap().my_team().unwrap().team_id, "t2");
    let fake = Fake::new().with_override(|m, d| {
        (m == "getTeamRosterInfo").then(|| {
            let mut v = fx(&format!("roster_{}", d["teamId"].as_str().unwrap_or("t1")));
            v["myTeamIds"] = json!([]);
            v
        })
    });
    let e = provider_with(settings(&tmp.path().join("b")), fake).load().unwrap_err();
    assert!(err_text(&e).contains("t1=Caelan's Crushers, t2=Rival Team"), "{e}");
    let mut s = settings(&tmp.path().join("c"));
    s.fantrax_team = Some("nobody".into());
    let e = provider_with(s, Fake::new()).load().unwrap_err();
    assert!(err_text(&e).contains("FANTRAX_TEAM='nobody' matches no team"), "{e}");
}

#[test]
fn fpts_fallback_scoring_without_points() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = settings(tmp.path());
    s.fantrax_points = Map::new();
    let mut prov = provider_with(s, Fake::new());
    let ctx = prov.load().unwrap();
    assert_eq!(ctx.scoring.weights, fmap(&[(FPTS_KEY, 1.0)]));
    assert!(prov.extras().warnings.iter().any(|w| w.contains("FANTRAX_POINTS is not set")));
    let star = ctx.all_players().into_iter().find(|p| p.cid == "fantrax:p1").unwrap();
    let v = fm_engine::scoring::from_config(&ctx.scoring).value(&star.lines["season"].per_game());
    assert!((v - 4.6).abs() < 1e-9);
}

#[test]
fn cache_and_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Fake::new();
    let log = fake.log.clone();
    provider_with(settings(tmp.path()), fake).load().unwrap();
    assert!(!log.of("post").is_empty());
    for e in std::fs::read_dir(tmp.path().join("fantrax_cache")).unwrap().flatten() {
        assert!(!std::fs::read_to_string(e.path()).unwrap().contains("secret"));
    }
    let mut s = settings(tmp.path());
    s.fm_offline = true;
    let ctx = provider_with(s.clone(), Fake { boom: true, ..Fake::new() }).load().unwrap();
    assert!(ctx.my_team().unwrap().team_id == "t1" && ctx.free_agents.len() == 2);
    s.fm_data_dir = tmp.path().join("empty");
    let e = provider_with(s, Fake { boom: true, ..Fake::new() }).load().unwrap_err();
    assert!(err_text(&e).contains("Offline"), "{e}");
}

// ------------------------------------------------------------------ live-shaped fixtures

#[test]
fn period_view_gp_is_not_season_gp() {
    let period = fx("roster_period");
    assert_eq!(data_split(&period, None), None);
    let r = parse_roster(&period, &live_positions(), None, Some(today()));
    let eichel = r.slots.iter().filter_map(|s| s.player.as_ref()).find(|p| p.name == "Jack Eichel").unwrap().clone();
    assert!(eichel.lines.is_empty() && r.fantasy_points.is_empty() && r.ages[&eichel.cid] == 29.0);
    let proj = parse_roster(&fx("roster_projected"), &live_positions(), None, Some(today()));
    assert_eq!(proj.split.as_deref(), Some("projected"));
    let e = proj.slots.iter().filter_map(|s| s.player.as_ref()).find(|p| p.cid == eichel.cid).unwrap();
    let line = &e.lines["projected"];
    assert!(e.lines.len() == 1 && line.gp == 76 && line.stats["G"] == 28.0);
    assert!(line.stats["ENG"] == 0.0 && line.stats["FT"] == 0.0 && line.stats[FPTS_KEY] == 440.4);
    let goalie = proj.slots.iter().filter_map(|s| s.player.as_ref()).find(|p| p.is_goalie()).unwrap();
    assert!(goalie.lines["projected"].stats["SO"] == 3.0 && goalie.lines["projected"].stats["SV"] == 990.0);
    let mut ytd = fx("roster_ytd");
    assert_eq!(data_split(&ytd, None).as_deref(), Some("season"));
    assert!(parse_roster(&ytd, &live_positions(), None, None).slots.iter().filter_map(|s| s.player.as_ref()).all(|p| p.lines.is_empty()));
    let hdr: Vec<String> =
        ytd["tables"][0]["header"]["cells"].as_array().unwrap().iter().map(|c| c["shortName"].as_str().unwrap_or("").to_string()).collect();
    let rows = ytd["tables"][0]["rows"].as_array_mut().unwrap();
    let row = rows.iter_mut().find(|x| x["scorer"]["name"] == "Jack Eichel").unwrap();
    for (col, v) in [("GP", "5"), ("FPts", "30"), ("FP/G", "6"), ("G", "3")] {
        let i = hdr.iter().position(|h| h == col).unwrap();
        row["cells"][i]["content"] = json!(v);
    }
    let r = parse_roster(&ytd, &live_positions(), None, None);
    let e = r.slots.iter().filter_map(|s| s.player.as_ref()).find(|p| p.name == "Jack Eichel").unwrap();
    assert!(e.lines["season"].gp == 5 && e.lines["season"].stats["G"] == 3.0);
}

#[test]
fn timeframe_codes_from_response() {
    let codes = timeframe_codes(&fx("roster_period"));
    assert_eq!(codes["season"]["seasonOrProjection"], YTD);
    assert_eq!(codes["season"]["timeframeTypeCode"], "YEAR_TO_DATE");
    assert_eq!(codes["prior"]["seasonOrProjection"], "SEASON_31l_YEAR_TO_DATE");
    assert_eq!(codes["projected"]["seasonOrProjection"], PROJ);
    assert!(timeframe_codes(&json!({})).is_empty());
    assert_eq!(data_split(&json!({}), None).as_deref(), Some("season"));
}

#[test]
fn goalie_pool_positions_stats_owned_age() {
    let (players, rows, _) = parse_player_pool(&fx("pool_goalies_projected"), None, None);
    assert_eq!(players.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Sergei Murashov", "Jake Allen", "Kevin Lankinen"]);
    let m = &players[0];
    assert!(m.positions == ["G"] && m.is_goalie() && m.pct_owned == Some(64.0) && rows[&m.cid].age == Some(22.0));
    let line = &m.lines["projected"];
    assert_eq!(line.gp, 47);
    assert_eq!((line.stats["W"], line.stats["GA"], line.stats["SV"], line.stats["SO"]), (25.0, 127.0, 1180.0, 3.0));
    assert!(!line.stats.contains_key("PTS"));
    let (prior, _, _) = parse_player_pool(&fx("pool_goalies_prior"), None, None);
    assert_eq!(prior[0].lines.keys().collect::<Vec<_>>(), ["prior"]);
    let (period, _, _) = parse_player_pool(&fx("pool_goalies_period"), None, None);
    assert!(period.iter().all(|p| p.lines.is_empty()));
}

#[test]
fn league_rules_parse_per_game_lock() {
    let html = r#"<h6 title="lineups">Lineups</h6><ul class="rules"><li><span>Lineup changes are executed:</span>                        Daily </li>                  <li> Lineups will become locked 0:05 (hours:minutes) before scheduled start-time of game</li> </ul>"#;
    let rules = parse_league_rules(html);
    assert_eq!(rules.get(LINEUP_RULE), Some("Daily"));
    let lock = rules.get(LINEUP_LOCK_RULE);
    assert_eq!(lock, Some("0:05 (hours:minutes) before scheduled start-time of game"));
    assert!(parse_lock_per_game(lock));
    assert!(!parse_lock_per_game(Some("0:05 (hours:minutes) before scheduled start-time of first game of the day")));
    assert!(!parse_lock_per_game(None));
}

#[test]
fn league_rules_parse() {
    let rules = parse_league_rules(fx("league_rules")["content"].as_str().unwrap());
    assert_eq!(rules.get("Maximum Total Players"), Some("16"));
    assert_eq!(rules.get_int("Maximum Active Players"), Some(10));
    assert_eq!(rules.get("Maximum Reserve Players"), Some("6"));
    assert_eq!(rules.get("Maximum Injury Reserve Players"), Some("Not Used"));
    assert_eq!(rules.get("Maximum Minor League Players"), Some("Not Used"));
    assert_eq!(rules.get("Keeper league Type"), Some("Dynasty"));
    assert_eq!(rules.get("Allow trading of draft Picks"), Some("Yes"));
    assert_eq!(rules.get_int("Number of future years for draft pick trading"), Some(3));
    assert_eq!(rules.get_int("Number of rounds available for draft pick trading"), Some(10));
    assert_eq!(rules.get(LINEUP_RULE), Some("Weekly every Monday"));
    assert_eq!(rules.get_int("Playoffs will begin in this Scoring Period"), Some(25));
    assert_eq!(rules.get_int("Number of teams qualifying for playoffs"), Some(6));
    let ma: Vec<(&str, Option<i64>)> = rules.positions.iter().map(|(p, v)| (p.as_str(), v["max_active"])).collect();
    assert_eq!(ma, [("F", Some(5)), ("D", Some(3)), ("G", Some(2))]);
    assert_eq!(rules.scoring["Skaters"]["Ft"], 3.0);
    assert_eq!(rules.scoring["Goalies"]["G"], 20.0);
    let (base, goalie, notes) = rules_weights(&rules);
    assert_eq!(base, rule_weights());
    assert_eq!(goalie, fmap(&[("A", 3.0), ("G", 20.0)]));
    assert!(notes.is_empty());
}

#[test]
fn league_rules_total_max_per_position() {
    let content = fx("league_rules")["content"].as_str().unwrap().to_string();
    let rules = parse_league_rules(&content);
    assert!(rules.positions.values().all(|v| v["max"].is_none()));
    assert!(rules_position_limits(Some(&rules)).is_empty() && rules_max_roster(Some(&rules)) == Some(16));
    let goalie_row = "<td><p>Goalie (G)</p></td><td><p>0</p></td><td><p>2</p></td><td><p></p></td>";
    let defense_row = "<td><p>Defense (D)</p></td><td><p>0</p></td><td><p>3</p></td><td><p></p></td>";
    assert!(content.contains(goalie_row) && content.contains(defense_row));
    let capped = content
        .replace(goalie_row, &goalie_row.replace("<p></p>", "<p>3</p>"))
        .replace(defense_row, &defense_row.replace("<p></p>", "<p>6</p>"));
    let rules = parse_league_rules(&capped);
    assert_eq!(rules.positions["G"].iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(), [
        ("min_active", Some(0)),
        ("max_active", Some(2)),
        ("max", Some(3))
    ]);
    assert_eq!(rules_position_limits(Some(&rules)).into_iter().collect::<Vec<_>>(), [("D".to_string(), 6), ("G".to_string(), 3)]);
    let table = "<table><tr><th><p>Pos</p></th><th><p>Total Max</p></th><th><p>Min Active</p></th>\
                 <th><p>Max Active</p></th></tr><tr><td><p>Goalie (G)</p></td><td><p>4</p></td>\
                 <td><p>1</p></td><td><p>2</p></td></tr></table>";
    let rules = parse_league_rules(table);
    assert_eq!((rules.positions["G"]["min_active"], rules.positions["G"]["max_active"], rules.positions["G"]["max"]), (Some(1), Some(2), Some(4)));
    assert_eq!(rules_max_roster(Some(&parse_league_rules(""))), None);
    assert!(rules_position_limits(None).is_empty());
}

fn synthetic(n: i64, goalie: bool, weights: &Map<String, f64>, goalie_weights: Option<&Map<String, f64>>, seed: i64) -> Vec<fm_core::Player> {
    use fm_core::models::{Split, StatLine};
    let mut rnd = fm_pycompat::random::PyRandom::new(seed);
    let scoring = fm_engine::scoring::PointsScoring::new(weights.clone(), Some(goalie_weights.cloned().unwrap_or_default()));
    let mut out = Vec::new();
    for i in 0..n {
        let mut st: Map<String, f64> = Map::new();
        let gp;
        if goalie {
            gp = rnd.randint(10, 60);
            st.insert("W".into(), rnd.randint(0, gp) as f64);
            st.insert("GA".into(), rnd.randint(gp, 3 * gp) as f64);
            st.insert("SV".into(), rnd.randint(20 * gp, 30 * gp) as f64);
            st.insert("SO".into(), rnd.randint(0, 5) as f64);
            st.insert("A".into(), rnd.randint(0, 3) as f64);
            st.insert("G".into(), if i == 0 { 1.0 } else { 0.0 });
            st.insert("PIM".into(), rnd.randint(0, 4) as f64);
        } else {
            gp = rnd.randint(20, 82);
            for (k, a, b) in
                [("G", 0, 50), ("A", 0, 70), ("SOG", 50, 300), ("HIT", 0, 250), ("BLK", 0, 150), ("PPP", 0, 30), ("PIM", 0, 80), ("ENG", 0, 4), ("FT", 0, 3), ("SHG", 0, 3)]
            {
                st.insert(k.into(), rnd.randint(a, b) as f64);
            }
            let pts = st["G"] + st["A"];
            st.insert("PTS".into(), pts);
        }
        let fpts = fm_pycompat::num::round_to(scoring.value(&st), 4);
        st.insert("GP".into(), gp as f64);
        st.insert(FPTS_KEY.into(), fpts);
        let mut p = synthetic_player(&format!("{}{i}", if goalie { 'g' } else { 's' }), gp, &[], None, goalie, "projected");
        p.positions = if goalie { vec!["G".into()] } else { vec!["F".into()] };
        p.lines.clear();
        p.lines.insert("projected".into(), StatLine { split: Split::Projected, gp, stats: st });
        out.push(p);
    }
    out
}

#[test]
fn fit_weights_recovers_league_rules() {
    let base = rule_weights();
    let goalie = fmap(&[("A", 3.0), ("G", 20.0)]);
    let mut players = synthetic(60, false, &base, None, 1);
    players.extend(synthetic(25, true, &base, Some(&goalie), 2));
    let res = fit_weights(players.iter());
    let mut sorted_w: Vec<_> = res.weights.iter().map(|(k, v)| (k.clone(), *v)).collect();
    sorted_w.sort_by(|a, b| a.0.cmp(&b.0));
    let mut sorted_b: Vec<_> = base.iter().map(|(k, v)| (k.clone(), *v)).collect();
    sorted_b.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(sorted_w, sorted_b);
    assert_eq!(res.goalie_weights.get("A"), Some(&3.0));
    assert_eq!(res.goalie_weights.get("G"), Some(&20.0));
    assert!(res.n == 85 && res.mae < 1e-6 && res.residual_max < 1e-6);
    assert!(res.points_line().starts_with("FANTRAX_POINTS=G=4,A=2,"), "{}", res.points_line());
    assert!(weights_check(&fmap(&[("G", 3.0), ("A", 2.0)]), players.iter(), None).0 > 1.0);
    assert!(weights_check(&res.weights, players.iter(), Some(&res.goalie_weights)).0 < 1e-6);
}

#[test]
fn fit_weights_rounds_and_drops_tiny() {
    let base = fmap(&[("G", 4.0), ("A", 2.0), ("SOG", 0.02), ("HIT", 0.333)]);
    let res = fit_weights(synthetic(40, false, &base, None, 1).iter());
    assert_eq!(res.weights, fmap(&[("G", 4.0), ("A", 2.0), ("HIT", 0.33)]));
    assert!(res.goalie_weights.is_empty() && res.mae > 0.0 && res.mae < 0.2);
    assert_eq!(fit_weights(std::iter::empty()).n, 0);
}

#[test]
fn provider_live_shape_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = settings(tmp.path());
    s.fantrax_dynasty = false;
    let fake = Fake::live();
    let log = fake.log.clone();
    let mut prov = provider_with(s, fake);
    let ctx = prov.load().unwrap();
    assert_eq!(prov.scoring_source, "Fantrax league rules");
    assert_eq!(ctx.scoring.weights, rule_weights());
    assert_eq!(ctx.scoring.goalie_weights, fmap(&[("A", 3.0), ("G", 20.0)]));
    assert!(prov.weights_mae.abs() < 1e-9 && prov.weights_checked == 10);
    assert!(prov.config_mae > 0.1);
    assert_eq!(ctx.roster_shape.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(), [("BN", 6), ("F", 5), ("D", 3), ("G", 2)]);
    assert!(ctx.dynasty);
    assert!(ctx.position_limits.is_empty() && ctx.max_roster_size == Some(16));
    let me = ctx.my_team().unwrap();
    let eichel = me.players_iter().find(|p| p.name == "Jack Eichel").unwrap();
    assert!(eichel.lines.len() == 1 && eichel.lines.contains_key("projected") && eichel.gp("season") == 0);
    assert_eq!(prov.fantasy_points[&eichel.cid], (Some(440.4), Some(5.79)));
    assert_eq!(prov.ages[&eichel.cid], 29.0);
    let owned: Vec<(&str, Option<f64>)> = me.players_iter().map(|p| (p.name.as_str(), p.pct_owned)).collect();
    assert_eq!(owned, [
        ("Jack Eichel", Some(99.0)),
        ("Dylan Holloway", Some(97.0)),
        ("William Nylander", Some(99.0)),
        ("Mackenzie Blackwood", Some(91.0))
    ]);
    let change: Vec<Option<f64>> = me.players_iter().map(|p| p.pct_owned_change).collect();
    assert_eq!(change, [Some(0.0), Some(-1.0), Some(0.0), Some(-2.0)]);
    let fas = &ctx.free_agents;
    assert!(fas.iter().all(|p| p.pct_owned_change.is_some()));
    let goalies: Vec<&fm_core::Player> = fas.iter().filter(|p| p.is_goalie()).collect();
    assert_eq!(fas.len(), 6);
    assert_eq!(goalies.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Sergei Murashov", "Jake Allen", "Kevin Lankinen"]);
    assert!(goalies[0].pct_owned == Some(64.0) && prov.ages[&goalies[0].cid] == 22.0);
    assert_eq!(goalies[0].lines["projected"].stats["W"], 25.0);
    let reqs: HashSet<(String, String)> = log
        .data_of("getPlayerStats")
        .iter()
        .filter(|d| d["statusOrTeamFilter"] == "ALL_AVAILABLE")
        .map(|d| (d["positionOrGroup"].as_str().unwrap().to_string(), d["seasonOrProjection"].as_str().unwrap().to_string()))
        .collect();
    let want: HashSet<(String, String)> = [("HOCKEY_SKATING", PROJ), ("POS_201", PROJ), ("HOCKEY_SKATING", YTD), ("POS_201", YTD)]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(reqs, want);
    assert_eq!(log.data_of("getTeamRosterInfo").iter().filter(|r| r["seasonOrProjection"] == PROJ).count(), 2);
    assert_eq!(prov.draft_picks[&2027][0].round, 1);
    assert_eq!(prov.draft_picks[&2027][0].original_owner.as_deref(), Some("t1"));

    let text = prov.describe_settings_load().unwrap().join("\n");
    for s in [
        "Scoring (Fantrax league rules): A=2",
        "goalies: A=3, G=20",
        "Scoring check: mean abs error 0.000",
        "Roster: max total 16, active 10, reserve 6, IR Not Used, minors Not Used",
        "Active by position (max): F 5, D 3, G 2",
        "Keeper league type: Dynasty",
        "Whole roster by position (Total Max): no limits",
        "Draft picks tradeable: Yes (3 future years, 10 rounds)",
        "Lineup changes: weekly (Rules page: Weekly every Monday)",
        "Playoffs: start scoring period 25",
        ", 6 teams",
        "My draft picks: 2027: 10 picks (rounds 1-10)",
        "Dynasty: yes (from Fantrax keeper league type",
        "FANTRAX_POINTS (not used: league rules win)",
        "To match the league rules set: FANTRAX_POINTS=A=2,",
        "Free agents loaded: 6 (3 goalies)",
    ] {
        assert!(text.contains(s), "{s}\n{text}");
    }
    assert!(!text.contains("Commish") && !text.contains("secret"));
    assert_eq!(prov.lineup_lock(), "weekly");
    assert_eq!(ctx.lineup_lock, Some(fm_core::models::LineupLock::Weekly));
    let raw = prov.raw_settings_load().unwrap();
    assert_eq!(raw["league_rules"]["Keeper League"]["Keeper league Type"], "Dynasty");
    assert_eq!(raw["position_limits"]["G"]["max_active"], 2);
    assert_eq!(raw["draftPickTradingAllowed"], true);
    let fit = prov.fit_points_load().unwrap();
    assert!(fit.n == 10 && fit.mae < 0.01, "{fit:?}");
    let via_trait = LeagueProvider::fit_points(&prov).unwrap();
    assert_eq!(via_trait["n"], 10);
}

#[test]
fn fantrax_points_fallback_and_fit_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Fake::live().with_override(|m, _| (m == "getLeagueRulesOld").then(|| json!({"content": ""})));
    let mut prov = provider_with(settings(tmp.path()), fake);
    let ctx = prov.load().unwrap();
    assert!(prov.scoring_source == "FANTRAX_POINTS" && ctx.scoring.weights["G"] == 3.0);
    assert!(prov.weights_mae > 0.1);
    assert_eq!(ctx.roster_shape.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(), [("F", 3), ("G", 1), ("BN", 6)]);
    let text = prov.describe_settings_load().unwrap().join("\n");
    assert!(text.contains("FANTRAX_POINTS check: mean abs error") && text.contains("--fit-points"));
}

#[test]
fn parse_taken_ownership_reads_rostered_rows() {
    let got = parse_taken_ownership(&fx("pool_taken"));
    assert_eq!(got["fantrax:03924"], (Some(99.0), Some(29.0)));
    assert_eq!(got["fantrax:03mbn"], (Some(91.0), Some(29.0)));
    let pool = parse_taken_ownership(&fx("player_pool"));
    assert_eq!(pool.keys().collect::<Vec<_>>(), ["fantrax:r1"]);
}

// ------------------------------------------------------------------ harness capture

fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32) -> chrono::NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, mi, 0).unwrap()
}

#[test]
fn parse_fantrax_when_formats_and_season_inference() {
    assert_eq!(parse_fantrax_when(Some("Mon Sep 21, 2026, 10:15AM"), None), Some(dt(2026, 9, 21, 10, 15)));
    assert_eq!(parse_fantrax_when(Some("Sep 21, 10:15 AM EDT"), Some(2026)), Some(dt(2026, 9, 21, 10, 15)));
    assert_eq!(parse_fantrax_when(Some("Jan 3, 9:00 PM EST"), Some(2026)), Some(dt(2027, 1, 3, 21, 0)));
    assert_eq!(parse_fantrax_when(Some("<b>Tue Oct 6, 2026, 1:05PM</b>"), None), Some(dt(2026, 10, 6, 13, 5)));
    assert_eq!(parse_fantrax_when(Some("yesterday"), None), None);
    assert_eq!(parse_fantrax_when(None, None), None);
}

#[test]
fn fantrax_activity_maps_transactions_and_proposals() {
    use fm_core::models::ActivityAction as A;
    let tmp = tempfile::tempdir().unwrap();
    let mut prov = provider_with(settings(tmp.path()), Fake::new());
    let items = prov.activity_load(None);
    let tx: Vec<(A, Option<&str>, Option<&str>, Option<&str>)> = items
        .iter()
        .filter(|i| i.action != A::Proposed)
        .map(|i| (i.action, i.cid.as_deref(), i.team_id.as_deref(), i.group_id.as_deref()))
        .collect();
    assert_eq!(tx, [(A::Add, Some("fantrax:p4"), Some("t1"), Some("a1")), (A::Drop, Some("fantrax:x9"), Some("t1"), Some("a1"))]);
    assert_eq!(items[0].ts.isoformat(), "2026-09-21T10:15:00");
    assert_eq!(items[0].team_name.as_deref(), Some("Caelan's Crushers"));
    let prop: Vec<(Option<&str>, Option<&str>, Option<&str>)> = items
        .iter()
        .filter(|i| i.action == A::Proposed)
        .map(|i| (i.cid.as_deref(), i.team_id.as_deref(), i.counterparty_id.as_deref()))
        .collect();
    assert_eq!(prop, [(Some("fantrax:r2"), Some("t1"), Some("t2"))]);
    let since = NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
    let filtered: Vec<_> = items.iter().filter(|i| i.ts.date() >= since).cloned().collect();
    assert_eq!(prov.activity_load(Some(since)), filtered);
    assert_eq!(LeagueProvider::activity(&prov, Some(since)).unwrap(), filtered);
    let mut mv = Map::new();
    mv.insert("from".to_string(), json!("t1"));
    mv.insert("to".to_string(), json!("t2"));
    mv.insert("player_cid".to_string(), json!("fantrax:a"));
    let dated = activity_from_pending(
        &[PendingTradeInfo { trade_id: "x".into(), proposed_by: Some("t1".into()), moves: vec![mv], proposed_at: Some("Oct 2, 9:00 PM EDT".into()) }],
        &Map::new(),
        Some(2026),
        None,
    );
    assert_eq!(dated[0].ts.isoformat(), "2026-10-02T21:00:00");
}

#[test]
fn fantrax_trade_rows_become_in_and_out() {
    use fm_core::models::ActivityAction as A;
    let row = |team: &str, cid: &str, name: &str| TransactionInfo {
        tx_id: "T".into(),
        team_id: Some(team.into()),
        when: Some("Mon Oct 5, 2026, 9:00AM".into()),
        kind: "TRADE".into(),
        player_cid: Some(cid.into()),
        player_name: Some(name.into()),
    };
    let got: HashSet<(A, String, String, String)> = activity_from_transactions(&[row("t1", "fantrax:a", "A"), row("t2", "fantrax:b", "B")], &Map::new(), None)
        .into_iter()
        .map(|i| (i.action, i.cid.unwrap(), i.team_id.unwrap(), i.counterparty_id.unwrap()))
        .collect();
    let want: HashSet<(A, String, String, String)> = [
        (A::TradeIn, "fantrax:a", "t1", "t2"),
        (A::TradeOut, "fantrax:a", "t2", "t1"),
        (A::TradeIn, "fantrax:b", "t2", "t1"),
        (A::TradeOut, "fantrax:b", "t1", "t2"),
    ]
    .iter()
    .map(|(a, b, c, d)| (*a, b.to_string(), c.to_string(), d.to_string()))
    .collect();
    assert_eq!(got, want);
}

#[test]
fn fantrax_lineup_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let mut prov = provider_with(settings(tmp.path()), Fake::new());
    let day = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
    let rows = prov.lineup_snapshot_load(Some(day));
    let mine: Vec<_> = rows.iter().filter(|r| r.team_id == "t1").collect();
    assert!(!mine.is_empty() && rows.iter().all(|r| r.date == day));
    assert!(rows.iter().all(|r| r.starting == !["BN", "IR", "MIN", "TAXI"].contains(&r.slot.as_str())));
    assert!(mine.iter().any(|r| ["BN", "IR", "MIN"].contains(&r.slot.as_str())));
    assert_eq!(LeagueProvider::lineup_snapshot(&prov, Some(day)).unwrap(), rows);
}

#[test]
fn fantrax_activity_failure_is_a_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = settings(tmp.path());
    s.fantrax_league_id = None; // load() fails
    let mut prov = FantraxProvider::new(s, false);
    assert!(prov.activity_load(None).is_empty() && prov.lineup_snapshot_load(None).is_empty());
    assert!(prov.extras().warnings.iter().any(|w| w.contains("activity unavailable")));
}

#[test]
fn player_pool_parses_roster_percent_change() {
    let (players, rows, _) = parse_player_pool(&fx("player_pool"), None, None);
    let by: HashMap<&str, &fm_core::Player> = players.iter().map(|p| (p.cid.as_str(), p)).collect();
    assert!(by["fantrax:f1"].pct_owned_change == Some(2.0) && rows["fantrax:f1"].owned_change == Some(2.0));
    assert_eq!(by["fantrax:f2"].pct_owned_change, Some(0.0));
    let (proj, _, _) = parse_player_pool(&fx("pool_skaters_projected"), None, None);
    assert!(proj[0].pct_owned == Some(66.0) && proj[0].pct_owned_change == Some(-7.0));
    let header = fx("pool_skaters_projected")["tableHeader"]["cells"].clone();
    let header = header.as_array().unwrap();
    let plus_minus = header.iter().position(|c| c["shortName"] == "+/-").unwrap();
    assert!(!stat_columns(header).iter().any(|(i, _)| *i == plus_minus));
    let line = proj[0].lines.get("projected").or_else(|| proj[0].lines.get("season")).unwrap();
    assert!(!line.stats.contains_key("PM"));
    let cells: Vec<Value> = ["1", "FA", "24", "1", "1", "5%", "\u{2212}3%"].iter().map(|c| json!({"content": c})).collect();
    assert_eq!(parse_row_cells(header, &cells).owned_change, Some(-3.0));
}

#[test]
fn taken_rows_carry_roster_percent_change() {
    let rows = parse_taken_rows(&fx("pool_taken"));
    assert_eq!(rows["fantrax:03924"].owned_change, Some(0.0));
    assert_eq!(rows["fantrax:051yz"].owned_change, Some(-1.0));
    assert!(rows["fantrax:03mbn"].owned == Some(91.0) && rows["fantrax:03mbn"].owned_change == Some(-2.0));
}

#[test]
fn lineup_lock_parse() {
    assert_eq!(parse_lineup_lock(Some("Daily")), "daily");
    assert_eq!(parse_lineup_lock(Some("Weekly                 every Monday")), "weekly");
    assert_eq!(parse_lineup_lock(None), "weekly");
    assert_eq!(parse_lineup_lock(Some("")), "weekly");
}

#[test]
fn parse_player_pool_waiver_status_vs_free_agent() {
    let mut data = fx("player_pool");
    data["statsTable"][1]["cells"][1]["content"] = json!("W (Wed)");
    let thu = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
    let wed = NaiveDate::from_ymd_opt(2026, 10, 7).unwrap();
    let (players, rows, _) = parse_player_pool(&data, None, Some(thu));
    let by: HashMap<&str, &fm_core::Player> = players.iter().map(|p| (p.cid.as_str(), p)).collect();
    assert_eq!(by["fantrax:f2"].waiver_until, Some(wed));
    assert_eq!(rows["fantrax:f2"].waiver_day, Some(2));
    assert!(by["fantrax:f1"].waiver_until.is_none() && by["fantrax:p4"].waiver_until.is_none());
    let (same_day, _, _) = parse_player_pool(&data, None, Some(wed));
    assert_eq!(same_day[1].waiver_until, Some(wed));
}

#[test]
fn rules_stat_limits_reports_gp_caps_and_goalie_minimums() {
    let mk = |pairs: &[(&str, &str, &str)]| {
        let mut r = LeagueRules::default();
        for (sect, k, v) in pairs {
            r.settings.entry(sect.to_string()).or_default().insert(k.to_string(), v.to_string());
        }
        r
    };
    let live = mk(&[
        ("Restrictions", "Minimum Total Players", "0"),
        ("Restrictions", "Maximum Active Players", "12"),
        ("Claims/Drops", "Max # of claims per week", "3"),
    ]);
    assert_eq!(rules_stat_limits(Some(&live)), Some(vec![]));
    let capped = mk(&[
        ("Restrictions", "Max Games Played (G)", "82"),
        ("Restrictions", "Minimum goalie games per week", "2"),
        ("Restrictions", "Max Games Played (F)", "Not Used"),
    ]);
    assert_eq!(rules_stat_limits(Some(&capped)), Some(vec!["Max Games Played (G): 82".into(), "Minimum goalie games per week: 2".into()]));
    assert_eq!(rules_stat_limits(None), None);
    assert_eq!(rules_stat_limits(Some(&LeagueRules::default())), None);
}

#[test]
fn claim_limit_formats() {
    assert_eq!(parse_claim_limit(Some("10")), Some(10));
    assert_eq!(parse_claim_limit(Some("3 (Free Agents:Unlimited, Waiver Wire:Unlimited) starting every Monday")), Some(3));
    assert_eq!(parse_claim_limit(Some("Unlimited (Free Agents:2, Waiver Wire:1)")), Some(3));
    assert_eq!(parse_claim_limit(Some("Unlimited")), None);
    assert_eq!(parse_claim_limit(None), None);
}

#[test]
fn msg_block_and_key_match_python() {
    // fantraxapi Method.msg_block: str() values, None dropped; key = sha256(json.dumps(..., sort_keys))
    let mut d = Map::new();
    d.insert("view".to_string(), json!("STATS"));
    d.insert("n".to_string(), json!(3));
    d.insert("skip".to_string(), Value::Null);
    let m = msg_block("lg1", "getTeamRosterInfo", &d);
    assert_eq!(m, json!({"method": "getTeamRosterInfo", "data": {"leagueId": "lg1", "view": "STATS", "n": "3"}}));
}

// ------------------------------------------------------------------ auth (fantrax_auth.py)

fn auth_settings(dir: &Path, creds: bool) -> Settings {
    let mut s = settings(dir);
    s.fantrax_cookie = None;
    if creds {
        s.fantrax_username = Some(Secret::new(USER));
        s.fantrax_password = Some(Secret::new(PASSWORD));
    }
    s
}

#[derive(Clone)]
struct TestClock(Arc<Mutex<f64>>);

impl TestClock {
    fn new() -> Self {
        TestClock(Arc::new(Mutex::new(1_800_000_000.0)))
    }
    fn add(&self, s: f64) {
        *self.0.lock().unwrap() += s;
    }
    fn get(&self) -> f64 {
        *self.0.lock().unwrap()
    }
}

fn ok_login() -> LoginCfg {
    LoginCfg { data: json!({"userInfo": {"userId": "u1"}}), errors: None, body: None }
}

fn make_auth(s: Settings, log: &Log, clock: &TestClock, cfg: LoginCfg, still_logged_out: bool) -> FantraxAuth {
    let (log, c) = (log.clone(), clock.clone());
    FantraxAuth::new(s)
        .with_session_factory(Box::new(move || Box::new(Fake::login_session(log.clone(), cfg.clone(), still_logged_out))))
        .with_clock(Box::new(move || c.get()))
}

fn is_login_failed(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ProviderError>(), Some(ProviderError::LoginFailed(_)))
}

#[test]
fn auth_not_logged_in_triggers_one_login_and_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let (log, clock) = (Log::default(), TestClock::new());
    let auth = make_auth(auth_settings(tmp.path(), true), &log, &clock, ok_login(), false);
    let expired = Fake::new().not_logged_in(&[
        "getFantasyLeagueInfo",
        "getTeamRosterInfo",
        "getStandings",
        "getPlayerStats",
        "getLeagueRulesOld",
        "getTradeBlocks",
        "getPendingTransactions",
        "getTransactionDetailsHistory",
    ]);
    let expired_log = expired.log.clone();
    let mut prov = FantraxProvider::new(auth_settings(tmp.path(), true), false)
        .with_session(Box::new(expired))
        .with_today(today())
        .with_auth(auth);
    let lc = prov.load().unwrap();
    assert!(!lc.teams.is_empty() && log.of("login").len() == 1);
    assert_eq!(expired_log.of("post").len(), 1);
    assert_eq!(prov.extras().warnings.iter().filter(|w| *w == "Fantrax session refreshed by login").count(), 1);
    let msg = &log.of("login")[0];
    assert_eq!(msg["msgs"], json!([{"method": "login", "data": {"u": USER, "p": PASSWORD, "t": "", "v": 3}}]));
    assert!(msg["uiv"] == 3 && msg["refUrl"] == "https://www.fantrax.com/login");
    let saved_text = std::fs::read_to_string(tmp.path().join(SESSION_FILE)).unwrap();
    let saved: Value = serde_json::from_str(&saved_text).unwrap();
    assert_eq!(saved["cookies"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect::<Vec<_>>(), ["FX_RM"]);
    assert!(saved["source"] == "login" && saved["last_login_ok"] == true);
    assert!(!saved_text.contains(PASSWORD) && !saved_text.contains(USER));
    let reloaded = prov.with_auth_mut(|a| a.load_session()).unwrap();
    assert_eq!(reloaded.cookie("FX_RM").as_deref(), Some("fresh-remember-me"));
}

#[test]
fn auth_login_failure_during_refresh_is_login_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let (log, clock) = (Log::default(), TestClock::new());
    let cfg = LoginCfg { data: json!({}), errors: Some(json!([{"code": "INVALID_CREDENTIALS", "text": "x"}])), body: None };
    let auth = make_auth(auth_settings(tmp.path(), true), &log, &clock, cfg, false);
    let mut prov = FantraxProvider::new(auth_settings(tmp.path(), true), false)
        .with_session(Box::new(Fake::new().not_logged_in(&["getFantasyLeagueInfo"])))
        .with_today(today())
        .with_auth(auth);
    let e = prov.load().unwrap_err();
    let text = e.to_string();
    assert!(is_login_failed(&e), "{e}");
    assert!(text.contains("rejected FANTRAX_USERNAME / FANTRAX_PASSWORD") && text.contains("FANTRAX_COOKIE"));
    assert!(!text.contains(PASSWORD) && !text.contains(USER));
    assert_eq!(log.of("login").len(), 1);
    assert_eq!(prov.with_auth_mut(|a| a.status())["last_login_ok"], false);
}

#[test]
fn auth_still_logged_out_after_login() {
    let tmp = tempfile::tempdir().unwrap();
    let (log, clock) = (Log::default(), TestClock::new());
    let auth = make_auth(auth_settings(tmp.path(), true), &log, &clock, ok_login(), true);
    let mut prov = FantraxProvider::new(auth_settings(tmp.path(), true), false)
        .with_session(Box::new(Fake::new().not_logged_in(&["getFantasyLeagueInfo"])))
        .with_today(today())
        .with_auth(auth);
    let e = prov.load().unwrap_err();
    assert!(e.to_string().contains("accepted the login") && e.to_string().contains("still says you are not logged in"), "{e}");
    assert_eq!(log.of("login").len(), 1);
}

#[test]
fn auth_no_credentials_keeps_cookie_advice() {
    let tmp = tempfile::tempdir().unwrap();
    let (log, clock) = (Log::default(), TestClock::new());
    let mut s = auth_settings(tmp.path(), false);
    s.fantrax_cookie = Some(Secret::new("FX_RM=x"));
    let auth = make_auth(s.clone(), &log, &clock, ok_login(), false);
    let mut prov = FantraxProvider::new(s, false)
        .with_session(Box::new(Fake::new().not_logged_in(&["getFantasyLeagueInfo"])))
        .with_today(today())
        .with_auth(auth);
    let e = prov.load().unwrap_err().to_string();
    let i = e.find("FANTRAX_USERNAME and FANTRAX_PASSWORD").expect("advice");
    assert!(e[i..].contains("FANTRAX_COOKIE"));
    assert!(log.of("login").is_empty());
}

#[test]
fn auth_login_failure_reasons() {
    let cases = [
        (LoginCfg { data: json!({}), errors: Some(json!([{"code": "BAD_INTERACTION"}])), body: None }, "reCAPTCHA"),
        (LoginCfg { data: json!({"tfa": true}), errors: None, body: None }, "two-factor"),
        (LoginCfg { data: json!({"passwordExpired": true}), errors: None, body: None }, "expired"),
        (LoginCfg { data: json!({}), errors: None, body: None }, "endpoint may have changed"),
        (LoginCfg { data: json!({}), errors: None, body: Some(json!({"pageError": {"code": "STALE_CLIENT"}})) }, "STALE_CLIENT"),
    ];
    for (cfg, want) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let (log, clock) = (Log::default(), TestClock::new());
        let mut auth = make_auth(auth_settings(tmp.path(), true), &log, &clock, cfg, false);
        let e = auth.refresh().err().expect("login must fail");
        assert!(is_login_failed(&e) && e.to_string().contains(want), "{want}: {e}");
        assert!(e.to_string().contains("copy the whole Cookie request header"));
        assert!(!tmp.path().join(SESSION_FILE).is_file() || auth.status()["saved_session"] == false);
    }
}

#[test]
fn auth_login_rate_limited_per_process_and_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (log, clock) = (Log::default(), TestClock::new());
    let mut auth = make_auth(auth_settings(tmp.path(), true), &log, &clock, ok_login(), false);
    auth.refresh().unwrap();
    clock.add(60.0);
    let e = auth.refresh().err().unwrap();
    assert!(is_login_failed(&e) && e.to_string().contains("less than 10 minutes ago"));
    // a new process still sees the file timestamp
    fa::forget_attempt(&auth.path());
    let log2 = Log::default();
    let other = make_auth(auth_settings(tmp.path(), true), &log2, &clock, ok_login(), false);
    let e = other.login().err().unwrap();
    assert!(e.to_string().contains("not trying again for 9 min"), "{e}");
    // ... and the process remembers without the file
    std::fs::remove_file(tmp.path().join(SESSION_FILE)).unwrap();
    let auth2 = make_auth(auth_settings(tmp.path(), true), &Log::default(), &clock, ok_login(), false);
    fa::set_attempt(&auth2.path(), clock.get());
    assert!(is_login_failed(&auth2.login().err().unwrap()));
    clock.add(fa::LOGIN_MIN_INTERVAL + 1.0);
    auth.refresh().unwrap();
    assert_eq!(log.of("login").len(), 2);
    assert!(log2.of("login").is_empty());
}

#[test]
fn auth_rate_limit_applies_to_failed_logins() {
    let tmp = tempfile::tempdir().unwrap();
    let (log, clock) = (Log::default(), TestClock::new());
    let cfg = LoginCfg { data: json!({}), errors: Some(json!([{"code": "INVALID_CREDENTIALS"}])), body: None };
    let mut auth = make_auth(auth_settings(tmp.path(), true), &log, &clock, cfg, false);
    assert!(auth.refresh().err().unwrap().to_string().contains("rejected"));
    assert!(auth.refresh().err().unwrap().to_string().contains("less than 10 minutes"));
    assert_eq!(log.of("login").len(), 1);
}

fn saved_file(dir: &Path, value: &str) {
    let v = json!({"version": 1, "saved_at": 1_799_999_000.0, "source": "login",
                   "cookies": [{"name": "FX_RM", "value": value, "domain": ".fantrax.com", "path": "/", "expires": null}]});
    std::fs::write(dir.join(SESSION_FILE), v.to_string()).unwrap();
}

#[test]
fn auth_precedence_saved_then_cookie_then_login() {
    let tmp = tempfile::tempdir().unwrap();
    let (log, clock) = (Log::default(), TestClock::new());
    let mut s = auth_settings(tmp.path(), true);
    s.fantrax_cookie = Some(Secret::new("FX_RM=from-env"));
    let mut auth = make_auth(s, &log, &clock, ok_login(), false);
    saved_file(tmp.path(), "saved");
    assert_eq!(auth.ensure_session(true).unwrap().cookie("FX_RM").as_deref(), Some("saved"));
    assert_eq!(auth.source.as_deref(), Some("saved"));
    auth.mark_expired();
    assert_eq!(auth.ensure_session(true).unwrap().cookie("FX_RM").as_deref(), Some("from-env"));
    assert_eq!(auth.source.as_deref(), Some("cookie"));
    let log2 = Log::default();
    let mut auth2 = make_auth(auth_settings(tmp.path(), true), &log2, &clock, ok_login(), false);
    assert_eq!(auth2.ensure_session(true).unwrap().cookie("FX_RM").as_deref(), Some("fresh-remember-me"));
    assert_eq!(auth2.source.as_deref(), Some("login"));
    assert!(log.of("login").is_empty() && log2.of("login").len() == 1);
    fa::forget_attempt(&auth2.path());
    std::fs::remove_file(tmp.path().join(SESSION_FILE)).unwrap();
    let mut none = make_auth(auth_settings(tmp.path(), false), &Log::default(), &clock, ok_login(), false);
    let e = none.ensure_session(true).err().unwrap().to_string();
    let i = e.find("FANTRAX_USERNAME and FANTRAX_PASSWORD").expect("advice");
    assert!(e[i..].contains("FANTRAX_COOKIE"));
}

#[test]
fn auth_stale_saved_session_falls_back_to_cookie_without_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let (log, clock) = (Log::default(), TestClock::new());
    let mut s = auth_settings(tmp.path(), false);
    s.fantrax_cookie = Some(Secret::new("FX_RM=from-env"));
    let mut auth = make_auth(s, &log, &clock, ok_login(), false);
    saved_file(tmp.path(), "saved");
    auth.ensure_session(true).unwrap();
    let sess = auth.recover().unwrap().expect("cookie session");
    assert_eq!(sess.cookie("FX_RM").as_deref(), Some("from-env"));
    assert_eq!(auth.source.as_deref(), Some("cookie"));
    assert!(log.of("login").is_empty());
    assert_eq!(auth.status()["saved_session_expired"], true);
    assert!(auth.recover().unwrap().is_none());
}

#[test]
fn auth_session_roundtrip_and_expired_cookies() {
    let tmp = tempfile::tempdir().unwrap();
    let (log, clock) = (Log::default(), TestClock::new());
    let auth = make_auth(auth_settings(tmp.path(), true), &log, &clock, ok_login(), false);
    let now = clock.get() as i64;
    let mut s = HttpSession::new();
    let ck = |n: &str, v: &str, d: &str, e: Option<i64>| Cookie { name: n.into(), value: v.into(), domain: d.into(), path: "/".into(), expires: e };
    s.set_cookie(ck("FX_RM", "rm", ".fantrax.com", None));
    s.set_cookie(ck("ui", "u", "www.fantrax.com", Some(now + 3600)));
    s.set_cookie(ck("old", "o", ".fantrax.com", Some(now - 10)));
    s.set_cookie(ck("other", "x", ".example.com", None));
    auth.save_session(&s, "login").unwrap();
    let loaded = auth.load_session().unwrap();
    let mut got: Vec<(String, String)> = loaded.cookies().into_iter().map(|c| (c.name, c.value)).collect();
    got.sort();
    assert_eq!(got, [("FX_RM".to_string(), "rm".to_string()), ("ui".into(), "u".into())]);
    let st = auth.status();
    assert!(st["saved_session"] == true && st["saved_session_has_auth_cookie"] == true && st["saved_session_age_seconds"] == 0.0);
    assert!(auth.logout() && auth.load_session().is_none() && !auth.logout());
}

#[test]
fn auth_ping_bypasses_cache_and_records() {
    let tmp = tempfile::tempdir().unwrap();
    let (log, clock) = (Log::default(), TestClock::new());
    let auth = make_auth(auth_settings(tmp.path(), false), &log, &clock, ok_login(), false);
    let fake = Fake::new();
    let calls = fake.log.clone();
    let prov = FantraxProvider::new(auth_settings(tmp.path(), false), false).with_session(Box::new(fake)).with_today(today()).with_auth(auth);
    assert_eq!(prov.ping().unwrap()["ok"], true);
    assert_eq!(prov.ping().unwrap()["league_id"], "lg1");
    assert_eq!(calls.of("post").len(), 2);
    let st = prov.with_auth_mut(|a| a.status());
    assert!(st["last_ping_ok"] == true && st["last_ping_at"] == clock.get());
}

#[test]
fn auth_status_and_messages_never_carry_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = auth_settings(tmp.path(), true);
    s.fantrax_cookie = Some(Secret::new("FX_RM=cookie-sentinel-7"));
    let auth = FantraxAuth::new(s.clone());
    saved_file(tmp.path(), "saved-cookie-sentinel");
    let st = auth.status().to_string();
    let dbg = format!("{s:?}");
    for text in [st.as_str(), dbg.as_str()] {
        for secret in [USER, PASSWORD, "cookie-sentinel-7", "saved-cookie-sentinel"] {
            assert!(!text.contains(secret), "{secret} leaked");
        }
    }
    assert_eq!(auth.status()["credentials"], json!({"FANTRAX_USERNAME": true, "FANTRAX_PASSWORD": true, "FANTRAX_COOKIE": true, "FANTRAX_COOKIE_FILE": false}));
}

#[test]
fn set_cookie_header_parsing() {
    let c = parse_set_cookie("FX_RM=abc; Domain=fantrax.com; Path=/; Expires=Wed, 21 Oct 2026 07:28:00 GMT; HttpOnly", "www.fantrax.com").unwrap();
    assert_eq!((c.name.as_str(), c.value.as_str(), c.domain.as_str(), c.path.as_str()), ("FX_RM", "abc", ".fantrax.com", "/"));
    assert_eq!(c.expires, Some(1792567680));
    let c = parse_set_cookie("JSESSIONID=x", "www.fantrax.com").unwrap();
    assert_eq!((c.domain.as_str(), c.expires), ("www.fantrax.com", None));
}
