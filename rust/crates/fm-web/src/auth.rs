//! Rust port of `fantasy_manager/web/auth.py`: dashboard access control (one shared password, an
//! HMAC-signed session cookie, a per-IP login limiter, a loopback-only fallback without a password
//! and the insecure-bind guard).
//!
//! Cookie format is byte-compatible with Python: `fm_session = v1.<issued>.<token>.<sig>` where
//! `sig = b64url-nopad(HMAC-SHA256(key, "<body>|<pwtag>"))`, `pwtag = hex(HMAC-SHA256(key,
//! "pw:"+password))[:16]`. The key is `FM_WEB_SECRET` or `<data>/web_secret` (shared with Python).
//!
//! Wiring: [`install`] adds the access-control layer (and, with a password, `GET/POST /login` and
//! `POST /logout`) to a router; [`auth_layer`] returns the bare layer for callers that register
//! their own login routes. Peer address comes from `ConnectInfo<SocketAddr>` (serve with
//! `into_make_service_with_connect_info`); without it the peer is "" like Python's missing client.
//!
//! Deviations: the login page is produced by a caller-supplied [`LoginRenderer`] (the W10 template
//! layer renders `login.html`); a plain built-in form is the fallback. `::ffff:127.0.0.1` is not
//! treated as loopback (same as Python < 3.13, and the stricter choice).

use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use fm_core::config::{secret_value, Settings};
use hmac::{Hmac, Mac};
use indexmap::IndexMap;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub const COOKIE_NAME: &str = "fm_session";
pub const SECRET_FILE: &str = "web_secret";
pub const DEFAULT_SESSION_DAYS: i64 = 30;
pub const MAX_FAILURES: usize = 5;
pub const FAILURE_WINDOW: f64 = 15.0 * 60.0;
pub const DEFAULT_TRUSTED_PROXIES: [&str; 2] = ["127.0.0.1", "::1"];
pub const PUBLIC_PATHS: [&str; 3] = ["/login", "/logout", "/healthz"];
pub const PUBLIC_PREFIXES: [&str; 1] = ["/static/"];
pub const MIN_PASSWORD_LENGTH: usize = 12;
const TOKEN_VERSION: &str = "v1";

type HmacSha256 = Hmac<Sha256>;

/// Refused to listen on a non-loopback interface without a dashboard password.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct InsecureBindError(pub String);

// --------------------------------------------------------------------------- hosts and proxies

