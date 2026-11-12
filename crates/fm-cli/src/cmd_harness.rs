//! `fm harness ...` (port of `fantasy_manager/cli_harness.py`): capture and match
//! recommendations against what happened, grade them and keep the bar (`grade`, `status`),
//! correct a bounded set of valuation parameters (`refit`, `rollback`, `params`,
//! `auto-apply`), plus `daily`, `pregame`, `odds`, `ledger` and `rebuild`. Owner: work package
//! `W10-cli-ops`.
//!
//! The sub-app ignores the global `--json` (each command has its own `--json`). Every
//! `json.dumps` keeps the options of the Python call: `indent=2, default=str` for the payload
//! dumps, `indent=2` for `rollback`, compact (default separators) for `auto-apply` and the
//! `rollback` error. Rich markup (colours, rules, bold) is dropped from the human output.
//!
//! Deviations: the clock is the run's (`FM_NOW` / snapshot) instead of `date.today()`; error
//! type names in `"<Type>: <msg>"` strings are always `Error`; the daily steps run in Python's
//! order but the league loads are sequential.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use chrono::{Datelike, Duration, NaiveDate};
use clap::{Args, FromArgMatches};
use fm_backtest::data::CountingFetcher;
use fm_core::prefs::{harness_auto_apply, set_pref, HARNESS_AUTO_APPLY_KEY};
use fm_engine::recommend::injuries::History;
use fm_engine::recommend::plan::{archive_plan, build_week_plan, PlanOpts};
use fm_harness::deployment::pull_deployment;
use fm_harness::episode_match::{match_episodes, store_lineup_lock};
use fm_harness::ingest::{ingest_archive, record_run};
use fm_harness::ledger::{Ledger, Row, TABLES};
use fm_harness::matchup_calib::{calibration_report, capture_matchup, summary_line, MIN_MATCHUPS};
use fm_harness::metrics::{grade_week, known_leagues, status_report};
use fm_harness::outcomes::{grade_outcomes, store_scoring};
use fm_harness::params_store::ParamsStore;
use fm_harness::realized::{pull_lineups, pull_realized, pull_transactions};
use fm_harness::refit::{auto_apply_start, current_knobs, daily_refit_mode, is_refit_day, next_refit_day, refit_start, run_refit, tier, RefitOptions, KNOBS};
use fm_http::HttpCache;
use fm_pycompat::pyjson::{dumps, DumpOpts};
use fm_sources::nhl::NhlClient;
use rusqlite::ToSql;
use serde_json::{json, Map as JMap, Value};

use crate::app::CommandSpec;
use crate::cmd_backtest::{fixed, load_league, load_recs, local_now, py_str};
use crate::common::*;

pub const LEAGUES: [&str; 2] = ["espn", "fantrax"];
pub const TX_LOOKBACK_DAYS: i64 = 7;

// --------------------------------------------------------------------------- registry

const LEAGUE_HELP: &str = "espn | fantrax | all";

/// `harness_app`.
pub fn harness_spec() -> CommandSpec {
    CommandSpec {
        name: "harness",
        order: 1010,
        build: || {
            clap::Command::new("harness")
                .about("Evaluation harness: capture recommendations, moves and results; match and grade them.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(DailyArgs::augment_args(clap::Command::new("daily").about(
                    "Idempotent daily capture: archive -> ingest -> transactions / lineups / NHL results -> match -> NHL deployment -> Daily Faceoff lines -> betting odds (-> grade on Mondays or with --grade).",
                )))
                .subcommand(PregameArgs::augment_args(clap::Command::new("pregame").about(
                    "Pre-game afternoon run (schedule it ~17:00 local): refresh starting goalies and injuries and report only what changed since this morning.",
                )))
                .subcommand(OddsArgs::augment_args(clap::Command::new("odds").about(
                    "Betting-market context: fetch NHL moneylines / totals from The Odds API (at most once a day), archive them and show the implied team totals.",
                )))
                .subcommand(LedgerArgs::augment_args(
                    clap::Command::new("ledger").about("Recent recommendation episodes with their match status (and my user-only moves)."),
                ))
                .subcommand(ReviewArgs::augment_args(clap::Command::new("review").about(
                    "How right the model was: every suggestion (taken or not) and your own moves marked against what the players scored so far, and whether its top free agents beat the market's.",
                )))
                .subcommand(RebuildArgs::augment_args(
                    clap::Command::new("rebuild").about("Drop and rebuild the archive-derived tables from data/archive, then re-match."),
                ))
                .subcommand(GradeArgs::augment_args(
                    clap::Command::new("grade").about("Grade every matured outcome, then compute that week's bar (metric snapshots + trust labels)."),
                ))
                .subcommand(StatusArgs::augment_args(
                    clap::Command::new("status").about("The bar (projection accuracy, hit rates, calibration, trust labels) and what the ledger holds."),
                ))
                .subcommand(RefitArgs::augment_args(
                    clap::Command::new("refit").about("Refit the eligible valuation parameters from the ledger (bounded and gated; docs/harness.md)."),
                ))
                .subcommand(RollbackArgs::augment_args(
                    clap::Command::new("rollback").about("Roll the active params version back (to its parent by default)."),
                ))
                .subcommand(ParamsArgs::augment_args(
                    clap::Command::new("params").about("The active valuation params (packaged fit + harness version) and the refittable values."),
                ))
                .subcommand(AutoApplyArgs::augment_args(clap::Command::new("auto-apply").about(
                    "Show or set whether `fm harness daily` may apply a passing Tier A refit by itself on a refit day (saved as harness_auto_apply in prefs.json, default on).",
                )))
        },
        run: |cli, m| match m.subcommand() {
            Some(("daily", s)) => daily_cmd(cli, &DailyArgs::from_arg_matches(s)?),
            Some(("pregame", s)) => pregame_cmd(cli, &PregameArgs::from_arg_matches(s)?),
            Some(("odds", s)) => odds_cmd(cli, &OddsArgs::from_arg_matches(s)?),
            Some(("ledger", s)) => ledger_cmd(cli, &LedgerArgs::from_arg_matches(s)?),
            Some(("rebuild", s)) => rebuild_cmd(cli, &RebuildArgs::from_arg_matches(s)?),
            Some(("review", s)) => review_cmd(cli, &ReviewArgs::from_arg_matches(s)?),
            Some(("grade", s)) => grade_cmd(cli, &GradeArgs::from_arg_matches(s)?),
            Some(("status", s)) => status_cmd(cli, &StatusArgs::from_arg_matches(s)?),
            Some(("refit", s)) => refit_cmd(cli, &RefitArgs::from_arg_matches(s)?),
            Some(("rollback", s)) => rollback_cmd(cli, &RollbackArgs::from_arg_matches(s)?),
            Some(("params", s)) => params_cmd(cli, &ParamsArgs::from_arg_matches(s)?),
            Some(("auto-apply", s)) => auto_apply_cmd(cli, &AutoApplyArgs::from_arg_matches(s)?),
            _ => Err(Exit(2).into()),
        },
    }
}

impl crate::app::Harness {
    pub fn commands(&self) -> Vec<CommandSpec> {
        vec![harness_spec()]
    }
}

// --------------------------------------------------------------------------- helpers

/// `_leagues(league)`.
fn leagues(league: &str) -> Result<Vec<String>> {
    let l = if league.is_empty() { "all".to_string() } else { league.to_lowercase() };
    if l == "all" || l == "both" {
        return Ok(LEAGUES.iter().map(|s| s.to_string()).collect());
    }
    if !LEAGUES.contains(&l.as_str()) {
        echo(format!("Unknown league {}; expected espn, fantrax or all.", py_repr(league)));
        return Err(Exit(2).into());
    }
    Ok(vec![l])
}

fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// `_parse_day(s)`.
fn parse_day(s: Option<&str>) -> Result<Option<NaiveDate>> {
    match s.filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => match fm_pycompat::time::date_fromisoformat(s) {
            Some(d) => Ok(Some(d)),
            None => {
                echo(format!("Bad date {}; use YYYY-MM-DD.", py_repr(s)));
                Err(Exit(2).into())
            }
        },
    }
}

fn open_ledger(cli: &Cli) -> Result<Ledger> {
    Ledger::open(cli.data_dir())
}

/// The valuation params of this data dir (Python's lazy `load_params()`).
fn install_params(cli: &Cli) {
    fm_providers::pipeline::install_params(&cli.settings);
}

/// `str(Path)`: separators normalised like `pathlib` on this platform.
fn path_str(p: &std::path::Path) -> String {
    p.components().collect::<PathBuf>().display().to_string()
}

fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// `last_monday(day)`.
pub fn last_monday(day: NaiveDate) -> NaiveDate {
    day - Duration::days(day.weekday().num_days_from_monday() as i64)
}

fn err_text(e: &anyhow::Error) -> String {
    format!("Error: {e:#}")
}

