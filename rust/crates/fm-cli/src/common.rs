//! Shared CLI helpers: Rust port of the helper part of `fantasy_manager/cli.py` (`_opts`,
//! `_fail`, `_cache`, `_load_context`, `_load_all`, `_dump`, `_meta`, `credit_lines`, `_footer`,
//! `_params_source`, `_vlabel`, `_fmt`, `_pos`, `_age`, `_is_prospect`, `_dyn_value`,
//! `_dyn_json`, `_strength_cell`, `_rank_cell`, `_gain_cell`, `_why`, `_rec_table`, `_status`,
//! `_stat_limits_line`, `_moves_line`, `_moves_json`, `_short_err`, `_safe`, `_collect_logs`,
//! `_json_out`, `_recs_json`, `_reason`, `_reason_val`, `_trade_wants`, `_trade_offered`,
//! `_history`, `_run_advise`, `_llm_client`, `_fetch_news`, `_match_news`, `_explain`,
//! `_age_text`). Owner: work package `W9-cli-core`; W10 command modules reuse these.
//!
//! Deviations: rich tables are rendered with comfy-table ([`Table`]: same title, columns, rows
//! and order; box drawing differs); rich markup (colours, bold) is dropped. Python's exceptions
//! for user errors are [`Exit`] / [`fail`]. `_short_err` names the error type only for the typed
//! errors the port has (`ProviderError`, `CacheMiss`, `LLMError`), else `Error`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Args, ValueEnum};
use comfy_table::{presets, CellAlignment, ContentArrangement};
use fm_core::config::Settings;
use fm_core::models::{DynastyMode, LeagueContext, Player, Reason, Recommendation, ScoringKind};
use fm_core::provider::LeagueProvider;
use fm_core::values::Horizon;
use fm_core::{DynastyValue, Map, PyDateTime};
use fm_engine::recommend::injuries::{History, StatusHistory};
use fm_engine::valuation::schedule::ScheduleMemo;
use fm_http::{CacheMiss, HttpCache};
use fm_providers::enrich::Clock;
use fm_providers::pipeline::{self, LoadError, LoadOptions, Loaded};
use fm_pycompat::fmt::{fixed, general};
use fm_pycompat::pyjson::{dumps, DumpOpts};
use fm_sources::news::NewsItem;
use serde_json::{json, Value};

pub use crate::app::CommandSpec;

// --------------------------------------------------------------------------- option enums

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LeagueName {
    Espn,
    Fantrax,
}

