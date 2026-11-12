//! Rust port of `fantasy_manager/cli.py`: the typer app (root callback with `--league/-l` and
//! `--json`), the command registry and dispatch. Owner: work package `W9-cli-core`.
//!
//! # Command registry (how other packages add commands)
//! Every top-level command (or sub-app such as `harness`, `backtest`, `auth`) is one
//! [`CommandSpec`]. Each command module owns a marker type here ([`Recs`] for `cmd_recs.rs`,
//! [`Analysis`], [`Harness`], [`Backtest`], ...) and registers its commands with an inherent
//! method in ITS OWN file, without editing this one:
//!
//! ```ignore
//! use crate::app::CommandSpec;
//! impl crate::app::Recs {
//!     pub fn commands(&self) -> Vec<CommandSpec> {
//!         vec![
//!             crate::command_spec!("trades", 80, TradesArgs, "Trade ideas ...", trades),
//!             CommandSpec { name: "trade-eval", order: 90, build: || ..., run: |cli, m| ... },
//!         ]
//!     }
//! }
//! ```
//! `order` is the position in `fm --help` (Python registration order: see [`ORDER`]); `run`
//! receives the root context ([`Cli`]: root `--league` / `--json`, settings, clock, snapshot) and
//! the sub-command's matches. Sub-apps build a `clap::Command` with their own sub-commands and
//! dispatch inside `run`. Errors: return [`crate::common::fail`] (prints `Error: ...`, exit 1) or
//! [`crate::common::Exit`] (code only); any other error prints `Error: <e>` and exits 1.
//!
//! Rust-only, hidden: root `--from-snapshot <fixture dir>` loads every league from a parity
//! snapshot (`pipeline::from_snapshot`; a directory holding `<league>/snapshot.json` works too)
//! with the snapshot's pinned clock; `FM_NOW=<ISO datetime>` pins the clock otherwise. In
//! snapshot mode the data dir is `FM_DATA_DIR` if set, else the fixtures' `_datadir`, and the
//! season length is pinned to Python's 82 games in 186 days.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Local, TimeZone, Utc};
use clap::{Arg, ArgAction, ArgMatches, Command};
use fm_core::config::Settings;
use fm_providers::enrich::Clock;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::common::{Cli, Exit};

/// One top-level command of `fm` (see the module docs).
#[derive(Clone, Copy)]
pub struct CommandSpec {
    pub name: &'static str,
    /// position in `fm --help` (lower first)
    pub order: u32,
    /// the clap command (its name must equal `name`)
    pub build: fn() -> Command,
    pub run: fn(&mut Cli, &ArgMatches) -> anyhow::Result<()>,
}

/// A [`CommandSpec`] for a command whose arguments are one `#[derive(clap::Args)]` struct:
/// `command_spec!("name", order, ArgsType, "help", handler)` where
/// `handler: fn(&mut Cli, &ArgsType) -> anyhow::Result<()>`.
#[macro_export]
macro_rules! command_spec {
    ($name:literal, $order:expr, $args:ty, $about:expr, $handler:path) => {
        $crate::app::CommandSpec {
            name: $name,
            order: $order,
            build: || <$args as clap::Args>::augment_args(clap::Command::new($name).about($about)),
            run: |cli, m| {
                let a = <$args as clap::FromArgMatches>::from_arg_matches(m)?;
                $handler(cli, &a)
            },
        }
    };
}

/// One marker per command module. A module registers its commands by adding an inherent
/// method to its marker, anywhere in its own file:
/// `impl crate::app::Recs { pub fn commands(&self) -> Vec<CommandSpec> { vec![...] } }`.
/// Inherent methods take precedence over the [`NoCommands`] fallback below, so a module that
/// has not defined one yet simply contributes nothing (no edit of this file needed).
pub struct League;
pub struct Admin;
pub struct Recs;
pub struct Analysis;
pub struct Harness;
pub struct Backtest;

/// Fallback `commands()` for marker types whose module defines none.
pub trait NoCommands {
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
}