fn s_of<'a>(r: &'a Row, k: &str) -> &'a str {
    r.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn fv(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// `_f(v, nd, pct)`.
fn ff(v: Option<f64>, nd: usize, pct: bool) -> String {
    match v {
        None => "-".to_string(),
        Some(x) if pct => format!("{}%", fixed(x * 100.0, 0)),
        Some(x) => fixed(x, nd),
    }
}

/// `_num(v, nd)`: floats with `nd` decimals, ints as is, None as `-`.
fn num(v: &Value, nd: usize) -> String {
    match v {
        Value::Null => "-".to_string(),
        Value::Number(n) if n.is_f64() => fixed(n.as_f64().unwrap_or(0.0), nd),
        other => py_str(other),
    }
}

fn count(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn obj(v: &Value) -> JMap<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// A summary printed as `{"k": v, ...}` rows: the ledger's `Row` as a JSON object.
fn row_val(r: &Row) -> Value {
    Value::Object(r.clone())
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

// --------------------------------------------------------------------------- daily steps

fn archived(data_dir: &std::path::Path, league: &str, day: NaiveDate) -> bool {
    let d = data_dir.join("archive");
    ["projections", "recs"].iter().all(|k| d.join(format!("{k}-{league}-{}.json", iso(day))).exists())
}

/// One league's daily capture (summary + the league's NHL schedule); a league that does not load
/// is reported under `skipped`.
fn daily_league(
    cli: &Cli,
    lg: &str,
    ledger: &Ledger,
    today: NaiveDate,
    since: NaiveDate,
    archive: bool,
    force_archive: bool,
) -> Result<(JMap<String, Value>, Option<fm_harness::deployment::Schedule>)> {
    let data_dir = cli.data_dir().to_path_buf();
    let mut out = JMap::new();
    out.insert("league".into(), json!(lg));
    out.insert("warnings".into(), json!([]));
    let mut warnings: Vec<String> = Vec::new();
    let need_archive = archive && (force_archive || !archived(&data_dir, lg, today));
    let cache = cli.cache()?;
    let mut recs = Vec::new();
    let loaded = match load_league(cli, lg, &cache) {
        Ok(d) => {
            warnings.extend(d.provider.warnings().iter().cloned());
            if need_archive {
                recs = load_recs(cli, &d, &mut warnings);
            }
            Some(d)
        }
        Err(e) => {
            let msg = match e.downcast_ref::<fm_providers::pipeline::LoadError>() {
                Some(fm_providers::pipeline::LoadError::Unavailable(m)) | Some(fm_providers::pipeline::LoadError::Failed(m)) => m.clone(),
                None => err_text(&e),
            };
            out.insert("skipped".into(), json!(msg));
            None
        }
    };
    let now = cli.now();
    if let Some(d) = &loaded {
        // Python stamps the snapshot with naive local `datetime.now()`
        let r = match fm_report::pregame::write_daily_snapshot(lg, &cli.settings, &d.lc, &d.values, &cli.now_local()) {
            Ok(p) => format!("written ({})", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            Err(e) => format!("failed ({})", err_text(&e)),
        };
        out.insert("pregame_snapshot".into(), json!(r));
    }
    let local = local_now(cli);
    match &loaded {
        Some(d) if need_archive => {
            let (_, pst) = fm_engine::archive::archive_projections(&d.lc, &data_dir, Some(&d.values), None, local)?;
            let (_, rst) = fm_engine::archive::archive_recommendations(&recs, &d.lc.provider, &data_dir, d.lc.as_of, Some(&d.lc.league_id), local)?;
            let mut line = format!("projections {pst}, recs {rst} ({} recs)", recs.len());
            // the weekly plan's predicted totals, graded against the final matchup score
            let plan = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<String> {
                let history = cli.history()?;
                let opts = PlanOpts {
                    recs: Some(&recs),
                    provider: Some(d.provider.as_ref()),
                    history: &History::Store(&history),
                    data_dir: Some(&data_dir),
                    ..PlanOpts::default()
                };
                let wp = build_week_plan(&d.lc, &d.values, &opts, &now, &cli.memo());
                Ok(archive_plan(&wp, &data_dir, Some(&d.lc.league_id), &now).unwrap_or("n/a").to_string())
            }));
            match plan {
                Ok(Ok(s)) => line.push_str(&format!(", plan {s}")),
                Ok(Err(e)) => warnings.push(format!("weekly plan not archived: {e}")),
                Err(_) => warnings.push("weekly plan not archived: panic".to_string()),
            }
            out.insert("archive".into(), json!(line));
        }
        _ if archive => {
            let s = if loaded.is_some() || archived(&data_dir, lg, today) { "already archived today" } else { "not archived (league unavailable)" };
            out.insert("archive".into(), json!(s));
        }
        _ => {}
    }
    if let Some(d) = &loaded {
        store_scoring(ledger, lg, &d.lc.scoring)?;
        // lineup matching and grading below read the lock back from the ledger
        let lock = fm_engine::recommend::lineup_lock::lineup_lock_for(&d.lc, Some(d.provider.as_ref()));
        store_lineup_lock(ledger, lg, lock)?;
    }
    let ing = ingest_archive(ledger, &data_dir, Some(&[lg.to_string()]), false)?;
    out.insert(
        "ingest".into(),
        json!({"files_ingested": ing.files_ingested, "projections": ing.projections, "recs": ing.recs, "episodes": ing.episodes}),
    );
    warnings.extend(ing.errors.iter().cloned());
    if let Some(d) = &loaded {
        let n_warn = d.provider.warnings().len();
        let tx = pull_transactions(ledger, d.provider.as_ref(), lg, Some(since), Some(&d.lc))?;
        out.insert("transactions".into(), json!(tx));
        let lineup_day = if lg == "fantrax" { today } else { today - Duration::days(1) };
        let rows = pull_lineups(ledger, d.provider.as_ref(), lg, lineup_day, Some(&d.lc), Some(today))?;
        out.insert("lineups".into(), json!({"day": iso(lineup_day), "rows": rows}));
        warnings.extend(d.provider.warnings().iter().skip(n_warn).cloned());
    }
    let m = match_episodes(ledger, lg, Some(today), None)?;
    out.insert(
        "match".into(),
        json!({"episodes": m.episodes, "by_status": serde_json::to_value(&m.by_status)?, "decisions": serde_json::to_value(&m.decisions)?}),
    );
    if let Some(d) = &loaded {
        // matchup win-probability snapshot + final scores of ended periods
        let mut mc = capture_matchup(ledger, lg, &d.lc, Some(d.provider.as_ref()), &d.values, today, &now, &cli.memo(), None, None);
        if let Some(o) = mc.as_object_mut() {
            if let Some(Value::Array(w)) = o.shift_remove("warnings") {
                warnings.extend(w.iter().map(py_str));
            }
        }
        out.insert("matchup".into(), mc);
    }
    out.insert("warnings".into(), json!(warnings));
    let schedule = loaded.as_ref().filter(|d| !d.lc.schedule.is_empty()).map(|d| d.lc.schedule.clone());
    Ok((out, schedule))
}

fn nhl_client(cache: &Arc<HttpCache>, today: NaiveDate) -> NhlClient {
    let fetcher = Arc::new(CountingFetcher::new(cache.clone(), today, 0.0));
    NhlClient::new(fetcher.into_fetch(), None, today)
}

/// Per-game TOI / PP / goalie starts for `day` into the ledger; never fails.
fn deployment_step(cli: &Cli, ledger: &Ledger, day: NaiveDate, schedule: Option<&fm_harness::deployment::Schedule>) -> Value {
    let mut out = JMap::new();
    out.insert("day".into(), json!(iso(day)));
    let res: Result<()> = (|| {
        let Some(schedule) = schedule.filter(|s| !s.is_empty()) else {
            out.insert("skipped".into(), json!("no NHL schedule loaded (league loads failed); nothing recorded"));
            return Ok(());
        };
        let cache = cli.cache()?;
        let client = nhl_client(&cache, cli.today());
        let r = pull_deployment(ledger, &client, day, None, None, Some(schedule), false)?;
        out.insert("skaters".into(), r.get("skaters").cloned().unwrap_or(json!(0)));
        out.insert("goalies".into(), r.get("goalies").cloned().unwrap_or(json!(0)));
        out.insert("not_final".into(), if truthy(r.get("not_final")) { r["not_final"].clone() } else { json!([]) });
        let scheduled = schedule.values().filter(|ds| ds.contains(&day)).count();
        out.insert("scheduled".into(), json!(scheduled));
        Ok(())
    })();
    if let Err(e) = res {
        out.insert("error".into(), json!(err_text(&e)));
    }
    Value::Object(out)
}

fn deployment_line(dp: &Value) -> String {
    let day = py_str(&dp["day"]);
    if truthy(dp.get("error")) {
        return format!("deployment {day}: {}", py_str(&dp["error"]));
    }
    if truthy(dp.get("skipped")) {
        return format!("deployment {day}: skipped ({})", py_str(&dp["skipped"]));
    }
    if truthy(dp.get("not_final")) {
        return format!(
            "deployment {day}: {} skater / {} goalie rows, games not final yet ({} teams scheduled); not marked pulled, the next run completes it",
            count(dp, "skaters"),
            count(dp, "goalies"),
            count(dp, "scheduled")
        );
    }
    if count(dp, "scheduled") == 0 {
        return format!("deployment {day}: no NHL regular-season games (schedule-confirmed), recorded");
    }
    format!("deployment {day}: {} skater / {} goalie rows", count(dp, "skaters"), count(dp, "goalies"))
}

/// Pull every NHL day the ledger missed since the first archived day (results, and deployment
/// when the schedule loaded), so a skipped daily run leaves no hole in the graded windows; `skip`
/// was pulled by this run already. Never fails.
fn catch_up_step(cli: &Cli, ledger: &Ledger, through: NaiveDate, skip: NaiveDate, schedule: Option<&fm_harness::deployment::Schedule>) -> Value {
    use fm_harness::realized::{backfill_start, unpulled_days};
    let mut out = JMap::new();
    let mut errors: Vec<String> = Vec::new();
    let res: Result<()> = (|| {
        let season_start = schedule.and_then(|s| s.values().flatten().min().copied());
        let Some(start) = backfill_start(ledger, through, season_start)? else { return Ok(()) };
        let cache = cli.cache()?;
        let client = nhl_client(&cache, cli.today());
        let mut done = Vec::new();
        for d in unpulled_days(ledger, "realized_pulls", start, through)?.into_iter().filter(|d| *d != skip) {
            match pull_realized(ledger, &client, d, None) {
                Ok(_) => done.push(iso(d)),
                Err(e) => errors.push(format!("NHL results {}: {}", iso(d), err_text(&e))),
            }
        }
        out.insert("realized_days".into(), json!(done));
        if let Some(schedule) = schedule.filter(|s| !s.is_empty()) {
            let mut dep = Vec::new();
            for d in unpulled_days(ledger, "deployment_pulls", start, through)?.into_iter().filter(|d| *d != skip) {
                match pull_deployment(ledger, &client, d, None, None, Some(schedule), false) {
                    Ok(_) => dep.push(iso(d)),
                    Err(e) => errors.push(format!("deployment {}: {}", iso(d), err_text(&e))),
                }
            }
            out.insert("deployment_days".into(), json!(dep));
        }
        Ok(())
    })();
    if let Err(e) = res {
        errors.push(err_text(&e));
    }
    out.insert("errors".into(), json!(errors));
    Value::Object(out)
}

fn catch_up_line(c: &Value) -> Option<String> {
    let days = |k: &str| c.get(k).and_then(|v| v.as_array()).map(|a| a.iter().map(py_str).collect::<Vec<_>>()).unwrap_or_default();
    let span = |v: &[String]| match (v.first(), v.last()) {
        (Some(a), Some(b)) if a != b => format!("{} day(s), {a} to {b}", v.len()),
        (Some(a), _) => format!("1 day, {a}"),
        _ => String::new(),
    };
    let (r, d) = (days("realized_days"), days("deployment_days"));
    let errs = days("errors");
    if r.is_empty() && d.is_empty() && errs.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !r.is_empty() {
        parts.push(format!("NHL results {}", span(&r)));
    }
    if !d.is_empty() {
        parts.push(format!("deployment {}", span(&d)));
    }
    let mut line = format!("caught up on missed days: {}", if parts.is_empty() { "none".to_string() } else { parts.join("; ") });
    if !errs.is_empty() {
        line.push_str(&format!(" ({} failed: {})", errs.len(), errs.join("; ")));
    }
    Some(line)
}

/// Make sure today's Daily Faceoff snapshot exists; never fails.
fn lines_step(cli: &Cli, today: NaiveDate) -> Value {
    use fm_providers::lines_enrich::{enrich_lines, LineSnapshotStore};
    let mut out = JMap::new();
    out.insert("day".into(), json!(iso(today)));
    let res: Result<()> = (|| {
        let store = LineSnapshotStore::new(cli.data_dir());
        if store.load(today).is_some() {
            out.insert("snapshot".into(), json!("already taken today"));
            return Ok(());
        }
        let cache = cli.cache()?;
        let mut ctx: fm_core::models::LeagueContext = serde_json::from_value(json!({
            "provider": "nhl", "league_id": "-", "season": today.year(), "name": "lines snapshot",
            "scoring": {"kind": "points"}, "roster_shape": {}, "teams": [], "free_agents": [],
            "matchup_period": null, "as_of": iso(today),
        }))?;
        let res = enrich_lines(&mut ctx, Some(&cache), None, Some(today), Some(&store), None, None, None, true, cli.clock);
        out.insert("snapshot".into(), json!(if res.snapshot_path.is_some() { "taken" } else { "not taken" }));
        out.insert("teams".into(), json!(res.teams));
        out.insert("starts".into(), json!(res.starts.len()));
        out.insert("warnings".into(), json!(res.warnings.iter().take(5).collect::<Vec<_>>()));
        Ok(())
    })();
    if let Err(e) = res {
        out.insert("error".into(), json!(err_text(&e)));
    }
    Value::Object(out)
}

fn lines_line(ln: &Value) -> String {
    if truthy(ln.get("error")) {
        return format!("lines {}: {}", py_str(&ln["day"]), py_str(&ln["error"]));
    }
    let extra = if ln.get("teams").is_some() {
        format!(" ({} teams, {} goalie reports)", count(ln, "teams"), count(ln, "starts"))
    } else {
        String::new()
    };
    format!("lines {}: Daily Faceoff snapshot {}{extra}", py_str(&ln["day"]), py_str(&ln["snapshot"]))
}

/// Fetch RotoWire / ESPN news into the 30-day store; never fails.
fn news_step(cli: &Cli) -> Value {
    let mut out = JMap::new();
    let res: Result<()> = (|| {
        let cache = cli.cache()?;
        let ft = fm_providers::enrich::make_fetch_text(cache, cli.today());
        let fetch = move |url: &str, params: Option<&fm_http::Params>| -> Result<String> {
            let empty = Vec::new();
            ft(url, params.unwrap_or(&empty), 0.0)
        };
        let (items, failed) = fm_sources::news::fetch_news(Some(&fetch), Some(cli.data_dir()), None, cli.now_utc());
        let oldest = items.iter().filter_map(|it| it.published.as_ref()).min_by_key(|p| p.local).map(|p| p.isoformat());
        out.insert("stored".into(), json!(items.len()));
        out.insert("oldest".into(), oldest.map(Value::from).unwrap_or(Value::Null));
        out.insert("failed".into(), json!(failed.iter().map(|(l, _)| format!("{l}: Error")).collect::<Vec<_>>()));
        Ok(())
    })();
    if let Err(e) = res {
        out.clear();
        out.insert("error".into(), json!(err_text(&e)));
    }
    Value::Object(out)
}

fn news_line(nw: &Value) -> String {
    if truthy(nw.get("error")) {
        return format!("news: {}", py_str(&nw["error"]));
    }
    let since = match nw.get("oldest").filter(|v| truthy(Some(v))) {
        Some(o) => format!(" since {}", py_str(o).chars().take(10).collect::<String>()),
        None => String::new(),
    };
    let failed = match nw.get("failed").and_then(|f| f.as_array()).filter(|f| !f.is_empty()) {
        Some(f) => format!(" (unavailable: {})", f.iter().map(py_str).collect::<Vec<_>>().join(", ")),
        None => String::new(),
    };
    format!("news: {} items stored{since}{failed}", count(nw, "stored"))
}

/// Betting-market odds for today: archive once per day, never fails. `games` stays a list of
/// [`fm_sources::odds::GameOdds`] values (`Vec`) next to the JSON summary.
fn odds_step(cli: &Cli, today: NaiveDate) -> (Value, Vec<fm_sources::odds::GameOdds>) {
    use fm_sources::odds::{archive_odds, load_odds, OddsClient};
    let mut out = JMap::new();
    out.insert("day".into(), json!(iso(today)));
    let mut games: Vec<fm_sources::odds::GameOdds> = Vec::new();
    let res: Result<()> = (|| {
        let mut client = OddsClient::from_settings(&cli.settings, None);
        if !client.available() {
            out.insert("skipped".into(), json!("ODDS_API_KEY not set"));
            return Ok(());
        }
        match load_odds(cli.data_dir(), today) {
            Some(snap) => {
                games = snap.games;
                out.insert("source".into(), json!("archive"));
                out.insert("archive".into(), json!("already archived today"));
                out.insert("fetched_at".into(), snap.header.get("fetched_at").cloned().unwrap_or(Value::Null));
                out.insert("quota".into(), snap.header.get("quota").cloned().unwrap_or(Value::Null));
            }
            None => {
                games = client.nhl_odds()?;
                let (_, st) = archive_odds(&games, cli.data_dir(), today, client.fetched_at, client.quota.as_ref(), &client.region, false)?;
                out.insert("source".into(), json!(if client.from_cache { "cache" } else { "api" }));
                out.insert("archive".into(), json!(st));
                out.insert(
                    "fetched_at".into(),
                    client.fetched_at.map(|t| json!(t.format("%Y-%m-%dT%H:%M:%S%.f+00:00").to_string())).unwrap_or(Value::Null),
                );
                out.insert("quota".into(), client.quota.as_ref().map(|q| json!(q)).unwrap_or(Value::Null));
                if !client.warnings.is_empty() {
                    out.insert("warnings".into(), json!(client.warnings));
                }
            }
        }
        out.insert("games".into(), json!(games.len()));
        Ok(())
    })();
    if let Err(e) = res {
        out.insert("error".into(), json!(err_text(&e)));
    }
    (Value::Object(out), games)
}

fn odds_line(o: &Value) -> String {
    if truthy(o.get("skipped")) {
        return format!("odds: skipped ({})", py_str(&o["skipped"]));
    }
    if truthy(o.get("error")) {
        return format!("odds: {}", py_str(&o["error"]));
    }
    let quota = match o.get("quota").and_then(|q| q.get("remaining")).filter(|v| !v.is_null()) {
        Some(q) => format!(", {} API credits left", py_str(q)),
        None => String::new(),
    };
    format!(
        "odds: {} upcoming games, archive {} (source {}{quota})",
        py_str(o.get("games").unwrap_or(&json!(0))),
        py_str(o.get("archive").unwrap_or(&Value::Null)),
        py_str(o.get("source").unwrap_or(&Value::Null))
    )
}

/// `_grade_leagues`: matured outcomes (as of `today`) then the bar for `week`, per league.
fn grade_leagues(ledger: &Ledger, lgs: &[String], week: NaiveDate, today: NaiveDate) -> Vec<Value> {
    let mut out = Vec::new();
    for lg in lgs {
        let mut r = JMap::new();
        r.insert("league".into(), json!(lg));
        let res: Result<()> = (|| {
            let gr = grade_outcomes(ledger, lg, today)?;
            r.insert("outcomes".into(), gr.to_dict());
            r.insert("outcomes_line".into(), json!(gr.line()));
            r.insert("week".into(), Value::Object(grade_week(ledger, lg, week)?));
            r.insert("matchup".into(), calibration_report(ledger, lg, MIN_MATCHUPS)?);
            Ok(())
        })();
        if let Err(e) = res {
            r.insert("error".into(), json!(err_text(&e)));
        }
        out.push(Value::Object(r));
    }
    out
}

/// `_run_refit(ledger, today, mode, only, leagues, force)`.
fn run_refit_dict(ledger: &Ledger, today: NaiveDate, mode: &str, only: Option<Vec<String>>, lgs: Option<Vec<String>>, force: bool) -> Result<Value> {
    let opts = RefitOptions { mode: mode.to_string(), only, leagues: lgs, force, ..RefitOptions::default() };
    Ok(run_refit(ledger, today, &opts)?.to_dict())
}

/// `_meta(ledger, key)` / `_set_meta`: the harness run-state kv.
fn meta_get(ledger: &Ledger, key: &str) -> Result<Option<String>> {
    let k = key.to_string();
    Ok(ledger.query("SELECT value FROM meta WHERE key=?", &[&k as &dyn ToSql])?.first().and_then(|r| r.get("value")).and_then(|v| v.as_str()).map(String::from))
}

fn meta_set(ledger: &Ledger, key: &str, value: &str) -> Result<()> {
    let (k, v) = (key.to_string(), value.to_string());
    ledger.execute("INSERT INTO meta(key, value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", &[&k as &dyn ToSql, &v])?;
    ledger.commit()
}

/// On a refit day (every 14 days from 2026-11-02) run the refit once; None otherwise.
fn daily_refit(cli: &Cli, ledger: &Ledger, today: NaiveDate) -> Option<Value> {
    let mode = daily_refit_mode(today, harness_auto_apply(cli.data_dir()))?;
    if meta_get(ledger, "refit:last_daily").ok().flatten().as_deref() == Some(iso(today).as_str()) {
        return None;
    }
    match run_refit_dict(ledger, today, mode, None, None, false) {
        Ok(r) => {
            let _ = meta_set(ledger, "refit:last_daily", &iso(today));
            Some(r)
        }
        Err(e) => Some(json!({"mode": mode, "error": err_text(&e)})),
    }
}

/// Post one line per league that did not load when webhooks are configured; None when not.
fn notify_failures(cli: &Cli, failures: &[(String, String)], command: &str) -> Option<Vec<String>> {
    use fm_report::pregame::{failure_message, has_webhooks};
    if failures.is_empty() || !has_webhooks(&cli.settings) {
        return None;
    }
    let text = failures.iter().map(|(lg, err)| format!("fm {command}: {}", failure_message(lg, err))).collect::<Vec<_>>().join("\n");
    Some(fm_report::notify::notify_all(&cli.settings, &text, None))
}

// --------------------------------------------------------------------------- daily

#[derive(Args, Clone, Debug)]
pub struct DailyArgs {
    #[arg(long, short = 'l', default_value = "all", help = LEAGUE_HELP)]
    pub league: String,
    /// Archive today's projections + recs first when not done yet.
    #[arg(long, overrides_with = "no_archive")]
    pub archive: bool,
    #[arg(long = "no-archive", overrides_with = "archive", hide = true)]
    pub no_archive: bool,
    /// Re-archive even if done today.
    #[arg(long = "force-archive")]
    pub force_archive: bool,
    /// Transactions since (default: 7 days ago).
    #[arg(long)]
    pub since: Option<String>,
    /// NHL game date to pull (default: yesterday).
    #[arg(long = "realized-day")]
    pub realized_day: Option<String>,
    /// Also grade outcomes and this week's bar (always on Mondays).
    #[arg(long)]
    pub grade: bool,
    #[arg(long = "json")]
    pub json: bool,
}

fn daily_cmd(cli: &mut Cli, a: &DailyArgs) -> Result<()> {
    install_params(cli);
    let today = cli.today();
    let archive = !a.no_archive;
    let since_day = parse_day(a.since.as_deref())?.unwrap_or(today - Duration::days(TX_LOOKBACK_DAYS));
    let rday = parse_day(a.realized_day.as_deref())?.unwrap_or(today - Duration::days(1));
    let lgs = leagues(&a.league)?;
    let t0 = std::time::Instant::now();
    let ledger = open_ledger(cli)?;
    let mut results: Vec<JMap<String, Value>> = Vec::new();
    let mut schedule = None;
    for lg in &lgs {
        let (r, sched) = daily_league(cli, lg, &ledger, today, since_day, archive, a.force_archive)?;
        if schedule.is_none() {
            schedule = sched;
        }
        results.push(r);
    }
    let mut realized = JMap::new();
    realized.insert("day".into(), json!(iso(rday)));
    let rres: Result<usize> = (|| {
        let cache = cli.cache()?;
        let client = nhl_client(&cache, today);
        pull_realized(&ledger, &client, rday, None)
    })();
    match rres {
        Ok(n) => {
            realized.insert("players".into(), json!(n));
        }
        Err(e) => {
            realized.insert("error".into(), json!(err_text(&e)));
        }
    }
    let deployment = deployment_step(cli, &ledger, rday, schedule.as_ref());
    let catch_up = catch_up_step(cli, &ledger, today - Duration::days(1), rday, schedule.as_ref());
    let lines = lines_step(cli, today);
    let news = news_step(cli);
    let (mut odds, _games) = odds_step(cli, today);
    if let Some(o) = odds.as_object_mut() {
        if !o.contains_key("games") || o["games"].is_array() {
            o.insert("games".into(), json!(0));
        }
    }
    let graded = if a.grade || today.weekday().num_days_from_monday() == 0 { Some(grade_leagues(&ledger, &lgs, last_monday(today), today)) } else { None };
    // outcomes are re-marked every day so `fm harness review` is current; the weekly bar stays on Mondays
    let outcomes: Option<Vec<Value>> = graded.is_none().then(|| {
        lgs.iter()
            .map(|lg| match grade_outcomes(&ledger, lg, today) {
                Ok(g) => json!(g.line()),
                Err(e) => json!(format!("{lg}: outcomes not graded ({})", err_text(&e))),
            })
            .collect()
    });
    let refit = daily_refit(cli, &ledger, today);
    let failures: Vec<(String, String)> =
        results.iter().filter(|r| truthy(r.get("skipped"))).map(|r| (py_str(&r["league"]), py_str(&r["skipped"]))).collect();
    let notified = notify_failures(cli, &failures, "harness daily");
    let seconds = (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    let summary = json!({
        "leagues": results, "realized": realized, "deployment": deployment, "catch_up": catch_up, "outcomes": outcomes, "lines": lines, "news": news, "odds": odds,
        "grade": graded, "refit": refit, "failure_notify": notified, "seconds": seconds,
    });
    let failed = !failures.is_empty();
    record_run(&ledger, "daily", Some(&a.league), Some(&iso(today)), Some(&summary), if failed || realized.contains_key("error") { "partial" } else { "ok" })?;
    drop(ledger);
    if a.json {
        dump(&summary);
        return Ok(());
    }
    for r in &results {
        let lg = py_str(&r["league"]);
        if truthy(r.get("skipped")) {
            echo(format!("{lg}: league not loaded ({})", py_str(&r["skipped"])));
        }
        if truthy(r.get("archive")) {
            echo(format!("{lg}: archive {}", py_str(&r["archive"])));
        }
        let ing = r.get("ingest").cloned().unwrap_or(json!({}));
        echo(format!("{lg}: ingest {} new file(s), {} episodes", count(&ing, "files_ingested"), count(&ing, "episodes")));
        if r.contains_key("transactions") {
            let ln = r.get("lineups").cloned().unwrap_or(json!({}));
            echo(format!(
                "{lg}: {} transaction rows since {since_day}, {} lineup rows for {}",
                py_str(&r["transactions"]),
                count(&ln, "rows"),
                py_str(ln.get("day").unwrap_or(&Value::Null))
            ));
        }
        let m = r.get("match").cloned().unwrap_or(json!({}));
        let joined = |k: &str| -> String {
            let mut v: Vec<(String, String)> = obj(m.get(k).unwrap_or(&Value::Null)).iter().map(|(a, b)| (a.clone(), py_str(b))).collect();
            v.sort();
            if v.is_empty() {
                "none".to_string()
            } else {
                v.iter().map(|(a, b)| format!("{a} {b}")).collect::<Vec<_>>().join(", ")
            }
        };
        echo(format!("{lg}: episodes {}; decisions {}", joined("by_status"), joined("decisions")));
        if let Some(mc) = r.get("matchup").filter(|v| truthy(Some(v))) {
            let wp = match mc.get("win_prob").and_then(fv) {
                Some(p) => format!(" (period {}, {}% to win)", py_str(mc.get("period").unwrap_or(&Value::Null)), fixed(p * 100.0, 0)),
                None => String::new(),
            };
            echo(format!("{lg}: matchup snapshot {}{wp}; {} final score(s) recorded", py_str(mc.get("snapshot").unwrap_or(&Value::Null)), count(mc, "results")));
        }
        for w in r["warnings"].as_array().into_iter().flatten() {
            echo(format!("  {}", py_str(w)));
        }
    }
    if realized.contains_key("error") {
        echo(format!("NHL results {}: {}", iso(rday), py_str(&realized["error"])));
    } else {
        let n = realized.get("players").and_then(|v| v.as_i64()).unwrap_or(0);
        echo(format!("NHL results {}: {n} players{}", iso(rday), if n > 0 { "" } else { " (no NHL regular-season games that day)" }));
    }
    echo(deployment_line(&summary["deployment"]));
    if let Some(l) = catch_up_line(&summary["catch_up"]) {
        echo(l);
    }
    for l in summary["outcomes"].as_array().into_iter().flatten() {
        echo(format!("outcomes {}", py_str(l)));
    }
    echo(lines_line(&summary["lines"]));
    echo(news_line(&summary["news"]));
    echo(odds_line(&summary["odds"]));
    for l in summary["failure_notify"].as_array().into_iter().flatten() {
        echo(format!("league failure notification: {}", py_str(l)));
    }
    for gr in summary["grade"].as_array().into_iter().flatten() {
        print_grade(gr);
    }
    if let Some(rf) = summary.get("refit").filter(|v| truthy(Some(v))) {
        if truthy(rf.get("error")) {
            echo(format!("refit failed: {}", py_str(&rf["error"])));
        } else {
            echo(format!("-- Refit ({}) --", py_str(rf.get("mode").unwrap_or(&Value::Null))));
            print_refit(rf);
        }
    }
    echo(format!("{}s", py_str(&summary["seconds"])));
    Ok(())
}

// --------------------------------------------------------------------------- pregame

#[derive(Args, Clone, Debug)]
pub struct PregameArgs {
    #[arg(long, short = 'l', default_value = "both", help = "espn | fantrax | both")]
    pub league: String,
    /// Post the changes to the Discord / Slack webhooks.
    #[arg(long)]
    pub notify: bool,
    #[arg(long = "json")]
    pub json: bool,
    /// With --notify: send nothing when nothing changed.
    #[arg(long = "quiet-if-unchanged")]
    pub quiet_if_unchanged: bool,
}

fn pregame_cmd(cli: &mut Cli, a: &PregameArgs) -> Result<()> {
    use fm_report::notify::any_failed;
    use fm_report::pregame::{failure_message, run_pregame, PregameOpts};
    install_params(cli);
    let lgs = leagues(&a.league)?;
    let news = news_step(cli);
    let mut results = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    // Python's run_pregame uses naive local `datetime.now()`: the day, "Pre-game 8:43 AM" and the
    // snapshot stamps are local wall clock (UTC here showed 2:43 PM and could roll the day over)
    let now = cli.now_local();
    for (i, lg) in lgs.iter().enumerate() {
        let cache = cli.cache()?;
        let opts = PregameOpts { notify: a.notify, force_refresh: i == 0, quiet_if_unchanged: a.quiet_if_unchanged, ..PregameOpts::default() };
        match run_pregame(lg, &cli.settings, Some(&cache), &now, opts) {
            Ok(r) => results.push(r),
            Err(e) => failures.push((lg.clone(), format!("{e:#}"))),
        }
    }
    let fail_notify = if a.notify { notify_failures(cli, &failures, "harness pregame") } else { None };
    let mut sent: Vec<String> = results.iter().flat_map(|r| r.notify.clone().unwrap_or_default()).collect();
    sent.extend(fail_notify.clone().unwrap_or_default());
    if a.json {
        let payload = json!({
            "leagues": results.iter().map(|r| r.to_dict()).collect::<Vec<_>>(),
            "failures": failures.iter().map(|(lg, e)| json!({"league": lg, "error": format!("Error: {e}"), "message": failure_message(lg, e)})).collect::<Vec<_>>(),
            "failure_notify": fail_notify,
            "news": news,
        });
        dump(&payload);
    } else {
        for r in &results {
            let base = match &r.baseline {
                Some(b) => format!("vs {b} snapshot {}", r.baseline_at.as_deref().unwrap_or("").chars().skip(11).take(5).collect::<String>()),
                None => "no earlier snapshot today".to_string(),
            };
            echo(format!("-- {}: pre-game ({base}) --", r.league));
            echo(&r.summary_text);
            for c in &r.changes {
                let scope = c.scope.as_deref().map(|s| format!("/{s}")).unwrap_or_default();
                echo(format!("  {}{scope}: {}", c.kind.as_str(), c.text));
            }
            if !r.refresh.is_empty() {
                echo(format!("refresh: {}", r.refresh.iter().map(|(k, v)| format!("{k} {}", py_str(v))).collect::<Vec<_>>().join("; ")));
            }
            for w in r.warnings.iter().take(8) {
                echo(format!("  {w}"));
            }
            match &r.notify {
                Some(n) => echo(format!("notify: {}", n.join("; "))),
                None if a.notify => echo("notify: nothing changed, nothing sent (--quiet-if-unchanged)"),
                None => {}
            }
        }
        for (lg, e) in &failures {
            echo(format!("{lg}: {}", failure_message(lg, e)));
        }
        for x in fail_notify.iter().flatten() {
            echo(format!("league failure notification: {x}"));
        }
        echo(news_line(&news));
    }
    if !failures.is_empty() || any_failed(&sent) {
        return Err(Exit(1).into());
    }
    Ok(())
}

// --------------------------------------------------------------------------- odds

#[derive(Args, Clone, Debug)]
pub struct OddsArgs {
    /// List every upcoming game, not only the next slate.
    #[arg(long = "all")]
    pub show_all: bool,
    #[arg(long = "json")]
    pub json: bool,
}

fn odds_cmd(cli: &mut Cli, a: &OddsArgs) -> Result<()> {
    let today = cli.today();
    let (o, games) = odds_step(cli, today);
    if a.json {
        let mut m = obj(&o);
        m.insert("games".into(), Value::Array(games.iter().map(|g| Value::Object(g.to_dict(false))).collect()));
        dump(&Value::Object(m));
        return Ok(());
    }
    if truthy(o.get("skipped")) {
        echo(format!(
            "Odds disabled: {}. Add ODDS_API_KEY to .env (free key at the-odds-api.com; one 2-credit call a day).",
            py_str(&o["skipped"])
        ));
        return Ok(());
    }
    if truthy(o.get("error")) {
        echo(format!("odds: {}", py_str(&o["error"])));
        return Err(Exit(1).into());
    }
    if games.is_empty() {
        echo("No upcoming NHL games with odds.");
    } else {
        let slate = games.iter().map(|g| g.game_date).filter(|d| *d >= today).min();
        let all = a.show_all || slate.is_none();
        let shown: Vec<_> = games.iter().filter(|g| all || Some(g.game_date) == slate).collect();
        let label = if all {
            "Upcoming games".to_string()
        } else if slate == Some(today) {
            "Tonight's games".to_string()
        } else {
            format!("Next slate ({})", iso(slate.unwrap_or(today)))
        };
        let mut t = Table::new(format!("{label}: market consensus (implied team totals = Poisson split of the total)"));
        for c in ["Date", "Away", "Home", "Away ML", "Home ML", "Home win", "Total", "O/U", "Away TT", "Home TT", "Books"] {
            t.col(c, if ["Date", "Away", "Home"].contains(&c) { L } else { R });
        }
        let ml = |v: Option<i64>| v.map(|x| format!("{x:+}")).unwrap_or_else(|| "-".to_string());
        let nm = |v: Option<f64>, nd: usize| v.map(|x| fixed(x, nd)).unwrap_or_else(|| "-".to_string());
        for gm in shown {
            let ou = match (gm.over_price, gm.under_price) {
                (Some(o), Some(u)) => format!("{o:+}/{u:+}"),
                _ => "-".to_string(),
            };
            t.row([
                iso(gm.game_date),
                gm.away.clone(),
                gm.home.clone(),
                ml(gm.away_ml),
                ml(gm.home_ml),
                gm.home_win_prob.map(|p| format!("{}%", fixed(p * 100.0, 0))).unwrap_or_else(|| "-".to_string()),
                nm(gm.total, 1),
                ou,
                nm(gm.implied_away_total, 2),
                nm(gm.implied_home_total, 2),
                gm.bookmaker_count.to_string(),
            ]);
        }
        t.print();
    }
    let q = o.get("quota").cloned().unwrap_or(Value::Null);
    let quota = match q.get("remaining").filter(|v| !v.is_null()) {
        Some(r) => format!("{} API credits left ({} used)", py_str(r), py_str(q.get("used").unwrap_or(&Value::Null))),
        None => "quota unknown".to_string(),
    };
    let fetched = o.get("fetched_at").filter(|v| truthy(Some(v))).map(py_str).unwrap_or_else(|| "-".to_string());
    echo(format!(
        "source {}, fetched {fetched}, archive {}; {quota}",
        py_str(o.get("source").unwrap_or(&Value::Null)),
        py_str(o.get("archive").unwrap_or(&Value::Null))
    ));
    for w in o.get("warnings").and_then(|w| w.as_array()).into_iter().flatten() {
        echo(format!("  {}", py_str(w)));
    }
    Ok(())
}

// --------------------------------------------------------------------------- ledger

#[derive(Args, Clone, Debug)]
pub struct ReviewArgs {
    #[arg(long, short = 'l', default_value = "all", help = LEAGUE_HELP)]
    pub league: String,
    /// Moves listed (newest first).
    #[arg(long, short = 'n', default_value_t = 20)]
    pub limit: usize,
    /// Re-mark outcomes against the latest NHL results first (`fm harness daily` does this every day).
    #[arg(long)]
    pub regrade: bool,
    #[arg(long = "json")]
    pub json: bool,
}

fn opt_num(v: &Value, nd: usize, signed: bool) -> String {
    match v.as_f64() {
        None => "-".to_string(),
        Some(x) if signed => fixed_signed(x, nd),
        Some(x) => fixed(x, nd),
    }
}

fn rate_cell(v: &Value) -> String {
    v.as_f64().map(|r| format!("{}%", fixed(r * 100.0, 0))).unwrap_or_else(|| "-".to_string())
}

fn review_cmd(cli: &mut Cli, a: &ReviewArgs) -> Result<()> {
    use fm_harness::review::review;
    let lgs = leagues(&a.league)?;
    let today = cli.today();
    let ledger = open_ledger(cli)?;
    let mut out = Vec::new();
    for lg in &lgs {
        if a.regrade {
            grade_outcomes(&ledger, lg, today)?;
        }
        out.push(review(&ledger, lg, today, a.limit)?);
    }
    drop(ledger);
    if a.json {
        dump(&json!({"as_of": iso(today), "leagues": out}));
        return Ok(());
    }
    for r in &out {
        let lg = py_str(&r["league"]).to_uppercase();
        let mv = &r["moves"];
        echo(format!("== {lg}: how right was the model? =="));
        if mv["n_moves"].as_i64().unwrap_or(0) == 0 {
            echo("No graded moves yet. Suggestions are graded from the day after they are captured; run `fm harness daily` every day.");
        } else {
            let mut t = Table::new("Moves so far (right / wrong = window finished; ahead / behind = still running)");
            t.cols(&[
                ("Who", L),
                ("Moves", R),
                ("Right", R),
                ("Wrong", R),
                ("Ahead", R),
                ("Behind", R),
                ("No result", R),
                ("Final hit rate (95% CI)", R),
                ("Pts predicted", R),
                ("Pts realized", R),
            ]);
            for g in mv["by_group"].as_array().into_iter().flatten().chain(std::iter::once(&mv["total"])) {
                let ci = &g["final_ci"];
                let rate = match g["final_rate"].as_f64() {
                    None => "-".to_string(),
                    Some(_) => format!("{} ({}-{})", rate_cell(&g["final_rate"]), rate_cell(&ci[0]), rate_cell(&ci[1])),
                };
                t.row([
                    py_str(&g["label"]),
                    py_str(&g["n"]),
                    py_str(&g["right"]),
                    py_str(&g["wrong"]),
                    py_str(&g["ahead"]),
                    py_str(&g["behind"]),
                    py_str(&g["no_result"]),
                    rate,
                    opt_num(&g["predicted_pts"], 1, true),
                    opt_num(&g["realized_pts"], 1, true),
                ]);
            }
            t.print();
            let mut t = Table::new("By kind");
            t.cols(&[("Kind", L), ("Moves", R), ("Right", R), ("Wrong", R), ("Ahead", R), ("Behind", R), ("Right so far", R)]);
            for k in mv["by_kind"].as_array().into_iter().flatten() {
                t.row([
                    py_str(&k["label"]),
                    py_str(&k["n"]),
                    py_str(&k["right"]),
                    py_str(&k["wrong"]),
                    py_str(&k["ahead"]),
                    py_str(&k["behind"]),
                    rate_cell(&k["so_far_rate"]),
                ]);
            }
            t.print();
            let shown = mv["moves"].as_array().map(|a| a.len()).unwrap_or(0);
            let mut t = Table::new(format!(
                "Latest {shown} of {} moves (points the move added so far; flags: FPG vs his last 15 days)",
                py_str(&mv["n_moves"])
            ));
            t.cols(&[("Day", L), ("Kind", L), ("Move", L), ("Who", L), ("Graded", R), ("Predicted", R), ("Realized", R), ("Verdict", L)]);
            for m in mv["moves"].as_array().into_iter().flatten() {
                let who = match py_str(&m["group"]).as_str() {
                    "followed" => "you took it",
                    "own" => "your own",
                    _ => "not taken",
                };
                let mut verdict = py_str(&m["verdict"]);
                if m["early"].as_bool() == Some(true) {
                    verdict.push_str(" (early)");
                }
                if m["model_pick"].is_string() {
                    verdict.push_str(&format!("; model's pick {}", opt_num(&m["model_pick_realized"], 1, true)));
                }
                t.row([
                    py_str(&m["day"]),
                    py_str(&m["kind"]),
                    py_str(&m["title"]),
                    who.to_string(),
                    format!("{}/{}d", py_str(&m["days_graded"]), py_str(&m["window_days"])),
                    opt_num(&m["predicted_so_far"], 1, true),
                    opt_num(&m["realized"], 1, true),
                    verdict,
                ]);
            }
            t.print();
        }
        let rk = &r["ranking"];
        if let Some(n) = rk["note"].as_str() {
            echo(format!("Free-agent ranking: {n}"));
            continue;
        }
        let ws: Vec<&Value> =
            rk["windows"].as_array().into_iter().flatten().filter(|w| w["evaluated"].as_i64().unwrap_or(0) > 0).collect();
        if ws.is_empty() {
            let no_rosters = rk["days_without_rosters"].as_i64().unwrap_or(0);
            echo(format!(
                "Free-agent ranking: nothing to check yet ({} archived day(s); a day counts once the next 7 days have been played and pulled{}).",
                py_str(&rk["projection_days"]),
                if no_rosters > 0 { format!("; {no_rosters} day(s) had no rosters captured") } else { String::new() }
            ));
            continue;
        }
        let mut t = Table::new(format!("Free-agent ranking: average points of each ranker's top {} free agents", py_str(&rk["top_k"])));
        t.cols(&[
            ("Next", L),
            ("Days", R),
            ("Model", R),
            ("Market (% rostered)", R),
            ("Hot (season FPG)", R),
            ("Best possible", R),
            ("Median FA", R),
            ("Model beat market", R),
            ("Model beat hot", R),
        ]);
        for w in ws {
            let n = w["evaluated"].as_i64().unwrap_or(0);
            t.row([
                format!("{} days{}", py_str(&w["days"]), if w["early"].as_bool() == Some(true) { " (early)" } else { "" }),
                n.to_string(),
                opt_num(&w["model"], 1, false),
                opt_num(&w["market"], 1, false),
                opt_num(&w["hot"], 1, false),
                opt_num(&w["best"], 1, false),
                opt_num(&w["median"], 1, false),
                format!("{}/{n}", py_str(&w["beat_market"])),
                format!("{}/{n}", py_str(&w["beat_hot"])),
            ]);
        }
        t.print();
    }
    Ok(())
}

#[derive(Args, Clone, Debug)]
pub struct LedgerArgs {
    #[arg(long, short = 'l', default_value = "all", help = LEAGUE_HELP)]
    pub league: String,
    /// waiver | lineup | trade | injury | sell_high | buy_low
    #[arg(long)]
    pub kind: Option<String>,
    /// Episode status (open | followed | partial | proposed | expired) or user_only (my moves without a rec).
    #[arg(long)]
    pub origin: Option<String>,
    /// Only episodes / moves since YYYY-MM-DD.
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long, short = 'n', default_value_t = 30)]
    pub limit: i64,
    #[arg(long = "json")]
    pub json: bool,
}

fn ledger_cmd(cli: &mut Cli, a: &LedgerArgs) -> Result<()> {
    let lgs = leagues(&a.league)?;
    let since_s = iso(parse_day(a.since.as_deref())?.unwrap_or(NaiveDate::from_ymd_opt(1900, 1, 1).unwrap_or_default()));
    let ph = vec!["?"; lgs.len()].join(",");
    let ledger = open_ledger(cli)?;
    let mut eps: Vec<Row> = Vec::new();
    let origin = a.origin.as_deref();
    if origin != Some("user_only") {
        let mut sql = format!(
            "SELECT league, kind, status, first_seen, last_seen, n_days, predicted_gain, gain_units, horizon_days, strength, acted_on, window_end, title FROM rec_episodes WHERE league IN ({ph}) AND last_seen >= ?"
        );
        let mut params: Vec<&dyn ToSql> = lgs.iter().map(|l| l as &dyn ToSql).collect();
        params.push(&since_s);
        if let Some(k) = &a.kind {
            sql.push_str(" AND kind = ?");
            params.push(k);
        }
        if let Some(o) = &a.origin {
            sql.push_str(" AND status = ?");
            params.push(o);
        }
        sql.push_str(" ORDER BY last_seen DESC, first_seen DESC LIMIT ?");
        params.push(&a.limit);
        eps = ledger.query(&sql, &params)?;
    }
    let mut moves: Vec<Row> = Vec::new();
    if origin.is_none() || origin == Some("user_only") {
        let mut sql = format!("SELECT league, day, kind, adds_json, drops_json FROM decisions WHERE origin='user_only' AND league IN ({ph}) AND day >= ?");
        let mut params: Vec<&dyn ToSql> = lgs.iter().map(|l| l as &dyn ToSql).collect();
        params.push(&since_s);
        if let Some(k) = &a.kind {
            sql.push_str(" AND kind = ?");
            params.push(k);
        }
        sql.push_str(" ORDER BY day DESC LIMIT ?");
        params.push(&a.limit);
        moves = ledger.query(&sql, &params)?;
        let mut names: std::collections::HashMap<String, Value> = Default::default();
        for r in ledger.query("SELECT DISTINCT cid, player_name FROM transactions WHERE is_me=1", &[])? {
            names.insert(s_of(&r, "cid").to_string(), r.get("player_name").cloned().unwrap_or(Value::Null));
        }
        for m in moves.iter_mut() {
            for (src, dst) in [("adds_json", "adds"), ("drops_json", "drops")] {
                let raw = m.shift_remove(src).and_then(|v| v.as_str().map(String::from)).filter(|s| !s.is_empty()).unwrap_or_else(|| "[]".to_string());
                let ids: Vec<Value> = serde_json::from_str(&raw)?;
                let named: Vec<Value> = ids
                    .iter()
                    .map(|c| {
                        let key = c.as_str().unwrap_or("");
                        match names.get(key) {
                            Some(n) if truthy(Some(n)) => n.clone(),
                            _ => c.clone(),
                        }
                    })
                    .collect();
                m.insert(dst.into(), Value::Array(named));
            }
        }
    }
    drop(ledger);
    if a.json {
        dump(&json!({"episodes": eps.iter().map(row_val).collect::<Vec<_>>(), "user_only": moves.iter().map(row_val).collect::<Vec<_>>()}));
        return Ok(());
    }
    if origin != Some("user_only") {
        if eps.is_empty() {
            echo("No episodes yet: run `fm harness daily`.");
        } else {
            let mut t = Table::new("Recommendation episodes (newest first)");
            t.cols(&[("League", L), ("Kind", L), ("Status", L), ("Seen", L), ("Days", R), ("Gain", R), ("Str", R), ("Recommendation", L)]);
            for e in &eps {
                let (fs, ls) = (s_of(e, "first_seen"), s_of(e, "last_seen"));
                let seen = if fs == ls { fs.to_string() } else { format!("{fs}..{}", ls.chars().skip(5).collect::<String>()) };
                let gain = match e.get("predicted_gain").and_then(fv) {
                    None => "-".to_string(),
                    Some(gv) => format!("{} {}", fixed_signed(gv, 2), s_of(e, "gain_units")).trim().to_string(),
                };
                t.row([
                    s_of(e, "league").to_string(),
                    s_of(e, "kind").to_string(),
                    s_of(e, "status").to_string(),
                    seen,
                    py_str(e.get("n_days").unwrap_or(&Value::Null)),
                    gain,
                    e.get("strength").and_then(fv).map(|x| fixed(x, 1)).unwrap_or_else(|| "-".to_string()),
                    s_of(e, "title").to_string(),
                ]);
            }
            t.print();
        }
    }
    if !moves.is_empty() {
        let mut t = Table::new("My moves without a recommendation (user_only)");
        for c in ["League", "Day", "Kind", "Added", "Dropped"] {
            t.col(c, L);
        }
        let join = |v: &Value| -> String {
            let s = v.as_array().into_iter().flatten().map(py_str).collect::<Vec<_>>().join(", ");
            if s.is_empty() {
                "-".to_string()
            } else {
                s
            }
        };
        for m in &moves {
            let kind = s_of(m, "kind");
            t.row([
                s_of(m, "league").to_string(),
                s_of(m, "day").to_string(),
                if kind.is_empty() { "-".to_string() } else { kind.to_string() },
                join(&m["adds"]),
                join(&m["drops"]),
            ]);
        }
        t.print();
    }
    Ok(())
}

fn fixed_signed(x: f64, nd: usize) -> String {
    fm_pycompat::fmt::fixed(x, nd, true)
}

// --------------------------------------------------------------------------- rebuild

#[derive(Args, Clone, Debug)]
pub struct RebuildArgs {
    /// Also drop pulled data (transactions, lineups, NHL results) that the archive cannot restore.
    #[arg(long = "all")]
    pub everything: bool,
}

fn rebuild_cmd(cli: &mut Cli, a: &RebuildArgs) -> Result<()> {
    install_params(cli);
    let ledger = open_ledger(cli)?;
    let keep: Vec<&str> = if a.everything { vec![] } else { vec!["transactions", "lineup_days", "realized_daily", "realized_pulls", "runs"] };
    for t in TABLES {
        if !keep.contains(t) {
            ledger.execute(&format!("DELETE FROM {t}"), &[])?;
        }
    }
    ledger.commit()?;
    let ing = ingest_archive(&ledger, cli.data_dir(), None, true)?;
    ParamsStore::new(None, Some(&ledger), None).sync_ledger(Some(&ledger))?;
    let mut lines = Vec::new();
    for r in ledger.query("SELECT DISTINCT league FROM rec_episodes", &[])? {
        lines.push(s_of(&r, "league").to_string());
    }
    lines.sort();
    lines.dedup();
    let mut matched = Vec::new();
    for lg in &lines {
        matched.push(match_episodes(&ledger, lg, Some(cli.today()), None)?.line());
    }
    record_run(&ledger, "rebuild", None, Some(&iso(cli.today())), Some(&json!({"ingest": ing.to_json(), "match": matched})), "ok")?;
    echo(format!(
        "Rebuilt from {} archive file(s): {} projection rows, {} recs, {} episodes.{}",
        ing.files_ingested,
        ing.projections,
        ing.recs,
        ing.episodes,
        if a.everything { "" } else { " Pulled transactions / lineups / NHL results kept." }
    ));
    for ln in &matched {
        echo(ln);
    }
    for e in &ing.errors {
        echo(format!("  {e}"));
    }
    Ok(())
}

// --------------------------------------------------------------------------- grade

fn print_grade(g: &Value) {
    if truthy(g.get("error")) {
        echo(format!("{}: grading failed: {}", py_str(&g["league"]), py_str(&g["error"])));
        return;
    }
    echo(py_str(g.get("outcomes_line").filter(|v| truthy(Some(v))).unwrap_or(&json!(""))));
    if let Some(mc) = g.get("matchup").filter(|v| truthy(Some(v))) {
        echo(summary_line(mc));
    }
    let w = g.get("week").cloned().unwrap_or(json!({}));
    if truthy(w.get("note")) {
        echo(format!("{}: {}", py_str(&g["league"]), py_str(&w["note"])));
        return;
    }
    let t = w.get("trust").cloned().unwrap_or(json!({}));
    echo(format!(
        "{}: bar for week {}: {} metric rows ({} reliable, {} provisional, {} hidden); {} matured 28-day and {} 7-day projection snapshot(s) of {}, {} matured outcome(s)",
        py_str(&g["league"]),
        py_str(w.get("week").unwrap_or(&Value::Null)),
        count(&w, "rows"),
        count(&t, "reliable"),
        count(&t, "provisional"),
        count(&t, "hidden"),
        count(&w, "matured_28d"),
        count(&w, "matured_7d"),
        count(&w, "snapshots"),
        count(&w, "outcomes_used"),
    ));
}

#[derive(Args, Clone, Debug)]
pub struct GradeArgs {
    /// Week to snapshot (a Monday, YYYY-MM-DD; another day means its week). Default: the most recent Monday.
    #[arg(long)]
    pub week: Option<String>,
    #[arg(long, short = 'l', default_value = "all", help = LEAGUE_HELP)]
    pub league: String,
    #[arg(long = "json")]
    pub json: bool,
}

fn grade_cmd(cli: &mut Cli, a: &GradeArgs) -> Result<()> {
    install_params(cli);
    let today = cli.today();
    let wk = last_monday(parse_day(a.week.as_deref())?.unwrap_or(today));
    let lgs = leagues(&a.league)?;
    let ledger = open_ledger(cli)?;
    let results = grade_leagues(&ledger, &lgs, wk, today);
    let status = if results.iter().any(|r| truthy(r.get("error"))) { "partial" } else { "ok" };
    record_run(&ledger, "grade", Some(&a.league), Some(&iso(today)), Some(&json!({"week": iso(wk), "leagues": results})), status)?;
    drop(ledger);
    if a.json {
        dump(&json!({"week": iso(wk), "leagues": results}));
        return Ok(());
    }
    for r in &results {
        print_grade(r);
    }
    Ok(())
}

// --------------------------------------------------------------------------- status

fn trust(t: &str) -> &str {
    t
}

fn print_projection(proj: &[Value]) {
    let find = |metric: &str, pool: &str| proj.iter().find(|r| r["metric"] == metric && r["pool"] == pool);
    let mut t = Table::new("Projection accuracy (fpg vs next 28 days, >= 8 GP; proj_week vs next 7 days)");
    for c in ["Pool", "MAE", "Spearman", "Skill vs to-date [95% CI]", "N", "Trust", "Week MAE", "rate / avail", "N wk", "Trust wk"] {
        t.col(c, if ["Pool", "Trust", "Trust wk"].contains(&c) { L } else { R });
    }
    for pool in ["F", "D", "G"] {
        let (m, sk, wk) = (find("proj_fpg_mae", pool), find("proj_fpg_skill", pool), find("proj_week_mae", pool));
        let Some(m) = m else { continue };
        let (d, dw) = (m["detail"].clone(), wk.map(|w| w["detail"].clone()).unwrap_or(json!({})));
        let ci = match sk {
            Some(s) if !s["ci_lo"].is_null() => format!(" [{}, {}]", ff(fv(&s["ci_lo"]), 0, true), ff(fv(&s["ci_hi"]), 0, true)),
            _ => String::new(),
        };
        t.row([
            pool.to_string(),
            ff(fv(&m["value"]), 2, false),
            ff(d.get("spearman").and_then(fv), 2, false),
            match sk {
                Some(s) => format!("{}{ci}", ff(fv(&s["value"]), 0, true)),
                None => "-".to_string(),
            },
            py_str(&m["n"]),
            trust(m["trust"].as_str().unwrap_or("")).to_string(),
            ff(wk.and_then(|w| fv(&w["value"])), 2, false),
            format!("{} / {}", ff(dw.get("rate_mae").and_then(fv), 2, false), ff(dw.get("avail_mae").and_then(fv), 2, false)),
            wk.map(|w| py_str(&w["n"])).unwrap_or_else(|| "0".to_string()),
            wk.and_then(|w| w["trust"].as_str()).unwrap_or("hidden").to_string(),
        ]);
    }
    t.print();
}

fn print_hits(hits: &[Value]) {
    let mut t = Table::new("Recommendation hit rates (complete windows)");
    for c in ["Kind:origin", "Hit rate", "95% CI", "Mean realized", "N", "Trust"] {
        t.col(c, if ["Kind:origin", "Trust"].contains(&c) { L } else { R });
    }
    for r in hits {
        let d = &r["detail"];
        let ci = if r["ci_lo"].is_null() { "-".to_string() } else { format!("{}-{}", ff(fv(&r["ci_lo"]), 0, true), ff(fv(&r["ci_hi"]), 0, true)) };
        t.row([
            py_str(&r["pool"]),
            ff(fv(&r["value"]), 0, true),
            ci,
            format!("{} {}", ff(d.get("mean_gain").and_then(fv), 2, false), d.get("units").map(py_str).unwrap_or_default()),
            py_str(&r["n"]),
            py_str(&r["trust"]),
        ]);
    }
    t.print();
}

fn print_calibration(cal: &Value) {
    if !truthy(Some(cal)) {
        return;
    }
    let bins = cal["detail"].get("bins").and_then(|b| b.as_array()).filter(|b| !b.is_empty());
    let Some(bins) = bins else {
        echo(format!("Calibration: {} graded rec(s) with a predicted gain ({}).", py_str(&cal["n"]), py_str(&cal["trust"])));
        return;
    };
    let mut t = Table::new(format!(
        "Calibration: predicted vs realized pts ({}, n={}, {})",
        py_str(cal["detail"].get("binning").unwrap_or(&Value::Null)),
        py_str(&cal["n"]),
        py_str(&cal["trust"])
    ));
    for c in ["Bin", "Predicted range", "Mean predicted", "Mean realized", "Hit rate", "N"] {
        t.col(c, R);
    }
    for b in bins {
        let f = |k: &str| fv(&b[k]).unwrap_or(f64::NAN);
        t.row([
            py_str(&b["bin"]),
            format!("{} .. {}", fixed(f("pred_lo"), 1), fixed(f("pred_hi"), 1)),
            fixed(f("mean_pred"), 2),
            fixed(f("mean_real"), 2),
            ff(fv(&b["hit_rate"]), 0, true),
            py_str(&b["n"]),
        ]);
    }
    t.print();
}

fn print_counterfactual(cf: &Value) {
    if !truthy(Some(cf)) {
        return;
    }
    let d = &cf["detail"];
    let df = |k: &str| d.get(k).and_then(fv);
    if cf["n"].as_i64().unwrap_or(0) != 0 {
        echo(format!(
            "Your moves vs the model's: you {} pts vs the model's pick {} pts on {} paired move(s); model better {}, model agreed with {} ({})",
            ff(df("my_mean"), 2, false),
            ff(df("model_mean"), 2, false),
            py_str(&cf["n"]),
            ff(df("model_better"), 0, true),
            ff(df("model_agreed"), 0, true),
            py_str(&cf["trust"])
        ));
    } else {
        echo(format!(
            "Your moves vs the model's: {} graded user-only waiver move(s), none paired with a model pick yet ({}).",
            d.get("moves").map(py_str).unwrap_or_else(|| "0".into()),
            py_str(&cf["trust"])
        ));
    }
}

fn print_matchup_calibration(mc: &Value) {
    if !truthy(Some(mc)) {
        return;
    }
    echo(format!("{} ({})", summary_line(mc), mc.get("trust").map(py_str).unwrap_or_else(|| "hidden".into())));
    if !truthy(mc.get("graded_snapshots")) {
        return;
    }
    let mut t = Table::new("Matchup win probability: reliability (all graded daily snapshots)");
    for c in ["Predicted", "N", "Mean predicted", "Observed win rate"] {
        t.col(c, R);
    }
    for b in mc.get("reliability").and_then(|r| r.as_array()).into_iter().flatten() {
        if b["n"].as_i64().unwrap_or(0) != 0 {
            t.row([
                format!("{}%-{}%", fixed(fv(&b["lo"]).unwrap_or(0.0) * 100.0, 0), fixed(fv(&b["hi"]).unwrap_or(0.0) * 100.0, 0)),
                py_str(&b["n"]),
                ff(fv(&b["mean_pred"]), 0, true),
                ff(fv(&b["observed"]), 0, true),
            ]);
        }
    }
    t.print();
}

fn print_bar(rep: &Value) {
    let pi = rep.get("params").cloned().unwrap_or(json!({}));
    let hash: String = pi.get("hash").and_then(|h| h.as_str()).filter(|h| !h.is_empty()).unwrap_or("-").chars().take(10).collect();
    echo(format!(
        "Params: {} (hash {hash}, {}; {} fitted version(s))",
        pi.get("version").map(py_str).unwrap_or_else(|| "-".into()),
        pi.get("source").map(py_str).unwrap_or_else(|| "-".into()),
        count(&pi, "versions")
    ));
    for (lg, l) in obj(rep.get("leagues").unwrap_or(&Value::Null)) {
        if truthy(l.get("graded")) {
            echo(format!("-- {lg}: bar as of week {} --", py_str(&l["week"])));
        } else {
            echo(format!("-- {lg}: not graded yet --"));
            echo("Run `fm harness grade` (it also runs with `fm harness daily` on Mondays).");
        }
        if let Some(p) = l.get("projection").and_then(|p| p.as_array()).filter(|p| !p.is_empty()) {
            print_projection(p);
        }
        if let Some(h) = l.get("hit_rates").and_then(|p| p.as_array()).filter(|p| !p.is_empty()) {
            print_hits(h);
        }
        print_calibration(l.get("calibration").unwrap_or(&Value::Null));
        print_counterfactual(l.get("counterfactual").unwrap_or(&Value::Null));
        let trades = l.get("trades").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        echo(format!("Trades (case list, never aggregated): {}", if trades.is_empty() { "none acted on yet".to_string() } else { trades.len().to_string() }));
        for c in trades.iter().take(10) {
            echo(format!(
                "  {} {} [{}, {}]: {} pts ({})",
                py_str(c.get("day").unwrap_or(&Value::Null)),
                c.get("title").filter(|v| truthy(Some(v))).map(py_str).unwrap_or_else(|| "-".into()),
                py_str(&c["origin"]),
                py_str(&c["window"]),
                ff(c.get("realized_gain").and_then(fv), 2, false),
                py_str(c.get("label").unwrap_or(&Value::Null))
            ));
        }
        let oc = l.get("outcomes").cloned().unwrap_or(json!({}));
        echo(format!(
            "Outcomes: {} rows, {} complete, {} without a realized gain, {} with unmatched players",
            count(&oc, "n"),
            count(&oc, "complete"),
            count(&oc, "ungradable"),
            count(&oc, "unmatched")
        ));
        if let Some(nj) = l.get("not_judgeable").and_then(|n| n.as_array()).filter(|n| !n.is_empty()) {
            let items: Vec<String> = nj
                .iter()
                .map(|x| {
                    format!(
                        "{} {} (n={}, need {} more)",
                        py_str(&x["metric"]),
                        py_str(&x["pool"]),
                        py_str(&x["n"]),
                        if x["need"].is_null() { "?".to_string() } else { py_str(&x["need"]) }
                    )
                })
                .collect();
            echo(format!("Not judgeable yet: {}", items.join("; ")));
        }
        print_matchup_calibration(l.get("matchup_calibration").unwrap_or(&Value::Null));
        match l.get("headline").and_then(|h| h.as_str()).filter(|h| !h.is_empty()) {
            Some(h) => echo(format!("Digest line: {h}")),
            None => echo("Digest line: hidden (nothing trustworthy yet)"),
        }
    }
}

#[derive(Args, Clone, Debug)]
pub struct StatusArgs {
    #[arg(long = "json")]
    pub json: bool,
    #[arg(long, short = 'l', default_value = "all", help = LEAGUE_HELP)]
    pub league: String,
}

fn status_cmd(cli: &mut Cli, a: &StatusArgs) -> Result<()> {
    install_params(cli);
    let ledger = open_ledger(cli)?;
    let wanted = leagues(&a.league)?;
    let known = known_leagues(&ledger)?;
    let mut sel: Vec<String> = known.into_iter().filter(|l| wanted.contains(l)).collect();
    if sel.is_empty() {
        sel = wanted;
    }
    let bar = status_report(&ledger, Some(&sel))?;
    let q = |sql: &str| -> Result<Vec<Value>> { Ok(ledger.query(sql, &[])?.iter().map(row_val).collect()) };
    let eps = q("SELECT league, kind, status, COUNT(*) n FROM rec_episodes GROUP BY 1, 2, 3 ORDER BY 1, 2, 3")?;
    let tx = q("SELECT league, is_me, COUNT(*) n FROM transactions GROUP BY 1, 2")?;
    let dec = q("SELECT league, origin, COUNT(*) n FROM decisions GROUP BY 1, 2")?;
    let proj = q("SELECT league, COUNT(DISTINCT as_of) days, COUNT(*) rows, SUM(inputs_json IS NOT NULL) with_inputs FROM projections GROUP BY 1")?;
    let lineups = q("SELECT league, COUNT(DISTINCT day) days, COUNT(*) rows FROM lineup_days GROUP BY 1")?;
    let pulls = q("SELECT COUNT(*) pulled, SUM(n_players > 0) with_games, MAX(game_date) last, SUM(n_players) player_rows FROM realized_pulls")?
        .into_iter()
        .next()
        .unwrap_or(Value::Null);
    let last_run = q("SELECT command, league, started_at, status FROM runs ORDER BY run_id DESC LIMIT 1")?;
    let path = path_str(&ledger.path);
    drop(ledger);
    let payload = json!({
        "db": path, "bar": bar, "episodes": eps, "transactions": tx, "decisions": dec, "projections": proj,
        "lineups": lineups, "realized": pulls, "last_run": last_run.first().cloned().unwrap_or(Value::Null),
    });
    if a.json {
        dump(&payload);
        return Ok(());
    }
    echo(&path);
    print_bar(&payload["bar"]);
    echo("-- Ledger --");
    if !eps.is_empty() {
        let mut t = Table::new("Recommendation episodes");
        t.cols(&[("League", L), ("Kind", L), ("Status", L), ("N", R)]);
        for e in &eps {
            t.row([py_str(&e["league"]), py_str(&e["kind"]), py_str(&e["status"]), py_str(&e["n"])]);
        }
        t.print();
    } else {
        echo("No episodes yet: run `fm harness daily`.");
    }
    for p in &proj {
        echo(format!(
            "{}: projections {} rows over {} day(s), {} with v2 inputs",
            py_str(&p["league"]),
            py_str(&p["rows"]),
            py_str(&p["days"]),
            if truthy(p.get("with_inputs")) { py_str(&p["with_inputs"]) } else { "0".into() }
        ));
    }
    for x in &tx {
        let who = if truthy(x.get("is_me")) { "mine" } else { "other teams" };
        echo(format!("{}: {} transaction rows ({who})", py_str(&x["league"]), py_str(&x["n"])));
    }
    for d in &dec {
        echo(format!("{}: {} decisions {}", py_str(&d["league"]), py_str(&d["n"]), py_str(&d["origin"])));
    }
    for ln in &lineups {
        echo(format!("{}: lineups {} rows over {} day(s)", py_str(&ln["league"]), py_str(&ln["rows"]), py_str(&ln["days"])));
    }
    let or0 = |k: &str| if truthy(pulls.get(k)) { py_str(&pulls[k]) } else { "0".to_string() };
    echo(format!(
        "NHL results: {} day(s) pulled, {} with games, {} player-games (last {})",
        or0("pulled"),
        or0("with_games"),
        or0("player_rows"),
        if truthy(pulls.get("last")) { py_str(&pulls["last"]) } else { "-".into() }
    ));
    if let Some(r) = last_run.first() {
        echo(format!(
            "Last run: {} {} at {} ({})",
            py_str(&r["command"]),
            if truthy(r.get("league")) { py_str(&r["league"]) } else { String::new() },
            py_str(&r["started_at"]),
            py_str(&r["status"])
        ));
    }
    Ok(())
}

// --------------------------------------------------------------------------- params, refit, rollback

fn print_refit(r: &Value) {
    if truthy(r.get("locked")) {
        echo(format!("Refit {}.", py_str(&r["lock_reason"])));
        return;
    }
    for n in r.get("notes").and_then(|n| n.as_array()).into_iter().flatten() {
        echo(py_str(n));
    }
    let lg = {
        let s = obj(r.get("leagues").unwrap_or(&Value::Null)).iter().map(|(k, v)| format!("{k} {}", py_str(v))).collect::<Vec<_>>().join(", ");
        if s.is_empty() {
            "none".to_string()
        } else {
            s
        }
    };
    echo(format!(
        "Replay table as of {}: {} matured 28-day obs ({} goalies) over {} weekly snapshot(s), {} matured 7-day obs (leagues: {lg}); active params {}",
        py_str(&r["as_of"]),
        py_str(&r["n_live"]),
        py_str(&r["n_goalie"]),
        py_str(&r["weeks"]),
        py_str(&r["n_week"]),
        py_str(&r["active_version"])
    ));
    let rp = r.get("replay").cloned().unwrap_or(json!({}));
    if truthy(rp.get("checked")) {
        echo(format!(
            "Replay check: {} rows, max |replay - archived fpg| {}, {} over 0.01; {} approximate rows (inputs without means / zeros)",
            py_str(&rp["checked"]),
            num(rp.get("max_abs_diff").unwrap_or(&Value::Null), 4),
            count(&rp, "over_0.01"),
            count(&rp, "approximate_rows")
        ));
    }
    if let Some(gg) = r.get("group_gains").and_then(|g| g.as_object()).filter(|g| !g.is_empty()) {
        let items: Vec<String> =
            gg.iter().map(|(k, v)| format!("{k} {}", fm_pycompat::fmt::percent(fv(v).unwrap_or(0.0), 2, true))).collect();
        echo(format!("Training-loss gain by group: {}", items.join(", ")));
    }
    if let Some(rows) = r.get("rows").and_then(|r| r.as_array()).filter(|r| !r.is_empty()) {
        let mut t = Table::new("Refit proposal (groups chosen by marginal gain; holdout = 2 most recent matured weeks)");
        for c in ["Param", "Tier", "Current", "Candidate", "Bound", "Holdout before", "Holdout after", "Hist before", "Hist after"] {
            t.col(c, if ["Param", "Tier", "Bound"].contains(&c) { L } else { R });
        }
        let g4 = |v: &Value| fm_pycompat::fmt::general(fv(v).unwrap_or(f64::NAN), Some(4), false);
        for row in rows {
            t.row([
                py_str(&row["param"]),
                py_str(&row["tier"]),
                g4(&row["current"]),
                g4(&row["candidate"]),
                py_str(&row["bound"]),
                num(&row["holdout_before"], 4),
                num(&row["holdout_after"], 4),
                num(&row["hist_before"], 4),
                num(&row["hist_after"], 4),
            ]);
        }
        t.print();
        let pct = |v: &Value, sign: bool| match fv(v) {
            None => "-".to_string(),
            Some(x) => fm_pycompat::fmt::percent(x, 2, sign),
        };
        for (obj_name, s) in obj(r.get("objectives").unwrap_or(&Value::Null)) {
            let ci = if s["ci_lo"].is_null() {
                "-".to_string()
            } else {
                format!("[{}, {}]", pct(&s["ci_lo"], true), pct(&s["ci_hi"], true))
            };
            echo(format!(
                "{obj_name}: holdout n={}, improvement {} (90% CI {ci}); history change {} (n={})",
                py_str(&s["n_holdout"]),
                pct(&s["gain"], true),
                pct(&s["hist_change"], true),
                count(&s, "n_hist")
            ));
        }
    }
    if let Some(gt) = r.get("gate").filter(|g| truthy(Some(g))) {
        echo(format!("Gate: {}", if truthy(gt.get("passed")) { "PASSED" } else { "FAILED" }));
        for reason in gt.get("reasons").and_then(|x| x.as_array()).into_iter().flatten() {
            echo(format!("  - {}", py_str(reason)));
        }
    }
    match r.get("action").and_then(|a| a.as_str()) {
        Some("applied") => echo(format!("Applied {} (undo with `fm harness rollback`).", py_str(&r["version"]))),
        Some("proposed") => echo(format!("Proposed {} (not applied).", py_str(&r["version"]))),
        Some("dry-run") => echo("Dry run: nothing written."),
        _ => {}
    }
}

#[derive(Args, Clone, Debug)]
pub struct RefitArgs {
    /// Fit and gate, write nothing.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Activate the candidate if it passes the gate.
    #[arg(long)]
    pub apply: bool,
    /// Comma-separated groups: k_inseason, recency_weights, projection_weight, k_projection (Tier A), availability, start_share, offnight_bonus (Tier B).
    #[arg(long)]
    pub only: Option<String>,
    #[arg(long, short = 'l', default_value = "all", help = LEAGUE_HELP)]
    pub league: String,
    /// Bypass the calendar lock (never the statistical gate).
    #[arg(long)]
    pub force: bool,
    /// Pretend today is YYYY-MM-DD.
    #[arg(long = "as-of")]
    pub as_of: Option<String>,
    #[arg(long = "json")]
    pub json: bool,
}

fn refit_cmd(cli: &mut Cli, a: &RefitArgs) -> Result<()> {
    install_params(cli);
    if a.dry_run && a.apply {
        echo("--dry-run and --apply are exclusive.");
        return Err(Exit(2).into());
    }
    let mode = if a.dry_run {
        "dry-run"
    } else if a.apply {
        "apply"
    } else {
        "propose"
    };
    let groups: Option<Vec<String>> =
        a.only.as_deref().map(|o| o.split(',').map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).collect());
    let today = parse_day(a.as_of.as_deref())?.unwrap_or(cli.today());
    let lgs = if ["all", "both"].contains(&a.league.to_lowercase().as_str()) || a.league.is_empty() { None } else { Some(leagues(&a.league)?) };
    let ledger = open_ledger(cli)?;
    let r = match run_refit_dict(&ledger, today, mode, groups, lgs, a.force) {
        Ok(r) => r,
        Err(e) => {
            echo(format!("{e:#}"));
            return Err(Exit(2).into());
        }
    };
    if mode != "dry-run" && !truthy(r.get("locked")) {
        let mut summary = JMap::new();
        for k in ["mode", "action", "version", "n_live", "groups_selected"] {
            summary.insert(k.into(), r.get(k).cloned().unwrap_or(Value::Null));
        }
        record_run(&ledger, "refit", Some(&a.league), Some(&iso(today)), Some(&Value::Object(summary)), "ok")?;
    }
    drop(ledger);
    if a.json {
        dump(&r);
        return Ok(());
    }
    print_refit(&r);
    Ok(())
}