impl LeagueName {
    pub fn as_str(&self) -> &'static str {
        match self {
            LeagueName::Espn => "espn",
            LeagueName::Fantrax => "fantrax",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum HorizonName {
    Week,
    Season,
}

impl HorizonName {
    pub fn horizon(&self) -> Horizon {
        match self {
            HorizonName::Week => Horizon::Week,
            HorizonName::Season => Horizon::Season,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            HorizonName::Week => "week",
            HorizonName::Season => "season",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ModeName {
    Contend,
    Balanced,
    Rebuild,
}

impl ModeName {
    pub fn mode(&self) -> DynastyMode {
        match self {
            ModeName::Contend => DynastyMode::Contend,
            ModeName::Balanced => DynastyMode::Balanced,
            ModeName::Rebuild => DynastyMode::Rebuild,
        }
    }
}

// Per-command `--league/-l` and `--json` (merged with the root options by `Cli::opts`; a plain
// comment: a doc comment would become the about text of every command that flattens it).
#[derive(Args, Clone, Debug, Default)]
pub struct LeagueOpts {
    /// Which league to use.
    #[arg(long = "league", short = 'l', value_enum)]
    pub league: Option<LeagueName>,
    /// Print JSON instead of tables.
    #[arg(long = "json")]
    pub json: bool,
}

// `--mode` (`_mode_opt()`), plain comment for the same reason.
#[derive(Args, Clone, Debug, Default)]
pub struct ModeOpt {
    /// Dynasty mode for this run only (not saved; `fm mode` saves one).
    #[arg(long = "mode", value_enum)]
    pub mode: Option<ModeName>,
}

impl ModeOpt {
    /// `_mode_val(mode)`
    pub fn val(&self) -> Option<DynastyMode> {
        self.mode.map(|m| m.mode())
    }
}

// --------------------------------------------------------------------------- exits

/// `typer.Exit(code)`: the message (if any) was already printed.
#[derive(Debug)]
pub struct Exit(pub i32);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit {}", self.0)
    }
}

impl std::error::Error for Exit {}

/// `_fail(msg)`: print `Error: msg` to stderr; return the exit-1 error to propagate.
pub fn fail(msg: impl std::fmt::Display) -> anyhow::Error {
    fail_code(msg, 1)
}

pub fn fail_code(msg: impl std::fmt::Display, code: i32) -> anyhow::Error {
    eprintln!("Error: {msg}");
    Exit(code).into()
}

/// Print a [`LoadError`] like `_load_context` and turn it into an [`Exit`]; other errors pass.
pub fn map_load_error(e: anyhow::Error) -> anyhow::Error {
    match e.downcast_ref::<LoadError>() {
        Some(LoadError::Unavailable(m)) => {
            eprintln!("{m}");
            Exit(2).into()
        }
        Some(LoadError::Failed(m)) => fail(m),
        None => e,
    }
}

// --------------------------------------------------------------------------- output

/// `typer.echo(s)` (stdout; a closed pipe is ignored).
pub fn echo(s: impl AsRef<str>) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_ref().as_bytes());
    let _ = out.write_all(b"\n");
}

/// `typer.echo(s, err=True)`.
pub fn echo_err(s: impl AsRef<str>) {
    eprintln!("{}", s.as_ref());
}

/// `_dump(data)`: `json.dumps(data, indent=2, default=str)` + newline.
pub fn dump(data: &Value) {
    echo(dumps(data, &DumpOpts::cli()));
}

/// serde value of anything serializable (pydantic `model_dump(mode="json")`).
pub fn to_json<T: serde::Serialize>(x: &T) -> Value {
    serde_json::to_value(x).unwrap_or(Value::Null)
}

pub fn date_json(d: Option<NaiveDate>) -> Value {
    d.map(|d| Value::String(d.to_string())).unwrap_or(Value::Null)
}

/// `_meta(lc)`.
pub fn meta(lc: &LeagueContext) -> Value {
    json!({"provider": lc.provider, "league_id": lc.league_id, "as_of": lc.as_of.to_string(),
           "season_start": date_json(lc.season_start),
           "sources": lc.source_notes, "warnings": lc.warnings})
}

/// `_json_out(payload, lc, errors)`: payload + `errors` + `meta`.
pub fn json_out(payload: Value, lc: Option<&LeagueContext>, errors: &[String]) {
    let mut out = match payload {
        Value::Object(o) => o,
        other => {
            let mut o = serde_json::Map::new();
            o.insert("data".into(), other);
            o
        }
    };
    out.insert("errors".into(), json!(errors));
    if let Some(lc) = lc {
        out.insert("meta".into(), meta(lc));
    }
    dump(&Value::Object(out));
}

/// `_recs_json(recs)`.
pub fn recs_json(recs: &[Recommendation]) -> Value {
    Value::Array(recs.iter().map(to_json).collect())
}

pub fn reason<'a>(r: &'a Recommendation, code: &str) -> Option<&'a Reason> {
    r.reasons.iter().find(|x| x.code == code)
}

pub fn reason_val(r: &Recommendation, code: &str) -> Option<f64> {
    reason(r, code).and_then(|x| x.value)
}

// --------------------------------------------------------------------------- tables

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Justify {
    Left,
    Right,
}

pub use Justify::{Left as L, Right as R};

/// A rich `Table` stand-in: title, columns (with justification), rows, optional row lines.
#[derive(Clone, Debug, Default)]
pub struct Table {
    pub title: Option<String>,
    pub cols: Vec<(String, Justify)>,
    pub rows: Vec<Vec<String>>,
    pub show_lines: bool,
}

impl Table {
    pub fn new(title: impl Into<String>) -> Table {
        Table { title: Some(title.into()), ..Table::default() }
    }
    pub fn untitled() -> Table {
        Table::default()
    }
    pub fn lines(mut self) -> Table {
        self.show_lines = true;
        self
    }
    pub fn col(&mut self, name: impl Into<String>, j: Justify) -> &mut Table {
        self.cols.push((name.into(), j));
        self
    }
    pub fn cols(&mut self, cols: &[(&str, Justify)]) -> &mut Table {
        for (c, j) in cols {
            self.col(*c, *j);
        }
        self
    }
    pub fn row<S: Into<String>>(&mut self, cells: impl IntoIterator<Item = S>) -> &mut Table {
        self.rows.push(cells.into_iter().map(Into::into).collect());
        self
    }

