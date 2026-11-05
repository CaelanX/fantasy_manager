//! Rust port of `fantasy_manager/providers/fantrax_auth.py` (owner: work package `W3-fantrax`):
//! Fantrax login and session persistence.
//!
//! * Login is an ordinary fxpa message: `POST https://www.fantrax.com/fxpa/req?lgnu=1` with
//!   `{"method": "login", "data": {"u", "p", "t": "", "v": 3}}` after a GET of the login page
//!   (Cloudflare cookies). `responses[0].data.userInfo` = success; `tfa`, `passwordExpired` or
//!   `errors` (INVALID_CREDENTIALS, BAD_INTERACTION, ACCOUNT_LOCKED) = failure.
//! * The session is kept in `<FM_DATA_DIR>/fantrax_session.json` (cookies plus login bookkeeping,
//!   same JSON as Python). At most one login attempt per 10 minutes, per process and per file.
//! * Credentials and cookie values are never logged, printed or put in error messages.
//!
//! Deviation: `session_factory` returns a boxed [`FxSession`] (Python: a `requests.Session`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use fm_core::config::{secret_value, Settings};
use fm_core::{Map, ProviderError};
use fm_pycompat::fmt::fixed;
use fm_pycompat::pyjson::{dumps, DumpOpts};

use crate::fantrax::{build_session, jget, load_cookies, now_secs, Cookie, FxSession, BROWSER_UA, FXPA_URL};

pub const SESSION_FILE: &str = "fantrax_session.json";
pub const LOGIN_PAGE: &str = "https://www.fantrax.com/login";
/// seconds between login attempts, per process and per session file
pub const LOGIN_MIN_INTERVAL: f64 = 10.0 * 60.0;
pub const AUTH_COOKIE: &str = "FX_RM";
pub const SESSION_VERSION: i64 = 1;

pub const MANUAL_COOKIE_HINT: &str = "Log in at fantrax.com in a browser, open DevTools > Network, click any 'req?leagueId=' request and \
copy the whole Cookie request header into FANTRAX_COOKIE (or the file named by FANTRAX_COOKIE_FILE). \
See 'Fantrax setup' in the README.";

pub fn no_credentials_msg() -> String {
    format!(
        "No Fantrax login is configured. Set FANTRAX_USERNAME and FANTRAX_PASSWORD in .env (recommended: \
         fm logs in itself and keeps the session in FM_DATA_DIR), or set FANTRAX_COOKIE (or FANTRAX_COOKIE_FILE) \
         to a logged-in browser cookie. {MANUAL_COOKIE_HINT}"
    )
}

/// Last login attempt per session file in this process (in addition to the timestamp in the file).
static ATTEMPTS: LazyLock<Mutex<HashMap<String, f64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn attempts() -> std::sync::MutexGuard<'static, HashMap<String, f64>> {
    ATTEMPTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Forget every in-process login attempt (tests; Python `_ATTEMPTS.clear()`).
pub fn clear_attempts() {
    attempts().clear();
}

/// Forget the in-process login attempt for one session file (tests: "a new process").
pub fn forget_attempt(path: &Path) {
    attempts().remove(&resolve_key(path));
}

/// Record an in-process login attempt for `path` (tests).
pub fn set_attempt(path: &Path, t: f64) {
    attempts().insert(resolve_key(path), t);
}

fn resolve_key(path: &Path) -> String {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().to_string()
}

fn login_failed(msg: String) -> anyhow::Error {
    ProviderError::LoginFailed(msg).into()
}

fn fmt_age_inner(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    if seconds < 90.0 {
        format!("{}s", fixed(seconds, 0, false))
    } else if seconds < 90.0 * 60.0 {
        format!("{} min", fixed(seconds / 60.0, 0, false))
    } else if seconds < 48.0 * 3600.0 {
        format!("{} h", fixed(seconds / 3600.0, 1, false))
    } else {
        format!("{} days", fixed(seconds / 86400.0, 1, false))
    }
}

/// `fmt_age(seconds)`: "?" for None.
pub fn fmt_age(seconds: Option<f64>) -> String {
    seconds.map(fmt_age_inner).unwrap_or_else(|| "?".into())
}

pub type SessionFactory = Box<dyn Fn() -> Box<dyn FxSession> + Send + Sync>;
pub type Clock = Box<dyn Fn() -> f64 + Send + Sync>;