/// True for 127.0.0.0/8, ::1 and "localhost"; false for 0.0.0.0, LAN/public IPs and names.
pub fn is_loopback_host(host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    if host.is_empty() {
        return false;
    }
    let h = host.trim().trim_matches(|c| c == '[' || c == ']').to_lowercase();
    if h == "localhost" {
        return true;
    }
    h.parse::<IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

/// Err when binding `host` would expose an unprotected dashboard.
pub fn check_bind(host: &str, password_set: bool, allow_insecure: bool) -> Result<(), InsecureBindError> {
    if password_set || allow_insecure || is_loopback_host(Some(host)) {
        return Ok(());
    }
    Err(InsecureBindError(format!(
        "refusing to serve the dashboard on {host} without a password: anyone who can reach this \
         machine could read your league data and change settings. Set FM_WEB_PASSWORD in .env \
         (see docs/hosting.md), bind to 127.0.0.1, or set FM_WEB_ALLOW_INSECURE=1 on a trusted \
         home network only."
    )))
}

pub fn parse_proxies(raw: Option<&str>) -> HashSet<String> {
    let items: HashSet<String> = raw
        .unwrap_or("")
        .split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    if items.is_empty() {
        DEFAULT_TRUSTED_PROXIES.iter().map(|s| s.to_string()).collect()
    } else {
        items
    }
}

fn peer(req: &Request) -> String {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_default()
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn first_header(headers: &HeaderMap, name: &str) -> Option<String> {
    header_str(headers, name).filter(|v| !v.is_empty()).map(|v| v.split(',').next().unwrap_or("").trim().to_string())
}

/// The client's IP. `X-Forwarded-For` is used only when the direct peer is a trusted proxy (so a
/// direct client can't spoof it to dodge the rate limit); its last entry is the address the proxy saw.
pub fn client_ip(peer: &str, headers: &HeaderMap, trusted: &HashSet<String>) -> String {
    if trusted.contains(peer) {
        if let Some(xff) = header_str(headers, "x-forwarded-for").filter(|v| !v.is_empty()) {
            let hops: Vec<&str> = xff.split(',').map(|h| h.trim()).filter(|h| !h.is_empty()).collect();
            if let Some(last) = hops.last() {
                return (*last).to_string();
            }
        }
    }
    if peer.is_empty() {
        "unknown".into()
    } else {
        peer.to_string()
    }
}

pub fn is_https(scheme: Option<&str>, peer: &str, headers: &HeaderMap, trusted: &HashSet<String>) -> bool {
    if scheme == Some("https") {
        return true;
    }
    trusted.contains(peer) && first_header(headers, "x-forwarded-proto").unwrap_or_default().to_lowercase() == "https"
}

// --------------------------------------------------------------------------- secret key

fn token_hex(nbytes: usize) -> String {
    let mut b = vec![0u8; nbytes];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

/// The signing key from `<data_dir>/web_secret`, created (random, 0600) on first use.
pub fn load_or_create_secret(data_dir: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let path = data_dir.join(SECRET_FILE);
    match std::fs::read_to_string(&path) {
        Ok(t) => {
            let t = t.trim();
            if t.chars().count() >= 32 {
                return Ok(t.as_bytes().to_vec());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(data_dir)?;
    let value = token_hex(32);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut fh = match opts.open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // another worker created it first (or it was too short: replace)
            let t = std::fs::read_to_string(&path)?;
            let t = t.trim();
            if t.chars().count() >= 32 {
                return Ok(t.as_bytes().to_vec());
            }
            let mut o = std::fs::OpenOptions::new();
            o.write(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            o.open(&path)?
        }
        Err(e) => return Err(e),
    };
    fh.write_all(format!("{value}\n").as_bytes())?;
    drop(fh);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!("created the dashboard session key at {}", path.display());
    Ok(value.into_bytes())
}

// --------------------------------------------------------------------------- policy

pub type SecretFn = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

/// `password` None = no login; then `allow_insecure` decides between local-only and open.
pub struct AuthConfig {
    pub password: Option<String>,
    /// Lazily resolved signing key source (so building the app writes nothing).
    pub secret: Option<SecretFn>,
    pub allow_insecure: bool,
    pub session_seconds: i64,
    pub trusted_proxies: HashSet<String>,
    pub max_failures: usize,
    pub failure_window: f64,
    key: OnceLock<Vec<u8>>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            password: None,
            secret: None,
            allow_insecure: false,
            session_seconds: DEFAULT_SESSION_DAYS * 86400,
            trusted_proxies: DEFAULT_TRUSTED_PROXIES.iter().map(|s| s.to_string()).collect(),
            max_failures: MAX_FAILURES,
            failure_window: FAILURE_WINDOW,
            key: OnceLock::new(),
        }
    }
}

impl std::fmt::Debug for AuthConfig {
    // never show the password or key
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AuthConfig(mode={:?})", self.mode())
    }
}

impl AuthConfig {
    pub fn with_password(mut self, pw: impl Into<String>) -> Self {
        self.password = Some(pw.into());
        self
    }

    pub fn with_secret_bytes(mut self, secret: impl Into<Vec<u8>>) -> Self {
        let s: Vec<u8> = secret.into();
        self.secret = Some(Arc::new(move || s.clone()));
        self
    }

    pub fn with_session_seconds(mut self, s: i64) -> Self {
        self.session_seconds = s;
        self
    }

    pub fn with_allow_insecure(mut self, v: bool) -> Self {
        self.allow_insecure = v;
        self
    }

    /// "password", "open" or "local-only".
    pub fn mode(&self) -> &'static str {
        if self.password.as_deref().is_some_and(|p| !p.is_empty()) {
            "password"
        } else if self.allow_insecure {
            "open"
        } else {
            "local-only"
        }
    }

    /// Python `bool(cfg.password)`: templates show the logout button only then.
    pub fn enabled(&self) -> bool {
        self.mode() == "password"
    }

    /// The signing key (resolved lazily, once).
    pub fn key(&self) -> &[u8] {
        self.key.get_or_init(|| {
            let s = self.secret.as_ref().map(|f| f()).unwrap_or_default();
            if s.is_empty() {
                tracing::warn!(
                    "no dashboard session key available; using a temporary one (everyone is logged out on restart)"
                );
                let mut b = vec![0u8; 32];
                rand::rngs::OsRng.fill_bytes(&mut b);
                b
            } else {
                s
            }
        })
    }

    /// Constant-time compare (digests first, so the length doesn't leak either).
    pub fn check_password(&self, attempt: &str) -> bool {
        let Some(pw) = self.password.as_deref().filter(|p| !p.is_empty()) else { return false };
        let a = Sha256::digest(attempt.as_bytes());
        let b = Sha256::digest(pw.as_bytes());
        ct_eq(&a, &b)
    }

    fn mac(&self) -> HmacSha256 {
        HmacSha256::new_from_slice(self.key()).expect("hmac accepts any key length")
    }

    fn pw_tag(&self) -> String {
        let mut m = self.mac();
        m.update(b"pw:");
        m.update(self.password.as_deref().unwrap_or("").as_bytes());
        hex::encode(m.finalize().into_bytes())[..16].to_string()
    }

    fn sign(&self, body: &str) -> String {
        let mut m = self.mac();
        m.update(format!("{body}|{}", self.pw_tag()).as_bytes());
        URL_SAFE_NO_PAD.encode(m.finalize().into_bytes())
    }

    pub fn make_token(&self, now: Option<f64>) -> String {
        let issued = now.unwrap_or_else(unix_now).trunc() as i64;
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let body = format!("{TOKEN_VERSION}.{issued}.{}", URL_SAFE_NO_PAD.encode(nonce));
        format!("{body}.{}", self.sign(&body))
    }

    pub fn verify_token(&self, token: Option<&str>, now: Option<f64>) -> bool {
        let Some(token) = token.filter(|t| !t.is_empty()) else { return false };
        if self.password.as_deref().is_none_or(|p| p.is_empty()) {
            return false;
        }
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 4 || parts[0] != TOKEN_VERSION {
            return false;
        }
        let body = parts[..3].join(".");
        if !ct_eq(parts[3].as_bytes(), self.sign(&body).as_bytes()) {
            return false;
        }
        let Ok(issued) = parts[1].parse::<i64>() else { return false };
        let now = now.unwrap_or_else(unix_now);
        (issued - 60) as f64 <= now && now < (issued + self.session_seconds) as f64
    }
}

/// Constant-time byte equality (length differences return early, like `hmac.compare_digest`).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// The policy for the real server (`fm web`).
pub fn auth_from_settings(settings: &Settings) -> AuthConfig {
    // Stray whitespace (a CR from a Windows-edited .env) would make the password impossible to type.
    let password = Some(secret_value(settings.fm_web_password.as_ref()).trim().to_string()).filter(|p| !p.is_empty());
    let secret_env = secret_value(settings.fm_web_secret.as_ref()).trim().to_string();
    let data_dir = settings.fm_data_dir.clone();
    let secret: SecretFn = Arc::new(move || {
        if !secret_env.is_empty() {
            return secret_env.as_bytes().to_vec();
        }
        match load_or_create_secret(&data_dir) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("could not read or create {} ({:?})", data_dir.join(SECRET_FILE).display(), e.kind());
                Vec::new()
            }
        }
    });
    let days = if settings.fm_web_session_days == 0 { DEFAULT_SESSION_DAYS } else { settings.fm_web_session_days };
    if let Some(p) = &password {
        if p.chars().count() < MIN_PASSWORD_LENGTH {
            tracing::warn!(
                "FM_WEB_PASSWORD is shorter than {MIN_PASSWORD_LENGTH} characters; use a long random passphrase for an internet-facing dashboard"
            );
        }
    }
    AuthConfig {
        password,
        secret: Some(secret),
        allow_insecure: settings.fm_web_allow_insecure,
        session_seconds: days.max(1) * 86400,
        trusted_proxies: parse_proxies(Some(&settings.fm_web_trusted_proxies)),
        ..AuthConfig::default()
    }
}