#[derive(Args, Clone, Debug)]
pub struct RollbackArgs {
    /// vNNNN or packaged (default: the active version's parent).
    #[arg(long)]
    pub to: Option<String>,
    #[arg(long = "json")]
    pub json: bool,
}

fn rollback_cmd(cli: &mut Cli, a: &RollbackArgs) -> Result<()> {
    install_params(cli);
    let ledger = open_ledger(cli)?;
    let out = match ParamsStore::new(None, Some(&ledger), None).rollback(a.to.as_deref(), "manual", None) {
        Ok(o) => o,
        Err(e) => {
            let msg = format!("{e:#}");
            if a.json {
                echo(dumps(&json!({"error": msg}), &DumpOpts::default()));
            } else {
                echo(&msg);
            }
            return Err(Exit(1).into());
        }
    };
    record_run(&ledger, "rollback", None, Some(&iso(cli.today())), Some(&out), "ok")?;
    drop(ledger);
    if a.json {
        echo(dumps(&out, &DumpOpts { indent: Some(2), ..DumpOpts::default() }));
        return Ok(());
    }
    let hash: String = out["hash"].as_str().unwrap_or("").chars().take(10).collect();
    echo(format!("Rolled back {} -> {} (params hash {hash}).", py_str(&out["from"]), py_str(&out["to"])));
    Ok(())
}

