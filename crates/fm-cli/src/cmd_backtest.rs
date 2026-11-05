//! `fm backtest ...` (port of `fantasy_manager/cli_backtest.py`): history table, model
//! evaluation, parameter fitting, projection archive, grading. Owner: work package
//! `W10-cli-ops`.
//!
//! The sub-app ignores the global `--json` (it has its own `--json` flags). Rich markup is
//! dropped from the human output, tables are rendered with [`Table`].

use std::sync::Arc;

use anyhow::Result;
use chrono::{Local, NaiveDate, NaiveDateTime};
use clap::{Args, FromArgMatches};
use fm_backtest::data::{load_table, parse_seasons, season_range, season_start_year, PlayerSeason, LAST_SEASON};
use fm_backtest::evaluate::summarize;
use fm_backtest::fit::REPORT_AGES;
use fm_backtest::pipeline::{latest_report, refresh_data, run_evaluation, run_fit};
use fm_backtest::scoring::load_scoring;
use fm_core::models::LeagueContext;
use fm_core::{Map, PlayerValue, Recommendation};
use fm_engine::archive::{archive_projections, archive_recommendations};
use fm_http::HttpCache;
use fm_providers::pipeline::{self, LoadError, LoadOptions, Loaded};
use fm_pycompat::pyjson::{dumps, DumpOpts};
use serde_json::{json, Value};

use crate::app::CommandSpec;
use crate::common::*;

/// `backtest_app`.
pub fn backtest_spec() -> CommandSpec {
    CommandSpec {
        name: "backtest",
        order: 1000,
        build: || {
            clap::Command::new("backtest")
                .about("Backtest projection models on NHL history, fit constants, archive projections.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(DataArgs::augment_args(
                    clap::Command::new("data").about("Build / refresh the per-player-season table (incremental; cached for 30 days)."),
                ))
                .subcommand(RunArgs::augment_args(
                    clap::Command::new("run").about("Evaluate the models; writes results-<date>.json and report-<date>.md under data/backtest."),
                ))
                .subcommand(FitArgs::augment_args(
                    clap::Command::new("fit").about("Fit shrinkage k, recency weights and age curves; writes data/backtest/fitted_params.json."),
                ))
                .subcommand(ArchiveArgs::augment_args(
                    clap::Command::new("archive").about("Snapshot provider projections (+ ours) and recommendations to data/archive (daily-safe)."),
                ))
                .subcommand(GradeArgs::augment_args(
                    clap::Command::new("grade").about("Score archived provider projections vs ours against a finished season's actuals."),
                ))
                .subcommand(clap::Command::new("report").about("Print the latest backtest report."))
        },
        run: |cli, m| match m.subcommand() {
            Some(("data", s)) => data_cmd(cli, &DataArgs::from_arg_matches(s)?),
            Some(("run", s)) => run_cmd(cli, &RunArgs::from_arg_matches(s)?),
            Some(("fit", s)) => fit_cmd(cli, &FitArgs::from_arg_matches(s)?),
            Some(("archive", s)) => archive_cmd(cli, &ArchiveArgs::from_arg_matches(s)?),
            Some(("grade", s)) => grade_cmd(cli, &GradeArgs::from_arg_matches(s)?),
            Some(("report", _)) => report_cmd(cli),
            _ => Err(Exit(2).into()),
        },
    }
}

impl crate::app::Backtest {
    pub fn commands(&self) -> Vec<CommandSpec> {
        vec![backtest_spec()]
    }
}

/// A JSON number / string the way Python's `f"{x}"` prints it.
pub(crate) fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        other => dumps(other, &DumpOpts::default()),
    }
}

/// `datetime.now()` (naive local) of the run's clock.
pub(crate) fn local_now(cli: &Cli) -> NaiveDateTime {
    cli.clock.utc().with_timezone(&Local).naive_local()
}