    pub fn render(&self) -> String {
        let mut t = comfy_table::Table::new();
        t.load_preset(if self.show_lines { presets::UTF8_FULL } else { presets::UTF8_FULL_CONDENSED });
        t.set_content_arrangement(ContentArrangement::Dynamic);
        // rich: COLUMNS, else the terminal width, else 80 when not a terminal
        match std::env::var("COLUMNS").ok().and_then(|c| c.trim().parse::<u16>().ok()) {
            Some(w) => {
                t.set_width(w);
            }
            None => {
                use std::io::IsTerminal;
                if !std::io::stdout().is_terminal() {
                    t.set_width(80);
                }
            }
        }
        t.set_header(self.cols.iter().map(|(c, _)| c.clone()));
        for r in &self.rows {
            t.add_row(r.clone());
        }
        for (i, (_, j)) in self.cols.iter().enumerate() {
            if let Some(c) = t.column_mut(i) {
                c.set_cell_alignment(if *j == Justify::Right { CellAlignment::Right } else { CellAlignment::Left });
            }
        }
        let body = t.to_string();
        match &self.title {
            Some(title) if !title.is_empty() => {
                let width = body.lines().next().map(|l| l.chars().count()).unwrap_or(0);
                let n = title.chars().count();
                let pad = width.saturating_sub(n) / 2;
                format!("{}{title}\n{body}", " ".repeat(pad))
            }
            _ => body,
        }
    }

    pub fn print(&self) {
        echo(self.render());
    }
}

// --------------------------------------------------------------------------- root context

/// The root typer context (`ctx.obj`) plus what Python read from globals: settings, the clock
/// (system, `FM_NOW` or the snapshot's) and the hidden `--from-snapshot` directory.
pub struct Cli {
    /// root `--league` (default espn)
    pub league: String,
    /// root `--json`
    pub json: bool,
    pub settings: Settings,
    pub snapshot: Option<PathBuf>,
    pub clock: Clock,
}

impl Cli {
    /// `_opts(ctx, league, json_out)`.
    pub fn opts(&self, o: &LeagueOpts) -> (String, bool) {
        (o.league.map(|l| l.as_str().to_string()).unwrap_or_else(|| self.league.clone()), o.json || self.json)
    }

    /// `_opts(ctx, None, json_out)[1]`.
    pub fn as_json(&self, json_out: bool) -> bool {
        json_out || self.json
    }

    pub fn now(&self) -> PyDateTime {
        PyDateTime::from_utc(self.clock.utc())
    }

    pub fn now_utc(&self) -> DateTime<Utc> {
        self.clock.utc()
    }

    /// Python `datetime.now()`: naive local wall clock (pregame / daily snapshot stamps, "8:43 AM").
    pub fn now_local(&self) -> PyDateTime {
        PyDateTime::naive(self.clock.utc().with_timezone(&chrono::Local).naive_local())
    }

    pub fn today(&self) -> NaiveDate {
        self.clock.today
    }

    pub fn data_dir(&self) -> &Path {
        &self.settings.fm_data_dir
    }

    /// A schedule memo with betting odds from the data dir.
    pub fn memo(&self) -> ScheduleMemo {
        ScheduleMemo::new(Some(self.settings.fm_data_dir.clone()))
    }

    /// `_cache()` (a tape-backed cache in snapshot mode).
    pub fn cache(&self) -> Result<Arc<HttpCache>> {
        match &self.snapshot {
            Some(root) => {
                let file = fm_providers::snapshot::snapshot_file(&crate::app::snapshot_dir_for(root, &self.league));
                match fm_providers::snapshot::tape_next_to(&file) {
                    Some(t) => Ok(Arc::new(HttpCache::from_tape(fm_http::Tape::load(&t)?))),
                    None => Ok(Arc::new(HttpCache::from_tape(Default::default()))),
                }
            }
            None => pipeline::open_cache(&self.settings),
        }
    }

    fn snapshot_load(&self, root: &Path, league: &str, mode: Option<DynastyMode>) -> Result<Loaded> {
        let dir = crate::app::snapshot_dir_for(root, league);
        let mut d = pipeline::from_snapshot(&dir, Some(&self.settings.fm_data_dir))?;
        if mode.is_some() {
            pipeline::resolve_mode(&mut d.lc, &self.settings, mode);
            d.dyn_values = pipeline::dynasty(&mut d.lc, &d.values, Some(d.provider.as_ref()));
        }
        Ok(d)
    }