#[derive(Args, Clone, Debug)]
pub struct ParamsArgs {
    /// List every version with its changelog.
    #[arg(long)]
    pub history: bool,
    #[arg(long = "json")]
    pub json: bool,
}

fn params_cmd(cli: &mut Cli, a: &ParamsArgs) -> Result<()> {
    use fm_engine::valuation::params as vparams;
    install_params(cli);
    let ledger = open_ledger(cli)?;
    let st = ParamsStore::new(None, Some(&ledger), None);
    st.sync_ledger(Some(&ledger))?;
    let versions: Vec<Value> = st.versions().into_iter().map(Value::Object).collect();
    let active = st.active_name();
    let dir = path_str(&st.dir);
    drop(ledger);
    let knobs = current_knobs(None);
    let today = cli.today();
    let mut kn = JMap::new();
    for k in KNOBS.iter() {
        kn.insert(
            k.name.into(),
            json!({"value": knobs.get(k.name).copied(), "group": k.group, "tier": tier(k.group), "bound": k.bound_label()}),
        );
    }
    let slim: Vec<Value> = versions
        .iter()
        .map(|v| {
            let mut m = JMap::new();
            for k in ["version", "status", "created", "parent"] {
                m.insert(k.into(), v.get(k).cloned().unwrap_or(Value::Null));
            }
            Value::Object(m)
        })
        .collect();
    let payload = json!({
        "active": active, "source": vparams::source(None), "hash": vparams::params_hash(None),
        "override_enabled": vparams::override_enabled(), "dir": dir,
        "knobs": kn,
        "next_refit": iso(if is_refit_day(today) { today } else { next_refit_day(today) }),
        "versions": if a.history { Value::Array(versions.clone()) } else { Value::Array(slim) },
    });
    if a.json {
        dump(&payload);
        return Ok(());
    }
    let hash: String = payload["hash"].as_str().unwrap_or("").chars().take(10).collect();
    echo(format!("Active params: {} ({}; hash {hash})", payload["active"].as_str().unwrap_or(""), py_str(&payload["source"])));
    if !truthy(payload.get("override_enabled")) {
        echo("FM_PARAMS_OVERRIDE=0: harness versions are ignored.");
    }
    let mut t = Table::new("Refittable parameters");
    for c in ["Param", "Group", "Tier", "Value", "Step bound"] {
        t.col(c, if c == "Value" { R } else { L });
    }
    for (name, k) in obj(&payload["knobs"]) {
        t.row([name, py_str(&k["group"]), py_str(&k["tier"]), g4(&k["value"]), py_str(&k["bound"])]);
    }
    t.print();
    echo(format!(
        "Next refit day: {} (locked until {}; Tier A auto-apply from 2026-11-16, Tier B proposals only until 2026-12-01).",
        py_str(&payload["next_refit"]),
        iso(refit_start())
    ));
    if versions.is_empty() {
        echo("No harness versions yet (packaged params only).");
    } else if a.history {
        let mut t = Table::new("Params versions");
        for c in ["Version", "Status", "Parent", "Created", "Changed", "Holdout", "Hist", "N live", "Changelog"] {
            t.col(c, L);
        }
        let or_dash = |v: &Value| if truthy(Some(v)) { py_str(v) } else { "-".to_string() };
        for v in &versions {
            let m = v.get("metrics").cloned().filter(|m| truthy(Some(m))).unwrap_or(json!({}));
            let nv = |k: &str| num(m.get(k).unwrap_or(&Value::Null), 4);
            let log = v
                .get("changelog")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
                .map(|e| format!("{} {} ({})", py_str(e.get("at").unwrap_or(&json!(""))).chars().take(10).collect::<String>(), py_str(&e["event"]), py_str(&e["by"])))
                .collect::<Vec<_>>()
                .join("; ");
            t.row([
                py_str(&v["version"]),
                or_dash(v.get("status").unwrap_or(&Value::Null)),
                or_dash(v.get("parent").unwrap_or(&Value::Null)),
                py_str(v.get("created").unwrap_or(&Value::Null)).chars().take(10).collect::<String>(),
                v.get("changed_keys").and_then(|c| c.as_array()).into_iter().flatten().map(py_str).collect::<Vec<_>>().join(", "),
                format!("{} -> {}", nv("holdout_before"), nv("holdout_after")),
                format!("{} -> {}", nv("hist_before"), nv("hist_after")),
                m.get("n_live").map(py_str).unwrap_or_else(|| "-".into()),
                log,
            ]);
        }
        t.print();
    } else {
        let items: Vec<String> = versions.iter().map(|v| format!("{} ({})", py_str(&v["version"]), py_str(v.get("status").unwrap_or(&Value::Null)))).collect();
        echo(format!("{} version(s): {} (--history for details)", versions.len(), items.join(", ")));
    }
    Ok(())
}