impl<T> NoCommands for T {}

/// Every module's commands (`cmd_league`, `cmd_admin`, `cmd_recs`, `cmd_analysis`,
/// `cmd_harness`, `cmd_backtest`).
#[allow(unused_imports)]
fn all_specs() -> Vec<CommandSpec> {
    use NoCommands as _;
    let mut v = Vec::new();
    v.extend(League.commands());
    v.extend(Admin.commands());
    v.extend(Recs.commands());
    v.extend(Analysis.commands());
    v.extend(Harness.commands());
    v.extend(Backtest.commands());
    v
}

/// Python registration order (typer lists commands first, then sub-apps), for `order`.
pub const ORDER: [(&str, u32); 32] = [
    ("settings", 10),
    ("picks", 20),
    ("roster", 30),
    ("waivers", 40),
    ("lineup", 50),
    ("injuries", 60),
    ("sync", 70),
    ("trades", 80),
    ("trade-eval", 90),
    ("flags", 100),
    ("advise", 110),
    ("alerts", 120),
    ("trending", 130),
    ("lines", 140),
    ("goalies", 150),
    ("news", 160),
    ("ask", 170),
    ("report", 180),
    ("notify", 190),
    ("mode", 200),
    ("web", 220),
    ("web-export", 230),
    ("schedule", 240),
    ("playoffs", 250),
    ("outlook", 260),
    ("matchup", 270),
    ("plan", 280),
    ("claims", 290),
    ("backtest", 1000),
    ("harness", 1010),
    ("auth", 1020),
    ("", u32::MAX),
];

pub const ABOUT: &str = "Fantasy hockey manager: roster values, lineups, waiver pickups, injuries and more.";

/// The registered commands, sorted by `order` then name (duplicates: first wins).
pub fn commands() -> Vec<CommandSpec> {
    let mut v = all_specs();
    v.sort_by(|a, b| (a.order, a.name).cmp(&(b.order, b.name)));
    let mut seen = std::collections::HashSet::new();
    v.retain(|c| seen.insert(c.name));
    v
}

/// The whole clap tree.
pub fn build_cli() -> Command {
    let mut root = Command::new("fm")
        .about(ABOUT)
        .arg_required_else_help(true)
        .subcommand_required(true)
        .arg(
            Arg::new("league")
                .long("league")
                .short('l')
                .value_parser(["espn", "fantrax"])
                .default_value("espn")
                .help("Which league to use."),
        )
        .arg(Arg::new("json").long("json").action(ArgAction::SetTrue).help("Print JSON instead of tables."))
        .arg(Arg::new("from_snapshot").long("from-snapshot").value_name("DIR").hide(true).value_parser(clap::value_parser!(PathBuf)));
    for c in commands() {
        root = root.subcommand((c.build)());
    }
    root
}

// --------------------------------------------------------------------------- clock / settings

/// `FM_NOW` (ISO datetime; naive = local time) or the system clock.
pub fn clock_from_env() -> anyhow::Result<Clock> {
    match std::env::var("FM_NOW").ok().filter(|s| !s.trim().is_empty()) {
        None => Ok(Clock::system()),
        Some(s) => {
            let s = s.trim();
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Ok(Clock::at(dt.with_timezone(&Utc)));
            }
            if let Some((naive, off)) = fm_pycompat::time::fromisoformat(s) {
                let utc = match off {
                    Some(secs) => naive - chrono::Duration::seconds(secs as i64),
                    None => match Local.from_local_datetime(&naive).earliest() {
                        Some(l) => l.with_timezone(&Utc).naive_utc(),
                        None => naive,
                    },
                };
                return Ok(Clock::at(Utc.from_utc_datetime(&utc)));
            }
            if let Some(d) = fm_pycompat::time::date_fromisoformat(s) {
                let naive = d.and_hms_opt(12, 0, 0).unwrap_or_default();
                let l = Local.from_local_datetime(&naive).earliest().map(|l| l.with_timezone(&Utc));
                return Ok(Clock::at(l.unwrap_or_else(|| Utc.from_utc_datetime(&naive))));
            }
            anyhow::bail!("FM_NOW={s:?} is not an ISO datetime")
        }
    }
}