// --------------------------------------------------------------------------- rate limit

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

pub fn monotonic_clock() -> Clock {
    let start = Instant::now();
    Arc::new(move || start.elapsed().as_secs_f64())
}

/// Failed login attempts per client IP in a sliding window (in memory, per process).
pub struct LoginLimiter {
    pub max_failures: usize,
    pub window: f64,
    pub max_clients: usize,
    clock: Clock,
    fails: Mutex<IndexMap<String, VecDeque<f64>>>,
}

impl LoginLimiter {
    pub fn new(max_failures: usize, window: f64, clock: Clock, max_clients: usize) -> Self {
        LoginLimiter { max_failures, window, max_clients, clock, fails: Mutex::new(IndexMap::new()) }
    }

    pub fn with_defaults() -> Self {
        Self::new(MAX_FAILURES, FAILURE_WINDOW, monotonic_clock(), 10_000)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, IndexMap<String, VecDeque<f64>>> {
        self.fails.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop expired entries of `ip` and return its queue (removed from the map when empty).
    fn prune(&self, map: &mut IndexMap<String, VecDeque<f64>>, ip: &str, now: f64) -> VecDeque<f64> {
        let Some(q) = map.get_mut(ip) else { return VecDeque::new() };
        while q.front().is_some_and(|t| *t <= now - self.window) {
            q.pop_front();
        }
        if q.is_empty() {
            map.shift_remove(ip);
            return VecDeque::new();
        }
        q.clone()
    }

    /// Seconds until `ip` may try again (0 = allowed now).
    pub fn retry_after(&self, ip: &str) -> i64 {
        let mut map = self.lock();
        let now = (self.clock)();
        let q = self.prune(&mut map, ip, now);
        if q.len() < self.max_failures {
            return 0;
        }
        (((q[0] + self.window - now + 0.999).trunc()) as i64).max(1)
    }

    /// Record a failure; returns the failures in the current window.
    pub fn fail(&self, ip: &str) -> usize {
        let mut map = self.lock();
        let now = (self.clock)();
        if map.len() >= self.max_clients {
            let keys: Vec<String> = map.keys().cloned().collect();
            for other in keys {
                self.prune(&mut map, &other, now);
            }
            if map.len() >= self.max_clients {
                // still full: drop the oldest client
                map.shift_remove_index(0);
            }
        }
        let mut q = self.prune(&mut map, ip, now);
        q.push_back(now);
        let n = q.len();
        map.insert(ip.to_string(), q);
        n
    }

    pub fn reset(&self, ip: &str) {
        self.lock().shift_remove(ip);
    }

    /// Number of tracked clients (tests: memory bound).
    pub fn tracked_clients(&self) -> usize {
        self.lock().len()
    }
}

// --------------------------------------------------------------------------- HTTP wiring

/// Characters `urllib.parse.quote(s, safe="")` leaves alone.
fn quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A same-site path to return to after login ("/" for anything else, never /login itself).
pub fn safe_login_next(target: Option<&str>) -> String {
    let t = match target {
        Some(t) if !t.is_empty() => t,
        _ => "/",
    };
    if !t.starts_with('/') || t.starts_with("//") || t.contains('\\') || t.chars().any(|c| (c as u32) < 32 || c as u32 == 127) {
        return "/".into();
    }
    let no_frag = t.split('#').next().unwrap_or("");
    let (path, query) = match no_frag.split_once('?') {
        Some((p, q)) => (p, q),
        None => (no_frag, ""),
    };
    if path == "/login" || path == "/logout" {
        return "/".into();
    }
    let path = if path.is_empty() { "/" } else { path };
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
}

pub fn is_public(path: &str) -> bool {
    PUBLIC_PATHS.contains(&path) || PUBLIC_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// Renders the login page: `(error, next) -> html` (W10 supplies the `login.html` template).
pub type LoginRenderer = Arc<dyn Fn(Option<&str>, &str) -> String + Send + Sync>;

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

/// Plain fallback login page.
pub fn default_login_page(error: Option<&str>, next: &str) -> String {
    let err = error.map(|e| format!("<p class=\"error\">{}</p>", html_escape(e))).unwrap_or_default();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Sign in</title></head><body>\
         <form method=\"post\" action=\"/login\">{err}<input type=\"hidden\" name=\"next\" value=\"{}\">\
         <input type=\"password\" name=\"password\" autofocus><button type=\"submit\">Sign in</button></form></body></html>",
        html_escape(next)
    )
}

/// Shared state of the layer and the login routes.
pub struct AuthState {
    pub cfg: AuthConfig,
    pub limiter: LoginLimiter,
    pub render_login: LoginRenderer,
}

impl AuthState {
    pub fn new(cfg: AuthConfig) -> Arc<Self> {
        Self::with(cfg, Arc::new(default_login_page), monotonic_clock())
    }

    pub fn with(cfg: AuthConfig, render_login: LoginRenderer, clock: Clock) -> Arc<Self> {
        let limiter = LoginLimiter::new(cfg.max_failures, cfg.failure_window, clock, 10_000);
        Arc::new(AuthState { cfg, limiter, render_login })
    }
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for h in headers.get_all(header::COOKIE) {
        let Ok(s) = h.to_str() else { continue };
        for part in s.split(';') {
            if let Some((k, v)) = part.trim().split_once('=') {
                if k == name {
                    return Some(v.trim_matches('"').to_string());
                }
            }
        }
    }
    None
}

fn authed(st: &AuthState, headers: &HeaderMap) -> bool {
    st.cfg.verify_token(cookie_value(headers, COOKIE_NAME).as_deref(), None)
}

fn here(req: &Request) -> String {
    let u = req.uri();
    match u.query().filter(|q| !q.is_empty()) {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_string(),
    }
}

fn plain(status: StatusCode, body: &str) -> Response {
    let mut r = Response::new(Body::from(body.to_string()));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

fn redirect(location: &str, status: StatusCode) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = status;
    if let Ok(v) = HeaderValue::from_str(location) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r
}

/// The access-control middleware (Python `access_control`).
pub async fn access_control(State(st): State<Arc<AuthState>>, req: Request, next: Next) -> Response {
    let cfg = &st.cfg;
    let path = req.uri().path().to_string();
    if cfg.mode() == "local-only" {
        let ip = client_ip(&peer(&req), req.headers(), &cfg.trusted_proxies);
        if is_loopback_host(Some(&ip)) || path == "/healthz" {
            return next.run(req).await;
        }
        tracing::warn!("refused dashboard request from {ip}: no FM_WEB_PASSWORD set");
        return plain(
            StatusCode::FORBIDDEN,
            "This dashboard only answers on this machine until FM_WEB_PASSWORD is set (see docs/hosting.md).\n",
        );
    }
    if is_public(&path) || authed(&st, req.headers()) {
        return next.run(req).await;
    }
    let login = format!("/login?next={}", quote(&here(&req)));
    if header_str(req.headers(), "hx-request").is_some_and(|v| !v.is_empty()) {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::UNAUTHORIZED;
        if let Ok(v) = HeaderValue::from_str(&login) {
            r.headers_mut().insert("hx-redirect", v);
        }
        return r;
    }
    if (req.method() == Method::GET || req.method() == Method::HEAD) && !path.starts_with("/api/") {
        return redirect(&login, StatusCode::SEE_OTHER);
    }
    let mut r = Response::new(Body::from("{\"detail\":\"login required\"}"));
    *r.status_mut() = StatusCode::UNAUTHORIZED;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Cookie realm=\"fantasy-manager\""));
    r
}