    fn opts_for(&self, deep: bool, mode: Option<DynastyMode>, cache: Option<Arc<HttpCache>>) -> LoadOptions {
        LoadOptions { deep, mode, cache, clock: Some(self.clock), ..LoadOptions::default() }
    }

    /// `_load_all(league, deep, cache, mode)` with user-facing errors.
    pub fn load_all(&self, league: &str, deep: bool, mode: Option<DynastyMode>) -> Result<Loaded> {
        self.load_all_with(league, deep, None, mode)
    }

    /// `_load_all` reusing an open cache.
    pub fn load_all_with(&self, league: &str, deep: bool, cache: Option<Arc<HttpCache>>, mode: Option<DynastyMode>) -> Result<Loaded> {
        let r = match &self.snapshot {
            Some(root) => self.snapshot_load(root, league, mode),
            None => pipeline::load_all(league, &self.settings, &self.opts_for(deep, mode, cache)),
        };
        r.map_err(map_load_error)
    }

    /// `_load_all` for several leagues (`pipeline::load_all_many`), errors already mapped.
    pub fn load_many(&self, leagues: &[&str], deep: bool, mode: Option<DynastyMode>) -> Vec<Result<Loaded>> {
        match &self.snapshot {
            Some(root) => leagues.iter().map(|l| self.snapshot_load(root, l, mode).map_err(map_load_error)).collect(),
            None => pipeline::load_all_many(leagues, &self.settings, &self.opts_for(deep, mode, None))
                .into_iter()
                .map(|r| r.map_err(map_load_error))
                .collect(),
        }
    }

    /// `_load_context(league, cache, mode)`: (ctx, provider) without enrichment / valuation.
    pub fn load_context(&self, league: &str, mode: Option<DynastyMode>) -> Result<(LeagueContext, Box<dyn LeagueProvider>)> {
        let r = match &self.snapshot {
            Some(root) => self.snapshot_load(root, league, mode).map(|d| (d.lc, d.provider)),
            None => {
                pipeline::install_params(&self.settings);
                pipeline::open_cache(&self.settings)
                    .and_then(|cache| pipeline::load_context(league, &self.settings, &cache, mode, Some(self.clock)))
            }
        };
        r.map_err(map_load_error)
    }

    /// `_history()`: the injury status history in the data dir.
    pub fn history(&self) -> Result<StatusHistory> {
        StatusHistory::new(&self.settings.fm_data_dir)
    }
}

// --------------------------------------------------------------------------- footers / notes

pub const MONEYPUCK_CREDIT: &str = "Expected goals: MoneyPuck.com";
pub const DFO_CREDIT: &str = "Lines, power-play units and starting goalies: Daily Faceoff (dailyfaceoff.com)";

fn nonempty(s: &Option<String>) -> bool {
    s.as_deref().is_some_and(|x| !x.is_empty())
}

/// `credit_lines(lc)`.
pub fn credit_lines(lc: &LeagueContext) -> Vec<String> {
    let players = lc.all_players();
    let notes = lc.source_notes.join(" ");
    let mut out = Vec::new();
    if players.iter().any(|p| p.ixg_per_game.is_some()) && !notes.contains("MoneyPuck") {
        out.push(MONEYPUCK_CREDIT.to_string());
    }
    if players.iter().any(|p| nonempty(&p.line) || nonempty(&p.pp_unit) || p.confirmed_start.is_some())
        && !notes.contains("Daily Faceoff")
    {
        out.push(DFO_CREDIT.to_string());
    }
    out
}

/// `_mode_line(lc)`.
pub fn mode_line(lc: &LeagueContext) -> String {
    pipeline::mode_line(lc)
}

/// `_footer(lc, errors)`.
pub fn footer(lc: &LeagueContext, errors: &[String]) {
    if lc.dynasty {
        echo(format!(
            "{}; {}-year horizon (change with `fm mode contend|balanced|rebuild` or the dashboard toggle)",
            mode_line(lc),
            lc.keeper_horizon_years
        ));
    }
    for n in lc.source_notes.iter().cloned().chain(credit_lines(lc)) {
        echo(n);
    }
    for w in lc.warnings.iter().chain(errors.iter()) {
        echo(format!("! {w}"));
    }
}