/// `_load_league_full` minus the recommendations: provider -> enrich -> valuate through the
/// shared pipeline (snapshot mode loads the fixture). Typed [`LoadError`]s are kept.
pub(crate) fn load_league(cli: &Cli, league: &str, cache: &Arc<HttpCache>) -> Result<Loaded> {
    match &cli.snapshot {
        Some(root) => pipeline::from_snapshot(&crate::app::snapshot_dir_for(root, league), Some(&cli.settings.fm_data_dir)),
        None => pipeline::load_all(
            league,
            &cli.settings,
            &LoadOptions { deep: false, mode: None, cache: Some(cache.clone()), clock: Some(cli.clock), ..LoadOptions::default() },
        ),
    }
}

/// The advise() recommendations of `_load_league_full(with_recs=True)`; failures become warnings.
pub(crate) fn load_recs(cli: &Cli, d: &Loaded, warnings: &mut Vec<String>) -> Vec<Recommendation> {
    use fm_engine::recommend::advise::{advise, AdviseOpts};
    use fm_engine::recommend::injuries::History;

    let history = match cli.history() {
        Ok(h) => h,
        Err(e) => {
            warnings.push(format!("recommendations failed: {e}"));
            return Vec::new();
        }
    };
    let opts = AdviseOpts { dynasty_values: d.dyn_values.as_ref(), history: &History::Store(&history), ..AdviseOpts::default() };
    let now = cli.now();
    let memo = cli.memo();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| advise(&d.lc, &d.values, &opts, &now, &memo))) {
        Ok(r) => r,
        Err(_) => {
            warnings.push("recommendations failed: panic".to_string());
            Vec::new()
        }
    }
}

// --------------------------------------------------------------------------- data

#[derive(Args, Clone, Debug)]
pub struct DataArgs {
    /// First season (e.g. 20102011 or 2010).
    #[arg(long = "from-season", default_value = "20102011")]
    pub from_season: String,
    /// Last season (default: last finished).
    #[arg(long = "to-season")]
    pub to_season: Option<String>,
    /// Seasons to fetch Nov 1 / Dec 1 / Jan 1 checkpoint windows for ('' to skip).
    #[arg(long, default_value = "20212022-20252026")]
    pub inseason: String,
    /// Refetch seasons already stored.
    #[arg(long)]
    pub force: bool,
}

fn data_cmd(cli: &mut Cli, a: &DataArgs) -> Result<()> {
    let first = *parse_seasons(Some(&a.from_season), &[])?.first().ok_or_else(|| fail("no first season"))?;
    let last = *parse_seasons(a.to_season.as_deref(), &[LAST_SEASON])?.last().ok_or_else(|| fail("no last season"))?;
    let inseason = if a.inseason.is_empty() { Vec::new() } else { parse_seasons(Some(&a.inseason), &[])? };
    let cache = cli.cache()?;
    let mut log = |m: &str| echo(m);
    let info = refresh_data(cache, cli.data_dir(), &season_range(first, last), &inseason, a.force, cli.today(), Some(&mut log))?;
    let n = info["seasons"].as_array().map(|s| s.len()).unwrap_or(0);
    echo(format!(
        "{} player-seasons over {} seasons, {} in-season checkpoints. {} network requests ({}s), {} cache hits, wall {}s.",
        py_str(&info["rows"]),
        n,
        py_str(&info["checkpoints"]),
        py_str(&info["network_requests"]),
        py_str(&info["network_seconds"]),
        py_str(&info["cache_hits"]),
        py_str(&info["wall_seconds"]),
    ));
    Ok(())
}

// --------------------------------------------------------------------------- run

#[derive(Args, Clone, Debug)]
pub struct RunArgs {
    /// Comma list: naive,marcel,fm_current,fm_fitted,fm_multi.
    #[arg(long)]
    pub models: Option<String>,
    /// e.g. 20162017-20252026 (default).
    #[arg(long)]
    pub seasons: Option<String>,
    /// espn | fantrax | default | path to JSON.
    #[arg(long, default_value = "espn")]
    pub scoring: String,
    /// Also score in-season checkpoints.
    #[arg(long, overrides_with = "no_inseason")]
    pub inseason: bool,
    #[arg(long = "no-inseason", overrides_with = "inseason", hide = true)]
    pub no_inseason: bool,
    #[arg(long = "json")]
    pub json: bool,
}

