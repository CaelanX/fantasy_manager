//! Rust port of `fantasy_manager/report/notify.py`: Discord / Slack incoming-webhook notifications.
//! Owner: work package `W9-report-pregame`.
//!
//! All functions return human-readable result strings and never fail. Webhook URLs are secrets, so
//! they never appear in results or logs.
//!
//! Deviations: the HTTP client is injectable through the [`Sender`] trait (Python injected an
//! `httpx.Client`); the default [`HttpSender`] is `reqwest::blocking` with the same 10 s timeout.
//! `sleep` is part of the trait so tests do not wait for `Retry-After`. Error class names in the
//! result strings are the httpx ones (`ConnectError`, ...) via [`SendError::Http`].

use std::time::Duration;

use fm_core::config::Settings;
use fm_pycompat::text::{py_len, py_rstrip, py_lstrip, py_strip, split_ws};
use serde_json::{json, Value};

pub const TIMEOUT: f64 = 10.0;
/// Discord hard limit is 2000 chars per message.
pub const DISCORD_CHUNK: usize = 1900;
/// Slack truncates message text around 4000 chars.
pub const SLACK_CHUNK: usize = 3900;
pub const MAX_RETRY_AFTER: f64 = 5.0;
/// Phone-sized messages (the pre-game run, `pregame`).
pub const CHANGES_CHUNK: usize = 1500;
pub const BULLET: &str = "\u{2022}";

pub const NO_WEBHOOKS: &str = "no webhooks configured (set DISCORD_WEBHOOK_URL and/or SLACK_WEBHOOK_URL)";

// --------------------------------------------------------------------------- transport

/// What `httpx.Response` gave the Python code.
#[derive(Clone, Debug, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub text: String,
    /// the `Retry-After` header, raw
    pub retry_after: Option<String>,
}

/// `httpx.TimeoutException` / `httpx.HTTPError` / anything else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    Timeout,
    /// an `httpx.HTTPError` subclass: its class name
    Http(String),
    Other(String),
}

/// The HTTP client of the webhook senders (Python's injectable `httpx.Client`).
pub trait Sender {
    fn post(&self, url: &str, payload: &Value) -> Result<HttpResponse, SendError>;
    fn sleep(&self, seconds: f64) {
        std::thread::sleep(Duration::from_secs_f64(seconds.clamp(0.0, 60.0)));
    }
}

/// `httpx.Client(timeout=10)` as a blocking reqwest client.
pub struct HttpSender {
    client: reqwest::blocking::Client,
}

impl HttpSender {
    pub fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs_f64(TIMEOUT))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        HttpSender { client }
    }
}

impl Default for HttpSender {
    fn default() -> Self {
        Self::new()
    }
}

impl Sender for HttpSender {
    fn post(&self, url: &str, payload: &Value) -> Result<HttpResponse, SendError> {
        let body = serde_json::to_vec(payload).map_err(|e| SendError::Other(e.to_string()))?;
        let resp = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .map_err(|e| {
                if e.is_timeout() {
                    SendError::Timeout
                } else if e.is_connect() {
                    SendError::Http("ConnectError".to_string())
                } else {
                    SendError::Http("RequestError".to_string())
                }
            })?;
        let status = resp.status().as_u16();
        let retry_after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
        let text = resp.text().map_err(|e| {
            if e.is_timeout() {
                SendError::Timeout
            } else {
                SendError::Http("ReadError".to_string())
            }
        })?;
        Ok(HttpResponse { status, text, retry_after })
    }
}

// --------------------------------------------------------------------------- chunking

/// `str.splitlines()`: split on \n, \r, \r\n, \v, \f, \x1c-\x1e, \x85,  ,  .
pub fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let is_break = matches!(c, '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}');
        if !is_break {
            continue;
        }
        out.push(&s[start..i]);
        let mut next = i + c.len_utf8();
        if c == '\r' {
            if let Some(&(j, '\n')) = it.peek() {
                it.next();
                next = j + 1;
            }
        }
        start = next;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Split into chunks of at most `size` chars, preferring line boundaries.
