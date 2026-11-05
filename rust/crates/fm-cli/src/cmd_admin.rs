//! Admin commands of `fantasy_manager/cli.py`: `mode`, `auth fantrax` (`_ts`), `web`,
//! `web-export`. Owner: work package `W9-cli-core`.
//!
//! `web` / `web-export` are wired to the not-yet-ported `fm_web::app` (W10-web-app): until it
//! exists they fail like Python's missing-extras path ("the web dashboard is not available").
//! Hook-up when W10-web-app lands: in [`web`] call `fm_web::app::run(host, port, league,
//! &cli.settings)` (map `InsecureBindError` to `fail`), in [`web_export`] build the auth-less
//! router and call `fm_web::export::export_site(router, out, &["espn", "fantrax"], limit)` on a
//! tokio runtime, then print as below.

use std::path::PathBuf;

use anyhow::Result;
use chrono::{Local, TimeZone};
use clap::{Args, FromArgMatches};
use fm_core::prefs::{dynasty_mode_info, mode_source_label, prefs_path, set_pref, DYNASTY_MODE_KEY};
use fm_core::ProviderError;
use fm_providers::fantrax::FantraxProvider;
use fm_providers::fantrax_auth::{fmt_age, FantraxAuth};
use serde_json::{json, Value};

use crate::app::CommandSpec;
use crate::common::*;

// --------------------------------------------------------------------------- mode

#[derive(Args, Clone, Debug, Default)]
pub struct ModeArgs {
    /// contend, balanced or rebuild. Omit to show the current mode.
    #[arg(value_enum)]
    pub mode: Option<ModeName>,
    /// Forget the saved mode (fall back to FANTRAX_MODE).
    #[arg(long = "clear")]
    pub clear: bool,
    /// Print JSON instead of tables.
    #[arg(long = "json")]
    pub json: bool,
}


pub fn mode_cmd(cli: &mut Cli, a: &ModeArgs) -> Result<()> {
    let as_json = cli.as_json(a.json);
    if a.mode.is_some() && a.clear {
        return Err(fail("give a mode or --clear, not both"));
    }
    let dir = cli.settings.fm_data_dir.clone();
    let path = prefs_path(&dir);
    let res = if let Some(m) = a.mode {
        set_pref(DYNASTY_MODE_KEY, Some(Value::String(m.mode().as_str().into())), &dir)
    } else if a.clear {
        set_pref(DYNASTY_MODE_KEY, None, &dir)
    } else {
        Ok(())
    };
    if let Err(e) = res {
        return Err(fail(format!("could not save {}: {e:#}", path.display())));
    }
    let (current, source) = dynasty_mode_info(&cli.settings);
    if as_json {
        dump(&json!({"mode": current.as_str(), "source": source, "source_label": mode_source_label(&source),
                     "prefs_file": path.to_string_lossy(), "changed": a.mode.is_some() || a.clear}));
        return Ok(());
    }
    if a.mode.is_some() {
        echo(format!(
            "Dynasty mode set to {} (saved to {}; a running dashboard recalculates on its next page load).",
            current.as_str(),
            path.display()
        ));
    } else if a.clear {
        echo(format!("Saved dynasty mode cleared; now {} (from {}).", current.as_str(), mode_source_label(&source)));
    } else {
        echo(format!("Dynasty mode: {} (from {})", current.as_str(), mode_source_label(&source)));
        echo("Set it with `fm mode contend|balanced|rebuild` or the dashboard's Mode toggle (saved, wins over FANTRAX_MODE), or --mode on a single command.");
    }
    Ok(())
}

// --------------------------------------------------------------------------- auth

#[derive(Args, Clone, Debug, Default)]
pub struct AuthFantraxArgs {
    /// Log in now with FANTRAX_USERNAME / FANTRAX_PASSWORD and save the session.
    #[arg(long = "login")]
    pub login: bool,
    /// Show the saved session and which credentials are configured (present/absent only).
    #[arg(long = "status")]
    pub status: bool,
    /// Delete the saved session file.
    #[arg(long = "logout")]
    pub logout: bool,
    /// Make one cheap authenticated request (keeps the session in use; re-logs in if needed).
    #[arg(long = "ping")]
    pub ping: bool,
    /// Print JSON instead of tables.
    #[arg(long = "json")]
    pub json: bool,
}