/// The snapshot file / clock for `--from-snapshot <dir>` and `league`.
pub fn snapshot_dir_for(root: &Path, league: &str) -> PathBuf {
    let sub = root.join(league);
    if sub.join("snapshot.json").is_file() {
        sub
    } else {
        root.to_path_buf()
    }
}

fn snapshot_data_dir(root: &Path, league: &str) -> PathBuf {
    if let Some(d) = std::env::var_os("FM_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let file = fm_providers::snapshot::snapshot_file(&snapshot_dir_for(root, league));
    file.parent()
        .and_then(Path::parent)
        .map(|p| p.join("_datadir"))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| file.parent().map(Path::to_path_buf).unwrap_or_default())
}

/// The pinned clock of a snapshot (its `now`).
fn snapshot_clock(root: &Path, league: &str) -> anyhow::Result<Clock> {
    let file = fm_providers::snapshot::snapshot_file(&snapshot_dir_for(root, league));
    let text = std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("reading {}: {e}", file.display()))?;
    // only the clock fields are needed here: avoid deserializing the whole context twice
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let now = v.get("now").and_then(|x| x.as_str()).and_then(|s| DateTime::parse_from_rfc3339(s).ok());
    let today = v.get("today").and_then(|x| x.as_str()).and_then(fm_pycompat::time::date_fromisoformat);
    match (now, today) {
        (Some(n), Some(t)) => {
            let n = n.with_timezone(&Utc);
            Ok(Clock { now: n.timestamp() as f64 + n.timestamp_subsec_micros() as f64 / 1e6, today: t })
        }
        _ => anyhow::bail!("{} has no pinned clock", file.display()),
    }
}

// --------------------------------------------------------------------------- log capture

/// Active `collect_logs` scopes: (target prefix, label, sink).
pub(crate) static CAPTURES: Mutex<Vec<(String, String, Vec<String>)>> = Mutex::new(Vec::new());

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        }
    }
}

/// Python `_LogCollector`: WARNING+ events whose target starts with an active scope's prefix are
/// appended to that scope as `"<label>: <message>"`.
struct CaptureLayer;

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        let mut caps = CAPTURES.lock().unwrap_or_else(|e| e.into_inner());
        if caps.is_empty() {
            return;
        }
        let target = event.metadata().target();
        let mut msg = MessageVisitor(String::new());
        event.record(&mut msg);
        for (prefix, label, sink) in caps.iter_mut() {
            if target.starts_with(prefix.as_str()) {
                sink.push(format!("{label}: {}", msg.0));
            }
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("FM_LOG")
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(false)
        .with_level(false)
        .with_filter(filter);
    let _ = tracing_subscriber::registry().with(CaptureLayer).with(fmt).try_init();
}

// --------------------------------------------------------------------------- run