/// Finds, creates, saves and refreshes the Fantrax cookie session.
///
/// `ensure_session()` precedence: saved session file (unless known expired) > FANTRAX_COOKIE /
/// FANTRAX_COOKIE_FILE > fresh login with FANTRAX_USERNAME / FANTRAX_PASSWORD. `source` tells
/// which one the current session came from ("saved", "cookie" or "login").
pub struct FantraxAuth {
    pub settings: Settings,
    pub data_dir: PathBuf,
    session_factory: SessionFactory,
    clock: Clock,
    pub source: Option<String>,
}

impl FantraxAuth {
    pub fn new(settings: Settings) -> Self {
        let data_dir = settings.fm_data_dir.clone();
        FantraxAuth {
            settings,
            data_dir,
            session_factory: Box::new(|| Box::new(build_session(&Map::new()))),
            clock: Box::new(now_secs),
            source: None,
        }
    }

    pub fn with_data_dir(mut self, data_dir: impl Into<PathBuf>) -> Self {
        self.data_dir = data_dir.into();
        self
    }

    pub fn with_session_factory(mut self, f: SessionFactory) -> Self {
        self.session_factory = f;
        self
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn now(&self) -> f64 {
        (self.clock)()
    }

    // -- configuration ----------------------------------------------------
    pub fn path(&self) -> PathBuf {
        self.data_dir.join(SESSION_FILE)
    }

    pub fn has_credentials(&self) -> bool {
        !secret_value(self.settings.fantrax_username.as_ref()).is_empty()
            && !secret_value(self.settings.fantrax_password.as_ref()).is_empty()
    }

    pub fn has_cookie(&self) -> bool {
        !secret_value(self.settings.fantrax_cookie.as_ref()).is_empty() || self.settings.fantrax_cookie_file.is_some()
    }

    // -- session file -----------------------------------------------------
    fn read_state(&self) -> Map<String, Value> {
        let Ok(text) = std::fs::read_to_string(self.path()) else { return Map::new() };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(o)) => o.into_iter().collect(),
            _ => Map::new(),
        }
    }

    /// Atomic write, readable by the owner only where the OS allows it.
    fn write_state(&self, state: &Map<String, Value>) -> std::io::Result<()> {
        let path = self.path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = serde_json::Map::new();
        out.insert("version".into(), Value::from(SESSION_VERSION));
        for (k, v) in state {
            out.insert(k.clone(), v.clone());
        }
        let text = dumps(&Value::Object(out), &DumpOpts::default());
        let tmp = path.with_file_name(format!("{SESSION_FILE}.tmp"));
        write_private(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, &path)
    }

    fn update_state(&self, changes: &[(&str, Value)]) {
        let mut state = self.read_state();
        for (k, v) in changes {
            state.insert(k.to_string(), v.clone());
        }
        let _ = self.write_state(&state); // bookkeeping only; the in-process limit still applies
    }

    pub fn save_session(&self, session: &dyn FxSession, source: &str) -> anyhow::Result<()> {
        let mut cookies = Vec::new();
        for c in session.cookies() {
            let dom = if c.domain.is_empty() { ".fantrax.com".to_string() } else { c.domain.clone() };
            if !dom.to_lowercase().contains("fantrax") {
                continue;
            }
            let mut o = serde_json::Map::new();
            o.insert("name".into(), Value::String(c.name.clone()));
            o.insert("value".into(), Value::String(c.value.clone()));
            o.insert("domain".into(), Value::String(dom));
            o.insert("path".into(), Value::String(if c.path.is_empty() { "/".into() } else { c.path.clone() }));
            o.insert("expires".into(), c.expires.map(Value::from).unwrap_or(Value::Null));
            cookies.push(Value::Object(o));
        }
        let mut state = self.read_state();
        state.insert("cookies".into(), Value::Array(cookies));
        state.insert("saved_at".into(), Value::from(self.now()));
        state.insert("source".into(), Value::String(source.to_string()));
        state.insert("expired".into(), Value::Bool(false));
        self.write_state(&state)?;
        Ok(())
    }

    fn saved_cookies(&self, state: &Map<String, Value>) -> Map<String, String> {
        let now = self.now();
        let mut out = Map::new();
        for c in state.get("cookies").and_then(|c| c.as_array()).map(|a| a.as_slice()).unwrap_or(&[]) {
            let Some(name) = jget(c, "name").filter(|n| crate::fantrax::truthy(n)) else { continue };
            if let Some(Value::Number(exp)) = jget(c, "expires") {
                if let Some(e) = exp.as_f64() {
                    if 0.0 < e && e < now {
                        continue;
                    }
                }
            }
            let value = jget(c, "value").map(crate::fantrax::py_str).unwrap_or_default();
            out.insert(crate::fantrax::py_str(name), value);
        }
        out
    }

    /// The saved session, or None if there is none or it is known to be expired.
    pub fn load_session(&self) -> Option<Box<dyn FxSession>> {
        let state = self.read_state();
        if state.is_empty() || state.get("expired").is_some_and(crate::fantrax::truthy) {
            return None;
        }
        let cookies = self.saved_cookies(&state);
        if cookies.is_empty() {
            return None;
        }
        let mut session = build_session(&Map::new());
        for c in state.get("cookies").and_then(|c| c.as_array()).map(|a| a.as_slice()).unwrap_or(&[]) {
            let Some(name) = jget(c, "name").and_then(|n| n.as_str()) else { continue };
            let Some(value) = cookies.get(name) else { continue };
            let s = |k: &str, d: &str| {
                jget(c, k).filter(|v| crate::fantrax::truthy(v)).map(crate::fantrax::py_str).unwrap_or_else(|| d.to_string())
            };
            session.set_cookie(Cookie {
                name: name.to_string(),
                value: value.clone(),
                domain: s("domain", ".fantrax.com"),
                path: s("path", "/"),
                expires: None,
            });
        }
        Some(Box::new(session))
    }

    pub fn mark_expired(&self) {
        if self.path().is_file() {
            self.update_state(&[("expired", Value::Bool(true)), ("expired_at", Value::from(self.now()))]);
        }
    }

    /// Delete the saved session file. True if one existed.
    pub fn logout(&self) -> bool {
        attempts().remove(&resolve_key(&self.path()));
        std::fs::remove_file(self.path()).is_ok()
    }

    pub fn record_ping(&self, ok: bool, error: Option<&str>) {
        self.update_state(&[
            ("last_ping_at", Value::from(self.now())),
            ("last_ping_ok", Value::Bool(ok)),
            ("last_ping_error", error.map(|e| Value::String(e.to_string())).unwrap_or(Value::Null)),
        ]);
    }

    // -- session selection ------------------------------------------------
    pub fn ensure_session(&mut self, allow_login: bool) -> anyhow::Result<Box<dyn FxSession>> {
        if let Some(saved) = self.load_session() {
            self.source = Some("saved".into());
            return Ok(saved);
        }
        let cookies = load_cookies(&self.settings)?;
        if !cookies.is_empty() {
            self.source = Some("cookie".into());
            return Ok(Box::new(build_session(&cookies)));
        }
        if self.has_credentials() && allow_login {
            return self.refresh();
        }
        Err(ProviderError::Message(no_credentials_msg()).into())
    }

    /// After Fantrax said WARNING_NOT_LOGGED_IN: a new session from the next source, or None.
    pub fn recover(&mut self) -> anyhow::Result<Option<Box<dyn FxSession>>> {
        if self.source.as_deref() == Some("saved") {
            self.mark_expired();
        }
        if self.has_credentials() {
            return self.refresh().map(Some);
        }
        if self.source.as_deref() == Some("saved") {
            let cookies = load_cookies(&self.settings)?;
            if !cookies.is_empty() {
                self.source = Some("cookie".into());
                return Ok(Some(Box::new(build_session(&cookies))));
            }
        }
        Ok(None)
    }

    /// Force a fresh login and save the session.
    pub fn refresh(&mut self) -> anyhow::Result<Box<dyn FxSession>> {
        let session = self.login()?;
        self.save_session(session.as_ref(), "login")?;
        self.source = Some("login".into());
        Ok(session)
    }

    // -- login ------------------------------------------------------------
    pub fn seconds_until_login_allowed(&self) -> f64 {
        let key = resolve_key(&self.path());
        let file_last = self
            .read_state()
            .get("last_login_attempt")
            .filter(|v| crate::fantrax::truthy(v))
            .and_then(crate::fantrax::py_float)
            .unwrap_or(0.0);
        let mem = attempts().get(&key).copied().unwrap_or(0.0);
        let last = file_last.max(mem);
        (last + LOGIN_MIN_INTERVAL - self.now()).max(0.0)
    }

    fn fail(&self, reason: &str, code: Option<&str>) -> anyhow::Error {
        self.update_state(&[
            ("last_login_ok", Value::Bool(false)),
            ("last_login_error", Value::String(code.unwrap_or(reason).to_string())),
        ]);
        login_failed(format!("Fantrax login failed: {reason} Until this is fixed, use a browser cookie: {MANUAL_COOKIE_HINT}"))
    }

    /// Log in with FANTRAX_USERNAME / FANTRAX_PASSWORD; returns a logged-in session.
    pub fn login(&self) -> anyhow::Result<Box<dyn FxSession>> {
        if !self.has_credentials() {
            return Err(ProviderError::Message(format!(
                "FANTRAX_USERNAME and FANTRAX_PASSWORD are not both set. {}",
                no_credentials_msg()
            ))
            .into());
        }
        let wait = self.seconds_until_login_allowed();
        if wait > 0.0 {
            return Err(login_failed(format!(
                "A Fantrax login was attempted less than {} minutes ago; not trying again for {} so a broken login cannot \
                 spam Fantrax. If logins keep failing, use a browser cookie instead: {MANUAL_COOKIE_HINT}",
                (LOGIN_MIN_INTERVAL / 60.0).floor() as i64,
                fmt_age_inner(wait)
            )));
        }
        let now = self.now();
        attempts().insert(resolve_key(&self.path()), now);
        self.update_state(&[("last_login_attempt", Value::from(now))]);

        let mut session = (self.session_factory)();
        session.set_header("User-Agent", BROWSER_UA);
        session.set_header("Referer", LOGIN_PAGE);
        let _ = session.get(LOGIN_PAGE); // like a browser: picks up Cloudflare cookies first
        let mut data = serde_json::Map::new();
        data.insert("u".into(), Value::String(secret_value(self.settings.fantrax_username.as_ref())));
        data.insert("p".into(), Value::String(secret_value(self.settings.fantrax_password.as_ref())));
        data.insert("t".into(), Value::String(String::new()));
        data.insert("v".into(), Value::from(3));
        let mut msg = serde_json::Map::new();
        msg.insert("method".into(), Value::String("login".into()));
        msg.insert("data".into(), Value::Object(data));
        let mut payload = serde_json::Map::new();
        payload.insert("msgs".into(), Value::Array(vec![Value::Object(msg)]));
        payload.insert("uiv".into(), Value::from(3));
        payload.insert("refUrl".into(), Value::String(LOGIN_PAGE.into()));
        payload.insert("dt".into(), Value::from(0));
        payload.insert("at".into(), Value::from(0));
        payload.insert("tz".into(), Value::String("UTC".into()));
        let resp = match session.post(FXPA_URL, &[("lgnu".to_string(), "1".to_string())], &Value::Object(payload)) {
            Ok(r) => r,
            Err(e) => return Err(self.fail(&format!("could not reach Fantrax ({}).", e.kind), None)),
        };
        let status = resp.status;
        let body = match resp.body {
            Ok(b) => b,
            Err(_) => {
                return Err(self.fail(
                    &format!("Fantrax returned a non-JSON response (HTTP {status}); the login endpoint may have changed."),
                    Some(&format!("HTTP_{status}")),
                ))
            }
        };
        if !body.is_object() {
            return Err(self.fail("unexpected response shape; the login endpoint may have changed.", Some("SHAPE")));
        }
        if let Some(pe) = jget(&body, "pageError").filter(|p| p.is_object()) {
            let code = jget(pe, "code")
                .filter(|c| crate::fantrax::truthy(c))
                .map(crate::fantrax::py_str)
                .unwrap_or_else(|| "PAGE_ERROR".into());
            return Err(self.fail(&format!("Fantrax answered {code}; the login endpoint may have changed."), Some(&code)));
        }
        let empty = Value::Object(Default::default());
        let first = jget(&body, "responses")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            .filter(|f| f.is_object())
            .unwrap_or(&empty);
        let data = jget(first, "data").filter(|d| d.is_object()).unwrap_or(&empty);
        let errs_v = jget(first, "errors")
            .filter(|e| crate::fantrax::truthy(e))
            .or_else(|| jget(data, "errors").filter(|e| crate::fantrax::truthy(e)));
        let errors: Vec<&Value> =
            errs_v.and_then(|e| e.as_array()).map(|a| a.iter().filter(|x| x.is_object()).collect()).unwrap_or_default();
        if let Some(user) = jget(data, "userInfo") {
            if user.as_object().is_some_and(|o| !o.is_empty()) {
                self.update_state(&[
                    ("last_login_ok", Value::Bool(true)),
                    ("last_login_error", Value::Null),
                    ("last_login_at", Value::from(self.now())),
                ]);
                return Ok(session);
            }
        }
        if jget(data, "tfa").is_some_and(crate::fantrax::truthy) {
            return Err(self.fail("the account uses two-factor authentication, which automated login cannot complete.", Some("TFA")));
        }
        if jget(data, "passwordExpired").is_some_and(crate::fantrax::truthy) {
            return Err(self.fail(
                "Fantrax says the password has expired; change it at fantrax.com and update FANTRAX_PASSWORD.",
                Some("PASSWORD_EXPIRED"),
            ));
        }
        let code = errors
            .first()
            .and_then(|e| jget(e, "code"))
            .filter(|c| crate::fantrax::truthy(c))
            .map(crate::fantrax::py_str)
            .unwrap_or_default();
        if !code.is_empty() {
            let reason = match code.as_str() {
                "INVALID_CREDENTIALS" => "Fantrax rejected FANTRAX_USERNAME / FANTRAX_PASSWORD; check them in .env.".to_string(),
                "BAD_INTERACTION" => "Fantrax's reCAPTCHA check blocked the automated login.".to_string(),
                "ACCOUNT_LOCKED" => "Fantrax says the account is locked; log in at fantrax.com to unlock it.".to_string(),
                other => format!("Fantrax answered {other}."),
            };
            return Err(self.fail(&reason, Some(&code)));
        }
        Err(self.fail("Fantrax returned no user info; the login endpoint may have changed.", Some("NO_USER")))
    }

    // -- status -----------------------------------------------------------
    /// Presence-only summary for `fm auth fantrax --status` (no cookie or credential values).
    pub fn status(&self) -> Value {
        let state = self.read_state();
        let now = self.now();
        let cookies = if state.is_empty() { Map::new() } else { self.saved_cookies(&state) };
        let ts = |key: &str| -> Value {
            match state.get(key) {
                Some(Value::Number(n)) => n.as_f64().map(Value::from).unwrap_or(Value::Null),
                Some(Value::Bool(b)) => Value::from(if *b { 1.0 } else { 0.0 }),
                _ => Value::Null,
            }
        };
        let has_cookies = state.get("cookies").is_some_and(crate::fantrax::truthy);
        let saved_age = match state.get("saved_at") {
            Some(Value::Number(n)) => n.as_f64().map(|s| Value::from(now - s)).unwrap_or(Value::Null),
            _ => Value::Null,
        };
        let mut creds = serde_json::Map::new();
        creds.insert("FANTRAX_USERNAME".into(), Value::Bool(!secret_value(self.settings.fantrax_username.as_ref()).is_empty()));
        creds.insert("FANTRAX_PASSWORD".into(), Value::Bool(!secret_value(self.settings.fantrax_password.as_ref()).is_empty()));
        creds.insert("FANTRAX_COOKIE".into(), Value::Bool(!secret_value(self.settings.fantrax_cookie.as_ref()).is_empty()));
        creds.insert("FANTRAX_COOKIE_FILE".into(), Value::Bool(self.settings.fantrax_cookie_file.is_some()));
        let wait = self.seconds_until_login_allowed();
        let mut o = serde_json::Map::new();
        o.insert("session_file".into(), Value::String(self.path().to_string_lossy().to_string()));
        o.insert("saved_session".into(), Value::Bool(has_cookies));
        o.insert("saved_session_source".into(), state.get("source").cloned().unwrap_or(Value::Null));
        o.insert("saved_session_age_seconds".into(), saved_age);
        o.insert(
            "saved_session_expired".into(),
            Value::Bool(state.get("expired").is_some_and(crate::fantrax::truthy) || (has_cookies && cookies.is_empty())),
        );
        o.insert("saved_session_has_auth_cookie".into(), Value::Bool(cookies.contains_key(AUTH_COOKIE)));
        o.insert("credentials".into(), Value::Object(creds));
        o.insert("last_login_attempt".into(), ts("last_login_attempt"));
        o.insert("last_login_ok".into(), state.get("last_login_ok").cloned().unwrap_or(Value::Null));
        o.insert("last_login_error".into(), state.get("last_login_error").cloned().unwrap_or(Value::Null));
        o.insert("last_ping_at".into(), ts("last_ping_at"));
        o.insert("last_ping_ok".into(), state.get("last_ping_ok").cloned().unwrap_or(Value::Null));
        o.insert("login_allowed_in_seconds".into(), Value::from(wait));
        Value::Object(o)
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(bytes)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}