/// `_params_source()`.
pub fn params_source() -> String {
    fm_engine::valuation::params::source(None)
}

/// `_stat_limits_line(lc)`.
pub fn stat_limits_line(lc: &LeagueContext) -> String {
    match &lc.stat_limits {
        None => "Games-played limits: unknown (the provider did not report them)".into(),
        Some(v) if v.is_empty() => "Games-played limits: none (no position GP caps or goalie minimum)".into(),
        Some(v) => format!("Games-played limits: {} (shown only: fm does not enforce them yet)", v.join("; ")),
    }
}

/// `_moves_line(lc)`.
pub fn moves_line(lc: &LeagueContext) -> String {
    let mut line = format!("Moves: {}", fm_engine::recommend::base::moves_text(lc));
    if let Some(f) = lc.faab_remaining {
        line += &format!("; FAAB left ${}", general(f, None, false));
    }
    line
}

/// `_moves_json(lc)`.
pub fn moves_json(lc: &LeagueContext) -> Value {
    let recent: serde_json::Map<String, Value> =
        lc.recent_adds.iter().map(|(k, v)| (k.clone(), Value::String(v.to_string()))).collect();
    json!({"limit_per_period": lc.moves_limit_per_period, "period": lc.moves_period_label,
           "used_this_period": lc.moves_used_this_period, "left": fm_engine::recommend::base::moves_left(lc),
           "limit_season": lc.moves_limit_season, "used_season": lc.moves_used_season,
           "period_start": date_json(lc.period_start), "period_end": date_json(lc.period_end),
           "recent_adds": Value::Object(recent), "faab_remaining": lc.faab_remaining})
}

// --------------------------------------------------------------------------- cells

/// SLOT_ORDER.get(slot, 99)
pub fn slot_order(slot: &str) -> usize {
    ["C", "LW", "RW", "F", "D", "UTIL", "G", "BN", "IR"].iter().position(|s| *s == slot).unwrap_or(99)
}

/// `_vlabel(lc)`.
pub fn vlabel(lc: &LeagueContext) -> &'static str {
    if lc.scoring.kind == ScoringKind::Points {
        "FPG"
    } else {
        "Val/G"
    }
}

/// `_fmt(v)`.
pub fn fmtv(v: Option<f64>) -> String {
    v.map(|x| fixed(x, 2, false)).unwrap_or_else(|| "-".into())
}

/// `_fmt(v, signed=True)`.
pub fn fmt_signed(v: Option<f64>) -> String {
    v.map(|x| fixed(x, 2, true)).unwrap_or_else(|| "-".into())
}

/// `_pos(p)`.
pub fn pos(p: &Player) -> String {
    let s = p.positions.iter().filter(|x| *x != "F").cloned().collect::<Vec<_>>().join("/");
    if s.is_empty() {
        p.positions.join("/")
    } else {
        s
    }
}

/// `_status(p)` (markup dropped).
pub fn status(p: &Player) -> String {
    p.status.as_str().to_string()
}

pub const PROSPECT_MAX_AGE: i64 = 22;
pub const PROSPECT_MAX_GP: i64 = 82;

/// `_age(p, lc, dyn)`.
pub fn age(p: &Player, lc: &LeagueContext, dyn_values: Option<&Map<String, DynastyValue>>) -> Option<f64> {
    if let Some(a) = dyn_values.and_then(|d| d.get(&p.cid)).and_then(|d| d.age) {
        return Some(a);
    }
    p.birth_date.map(|bd| (lc.as_of - bd).num_days() as f64 / 365.25)
}

/// `_is_prospect(p, age)`.
pub fn is_prospect(p: &Player, age: Option<f64>) -> bool {
    let Some(a) = age else { return false };
    if a >= (PROSPECT_MAX_AGE + 1) as f64 {
        return false;
    }
    let gp = p.career_gp.unwrap_or_else(|| p.gp("season") + p.gp("prior"));
    gp < PROSPECT_MAX_GP
}

/// `_dyn_value(d)`.
pub fn dyn_value(d: Option<&DynastyValue>) -> Option<f64> {
    d.map(|d| d.value)
}

/// `_dyn_json(d)`: `model_dump(mode="json", exclude={"player"})`.
pub fn dyn_json(d: Option<&DynastyValue>) -> Value {
    match d {
        None => Value::Null,
        Some(d) => {
            let mut v = to_json(d);
            if let Value::Object(o) = &mut v {
                o.shift_remove("player");
            }
            v
        }
    }
}

