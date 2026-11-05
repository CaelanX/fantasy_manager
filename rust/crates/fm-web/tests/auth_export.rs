//! Ports of tests/test_web_auth.py and tests/test_web_export.py, plus Python cookie interop.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, Response, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use fm_core::config::{Secret, Settings};
use fm_web::auth::*;
use fm_web::export::{export_site_with, page_file, rewrite};
use http_body_util::BodyExt;
use tower::ServiceExt;

const PW: &str = "correct horse battery staple";
const REMOTE: &str = "203.0.113.7";
const PROXY: &str = "127.0.0.1";

fn cfg() -> AuthConfig {
    AuthConfig::default().with_password(PW).with_secret_bytes(vec![b'k'; 64])
}

fn clock(cell: Arc<Mutex<f64>>) -> Clock {
    Arc::new(move || *cell.lock().unwrap())
}

fn app_with(state: Arc<AuthState>) -> Router {
    let r = Router::new()
        .route("/", get(|| async { "home" }))
        .route("/roster", get(|| async { "roster" }))
        .route("/player/a", get(|| async { "player" }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/static/style.css", get(|| async { "css" }))
        .route("/api/recs.json", get(|| async { "{}" }))
        .route("/explain", post(|| async { "x" }))
        .route("/refresh", post(|| async { "x" }));
    install(r, state)
}

fn app() -> Router {
    app_with(AuthState::new(cfg()))
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Resp {
    fn h(&self, k: &str) -> String {
        self.headers.get(k).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
    }
}

async fn send(app: &Router, method: &str, path: &str, peer: &str, headers: &[(&str, &str)], body: &str) -> Resp {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    if !body.is_empty() {
        b = b.header("content-type", "application/x-www-form-urlencoded");
    }
    let mut req = b.body(Body::from(body.to_string())).unwrap();
    if !peer.is_empty() {
        let ip: std::net::IpAddr = peer.parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(ip, 50000)));
    }
    let r: Response<Body> = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = r.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Resp { status: parts.status, headers: parts.headers, body: String::from_utf8_lossy(&bytes).into_owned() }
}

fn form(password: &str, next: &str) -> String {
    form_urlencoded::Serializer::new(String::new()).append_pair("password", password).append_pair("next", next).finish()
}

async fn login(app: &Router, peer: &str, password: &str, next: &str, headers: &[(&str, &str)]) -> Resp {
    send(app, "POST", "/login", peer, headers, &form(password, next)).await
}

fn session_cookie(r: &Resp) -> String {
    let sc = r.h("set-cookie");
    sc.split(';').next().unwrap().to_string()
}

// -- login required -----------------------------------------------------------------------

#[tokio::test]
async fn pages_redirect_to_login() {
    let a = app();
    for path in ["/", "/roster", "/player/a", "/?league=fantrax"] {
        let r = send(&a, "GET", path, REMOTE, &[], "").await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{path}");
        assert!(r.h("location").starts_with("/login?next=%2F"), "{path}");
    }
    assert_eq!(send(&a, "GET", "/?league=fantrax", REMOTE, &[], "").await.h("location"), "/login?next=%2F%3Fleague%3Dfantrax");
}

#[tokio::test]
async fn api_and_actions_need_login() {
    let a = app();
    for (m, p) in [("GET", "/api/recs.json"), ("POST", "/explain"), ("POST", "/refresh")] {
        let r = send(&a, m, p, REMOTE, &[], "").await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{p}");
        assert_eq!(r.body, "{\"detail\":\"login required\"}");
    }
}

#[tokio::test]
async fn public_paths_and_login_page() {
    let a = app();
    assert_eq!(send(&a, "GET", "/healthz", REMOTE, &[], "").await.status, StatusCode::OK);
    assert_eq!(send(&a, "GET", "/static/style.css", REMOTE, &[], "").await.status, StatusCode::OK);
    let r = send(&a, "GET", "/login?next=/roster", REMOTE, &[], "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("type=\"password\"") && r.body.contains("value=\"/roster\""));
    assert_eq!(r.h("cache-control"), "no-store");
}