fn fnum(x: Option<f64>, nd: usize) -> String {
    x.map(|v| fixed(v, nd)).unwrap_or_else(|| "-".to_string())
}

fn fpct(x: Option<f64>) -> String {
    x.map(|v| format!("{}%", fixed(v * 100.0, 0))).unwrap_or_else(|| "-".to_string())
}

fn run_cmd(cli: &mut Cli, a: &RunArgs) -> Result<()> {
    let models: Option<Vec<String>> = a
        .models
        .as_deref()
        .map(|m| m.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
    let seasons = parse_seasons(a.seasons.as_deref(), &[])?;
    let inseason = !a.no_inseason;
    let json_out = a.json;
    let mut log = |m: &str| {
        if !json_out {
            echo(m)
        }
    };
    let out = run_evaluation(
        cli.data_dir(),
        &a.scoring,
        models.as_deref(),
        if seasons.is_empty() { None } else { Some(&seasons) },
        inseason,
        None,
        cli.today(),
        None,
        Some(&mut log),
    )?;
    let summ = |kind: &str, pool: &str| -> Value {
        let mut m = serde_json::Map::new();
        for (model, s) in summarize(&out.rows, kind, pool) {
            let f = |x: Option<f64>| x.map(|v| json!(v)).unwrap_or(Value::Null);
            m.insert(
                model,
                json!({"mae": f(s.mae), "rmse": f(s.rmse), "bias": f(s.bias), "spearman": f(s.spearman),
                       "top50": f(s.top50), "top100": f(s.top100), "n": s.n, "seasons": s.seasons}),
            );
        }
        Value::Object(m)
    };
    if json_out {
        let mut summary = serde_json::Map::new();
        for k in ["preseason", "inseason"] {
            for p in ["skaters", "goalies"] {
                summary.insert(format!("{k}/{p}"), summ(k, p));
            }
        }
        dump(&json!({"results": out.results, "report": out.report, "summary": Value::Object(summary)}));
        return Ok(());
    }
    for (kind, pool) in [("preseason", "skaters"), ("preseason", "goalies"), ("inseason", "skaters")] {
        let s = summarize(&out.rows, kind, pool);
        if s.is_empty() {
            continue;
        }
        let mut t = Table::new(format!("{kind} - {pool} (mean over seasons)"));
        for c in ["model", "MAE", "RMSE", "bias", "Spearman", "top-50", "top-100", "n"] {
            t.col(c, if c == "model" { L } else { R });
        }
        for (m, v) in &s {
            t.row([
                m.clone(),
                fnum(v.mae, 3),
                fnum(v.rmse, 3),
                fnum(v.bias, 3),
                fnum(v.spearman, 3),
                fpct(v.top50),
                fpct(v.top100),
                v.n.to_string(),
            ]);
        }
        t.print();
    }
    echo(format!("Report: {}\nResults: {}", out.report, out.results));
    Ok(())
}

// --------------------------------------------------------------------------- fit

#[derive(Args, Clone, Debug)]
pub struct FitArgs {
    #[arg(long, default_value = "espn")]
    pub scoring: String,
    /// Bootstrap replicates for the age-curve intervals.
    #[arg(long, default_value_t = 200)]
    pub boot: usize,
    #[arg(long = "json")]
    pub json: bool,
}

/// `f"{x:.{nd}f}"`.
pub(crate) fn fixed(x: f64, nd: usize) -> String {
    fm_pycompat::fmt::fixed(x, nd, false)
}

/// `f"{x:g}"`.
pub(crate) fn g(x: f64) -> String {
    fm_pycompat::fmt::general(x, None, false)
}

fn f_of(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(f64::NAN)
}

fn fit_cmd(cli: &mut Cli, a: &FitArgs) -> Result<()> {
    let out = run_fit(cli.data_dir(), &a.scoring, a.boot, None, local_now(cli), None)?;
    if a.json {
        dump(&out);
        return Ok(());
    }
    let pre = &out["preseason"];
    let (ak, km, k1) = (&pre["app_k"], &pre["k_multi"], &pre["k"]);
    echo(format!(
        "Preseason shrinkage k, 3 seasons (the app's baseline): F {}, D {}, G {} (app {}/{}/{}); 1 season: F {}, D {}, G {}, skaters pooled {}",
        fixed(f_of(km, "F"), 0),
        fixed(f_of(km, "D"), 0),
        fixed(f_of(km, "G"), 0),
        g(f_of(ak, "F")),
        g(f_of(ak, "D")),
        g(f_of(ak, "G")),
        fixed(f_of(k1, "F"), 0),
        fixed(f_of(k1, "D"), 0),
        fixed(f_of(k1, "G"), 0),
        fixed(f_of(pre, "k_skater_pooled"), 0),
    ));
    if let Some(ins) = out.get("inseason").filter(|v| !v.is_null()) {
        let w = &ins["recency_weights"];
        echo(format!(
            "In-season: k skater {}, goalie {}; weights season {} / L30 {} / L15 {} / L7 {}  (MAE {} -> {})",
            fixed(f_of(ins, "k_skater"), 0),
            fixed(f_of(ins, "k_goalie"), 0),
            fixed(f_of(w, "season"), 2),
            fixed(f_of(w, "last30"), 2),
            fixed(f_of(w, "last15"), 2),
            fixed(f_of(w, "last7"), 2),
            fixed(f_of(ins, "mae_app"), 4),
            fixed(f_of(ins, "mae_fitted"), 4),
        ));
    }
    let mut t = Table::new("Age curve (level, peak = 1.00, 90% CI)");
    t.col("group", L);
    for age in REPORT_AGES {
        t.col(age.to_string(), R);
    }
    if let Some(rep) = out["age_curve_report"].as_object() {
        for (g, ages) in rep {
            let cell = |age: i64, f: &dyn Fn(&Value) -> String| f(&ages[age.to_string()]);
            let mut r = vec![g.clone()];
            r.extend(REPORT_AGES.iter().map(|a| cell(*a, &|v| fixed(f_of(v, "level"), 2))));
            t.row(r);
            let mut r = vec!["CI".to_string()];
            r.extend(REPORT_AGES.iter().map(|a| cell(*a, &|v| format!("{}-{}", fixed(f_of(v, "lo"), 2), fixed(f_of(v, "hi"), 2)))));
            t.row(r);
            let mut r = vec!["n".to_string()];
            r.extend(REPORT_AGES.iter().map(|a| cell(*a, &|v| py_str(&v["n"]))));
            t.row(r);
        }
    }
    t.print();
    echo(format!("Wrote {}", py_str(&out["path"])));
    echo("The app reads the packaged copy, which this command never overwrites. To adopt this fit:");
    echo(format!("  copy \"{}\" \"{}\"", py_str(&out["path"]), py_str(&out["packaged_path"])));
    echo("then run the tests and `fm backtest run` (fm_current should match fm_multi).");
    Ok(())
}

// --------------------------------------------------------------------------- archive

#[derive(Args, Clone, Debug)]
pub struct ArchiveArgs {
    /// espn | fantrax | all
    #[arg(long, short = 'l', default_value = "all")]
    pub league: String,
    /// Also archive recommendations.
    #[arg(long, overrides_with = "no_recs")]
    pub recs: bool,
    #[arg(long = "no-recs", overrides_with = "recs", hide = true)]
    pub no_recs: bool,
}

fn archive_cmd(cli: &mut Cli, a: &ArchiveArgs) -> Result<()> {
    let recs = !a.no_recs;
    let leagues: Vec<String> = if a.league == "all" { vec!["espn".into(), "fantrax".into()] } else { vec![a.league.clone()] };
    let mut failed = 0;
    for lg in &leagues {
        let t0 = std::time::Instant::now();
        let cache = cli.cache()?;
        let d = match load_league(cli, lg, &cache) {
            Ok(d) => d,
            Err(e) => {
                if let Some(le) = e.downcast_ref::<LoadError>() {
                    let msg = match le {
                        LoadError::Unavailable(m) | LoadError::Failed(m) => m.clone(),
                    };
                    echo(format!("{lg}: skipped ({msg})"));
                    failed += (a.league != "all") as i32;
                } else {
                    echo(format!("{lg}: failed to load (Error: {e:#})"));
                    failed += 1;
                }
                continue;
            }
        };
        let mut warnings: Vec<String> = d.provider.warnings().to_vec();
        let rec_list = if recs { load_recs(cli, &d, &mut warnings) } else { Vec::new() };
        let ctx: &LeagueContext = &d.lc;
        let values: &Map<String, PlayerValue> = &d.values;
        let now = local_now(cli);
        let (path, status) = archive_projections(ctx, cli.data_dir(), Some(values), None, now)?;
        let all = ctx.all_players();
        let n_proj = all.iter().filter(|p| p.lines.contains_key("projected")).count();
        echo(format!(
            "{lg}: projections {status} -> {} ({} players, {n_proj} with a provider projection)",
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            all.len()
        ));
        if recs {
            let (rpath, rstatus) = archive_recommendations(&rec_list, &ctx.provider, cli.data_dir(), ctx.as_of, Some(&ctx.league_id), now)?;
            echo(format!(
                "{lg}: recommendations {rstatus} -> {} ({} recs)",
                rpath.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                rec_list.len()
            ));
        }
        for w in &warnings {
            echo(format!("  {w}"));
        }
        echo(format!("  {}s", fixed(t0.elapsed().as_secs_f64(), 1)));
    }
    if failed > 0 {
        return Err(Exit(1).into());
    }
    Ok(())
}

// --------------------------------------------------------------------------- grade

#[derive(Args, Clone, Debug)]
pub struct GradeArgs {
    /// Finished season to grade, e.g. 20262027.
    #[arg(long)]
    pub season: String,
    /// first | last | YYYY-MM-DD
    #[arg(long = "snapshot", default_value = "first")]
    pub which: String,
    /// Grade all under one preset.
    #[arg(long)]
    pub scoring: Option<String>,
}

fn grade_cmd(cli: &mut Cli, a: &GradeArgs) -> Result<()> {
    let s = *parse_seasons(Some(&a.season), &[])?.first().ok_or_else(|| fail("no season"))?;
    let table = load_table(cli.data_dir())?;
    let actuals: Vec<PlayerSeason> = table.season_rows(s).into_iter().cloned().collect();
    if actuals.is_empty() {
        echo(format!("No stored actuals for {s}; run `fm backtest data --to-season {s}` first."));
        return Err(Exit(1).into());
    }
    let y = season_start_year(s) as i32;
    let cfg = match &a.scoring {
        Some(sc) => Some(load_scoring(sc)?.1),
        None => None,
    };
    let start = NaiveDate::from_ymd_opt(y, 7, 1);
    let end = NaiveDate::from_ymd_opt(y + 1, 6, 30);
    let out = fm_harness::outcomes::score_archive(&actuals, cli.data_dir(), &["espn", "fantrax"], &a.which, cfg.as_ref(), 20, start, end)?;
    dump(&out);
    Ok(())
}

// --------------------------------------------------------------------------- report

fn report_cmd(cli: &mut Cli) -> Result<()> {
    match latest_report(cli.data_dir()) {
        None => {
            echo("No report yet; run `fm backtest run`.");
            Err(Exit(1).into())
        }
        Some(p) => {
            echo(p.display().to_string());
            echo(std::fs::read_to_string(&p)?);
            Ok(())
        }
    }
}