pub fn strength_cell(r: &Recommendation) -> String {
    r.strength.map(|s| fixed(s, 1, false)).unwrap_or_else(|| "-".into())
}

pub fn rank_cell(r: &Recommendation, i: usize) -> String {
    let txt = fm_engine::recommend::strength::rank_text(r);
    if txt == "-" {
        i.to_string()
    } else {
        txt
    }
}

pub fn gain_cell(r: &Recommendation) -> String {
    fm_engine::recommend::strength::gain_text(r)
}

/// `_why(r, lines)`: headline first, then the reasons (without a repeat of the headline).
pub fn why(r: &Recommendation, lines: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    if let Some(h) = r.headline.as_deref().filter(|h| !h.is_empty()) {
        out.push(h.to_string());
    }
    for x in lines {
        if Some(x.as_str()) != r.headline.as_deref() {
            out.push(x.clone());
        }
    }
    out.join("\n")
}

/// `_rec_table(recs, title)`.
pub fn rec_table(recs: &[Recommendation], title: &str) -> Table {
    let mut t = Table::new(title).lines();
    t.cols(&[("#", R), ("Recommendation", L), ("Strength", R), ("Gain", R), ("Why", L)]);
    for (i, r) in recs.iter().enumerate() {
        let lines: Vec<String> = r.reasons.iter().filter(|x| x.code != "RAW_SCORE").map(|x| x.text.clone()).collect();
        let mut w = why(r, &lines);
        if let Some(n) = r.narrative.as_deref().filter(|n| !n.is_empty()) {
            w = format!("{n}\n{w}");
        }
        t.row([rank_cell(r, i + 1), r.title.clone(), strength_cell(r), gain_cell(r), w]);
    }
    t
}

// --------------------------------------------------------------------------- best effort

pub const LLM_HINT: &str =
    "LLM explanations are off: set OPENROUTER_API_KEY in .env (free models work: FM_LLM_MODEL=openrouter/free).";
pub const WEB_HINT: &str = "The web dashboard needs the optional web extras: pip install -e .[web]";

/// KIND_TITLE
pub fn kind_title(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "injury" => "Injuries",
        "lineup" => "Lineup",
        "waiver" => "Waivers",
        "trade" => "Trades",
        "sell_high" => "Sell high",
        "buy_low" => "Buy low",
        "alert" => "Alerts",
        _ => return None,
    })
}

fn error_type_name(e: &anyhow::Error) -> &'static str {
    for c in e.chain() {
        if c.downcast_ref::<fm_core::ProviderError>().is_some() {
            return "ProviderError";
        }
        if c.downcast_ref::<CacheMiss>().is_some() {
            return "CacheMiss";
        }
        if c.downcast_ref::<fm_sources::llm::LLMError>().is_some() {
            return "LLMError";
        }
    }
    "Error"
}

/// `_short_err(e)`.
pub fn short_err(e: &anyhow::Error) -> String {
    let msg = format!("{e:#}").split_whitespace().collect::<Vec<_>>().join(" ");
    let name = error_type_name(e);
    if msg.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {}", fm_pycompat::text::py_slice(&msg, None, Some(200)))
    }
}

/// `_safe(errors, label, fn)`: a failure (error or panic) becomes `"<label> failed (<err>)"`.
/// An [`Exit`] is not swallowed: it is returned as `Err`.
pub fn safe<T>(errors: &mut Vec<String>, label: &str, f: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(v)) => Ok(Some(v)),
        Ok(Err(e)) => {
            if e.downcast_ref::<Exit>().is_some() {
                return Err(e);
            }
            errors.push(format!("{label} failed ({})", short_err(&e)));
            Ok(None)
        }
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into());
            errors.push(format!("{label} failed (panic: {})", fm_pycompat::text::py_slice(&msg, None, Some(200))));
            Ok(None)
        }
    }
}