#[tokio::test]
async fn htmx_request_gets_hx_redirect() {
    let r = send(&app(), "GET", "/roster", REMOTE, &[("HX-Request", "true")], "").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.h("hx-redirect"), "/login?next=%2Froster");
}

// -- password and rate limit --------------------------------------------------------------

#[tokio::test]
async fn wrong_password_is_401_without_cookie() {
    let a = app();
    let r = login(&a, REMOTE, "hunter2-guess", "/", &[]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.body.contains("Wrong password") && r.headers.get("set-cookie").is_none());
    assert_eq!(send(&a, "GET", "/", REMOTE, &[], "").await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn rate_limit_five_failures_per_15_minutes() {
    let now = Arc::new(Mutex::new(1000.0));
    let a = app_with(AuthState::with(cfg(), Arc::new(default_login_page), clock(now.clone())));
    for _ in 0..5 {
        assert_eq!(login(&a, REMOTE, "nope", "/", &[]).await.status, StatusCode::UNAUTHORIZED);
    }
    let r = login(&a, REMOTE, PW, "/", &[]).await; // even the right password is refused now
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.h("retry-after"), "900");
    assert!(r.body.contains("Too many failed attempts"));
    assert_eq!(login(&a, "198.51.100.9", PW, "/", &[]).await.status, StatusCode::SEE_OTHER);
    *now.lock().unwrap() += 901.0;
    assert_eq!(login(&a, REMOTE, PW, "/", &[]).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn rate_limit_uses_forwarded_ip_only_from_a_trusted_proxy() {
    let now = Arc::new(Mutex::new(0.0));
    let a = app_with(AuthState::with(cfg(), Arc::new(default_login_page), clock(now)));
    let x1 = [("X-Forwarded-For", "192.0.2.1")];
    for _ in 0..5 {
        login(&a, PROXY, "nope", "/", &x1).await;
    }
    assert_eq!(login(&a, PROXY, PW, "/", &x1).await.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(login(&a, PROXY, PW, "/", &[("X-Forwarded-For", "192.0.2.2")]).await.status, StatusCode::SEE_OTHER);
    // a direct (untrusted) client can't dodge the limit by inventing X-Forwarded-For
    for i in 0..5 {
        let v = format!("10.0.0.{i}");
        login(&a, REMOTE, "nope", "/", &[("X-Forwarded-For", v.as_str())]).await;
    }
    assert_eq!(login(&a, REMOTE, PW, "/", &[("X-Forwarded-For", "10.9.9.9")]).await.status, StatusCode::TOO_MANY_REQUESTS);
}

#[test]
fn limiter_bounds_memory() {
    let lim = LoginLimiter::new(5, 900.0, Arc::new(|| 0.0), 3);
    for i in 0..10 {
        lim.fail(&format!("ip{i}"));
    }
    assert!(lim.tracked_clients() <= 3);
}

// -- session cookie -----------------------------------------------------------------------

#[tokio::test]
async fn login_sets_cookie_and_pages_work() {
    let a = app();
    let r = login(&a, REMOTE, PW, "/roster?league=espn", &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.h("location"), "/roster?league=espn");
    let sc = r.h("set-cookie").to_lowercase();
    assert!(sc.contains("fm_session=") && sc.contains("httponly") && sc.contains("samesite=lax") && !sc.contains("secure"));
    assert!(sc.contains("max-age=2592000"));
    let ck = session_cookie(&r);
    let page = send(&a, "GET", "/", REMOTE, &[("Cookie", ck.as_str())], "").await;
    assert_eq!((page.status, page.body.as_str()), (StatusCode::OK, "home"));
    assert_eq!(send(&a, "GET", "/api/recs.json", REMOTE, &[("Cookie", ck.as_str())], "").await.status, StatusCode::OK);
    assert_eq!(send(&a, "GET", "/login", REMOTE, &[("Cookie", ck.as_str())], "").await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn cookie_is_secure_behind_https_proxy_only() {
    let a = app();
    let r = login(&a, PROXY, PW, "/", &[("X-Forwarded-Proto", "https"), ("X-Forwarded-For", "192.0.2.5")]).await;
    assert!(r.h("set-cookie").to_lowercase().contains("secure"));
    let r = login(&a, REMOTE, PW, "/", &[("X-Forwarded-Proto", "https")]).await;
    assert!(!r.h("set-cookie").to_lowercase().contains("secure"));
}

#[tokio::test]
async fn open_redirects_are_refused() {
    let a = app();
    for bad in ["//evil.example/", "https://evil.example/", "/\\evil", "/login", "javascript:alert(1)"] {
        assert_eq!(login(&a, REMOTE, PW, bad, &[]).await.h("location"), "/", "{bad}");
    }
}

#[test]
fn tampered_expired_and_old_password_tokens_are_rejected() {
    let c = cfg().with_session_seconds(3600);
    let tok = c.make_token(Some(10_000.0));
    assert!(c.verify_token(Some(&tok), Some(10_100.0)));
    assert!(!c.verify_token(Some(&tok), Some(10_000.0 + 3601.0)));
    let mut bad = tok.clone();
    let last = bad.pop().unwrap();
    bad.push(if last == 'A' { 'B' } else { 'A' });
    assert!(!c.verify_token(Some(&bad), Some(10_100.0)));
    let later = tok.replacen(".10000.", ".99999.", 1);
    assert!(!c.verify_token(Some(&later), Some(99_999.0)));
    assert!(!c.verify_token(Some("garbage"), Some(10_100.0)) && !c.verify_token(None, None));
    let other_pw = AuthConfig::default().with_password("a new password").with_secret_bytes(vec![b'k'; 64]).with_session_seconds(3600);
    assert!(!other_pw.verify_token(Some(&tok), Some(10_100.0)));
    let other_key = AuthConfig::default().with_password(PW).with_secret_bytes(vec![b'z'; 64]).with_session_seconds(3600);
    assert!(!other_key.verify_token(Some(&tok), Some(10_100.0)));
}

#[tokio::test]
async fn forged_cookie_and_logout() {
    let a = app();
    let forged = [("Cookie", "fm_session=v1.9999999999.abc.forged")];
    assert_eq!(send(&a, "GET", "/", REMOTE, &forged, "").await.status, StatusCode::SEE_OTHER);
    let ck = session_cookie(&login(&a, REMOTE, PW, "/", &[]).await);
    assert_eq!(send(&a, "GET", "/", REMOTE, &[("Cookie", ck.as_str())], "").await.status, StatusCode::OK);
    let r = send(&a, "POST", "/logout", REMOTE, &[("Cookie", ck.as_str())], "").await;
    assert_eq!((r.status, r.h("location")), (StatusCode::SEE_OTHER, "/login".to_string()));
    assert!(r.h("set-cookie").contains("Max-Age=0"));
}

#[test]
fn password_compare_and_debug() {
    let c = cfg();
    assert!(c.check_password(PW) && !c.check_password(&PW[..PW.len() - 1]) && !c.check_password(""));
    assert!(!AuthConfig::default().check_password(""));
    let d = format!("{c:?}");
    assert!(!d.contains(PW) && d.contains("password"));
}

// -- no password: local mode, open mode ---------------------------------------------------

#[tokio::test]
async fn local_only_policy_serves_loopback_and_refuses_others() {
    let a = app_with(AuthState::new(AuthConfig::default()));
    assert_eq!(send(&a, "GET", "/", "127.0.0.1", &[], "").await.status, StatusCode::OK);
    assert_eq!(send(&a, "GET", "/api/recs.json", "::1", &[], "").await.status, StatusCode::OK);
    let r = send(&a, "GET", "/", REMOTE, &[], "").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("FM_WEB_PASSWORD"));
    let fwd = send(&a, "GET", "/", PROXY, &[("X-Forwarded-For", "203.0.113.9")], "").await;
    assert_eq!(fwd.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&a, "GET", "/healthz", REMOTE, &[], "").await.status, StatusCode::OK);
    assert_eq!(send(&a, "GET", "/login", "127.0.0.1", &[], "").await.status, StatusCode::NOT_FOUND);
    // no peer at all fails closed
    assert_eq!(send(&a, "GET", "/", "", &[], "").await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn allow_insecure_serves_everyone() {
    let a = app_with(AuthState::new(AuthConfig::default().with_allow_insecure(true)));
    assert_eq!(send(&a, "GET", "/", REMOTE, &[], "").await.status, StatusCode::OK);
}

// -- bind guard ---------------------------------------------------------------------------

#[test]
fn bind_policy() {
    for h in ["127.0.0.1", "localhost", "::1", "127.0.1.1", "[::1]"] {
        assert!(check_bind(h, false, false).is_ok(), "{h}");
    }
    for h in ["0.0.0.0", "::", "192.168.1.20", "myhost.example"] {
        let e = check_bind(h, false, false).unwrap_err();
        assert!(e.to_string().contains("FM_WEB_PASSWORD"), "{h}");
        assert!(check_bind(h, true, false).is_ok() && check_bind(h, false, true).is_ok());
    }
}

// -- settings and the secret key ----------------------------------------------------------

fn settings(dir: &std::path::Path) -> Settings {
    Settings { fm_data_dir: dir.join("data"), ..Settings::default() }
}

#[test]
fn auth_from_settings_and_secret_file() {
    let t = tempfile::tempdir().unwrap();
    let mut s = settings(t.path());
    s.fm_web_password = Some(Secret::new(PW));
    let c = auth_from_settings(&s);
    assert_eq!((c.mode(), c.password.as_deref()), ("password", Some(PW)));
    let path = t.path().join("data").join("web_secret");
    assert!(!path.exists());
    let key = c.key().to_vec();
    assert!(path.exists() && key.len() >= 32);
    assert_eq!(auth_from_settings(&s).key(), key.as_slice()); // reused across restarts
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim().as_bytes(), key.as_slice());
}

#[test]
fn secret_from_env_writes_no_file_and_whitespace_ignored() {
    let t = tempfile::tempdir().unwrap();
    let mut s = settings(t.path());
    s.fm_web_password = Some(Secret::new(format!("{PW}\r")));
    s.fm_web_secret = Some(Secret::new("s".repeat(40)));
    let c = auth_from_settings(&s);
    assert!(c.check_password(PW));
    assert_eq!(c.key(), "s".repeat(40).as_bytes());
    assert!(!t.path().join("data").join("web_secret").exists());
}

#[test]
fn policy_modes() {
    let t = tempfile::tempdir().unwrap();
    let mut s = settings(t.path());
    assert_eq!(auth_from_settings(&s).mode(), "local-only");
    s.fm_web_allow_insecure = true;
    assert_eq!(auth_from_settings(&s).mode(), "open");
}

#[test]
fn proxies_and_loopback_helpers() {
    assert_eq!(parse_proxies(None).len(), 2);
    assert!(parse_proxies(Some(" 10.0.0.1 , ")).contains("10.0.0.1"));
    assert_eq!(safe_login_next(Some("/roster?a=1#x")), "/roster?a=1");
    assert_eq!(safe_login_next(None), "/");
}

// -- Python interop -----------------------------------------------------------------------

#[test]
fn cookie_minted_by_python_verifies_in_rust() {
    // generated with fantasy_manager.web.auth.AuthConfig(password=PW, secret=b"k"*64).make_token(now=10000)
    let tok = "v1.10000.u19PNpPlEHAOtnZo.KShPmbYsonvoNGBsRhoBKsEKmVA_JeCk5jPAAXZ03jo";
    let c = cfg().with_session_seconds(3600);
    assert!(c.verify_token(Some(tok), Some(10_100.0)));
    assert!(!c.verify_token(Some(tok), Some(20_000.0)));
}

#[test]
fn cookie_minted_by_rust_verifies_in_python() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let py = PathBuf::from("C:/code/fantasy_manager/.venv/Scripts/python.exe");
    if !py.exists() {
        eprintln!("skipped: venv python not found");
        return;
    }
    let tok = cfg().make_token(None);
    let script = format!(
        "import sys; sys.path.insert(0, {root:?}); from fantasy_manager.web import auth; \
         c = auth.AuthConfig(password={PW:?}, secret=b'k'*64); sys.exit(0 if c.verify_token({tok:?}) else 1)",
        root = root.canonicalize().unwrap().to_string_lossy().replace('\\', "/").trim_start_matches("//?/")
    );
    let st = std::process::Command::new(py).arg("-I").arg("-c").arg(script).status().unwrap();
    assert!(st.success(), "python rejected the Rust-minted cookie");
}

// -- export -------------------------------------------------------------------------------

const PAGE: &str = r#"<html><head><link href="/static/style.css" rel="stylesheet">
<script src="https://cdnjs.cloudflare.com/ajax/libs/htmx/2.0.4/htmx.min.js" defer></script></head>
<body hx-boost="true"><a href="/roster?league=espn&amp;team=3#top">r</a> <a href="/player/espn:42?league=espn">p</a>
<a href="/schedule?league=espn&amp;week=2">next week</a> <a href="/api/recs.json?league=espn">json</a>
<a href="https://www.nhl.com/">nhl</a> <form method="post" action="/refresh?league=espn"></form></body></html>"#;

#[test]
fn page_file_names() {
    assert_eq!(page_file("/", "league=espn"), "index__league-espn.html");
    assert_eq!(page_file("/roster", "team=3&league=espn"), "roster__league-espn_team-3.html");
    assert_eq!(page_file("/player/espn:42", "league=espn"), "player-espn-42__league-espn.html");
}

#[test]
fn rewrite_points_links_at_files_and_drops_server_bits() {
    let (out, found) = rewrite(PAGE, "Tue Oct 27, 7:30 AM");
    assert!(out.contains("href=\"static/style.css\""));
    assert!(out.contains("href=\"roster__league-espn_team-3.html#top\""));
    assert!(out.contains("href=\"player-espn-42__league-espn.html\""));
    assert_eq!(found, vec!["/roster?league=espn&team=3", "/player/espn:42?league=espn"]);
    assert!(out.contains("href=\"https://www.nhl.com/\""));
    assert_eq!(out.matches("href=\"#\"").count(), 2);
    assert!(out.contains("action=\"#\"") && !out.contains("htmx") && !out.contains("hx-boost"));
    assert!(out.contains("Read-only snapshot, updated Tue Oct 27, 7:30 AM"));
}

#[tokio::test]
async fn export_crawls_reachable_pages() {
    let html = |b: &'static str| move || async move { axum::response::Html(format!("<html><head></head><body>{b}</body></html>")) };
    let router = Router::new()
        .route("/", get(html("<a href=\"/news?league=espn\">n</a>")))
        .fallback(get(html("other")))
        .route("/news", get(html("<a href=\"/?league=espn\">home</a>")));
    let t = tempfile::tempdir().unwrap();
    let st = t.path().join("st");
    std::fs::create_dir_all(&st).unwrap();
    std::fs::write(st.join("style.css"), "a{background:url(\"/static/x.png\")}").unwrap();
    let site = t.path().join("site");
    let res = export_site_with(router, &site, &st, &["espn"], 2500, "now").await.unwrap();
    let index = std::fs::read_to_string(site.join("index.html")).unwrap();
    assert_eq!(index, std::fs::read_to_string(site.join("index__league-espn.html")).unwrap());
    assert!(index.contains("href=\"news__league-espn.html\""));
    let css = std::fs::read_to_string(site.join("static/style.css")).unwrap();
    assert!(!css.contains("url(\"/static/"));
    assert_eq!((res.pages, res.errors.len(), res.truncated), (9, 0, false)); // "/" + the 8 seed pages, once each
}