/// The `auth` sub-app (`auth_app`).
fn auth_spec() -> CommandSpec {
    CommandSpec {
        name: "auth",
        order: 1020,
        build: || {
            clap::Command::new("auth")
                .about("Log in to a league site and manage the saved session.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(AuthFantraxArgs::augment_args(clap::Command::new("fantrax").about(
                    "Fantrax login: --login, --status (default), --logout, --ping. Never prints cookies or passwords.",
                )))
        },
        run: |cli, m| match m.subcommand() {
            Some(("fantrax", sub)) => auth_fantrax(cli, &AuthFantraxArgs::from_arg_matches(sub)?),
            _ => Err(Exit(2).into()),
        },
    }
}

/// `_ts(v)`: epoch seconds -> local ISO time (seconds), None for 0 / missing.
pub fn ts(v: &Value) -> Value {
    let Some(x) = v.as_f64().filter(|x| *x != 0.0) else { return Value::Null };
    match Local.timestamp_opt(x.floor() as i64, 0).single() {
        Some(t) => Value::String(t.format("%Y-%m-%dT%H:%M:%S%:z").to_string()),
        None => Value::Null,
    }
}

fn provider_error(e: &anyhow::Error) -> Option<String> {
    e.chain().find_map(|c| c.downcast_ref::<ProviderError>()).map(|p| p.to_string())
}

pub fn auth_fantrax(cli: &mut Cli, a: &AuthFantraxArgs) -> Result<()> {
    let as_json = cli.as_json(a.json);
    if [a.login, a.logout, a.ping].iter().filter(|x| **x).count() > 1 {
        return Err(fail("give only one of --login, --logout, --ping (with --status if you like)"));
    }
    let settings = cli.settings.clone();
    let mut auth = FantraxAuth::new(settings.clone());
    let mut out = serde_json::Map::new();
    if a.logout {
        let done = auth.logout();
        out.insert("logged_out".into(), json!(done));
        if !as_json {
            echo(if done { "Saved Fantrax session deleted." } else { "No saved Fantrax session." });
        }
    }
    if a.login {
        if let Err(e) = auth.refresh() {
            return Err(fail(provider_error(&e).unwrap_or_else(|| format!("{e:#}"))));
        }
        out.insert("login".into(), json!("ok"));
        if !as_json {
            echo(format!("Logged in to Fantrax; session saved to {}.", auth.path().display()));
        }
    }
    if a.ping {
        if settings.fm_offline {
            return Err(fail("--ping needs the network; unset FM_OFFLINE"));
        }
        let provider = FantraxProvider::new(settings.clone(), false).with_auth(FantraxAuth::new(settings.clone()));
        let p = match fm_core::provider::LeagueProvider::ping(&provider) {
            Ok(p) => p,
            Err(e) => return Err(fail(provider_error(&e).unwrap_or_else(|| format!("{e:#}")))),
        };
        if !as_json {
            let name = match p.get("league_name") {
                Some(Value::String(s)) if !s.is_empty() => format!(" ({s})"),
                _ => String::new(),
            };
            let lid = match p.get("league_id") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            let src = match p.get("session_source") {
                Some(Value::String(s)) => s.clone(),
                _ => "None".into(),
            };
            echo(format!("Fantrax session OK for league {lid}{name}; session from {src}."));
            for w in provider.all_warnings() {
                echo(format!("! {w}"));
            }
        }
        out.insert("ping".into(), p);
        auth = FantraxAuth::new(settings.clone());
    }
    if a.status || !(a.login || a.logout || a.ping) {
        let st = auth.status();
        let mut s = st.as_object().cloned().unwrap_or_default();
        s.insert("last_login_attempt".into(), ts(&st["last_login_attempt"]));
        s.insert("last_ping_at".into(), ts(&st["last_ping_at"]));
        out.insert("status".into(), Value::Object(s));
        if !as_json {
            let file = st["session_file"].as_str().unwrap_or("");
            if st["saved_session"].as_bool().unwrap_or(false) {
                let state = if st["saved_session_expired"].as_bool().unwrap_or(false) { "known expired" } else { "in use first" };
                let src = st["saved_session_source"].as_str().filter(|s| !s.is_empty()).unwrap_or("?");
                echo(format!(
                    "Saved session: yes, {} old, from {src} ({state}); {file}",
                    fmt_age(st["saved_session_age_seconds"].as_f64())
                ));
            } else {
                echo(format!("Saved session: none ({file})"));
            }
            let creds: Vec<String> = st["credentials"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| format!("{k} {}", if v.as_bool().unwrap_or(false) { "present" } else { "absent" }))
                        .collect()
                })
                .unwrap_or_default();
            echo(format!("Configured: {}", creds.join(", ")));
            let now = auth.now();
            match st["last_login_attempt"].as_f64().filter(|x| *x != 0.0) {
                Some(t) => {
                    let res = match st["last_login_ok"].as_bool() {
                        Some(true) => "succeeded",
                        Some(false) => "failed",
                        None => "unknown",
                    };
                    let err = if st["last_login_ok"].as_bool() == Some(false) {
                        let e = match &st["last_login_error"] {
                            Value::String(s) => s.clone(),
                            Value::Null => "None".into(),
                            v => v.to_string(),
                        };
                        format!(" ({e})")
                    } else {
                        String::new()
                    };
                    echo(format!("Last login: {res}{err}, {} ago", fmt_age(Some(now - t))));
                }
                None => echo("Last login: never"),
            }
            if let Some(t) = st["last_ping_at"].as_f64().filter(|x| *x != 0.0) {
                let ok = if st["last_ping_ok"].as_bool().unwrap_or(false) { "ok" } else { "failed" };
                echo(format!("Last ping: {ok}, {} ago", fmt_age(Some(now - t))));
            }
            let wait = st["login_allowed_in_seconds"].as_f64().unwrap_or(0.0);
            if wait > 0.0 {
                echo(format!("Next login attempt allowed in {}.", fmt_age(Some(wait))));
            }
            let has = |k: &str| st["credentials"][k].as_bool().unwrap_or(false);
            if !(has("FANTRAX_USERNAME") && has("FANTRAX_PASSWORD")) {
                echo("Tip: set FANTRAX_USERNAME and FANTRAX_PASSWORD in .env so fm logs in and refreshes the session itself.");
            }
        }
    }
    if as_json {
        dump(&Value::Object(out));
    }
    Ok(())
}