pub fn chunk_text(text: &str, size: usize) -> Vec<String> {
    let text = py_strip(text);
    if text.is_empty() {
        return vec![];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    for raw in py_splitlines(text) {
        let mut line: String = raw.to_string();
        while py_len(&line) > size {
            // hard-wrap an over-long line
            if !cur.is_empty() {
                chunks.push(std::mem::take(&mut cur));
            }
            let chars: Vec<char> = line.chars().collect();
            let found = chars[..size].iter().rposition(|&c| c == ' ');
            // `cut = line.rfind(" ", 0, size)`: -1 when absent; `cut if cut > size // 2 else size`
            let cut = match found {
                Some(i) if i > size / 2 => i,
                _ => size,
            };
            let head: String = chars[..cut].iter().collect();
            chunks.push(py_rstrip(&head).to_string());
            let rest: String = chars[cut..].iter().collect();
            line = py_lstrip(&rest).to_string();
        }
        let candidate = if cur.is_empty() { line.clone() } else { format!("{cur}\n{line}") };
        if py_len(&candidate) > size {
            chunks.push(std::mem::take(&mut cur));
            cur = line;
        } else {
            cur = candidate;
        }
    }
    if !py_strip(&cur).is_empty() {
        chunks.push(cur);
    }
    chunks.into_iter().filter(|c| !py_strip(c).is_empty()).collect()
}

// --------------------------------------------------------------------------- posting

/// `float(resp.headers.get("retry-after") or resp.json().get("retry_after") or 1.0)`; any failure is 1.0.
fn retry_wait(resp: &HttpResponse) -> f64 {
    fn py_float(s: &str) -> Option<f64> {
        let t = py_strip(s).replace('_', "");
        match t.to_lowercase().as_str() {
            "nan" | "+nan" | "-nan" => Some(f64::NAN),
            "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
            "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
            _ => {
                if t.is_empty() || t.contains(|c: char| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
                    None
                } else {
                    t.parse::<f64>().ok()
                }
            }
        }
    }
    if let Some(h) = resp.retry_after.as_deref().filter(|h| !h.is_empty()) {
        return py_float(h).unwrap_or(1.0);
    }
    let body: Value = match serde_json::from_str(&resp.text) {
        Ok(v) => v,
        Err(_) => return 1.0,
    };
    let Some(obj) = body.as_object() else { return 1.0 };
    match obj.get("retry_after") {
        Some(Value::Number(n)) => match n.as_f64() {
            Some(x) if x != 0.0 => x,
            _ => 1.0,
        },
        Some(Value::String(s)) if !s.is_empty() => py_float(s).unwrap_or(1.0),
        Some(Value::Bool(true)) => 1.0,
        _ => 1.0,
    }
}

fn post(sender: &dyn Sender, url: &str, payload: &Value) -> Result<HttpResponse, SendError> {
    let mut resp = sender.post(url, payload)?;
    if resp.status == 429 {
        // honour a short Retry-After once
        let wait = retry_wait(&resp);
        if wait <= MAX_RETRY_AFTER {
            sender.sleep(wait.max(0.0));
            resp = sender.post(url, payload)?;
        }
    }
    Ok(resp)
}

fn send_chunks(name: &str, url: Option<&str>, chunks: &[String], make_payload: &dyn Fn(&str) -> Value, sender: Option<&dyn Sender>) -> String {
    let Some(url) = url.filter(|u| !u.is_empty()) else { return format!("{name}: not configured") };
    if chunks.is_empty() {
        return format!("{name}: nothing to send");
    }
    let owned;
    let http: &dyn Sender = match sender {
        Some(s) => s,
        None => {
            owned = HttpSender::new();
            &owned
        }
    };
    let total = chunks.len();
    let mut sent = 0usize;
    for chunk in chunks {
        match post(http, url, &make_payload(chunk)) {
            Ok(resp) => {
                if resp.status >= 400 {
                    let body: String = split_ws(&resp.text).join(" ");
                    let body: String = body.chars().take(200).collect();
                    return format!("{name}: error HTTP {} after {sent}/{total} message(s): {body}", resp.status);
                }
                sent += 1;
            }
            Err(SendError::Timeout) => return format!("{name}: error timed out after {sent}/{total} message(s)"),
            Err(SendError::Http(t)) => return format!("{name}: error {t} after {sent}/{total} message(s)"),
            Err(SendError::Other(t)) => return format!("{name}: error {t}"),
        }
    }
    format!("{name}: sent {sent} message(s)")
}

fn discord_payload(c: &str) -> Value {
    json!({"content": c, "allowed_mentions": {"parse": []}})
}

fn slack_payload(c: &str) -> Value {
    json!({"text": c})
}

/// POST `{"content": ...}` chunks (<= 1900 chars) to a Discord webhook. Mentions are disabled.
pub fn send_discord(webhook_url: Option<&str>, text: &str, sender: Option<&dyn Sender>) -> String {
    send_chunks("discord", webhook_url, &chunk_text(text, DISCORD_CHUNK), &discord_payload, sender)
}

/// POST `{"text": ...}` chunks to a Slack incoming webhook.
pub fn send_slack(webhook_url: Option<&str>, text: &str, sender: Option<&dyn Sender>) -> String {
    send_chunks("slack", webhook_url, &chunk_text(text, SLACK_CHUNK), &slack_payload, sender)
}

fn hooks(settings: &Settings) -> (Option<String>, Option<String>) {
    let get = |s: &Option<fm_core::config::Secret>| s.as_ref().map(|x| x.expose().to_string()).filter(|u| !u.is_empty());
    (get(&settings.discord_webhook_url), get(&settings.slack_webhook_url))
}

/// Send to every configured webhook; one result string per configured target.
pub fn notify_all(settings: &Settings, text: &str, sender: Option<&dyn Sender>) -> Vec<String> {
    let (discord, slack) = hooks(settings);
    notify_all_urls(discord.as_deref(), slack.as_deref(), text, sender)
}

/// [`notify_all`] with the webhook URLs given directly.
pub fn notify_all_urls(discord: Option<&str>, slack: Option<&str>, text: &str, sender: Option<&dyn Sender>) -> Vec<String> {
    let mut results = Vec::new();
    if discord.is_some_and(|u| !u.is_empty()) {
        results.push(send_discord(discord, text, sender));
    }
    if slack.is_some_and(|u| !u.is_empty()) {
        results.push(send_slack(slack, text, sender));
    }
    if results.is_empty() {
        results.push(NO_WEBHOOKS.to_string());
    }
    results
}

pub fn any_failed(results: &[String]) -> bool {
    results.iter().any(|r| r.contains(": error"))
}

// --------------------------------------------------------------------------- short change alerts

/// `title` in bold (Discord `**`, Slack `*`) then one bullet per line; blank lines dropped.
pub fn format_changes(title: &str, lines: &[String], style: &str) -> String {
    let bold = if style == "discord" { "**" } else { "*" };
    let t = py_strip(title);
    let head = if !t.is_empty() { format!("{bold}{t}{bold}") } else { String::new() };
    let body: Vec<String> =
        lines.iter().filter(|ln| !py_strip(ln).is_empty()).map(|ln| format!("{BULLET} {}", split_ws(ln).join(" "))).collect();
    let mut all = Vec::new();
    if !head.is_empty() {
        all.push(head);
    }
    all.extend(body);
    all.join("\n")
}

/// Post a short bulleted alert (title + one bullet per line) to every configured webhook, in
/// messages of at most 1500 chars (split on bullet boundaries). Same result strings as
/// [`notify_all`].
pub fn notify_changes(settings: &Settings, title: &str, lines: &[String], sender: Option<&dyn Sender>) -> Vec<String> {
    let (discord, slack) = hooks(settings);
    notify_changes_urls(discord.as_deref(), slack.as_deref(), title, lines, sender)
}

/// [`notify_changes`] with the webhook URLs given directly.
pub fn notify_changes_urls(discord: Option<&str>, slack: Option<&str>, title: &str, lines: &[String], sender: Option<&dyn Sender>) -> Vec<String> {
    let mut results = Vec::new();
    if discord.is_some_and(|u| !u.is_empty()) {
        let chunks = chunk_text(&format_changes(title, lines, "discord"), CHANGES_CHUNK);
        results.push(send_chunks("discord", discord, &chunks, &discord_payload, sender));
    }
    if slack.is_some_and(|u| !u.is_empty()) {
        let chunks = chunk_text(&format_changes(title, lines, "slack"), CHANGES_CHUNK);
        results.push(send_chunks("slack", slack, &chunks, &slack_payload, sender));
    }
    if results.is_empty() {
        results.push(NO_WEBHOOKS.to_string());
    }
    results
}