/// The bare access-control layer; apply with `router.layer(auth_layer(state))`. In "open" mode
/// the middleware would still enforce nothing useful, so callers use [`install`], which skips it.
pub fn auth_layer(
    state: Arc<AuthState>,
) -> axum::middleware::FromFnLayer<
    impl Fn(State<Arc<AuthState>>, Request, Next) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> + Clone,
    Arc<AuthState>,
    (State<Arc<AuthState>>, Request),
> {
    axum::middleware::from_fn_with_state(state, |s: State<Arc<AuthState>>, r: Request, n: Next| {
        Box::pin(access_control(s, r, n)) as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>
    })
}

fn login_page(st: &AuthState, status: StatusCode, error: Option<&str>, nxt: &str, retry_after: Option<i64>) -> Response {
    let html = (st.render_login)(error, nxt);
    let mut r = Response::new(Body::from(html));
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(s) = retry_after {
        if let Ok(v) = HeaderValue::from_str(&s.to_string()) {
            h.insert(header::RETRY_AFTER, v);
        }
    }
    r
}

fn req_secure(st: &AuthState, req: &Request) -> bool {
    is_https(req.uri().scheme_str(), &peer(req), req.headers(), &st.cfg.trusted_proxies)
}

async fn login_form(State(st): State<Arc<AuthState>>, req: Request) -> Response {
    let next = form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
        .find(|(k, _)| k == "next")
        .map(|(_, v)| v.into_owned());
    let nxt = safe_login_next(next.as_deref());
    if authed(&st, req.headers()) {
        return redirect(&nxt, StatusCode::SEE_OTHER);
    }
    login_page(&st, StatusCode::OK, None, &nxt, None)
}