/// `with _collect_logs(target, sink, label): f()`: WARNING+ tracing events whose target starts
/// with `target` (a Rust module path, e.g. `fm_engine::recommend::advise`) are appended to
/// `sink` as `"<label>: <message>"`.
pub fn collect_logs<T>(target: &str, sink: &mut Vec<String>, label: &str, f: impl FnOnce() -> T) -> T {
    let idx = {
        let mut caps = crate::app::CAPTURES.lock().unwrap_or_else(|e| e.into_inner());
        caps.push((target.to_string(), label.to_string(), Vec::new()));
        caps.len() - 1
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    let mut caps = crate::app::CAPTURES.lock().unwrap_or_else(|e| e.into_inner());
    if idx < caps.len() {
        let (_, _, got) = caps.remove(idx);
        sink.extend(got);
    }
    drop(caps);
    match out {
        Ok(v) => v,
        Err(p) => std::panic::resume_unwind(p),
    }
}

// --------------------------------------------------------------------------- advise / trades

/// `_trade_wants(lc, provider)`.
pub fn trade_wants(lc: &LeagueContext, provider: &dyn LeagueProvider) -> Option<Map<String, Vec<String>>> {
    fm_engine::recommend::trades::trade_block_wants(lc, &provider.extras().trade_blocks)
}

/// `_trade_offered(provider)`.
pub fn trade_offered(provider: &dyn LeagueProvider) -> Option<Map<String, Vec<String>>> {
    fm_engine::recommend::trades::trade_block_offered(&provider.extras().trade_blocks)
}

/// `_run_advise(d, history, errors, limit)`: advise() for lineup / waivers / flags / injuries /
/// alerts plus the trade engine with the trade-block wants, merged in advise order.
pub fn run_advise(
    cli: &Cli,
    d: &Loaded,
    history: &History<'_>,
    errors: &mut Vec<String>,
    limit: Option<usize>,
) -> Result<Vec<Recommendation>> {
    use fm_engine::recommend::advise::{advise, merge_normalized, AdviseOpts};
    use fm_engine::recommend::trades::{recommend_trades, EngineOpts, TradeOpts};

    let now = cli.now();
    let memo = cli.memo();
    let include = ["lineup", "waivers", "flags", "injuries", "alerts"];
    let aopts = AdviseOpts { dynasty_values: d.dyn_values.as_ref(), history, include: &include, limit: None };
    let mut logs = Vec::new();
    let recs = {
        let r = collect_logs("fm_engine::recommend::advise", &mut logs, "advise", || {
            safe(errors, "advise", || Ok(advise(&d.lc, &d.values, &aopts, &now, &memo)))
        });
        r?.unwrap_or_default()
    };
    errors.extend(logs);
    let wants = trade_wants(&d.lc, d.provider.as_ref());
    let offered = trade_offered(d.provider.as_ref());
    let engine = EngineOpts { wants: wants.as_ref(), offered: offered.as_ref(), ..EngineOpts::default() };
    let topts = TradeOpts { dynasty_values: d.dyn_values.as_ref(), engine, ..TradeOpts::default() };
    let trades = safe(errors, "trades", || recommend_trades(&d.lc, &d.values, &topts, None, None))?.unwrap_or_default();
    Ok(merge_normalized(recs, trades, limit))
}

// --------------------------------------------------------------------------- news / LLM

/// `_llm_client()`.
pub fn llm_client(settings: &Settings) -> fm_sources::llm::openrouter::LLMClient {
    fm_sources::llm::openrouter::LLMClient::from_settings(settings)
}

/// `_fetch_news(cache, errors)`: every news source through the HTTP cache, merged into the
/// 30-day store; a failing source becomes `"<label> unavailable (<reason>)"`.
pub fn fetch_news(cli: &Cli, cache: &Arc<HttpCache>, errors: &mut Vec<String>) -> Vec<NewsItem> {
    let ft = fm_providers::enrich::make_fetch_text(cache.clone(), cli.today());
    let fetch = move |url: &str, params: Option<&fm_http::Params>| -> Result<String> {
        let empty = Vec::new();
        ft(url, params.unwrap_or(&empty), 0.0)
    };
    let (items, failed) = fm_sources::news::fetch_news(Some(&fetch), Some(cli.data_dir()), None, cli.now_utc());
    for (label, e) in &failed {
        let is_miss = e.chain().any(|c| c.downcast_ref::<CacheMiss>().is_some());
        let reason = if is_miss { "offline and not cached".to_string() } else { short_err(e) };
        errors.push(format!("{label} unavailable ({reason})"));
    }
    items
}

/// `_match_news(news, players, errors)`.
pub fn match_news(news: &[NewsItem], players: &[&Player]) -> Map<String, Vec<NewsItem>> {
    fm_sources::news_match::match_news_to_players(news, players)
}

/// `_explain(recs, d, errors, news, notes)`: LLM narratives when a key is configured, else the
/// hint (into `notes` when given, else `errors`). Returns the news list used.
pub fn explain(
    cli: &Cli,
    recs: &mut [Recommendation],
    d: &Loaded,
    errors: &mut Vec<String>,
    news: Option<Vec<NewsItem>>,
    notes: Option<&mut Vec<String>>,
) -> Vec<NewsItem> {
    use fm_sources::llm::SupportsComplete;

    let client = llm_client(&cli.settings);
    if !client.available() {
        match notes {
            Some(n) => n.push(LLM_HINT.to_string()),
            None => errors.push(LLM_HINT.to_string()),
        }
        return news.unwrap_or_default();
    }
    let news = match news {
        Some(n) => n,
        None => fetch_news(cli, &d.cache, errors),
    };
    let by_cid = {
        let players: Vec<&Player> = recs.iter().flat_map(|r| r.add.iter().chain(r.drop.iter())).collect();
        match_news(&news, &players)
    };
    let _ = safe(errors, "LLM narration", || {
        fm_sources::llm::openrouter::narrate(
            recs,
            Some(&d.lc),
            fm_sources::llm::context::NewsInput::ByCid(&by_cid),
            Some(&client),
            fm_sources::llm::openrouter::NARRATE_LIMIT,
        );
        Ok(())
    });
    if !recs.is_empty() && !recs.iter().any(|r| r.narrative.is_some()) {
        errors.push("LLM narration produced no usable explanations (see logs); showing heuristics only".into());
    }
    news
}

/// `_age_text(when, now)`: "5m ago" / "3h ago" / "2d ago" / "undated".
pub fn age_text(when: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(w) = when else { return "undated".into() };
    let secs = ((now - w).num_microseconds().unwrap_or(0) as f64 / 1e6).max(0.0);
    if secs < 3600.0 {
        format!("{}m ago", fixed(secs / 60.0, 0, false))
    } else if secs < 48.0 * 3600.0 {
        format!("{}h ago", fixed(secs / 3600.0, 0, false))
    } else {
        format!("{}d ago", fixed(secs / 86400.0, 0, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W11b live regression: pregame / daily snapshot / report stamps are Python's naive local
    /// `datetime.now()`; the CLI passed aware UTC, so pregame said "Pre-game 2:43 PM" at 8:43 AM CST.
    #[test]
    fn now_local_is_naive_local_wall_clock() {
        let utc = DateTime::parse_from_rfc3339("2026-11-05T14:43:00Z").unwrap().with_timezone(&Utc);
        let cli = Cli { league: "espn".into(), json: true, settings: Settings::default(), snapshot: None, clock: Clock::at(utc) };
        let loc = cli.now_local();
        assert!(!loc.is_aware());
        assert_eq!(loc.local, utc.with_timezone(&chrono::Local).naive_local());
        assert_eq!(fm_report::pregame::clock(&loc), {
            use chrono::Timelike;
            let l = utc.with_timezone(&chrono::Local);
            format!("{}:{:02} {}", if l.hour() % 12 == 0 { 12 } else { l.hour() % 12 }, l.minute(), if l.hour() < 12 { "AM" } else { "PM" })
        });
        assert!(cli.now().is_aware());
    }

    #[test]
    fn age_text_buckets() {
        let now = Utc::now();
        assert_eq!(age_text(None, now), "undated");
        assert_eq!(age_text(Some(now - chrono::Duration::minutes(5)), now), "5m ago");
        assert_eq!(age_text(Some(now - chrono::Duration::hours(3)), now), "3h ago");
        assert_eq!(age_text(Some(now - chrono::Duration::days(3)), now), "3d ago");
    }

    #[test]
    fn table_renders_title_and_cells() {
        let mut t = Table::new("Teams");
        t.cols(&[("ID", L), ("Pts", R)]);
        t.row(["a", "1.00"]);
        let s = t.render();
        assert!(s.lines().next().unwrap().trim() == "Teams");
        assert!(s.contains("1.00") && s.contains("ID"));
    }

    #[test]
    fn short_err_names_typed_errors() {
        let e: anyhow::Error = fm_core::ProviderError::Message("x  y".into()).into();
        assert_eq!(short_err(&e), "ProviderError: x y");
        assert_eq!(short_err(&anyhow::anyhow!("")), "Error");
    }
}