// --------------------------------------------------------------------------- web

#[derive(Args, Clone, Debug)]
pub struct WebArgs {
    /// Interface to bind (127.0.0.1 = this PC only; 0.0.0.0 = every interface, needs FM_WEB_PASSWORD or FM_WEB_ALLOW_INSECURE=1).
    #[arg(long = "host", default_value = "127.0.0.1")]
    pub host: String,
    /// Port to listen on.
    #[arg(long = "port", default_value_t = 8765)]
    pub port: u16,
    /// Which league to use.
    #[arg(long = "league", short = 'l', value_enum)]
    pub league: Option<LeagueName>,
}


pub fn web(cli: &mut Cli, a: &WebArgs) -> Result<()> {
    let (_league, _) = cli.opts(&LeagueOpts { league: a.league, json: false });
    let password_set = cli.settings.fm_web_password.as_ref().is_some_and(|p| !p.expose().is_empty());
    if let Err(e) = fm_web::auth::check_bind(&a.host, password_set, cli.settings.fm_web_allow_insecure) {
        return Err(fail(e));
    }
    Err(fail("the web dashboard is not available (fm_web::app is not ported yet; use the Python `fm web`)"))
}

#[derive(Args, Clone, Debug)]
pub struct WebExportArgs {
    /// Folder to write (emptied first).
    #[arg(long = "out", short = 'o', default_value = "site")]
    pub out: PathBuf,
    /// Most pages to write.
    #[arg(long = "limit", default_value_t = 2500)]
    pub limit: usize,
}


pub fn web_export(_cli: &mut Cli, a: &WebExportArgs) -> Result<()> {
    let _ = (&a.out, a.limit);
    Err(fail("the web dashboard is not available (fm_web::app is not ported yet; use the Python `fm web-export`)"))
}

impl crate::app::Admin {
    /// This module's commands (see `app` for the registry).
    pub fn commands(&self) -> Vec<crate::app::CommandSpec> {
        vec![
            crate::command_spec!("mode", 200, ModeArgs, "Show or set the dynasty mode used by every command and the dashboard (saved in prefs.json).", mode_cmd),
            crate::command_spec!("web", 220, WebArgs, "Start the mobile-friendly web dashboard (needs the optional web extras).\n\nWith FM_WEB_PASSWORD set every page needs a login. Without it the dashboard answers only on this machine, and a non-loopback --host is refused unless FM_WEB_ALLOW_INSECURE=1.", web),
            crate::command_spec!("web-export", 230, WebExportArgs, "Write a read-only static snapshot of the dashboard (both leagues) for static hosting.\nThe files hold league data in clear text: encrypt them before publishing (docs/hosting.md).", web_export),
            auth_spec(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_formats_local_seconds() {
        assert_eq!(ts(&Value::Null), Value::Null);
        assert_eq!(ts(&json!(0.0)), Value::Null);
        let s = ts(&json!(1_700_000_000.5));
        assert!(s.as_str().unwrap().len() == 25, "{s}");
    }
}