fn panic_text(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

/// Parse `args` (incl. argv[0]) and run; returns the process exit code.
pub fn run_args<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let matches = match build_cli().try_get_matches_from(args) {
        Ok(m) => m,
        Err(e) => {
            let _ = e.print();
            return e.exit_code();
        }
    };
    let league = matches.get_one::<String>("league").cloned().unwrap_or_else(|| "espn".into());
    let json = matches.get_flag("json");
    let snapshot = matches.get_one::<PathBuf>("from_snapshot").cloned();
    let (clock, settings) = match &snapshot {
        Some(root) => {
            let clock = match snapshot_clock(root, &league) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    return 1;
                }
            };
            let settings = Settings { fm_data_dir: snapshot_data_dir(root, &league), fm_offline: true, ..Settings::default() };
            // parity snapshots were captured by Python, which assumed 82 games in 186 days
            // whatever the schedule said; the live path reads the season length from the schedule
            fm_engine::valuation::adjust::pin_season_length(Some(fm_engine::valuation::adjust::SeasonLength::DEFAULT));
            // and valued an injured player's missed games at zero even when an IR slot held him
            fm_engine::valuation::valuate::disable_ir_fill(true);
            // and valued a goalie start in the daily lineup without the opponent, floored at zero
            fm_engine::recommend::lineup::disable_goalie_matchup(true);
            // and used the ESPN-point thresholds unscaled in every league, with no error ranges
            fm_engine::scoring::disable_point_scale(true);
            // and valued a roster by its best starting lineup, the bench and a third goalie at zero
            fm_engine::recommend::started::disable_started_games(true);
            // and projected goalie wins and shots against from the goalie, not his team
            fm_engine::valuation::valuate::disable_team_goalie(true);
            // and valued the dynasty future with one replacement level and a rising goalie curve
            fm_engine::valuation::dynasty::disable_positional_dynasty(true);
            // and scored an offer that is even by market value at 50%, without checking it from
            // the other manager's seat
            fm_engine::recommend::trades::pin_python_acceptance(true);
            (clock, settings)
        }
        None => {
            let clock = match clock_from_env() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    return 1;
                }
            };
            match Settings::load(clock.today) {
                Ok(s) => (clock, s),
                Err(e) => {
                    eprintln!("Error: invalid settings: {e:#}");
                    return 1;
                }
            }
        }
    };
    let mut cli = Cli { league, json, settings, snapshot, clock };
    let Some((name, sub)) = matches.subcommand() else { return 2 };
    let Some(spec) = commands().into_iter().find(|c| c.name == name) else { return 2 };
    let res = catch_unwind(AssertUnwindSafe(|| (spec.run)(&mut cli, sub)));
    match res {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => error_exit(&e),
        Err(p) => {
            eprintln!("Error: {}", panic_text(p));
            1
        }
    }
}

/// Print a command error the way Python surfaces it; returns the exit code.
pub fn error_exit(e: &anyhow::Error) -> i32 {
    if let Some(Exit(code)) = e.downcast_ref::<Exit>() {
        return *code;
    }
    if let Some(le) = e.downcast_ref::<fm_providers::pipeline::LoadError>() {
        return match le {
            fm_providers::pipeline::LoadError::Unavailable(m) => {
                eprintln!("{m}");
                2
            }
            fm_providers::pipeline::LoadError::Failed(m) => {
                eprintln!("Error: {m}");
                1
            }
        };
    }
    eprintln!("Error: {e:#}");
    1
}

/// The binary's entry point.
pub fn run() -> i32 {
    init_tracing();
    run_args(std::env::args_os())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_holds_core_commands_in_python_order() {
        let names: Vec<&str> = commands().iter().map(|c| c.name).collect();
        for n in ["settings", "picks", "roster", "waivers", "lineup", "injuries", "sync", "mode", "auth", "web", "web-export"] {
            assert!(names.contains(&n), "{n} missing from {names:?}");
        }
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("settings") < pos("picks") && pos("picks") < pos("roster") && pos("sync") < pos("mode"));
        build_cli().debug_assert();
    }

    #[test]
    fn every_command_has_help() {
        let root = build_cli();
        for c in root.get_subcommands() {
            assert!(c.get_about().is_some(), "{} has no help", c.get_name());
        }
    }

    #[test]
    fn usage_errors_exit_2() {
        assert_eq!(run_args(["fm", "roster", "--league", "yahoo"]), 2);
        assert_eq!(run_args(["fm", "no-such-command"]), 2);
        assert_eq!(run_args(["fm", "mode", "contend", "--clear", "--json"]), 1);
    }

    #[test]
    fn fm_now_parses() {
        std::env::set_var("FM_NOW", "2026-11-04T15:00:00+00:00");
        let c = clock_from_env().unwrap();
        std::env::remove_var("FM_NOW");
        assert_eq!(c.now, 1793804400.0);
    }
}