async fn login_submit(State(st): State<Arc<AuthState>>, req: Request) -> Response {
    let cfg = &st.cfg;
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap_or_default();
    let bytes = &bytes[..bytes.len().min(8192)];
    let mut fields: IndexMap<String, String> = IndexMap::new();
    for (k, v) in form_urlencoded::parse(bytes) {
        fields.insert(k.into_owned(), v.into_owned());
    }
    let nxt = safe_login_next(fields.get("next").map(|s| s.as_str()));
    let peer_ip = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string()).unwrap_or_default();
    let ip = client_ip(&peer_ip, &parts.headers, &cfg.trusted_proxies);
    let wait = st.limiter.retry_after(&ip);
    if wait > 0 {
        tracing::warn!("dashboard login from {ip} blocked: too many failed attempts (retry in {wait}s)");
        let mins = ((wait + 59) / 60).max(1);
        let msg = format!("Too many failed attempts. Try again in {mins} minute{}.", if mins != 1 { "s" } else { "" });
        return login_page(&st, StatusCode::TOO_MANY_REQUESTS, Some(&msg), &nxt, Some(wait));
    }
    if !cfg.check_password(fields.get("password").map(|s| s.as_str()).unwrap_or("")) {
        let n = st.limiter.fail(&ip);
        tracing::warn!(
            "dashboard login failed from {ip} ({n} of {} allowed in {} min)",
            cfg.max_failures,
            (cfg.failure_window / 60.0).floor() as i64
        );
        return login_page(&st, StatusCode::UNAUTHORIZED, Some("Wrong password."), &nxt, None);
    }
    st.limiter.reset(&ip);
    tracing::info!("dashboard login from {ip}");
    let secure = is_https(parts.uri.scheme_str(), &peer_ip, &parts.headers, &cfg.trusted_proxies);
    let mut resp = redirect(&nxt, StatusCode::SEE_OTHER);
    let cookie = format!(
        "{COOKIE_NAME}={}; HttpOnly; Max-Age={}; Path=/; SameSite=lax{}",
        cfg.make_token(None),
        cfg.session_seconds,
        if secure { "; Secure" } else { "" }
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

async fn logout(State(st): State<Arc<AuthState>>, req: Request) -> Response {
    let secure = req_secure(&st, &req);
    let mut resp = redirect("/login", StatusCode::SEE_OTHER);
    let cookie = format!(
        "{COOKIE_NAME}=\"\"; expires=Thu, 01 Jan 1970 00:00:00 GMT; HttpOnly; Max-Age=0; Path=/; SameSite=lax{}",
        if secure { "; Secure" } else { "" }
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

/// Add the access-control layer (and, with a password, the /login + /logout routes) to `router`.
/// Open mode installs nothing (a warning is logged), like Python.
pub fn install<S>(router: Router<S>, state: Arc<AuthState>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    if state.cfg.mode() == "open" {
        tracing::warn!("dashboard running WITHOUT a password (FM_WEB_ALLOW_INSECURE=1)");
        return router;
    }
    let mut router = router;
    if state.cfg.enabled() {
        let login_routes: Router<S> = Router::new()
            .route("/login", get(login_form).post(login_submit))
            .route("/logout", post(logout))
            .with_state(state.clone());
        router = router.merge(login_routes);
    }
    router.layer(auth_layer(state))
}