fn g4(v: &Value) -> String {
    match fv(v) {
        Some(x) => fm_pycompat::fmt::general(x, Some(4), false),
        None => py_str(v),
    }
}

// --------------------------------------------------------------------------- auto-apply

#[derive(Args, Clone, Debug)]
pub struct AutoApplyArgs {
    /// on | off (omit to show the current setting)
    pub state: Option<String>,
    #[arg(long = "json")]
    pub json: bool,
}

fn auto_apply_cmd(cli: &mut Cli, a: &AutoApplyArgs) -> Result<()> {
    if let Some(state) = &a.state {
        let v = state.trim().to_lowercase();
        if v != "on" && v != "off" {
            echo(format!("Expected on or off, got {}.", py_repr(state)));
            return Err(Exit(2).into());
        }
        set_pref(HARNESS_AUTO_APPLY_KEY, Some(json!(v == "on")), cli.data_dir())?;
    }
    let on = harness_auto_apply(cli.data_dir());
    let from = iso(auto_apply_start());
    if a.json {
        echo(dumps(&json!({"auto_apply": on, "from": from}), &DumpOpts::default()));
        return Ok(());
    }
    if on {
        echo(format!(
            "Auto-apply is on: from {from}, `fm harness daily` applies a Tier A refit that passes the gate on a refit day (undo with `fm harness rollback`)."
        ));
    } else {
        echo("Auto-apply is off: refit days only store proposals; apply one with `fm harness refit --apply`.");
    }
    Ok(())
}
