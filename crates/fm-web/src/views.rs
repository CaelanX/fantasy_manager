//! Rust port of `fantasy_manager/web/views.py`: the view models of the dashboard templates.
//! Owner: work package `W9-web-views`.
//!
//! Every function returns the same dict the Python function returned, as a `serde_json::Value`
//! (insertion-ordered): that value IS the minijinja context for the page. Engine objects that
//! Python put in the dicts (Player, FantasyTeam, Recommendation, Matchup, WeekPlan, ...) are
//! embedded as their serde JSON (= pydantic `model_dump(mode="json")`), plus the `@property`
//! values the templates read (Matchup.margin / my_projected_total / their_projected_total,
//! PositionGap.gap, Period.days / label, WeekView.label, WeekPlan.lineup_gain / moves_gain,
//! PlanSince.empty, TeamOutlook.future_avg / mine). Dates are ISO strings.
//!
//! Python's `LoadResult` (defined in web/app.py) is [`LoadResult`] here, with a typed [`Memo`]
//! instead of the `memo` dict. Differences from Python worth knowing (also for W10):
//! * the clock is explicit: `res.now` / `res.today` (Python read `datetime.now()` inside the
//!   engines); `res.matchup_seed` pins the matchup Monte Carlo (None = random, like Python).
//! * `provider` arguments are `Option<&dyn LeagueProvider>`; `provider_for(loader, league)` is W10's.
//! * `strength_info(r, recs)` finds `r` in `recs` by identity (pointer), like `x is r`.
//! * `need_trades` builds its own `Outlook` (with `provider=None`) instead of reading the memoised
//!   one: an `Outlook<'a>` borrows the context and cannot sit in the memo. Python memoised the
//!   first caller's outlook (provider or not) - the result differs only if the provider changes
//!   the outlook.
//! * `fm_report::digest::credits` is not ported yet at the time of writing: `credits` is local.
//! * exception text of the "... unavailable" warnings: `Error: <msg>` unless the error is a
//!   `CoreError` (`LookupError` / `ValueError`).
//!
//! Template-side calls on the embedded objects (for W10's environment): Player methods
//! `p.gp("season")`, `p.is_goalie`, `p.positions`, `p.start_source`, `date.strftime(fmt)` on the
//! ISO date strings (`w.strftime`, `w.isoformat`), `Grid.week_index`, `Outlook.direction`; none
//! of them are precomputed here beyond what Python's view dicts already held.

use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use chrono::{Duration, NaiveDate, NaiveDateTime, TimeZone};
use fm_core::models::{FantasyTeam, LeagueContext, Player, RecKind, Recommendation, SKATER_STATS, GOALIE_STATS};
use fm_core::provider::LeagueProvider;
use fm_core::values::{DynastyValue, Horizon, PlayerValue};
use fm_core::{Cid, CoreError, Map, PyDateTime};
use fm_engine::analysis::goalie_plan::{goalie_week, GoalieWeek, GoalieWeekOpts};
use fm_engine::analysis::matchup::{current_matchup, Matchup, MatchupOpts};
use fm_engine::analysis::outlook::{build_outlook, Outlook, SIMS};
use fm_engine::analysis::playoffs::{playoff_plan, PlayoffPlan};
use fm_engine::analysis::schedule_grid::{
    default_week, playoff_weeks, season_grid, streaming_targets, week_rows, Period, PlayoffWeeks, StreamingOpts,
};
use fm_engine::recommend::claims::{plan_claims, ClaimPlan, CLAIM_LIMIT};
use fm_engine::recommend::injuries::StatusSnapshot;
use fm_engine::recommend::lineup::{current_total, daily_plan, optimal_lineup, starting_slots};
use fm_engine::recommend::market::market_disagrees;
use fm_engine::recommend::plan::{build_week_plan, PlanOpts, WeekPlan};
use fm_engine::recommend::plan_memory::latest_snapshot;
use fm_engine::recommend::trades::{
    evaluate_offer, exploit_opportunities, exploit_text, offer_summary, pick_assets, pitch_of, recommend_need_trades,
    trade_block_offered, trade_block_wants, EngineOpts, OutlookView, PickRef, TradeOpts, EXPLOIT_PER_TEAM, NEED_PER_TEAM,
};
use fm_engine::valuation::dynasty::mode_base;
use fm_engine::valuation::picks::{picks_by_holder, value_picks};
use fm_engine::valuation::regression::luck_signals;
use fm_engine::valuation::schedule::{context_window, ScheduleMemo, OFFNIGHT_THRESHOLD, WEEK_DAYS};
use fm_harness::track::{rec_record, record_text, strip, TrackBook, STRIP_KINDS, TRACK_DAYS};
use fm_pycompat::fmt::{fixed, general};
use fm_pycompat::num::{py_round_i64, py_sum, round_to};
use fm_pycompat::order::{cmp_f64, first_min_by};
use fm_pycompat::time::{date_fromisoformat, strftime_date};
use serde::Serialize;
use serde_json::{json, Value};

pub const SKATER_RATES: [&str; 8] = ["G", "A", "PTS", "SOG", "HIT", "BLK", "PPP", "PIM"];
pub const GOALIE_RATES: [&str; 5] = ["GS", "W", "GAA", "SVPCT", "SO"];
pub const RATE_LABEL: [(&str, &str); 1] = [("SVPCT", "SV%")];
pub const ALERT_STATUSES: [&str; 5] = ["dtd", "out", "ir", "ltir", "suspended"];
pub const SLOT_ORDER: [&str; 9] = ["C", "LW", "RW", "F", "D", "UTIL", "G", "BN", "IR"];
pub const SPLIT_ORDER: [&str; 8] = ["season", "last30", "last15", "last7", "prior", "prior2", "prior3", "projected"];
pub const POSITION_FILTERS: [&str; 6] = ["C", "LW", "RW", "F", "D", "G"];
pub const PROSPECT_MAX_AGE: f64 = 22.0;
pub const PROSPECT_MAX_GP: i64 = 82;
pub const NET_KEYS: [&str; 6] = ["fpg", "fpg_season", "proj_week", "games_n", "vorp", "dyn"];
pub const TRACK_TTL: f64 = 300.0;
/// free-agent list sort keys (query value -> label); "vorp" is the default
pub const FA_SORTS: [(&str, &str); 3] = [("vorp", "season VORP"), ("fpg_week", "FPG this week"), ("games", "games this week")];

pub fn move_label(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "trade" => ("Get", "Give"),
        "lineup" => ("Start", "Sit"),
        "sell_high" | "buy_low" => ("Buy", "Sell"),
        _ => ("Add", "Drop"),
    }
}

fn kind_plural(kind: &str) -> &'static str {
    match kind {
        "injury" => "injury moves",
        "lineup" => "lineup moves",
        "waiver" => "waivers",
        "trade" => "trades",
        "sell_high" => "sell-high flags",
        "buy_low" => "buy-low flags",
        "alert" => "alerts",
        _ => "moves",
    }
}

pub fn gain_unit_label(units: &str) -> Option<&'static str> {
    Some(match units {
        "week_pts" => "pts/wk",
        "matchup_pts" => "pts",
        "season_fpg" => "FPG",
        "lineup_fpg" => "lineup FPG",
        "dynasty" => "dynasty value",
        _ => return None,
    })
}

pub fn split_label(split: &str) -> String {
    match split {
        "season" => "Season",
        "last7" => "Last 7 days",
        "last15" => "Last 15 days",
        "last30" => "Last 30 days",
        "projected" => "Projected",
        "prior" => "Prior season",
        "prior2" => "2 seasons ago",
        "prior3" => "3 seasons ago",
        other => other,
    }
    .to_string()
}

fn slot_rank(slot: &str) -> usize {
    SLOT_ORDER.iter().position(|s| *s == slot).unwrap_or(99)
}

// --------------------------------------------------------------------------- LoadResult + memo

/// Typed replacement for `LoadResult.memo` (a dict keyed by view name).
#[derive(Default)]
pub struct Memo(Mutex<HashMap<String, Arc<dyn Any + Send + Sync>>>);

impl Memo {
    /// The memoised value of `key`, computing it with `f` on first use. The lock is not held while
    /// `f` runs (builders nest, like Python's).
    pub fn get_or<T: Any + Send + Sync>(&self, key: &str, f: impl FnOnce() -> T) -> Arc<T> {
        if let Some(hit) = self.0.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
            if let Ok(v) = hit.clone().downcast::<T>() {
                return v;
            }
        }
        let built: Arc<T> = Arc::new(f());
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match m.get(key).and_then(|h| h.clone().downcast::<T>().ok()) {
            Some(existing) => existing,
            None => {
                m.insert(key.to_string(), built.clone());
                built
            }
        }
    }
}

/// `web.app.LoadResult` plus the explicit clock / paths the Rust engines need.
pub struct LoadResult {
    pub ctx: LeagueContext,
    pub values: Map<Cid, PlayerValue>,
    pub dynasty: Option<Map<Cid, DynastyValue>>,
    pub recs: Vec<Recommendation>,
    pub news_by_cid: Map<String, Vec<Value>>,
    pub loaded_at: NaiveDateTime,
    pub warnings: Mutex<Vec<String>>,
    pub mode: Option<String>,
    pub mode_source: Option<String>,
    pub compute_seconds: Option<f64>,
    pub recalc_only: bool,
    /// cid -> recorded status changes, oldest first
    pub status_log: Map<String, Vec<StatusSnapshot>>,
    pub explanations: Mutex<Map<String, Value>>,
    pub memo: Memo,
    /// the request clock (Python read it inside the engines)
    pub now: PyDateTime,
    pub today: NaiveDate,
    pub sched: ScheduleMemo,
    pub data_dir: PathBuf,
    /// pins the matchup win-probability Monte Carlo (None: random like Python)
    pub matchup_seed: Option<i64>,
}

impl LoadResult {
    pub fn new(
        ctx: LeagueContext,
        values: Map<Cid, PlayerValue>,
        dynasty: Option<Map<Cid, DynastyValue>>,
        recs: Vec<Recommendation>,
        now: PyDateTime,
        today: NaiveDate,
        data_dir: PathBuf,
    ) -> LoadResult {
        LoadResult {
            ctx,
            values,
            dynasty,
            recs,
            news_by_cid: Map::new(),
            loaded_at: now.naive_local_or_utc(),
            warnings: Mutex::new(vec![]),
            mode: None,
            mode_source: None,
            compute_seconds: None,
            recalc_only: false,
            status_log: Map::new(),
            explanations: Mutex::new(Map::new()),
            memo: Memo::default(),
            now,
            today,
            sched: ScheduleMemo::new(Some(data_dir.clone())),
            data_dir,
            matchup_seed: None,
        }
    }

    pub fn warn(&self, msg: String) {
        self.warnings.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
    }

    pub fn warnings(&self) -> Vec<String> {
        self.warnings.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

trait NaiveOrUtc {
    fn naive_local_or_utc(&self) -> NaiveDateTime;
}

impl NaiveOrUtc for PyDateTime {
    fn naive_local_or_utc(&self) -> NaiveDateTime {
        match self.to_utc() {
            Some(u) => u.with_timezone(&chrono::Local).naive_local(),
            None => self.date().and_hms_opt(0, 0, 0).unwrap_or_default(),
        }
    }
}

// --------------------------------------------------------------------------- small helpers

fn ser<T: Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

fn fv(x: Option<f64>) -> Value {
    x.map_or(Value::Null, Value::from)
}

fn ds(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn get_f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

fn err_type(e: &anyhow::Error) -> &'static str {
    match e.downcast_ref::<CoreError>() {
        Some(CoreError::Lookup(_)) => "LookupError",
        Some(CoreError::Value(_)) => "ValueError",
        None => "Error",
    }
}

fn err_text(e: &anyhow::Error) -> String {
    match e.downcast_ref::<CoreError>() {
        Some(CoreError::Lookup(m)) | Some(CoreError::Value(m)) => m.clone(),
        None => e.to_string(),
    }
}

pub fn pos_str(p: &Player) -> String {
    let non_f: Vec<&str> = p.positions.iter().map(String::as_str).filter(|x| *x != "F").collect();
    if !non_f.is_empty() {
        return non_f.join("/");
    }
    if !p.positions.is_empty() {
        return p.positions.join("/");
    }
    "-".to_string()
}

/// `my_team(ctx)`: the flagged team, else the first.
pub fn my_team(ctx: &LeagueContext) -> Option<&FantasyTeam> {
    ctx.my_team().ok().or_else(|| ctx.teams.first())
}

pub fn player_age(p: &Player, as_of: NaiveDate, dynasty: Option<&Map<Cid, DynastyValue>>) -> Option<f64> {
    if let Some(a) = dynasty.and_then(|d| d.get(&p.cid)).and_then(|d| d.age) {
        return Some(a);
    }
    p.birth_date.map(|b| (as_of - b).num_days() as f64 / 365.25)
}

/// Age <= 22 with fewer than 82 career NHL games (same rule as the CLI roster table).
pub fn is_prospect(p: &Player, age: Option<f64>) -> bool {
    let Some(age) = age else { return false };
    if age >= PROSPECT_MAX_AGE + 1.0 {
        return false;
    }
    let gp = p.career_gp.unwrap_or_else(|| p.gp("season") + p.gp("prior"));
    gp < PROSPECT_MAX_GP
}

// --------------------------------------------------------------------------- schedule

/// `week_start(ctx)`: (start, days, preseason) of the projection window.
pub fn week_start(ctx: &LeagueContext) -> (NaiveDate, i64, bool) {
    if !ctx.schedule.is_empty() {
        if let Some(w) = context_window(ctx, WEEK_DAYS) {
            return (w.start, w.days, w.preseason);
        }
    }
    (ctx.as_of, 7, false)
}

#[derive(Clone, Debug)]
pub struct Game {
    pub date: NaiveDate,
    pub opp: Option<String>,
    pub off: bool,
    pub league_games: i64,
}

impl Game {
    pub fn to_value(&self) -> Value {
        json!({"date": ds(self.date), "opp": self.opp, "off": self.off, "league_games": self.league_games})
    }
}

/// NHL games for `team` in [start, start + days): date, opponent label, off-night flag.
pub fn team_games(ctx: &LeagueContext, team: Option<&str>, start: NaiveDate, days: i64) -> Vec<Game> {
    let Some(team) = team.filter(|t| !t.is_empty()) else { return vec![] };
    let end = start + Duration::days(days - 1);
    let opp = ctx.opponents.get(team);
    let mut out = Vec::new();
    for d in ctx.schedule.get(team).map(|v| v.as_slice()).unwrap_or(&[]) {
        if start <= *d && *d <= end {
            let n = ctx.games_per_day.get(d).copied().unwrap_or(0);
            out.push(Game {
                date: *d,
                opp: opp.and_then(|o| o.get(d)).cloned(),
                off: 0 < n && n < OFFNIGHT_THRESHOLD,
                league_games: n,
            });
        }
    }
    out
}

/// `"3: @TOR, BOS (off), @MTL"` style summary (opponent, or weekday when unknown).
pub fn games_text(games: &[Game]) -> String {
    if games.is_empty() {
        return "0".to_string();
    }
    let parts: Vec<String> = games
        .iter()
        .map(|g| {
            let base = match &g.opp {
                Some(o) if !o.is_empty() => o.clone(),
                _ => strftime_date(g.date, "%a"),
            };
            base + if g.off { " (off)" } else { "" }
        })
        .collect();
    format!("{}: {}", games.len(), parts.join(", "))
}

// --------------------------------------------------------------------------- player rows

/// Per-game box-score line: blended model rates, else the season (or prior) per-game line.
pub fn rate_line(p: &Player, pv: Option<&PlayerValue>) -> Value {
    let cols: &[&str] = if p.is_goalie() { &GOALIE_RATES } else { &SKATER_RATES };
    let mut rates: Map<String, f64> = pv.map(|v| v.rates.clone()).unwrap_or_default();
    if rates.is_empty() {
        for split in ["season", "prior", "prior2"] {
            if let Some(line) = p.lines.get(split) {
                if line.gp > 0 {
                    rates = line.per_game();
                    break;
                }
            }
        }
    }
    if !rates.contains_key("PTS") && (rates.contains_key("G") || rates.contains_key("A")) {
        let v = rates.get("G").copied().unwrap_or(0.0) + rates.get("A").copied().unwrap_or(0.0);
        rates.insert("PTS".into(), v);
    }
    if !rates.contains_key("PPP") && (rates.contains_key("PPG") || rates.contains_key("PPA")) {
        let v = rates.get("PPG").copied().unwrap_or(0.0) + rates.get("PPA").copied().unwrap_or(0.0);
        rates.insert("PPP".into(), v);
    }
    let mut o = serde_json::Map::new();
    for c in cols {
        o.insert((*c).to_string(), fv(rates.get(*c).copied()));
    }
    Value::Object(o)
}

/// Daily Faceoff line / power-play unit labels, e.g. `["F1", "PP1"]`.
pub fn unit_badges(p: &Player) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(l) = p.line.as_deref().filter(|s| !s.is_empty()) {
        out.push(fm_pycompat::text::py_upper(l));
    }
    if let Some(l) = p.pp_unit.as_deref().filter(|s| !s.is_empty()) {
        out.push(fm_pycompat::text::py_upper(l));
    }
    out
}

/// MoneyPuck finishing luck of a skater (None without xG data).
pub fn xg_luck(p: &Player) -> Value {
    let Some(sig) = luck_signals(p, None, None, None, None) else { return Value::Null };
    if sig.gp <= 0 {
        return Value::Null;
    }
    json!({"gmx": sig.goals_minus_ixg, "per_gp": sig.goals_minus_ixg / sig.gp as f64, "ixg": sig.ixg_per_game,
           "goals_pg": sig.goals_per_game, "gp": sig.gp, "prior": sig.from_prior, "onice_sh_delta": sig.onice_sh_pct_delta})
}

pub const MONEYPUCK_CREDIT: &str = "Expected goals: MoneyPuck.com";
pub const DFO_CREDIT: &str = "Lines, power-play units and starting goalies: Daily Faceoff (dailyfaceoff.com)";

/// Data credits owed on pages that show MoneyPuck xG or Daily Faceoff lines / starts.
pub fn credits(ctx: &LeagueContext) -> Vec<String> {
    let players = ctx.all_players();
    let mut out = Vec::new();
    if players.iter().any(|p| p.ixg_per_game.is_some()) {
        out.push(MONEYPUCK_CREDIT.to_string());
    }
    if players.iter().any(|p| p.line.as_deref().is_some_and(|s| !s.is_empty()) || p.pp_unit.as_deref().is_some_and(|s| !s.is_empty()) || p.confirmed_start.is_some()) {
        out.push(DFO_CREDIT.to_string());
    }
    out
}

/// Per-position / roster-size maximums for the footer's league line.
pub fn limits_text(ctx: &LeagueContext) -> Option<String> {
    fm_engine::recommend::base::position_limits_text(ctx)
}

fn week_start_memo(res: &LoadResult) -> (NaiveDate, i64, bool) {
    *res.memo.get_or("week_start", || week_start(&res.ctx))
}

/// `player_row(res, p, slot, owner)`.
pub fn player_row(res: &LoadResult, p: &Player, slot: Option<&str>, owner: Option<&str>) -> Value {
    let ctx = &res.ctx;
    let pv = res.values.get(&p.cid);
    let d = res.dynasty.as_ref().and_then(|d| d.get(&p.cid));
    let (start, days, _) = week_start_memo(res);
    let age = player_age(p, ctx.as_of, res.dynasty.as_ref());
    let games = team_games(ctx, p.team.as_deref(), start, days);
    let g7 = pv.and_then(|v| v.games_next7);
    let games_n = match g7 {
        Some(g) => json!(g),
        None if !games.is_empty() => json!(games.len()),
        None => Value::Null,
    };
    let games_txt = if !games.is_empty() {
        games_text(&games)
    } else if let Some(g) = g7 {
        g.to_string()
    } else {
        "-".to_string()
    };
    json!({
        "p": ser(p), "cid": p.cid, "name": p.name, "slot": slot, "owner": owner, "pos": pos_str(p),
        "team": p.team.clone().unwrap_or_else(|| "FA".into()), "age": age, "prospect": is_prospect(p, age),
        "status": p.status.as_str(), "note": p.status_note, "gp": p.gp("season"), "goalie": p.is_goalie(),
        "fpg": pv.map(|v| v.fpg), "fpg_season": pv.map(|v| v.fpg_season), "fpg_week": pv.map(|v| v.fpg_week),
        "proj_week": pv.and_then(|v| v.proj_week),
        "games": games.iter().map(Game::to_value).collect::<Vec<_>>(), "games_n": games_n, "games_text": games_txt,
        "offnight": pv.and_then(|v| v.offnight_next7), "vorp": pv.map(|v| v.vorp),
        "dyn": d.map(|d| d.value), "pct": p.pct_owned,
        "start_share": pv.and_then(|v| v.start_share), "rates": rate_line(p, pv),
        "badges": unit_badges(p), "line_change": p.line_change, "confirmed_start": p.confirmed_start,
        "xg": xg_luck(p),
    })
}

// --------------------------------------------------------------------------- moves

fn row_rates(row: &Value, k: &str) -> Option<f64> {
    row.get("rates").and_then(|r| r.get(k)).and_then(|v| v.as_f64())
}

fn total(side: &[&Value], get: &dyn Fn(&Value) -> Option<f64>) -> Option<f64> {
    let vals: Vec<Option<f64>> = side.iter().map(|x| get(x)).collect();
    if vals.iter().any(|v| v.is_some()) {
        Some(py_sum(vals.into_iter().flatten()))
    } else {
        None
    }
}

/// Side-by-side players of a move (in vs out) plus a net column when a move has both sides.
pub fn compare(res: &LoadResult, r: &Recommendation) -> Value {
    let lab = move_label(r.kind.as_str());
    let mut cols: Vec<(&str, String, Value)> = Vec::new();
    for p in &r.add {
        cols.push(("in", lab.0.to_string(), player_row(res, p, None, None)));
    }
    for p in &r.drop {
        cols.push(("out", lab.1.to_string(), player_row(res, p, None, None)));
    }
    let shown: Vec<String> = cols.iter().map(|c| c.2["cid"].as_str().unwrap_or("").to_string()).collect();
    let subj_verb = if matches!(r.kind, RecKind::Injury | RecKind::Waiver) && r.title.contains("to IR") {
        "To IR"
    } else if r.kind == RecKind::Lineup {
        "Moves"
    } else {
        "Player"
    };
    for p in &r.subjects {
        if !shown.contains(&p.cid) {
            cols.push(("subj", subj_verb.to_string(), player_row(res, p, None, None)));
        }
    }
    let goalies: Vec<bool> = cols.iter().map(|c| c.2["goalie"].as_bool().unwrap_or(false)).collect();
    let mut rate_cols: Vec<&str> = Vec::new();
    if !goalies.iter().all(|g| *g) {
        rate_cols.extend(SKATER_RATES);
    }
    if goalies.iter().any(|g| *g) {
        rate_cols.extend(GOALIE_RATES);
    }
    let mut net = Value::Null;
    if !r.add.is_empty() && !r.drop.is_empty() {
        let ins: Vec<&Value> = cols.iter().filter(|c| c.0 == "in").map(|c| &c.2).collect();
        let outs: Vec<&Value> = cols.iter().filter(|c| c.0 == "out").map(|c| &c.2).collect();
        let mut n = serde_json::Map::new();
        for k in NET_KEYS {
            let (a, b) = (total(&ins, &|x| get_f(x, k)), total(&outs, &|x| get_f(x, k)));
            n.insert(k.to_string(), fv(a.zip(b).map(|(a, b)| a - b)));
        }
        let same_type = goalies.iter().all(|g| *g == goalies[0]);
        for k in &rate_cols {
            if !same_type || *k == "GAA" || *k == "SVPCT" {
                n.insert(k.to_string(), Value::Null);
                continue;
            }
            let (a, b) = (total(&ins, &|x| row_rates(x, k)), total(&outs, &|x| row_rates(x, k)));
            n.insert(k.to_string(), fv(a.zip(b).map(|(a, b)| a - b)));
        }
        net = Value::Object(n);
    }
    let rows: Vec<&Value> = cols.iter().map(|c| &c.2).collect();
    let has_pct = rows.iter().any(|x| !x["pct"].is_null());
    let has_proj = rows.iter().any(|x| !x["proj_week"].is_null());
    let has_xg = rows.iter().any(|x| !x["xg"].is_null());
    json!({
        "cols": cols.iter().map(|c| json!({"side": c.0, "verb": c.1, "row": c.2})).collect::<Vec<_>>(),
        "rate_cols": rate_cols, "net": net, "has_dyn": res.dynasty.is_some(), "has_pct": has_pct,
        "has_proj": has_proj, "has_xg": has_xg,
    })
}

fn confidence(r: &Recommendation) -> (Option<f64>, Option<&'static str>) {
    let mut c: Option<f64> = None;
    for x in &r.reasons {
        if x.code == "CONFIDENCE" && x.value.is_some() {
            c = x.value;
        } else if x.code == "GP" && x.baseline.is_some() && c.is_none() {
            c = x.baseline;
        }
    }
    match c {
        None => (None, None),
        Some(c) => (Some(c), Some(if c < 0.5 { "low confidence" } else if c < 0.8 { "medium confidence" } else { "high confidence" })),
    }
}

pub fn horizon_label(days: Option<i64>) -> Option<String> {
    let days = days?;
    if days <= 7 {
        return Some(if days == 7 { "next 7 days".to_string() } else { format!("next {days} days") });
    }
    if days >= 120 {
        return Some("season".to_string());
    }
    Some(format!("next {days} days"))
}

/// Predicted gain of a move with explicit units and horizon.
pub fn gain_info(r: &Recommendation, res: Option<&LoadResult>) -> Value {
    let (conf, conf_label) = confidence(r);
    let mut value = r.predicted_gain;
    let mut unit: Option<String> = None;
    let mut horizon: Option<String> = None;
    let mut dyn_: Option<f64> = None;
    if value.is_some() {
        let units = r.gain_units.as_ref().map(|g| g.as_str());
        unit = Some(match units {
            Some(u) => gain_unit_label(u).map(String::from).unwrap_or_else(|| u.to_string()),
            None => String::new(),
        });
        horizon = horizon_label(r.horizon_days);
        if let Some(mg) = r.reason("MATCHUP_GAIN").filter(|m| m.value.is_some()) {
            value = mg.value;
            unit = Some("pts".into());
            horizon = Some("rest of this matchup".into());
        }
        if horizon.is_none() {
            horizon = match units {
                Some("season_fpg") | Some("lineup_fpg") => Some("rest of season".into()),
                Some("dynasty") => Some("multi-season".into()),
                _ => None,
            };
        }
    } else {
        if r.kind == RecKind::Lineup && r.reason("LINEUP_GAIN").is_some() {
            let x = r.reason("LINEUP_GAIN").expect("reason");
            value = x.value;
            let week = x.text.contains("(week)");
            let has_proj = res.is_some_and(|res| r.add.iter().any(|p| res.values.get(&p.cid).and_then(|v| v.proj_week).is_some()));
            unit = Some(if week && has_proj { "pts/wk" } else { "FPG" }.into());
            horizon = Some(if week { "this week" } else { "season" }.into());
        } else if r.kind == RecKind::Waiver && r.reason("VORP_DELTA").is_some() {
            let x = r.reason("VORP_DELTA").expect("reason");
            value = x.value;
            unit = Some("FPG".into());
            horizon = Some(if x.text.contains("(week)") { "this week" } else { "season" }.into());
        } else if r.kind == RecKind::Trade && r.reason("DELTA_ME").is_some() {
            value = r.reason("DELTA_ME").and_then(|x| x.value);
            unit = Some("lineup FPG".into());
            horizon = Some("season".into());
        } else if r.kind == RecKind::Injury && r.reason("WAIVER_ADD").is_some() {
            value = r.reason("WAIVER_ADD").and_then(|x| x.value);
            unit = Some("FPG added".into());
        }
        if let Some(x) = r.reason("DYNASTY_GAIN") {
            dyn_ = x.value;
        } else if let Some(x) = r.reason("DYNASTY_DELTA") {
            dyn_ = x.baseline;
        }
    }
    let Some(value) = value else { return Value::Null };
    json!({"value": value, "unit": unit, "horizon": horizon, "confidence": conf, "confidence_label": conf_label,
           "dynasty": dyn_, "trade": trade_info(r)})
}

/// The consensus-check badge, or null when the move carries no MARKET_CHECK.
pub fn market_info(r: &Recommendation) -> Value {
    let Some(flag) = market_disagrees(r) else { return Value::Null };
    let checks: Vec<&str> = r.reasons.iter().filter(|x| x.code == "MARKET_CHECK").map(|x| x.text.as_str()).collect();
    json!({"disagrees": flag, "label": if flag { "model disagrees with market" } else { "market agrees" },
           "title": checks.join(" | ")})
}

type TrackKey = (String, String);

fn track_cache() -> &'static Mutex<HashMap<TrackKey, (Instant, Arc<TrackBook>)>> {
    static C: OnceLock<Mutex<HashMap<TrackKey, (Instant, Arc<TrackBook>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The league's graded-outcome book, reloaded every [`TRACK_TTL`] seconds.
pub fn track_book(data_dir: &Path, league: &str, today: NaiveDate) -> Arc<TrackBook> {
    let key = (data_dir.display().to_string(), league.to_string());
    let mut c = track_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, b)) = c.get(&key) {
        if t.elapsed().as_secs_f64() <= TRACK_TTL {
            return b.clone();
        }
    }
    let book = TrackBook::load(data_dir, league, today, TRACK_DAYS)
        .unwrap_or_else(|_| TrackBook { league: league.to_string(), days: TRACK_DAYS, ..Default::default() });
    let book = Arc::new(book);
    c.insert(key, (Instant::now(), book.clone()));
    book
}

/// Drop the cached track books (tests).
pub fn clear_track_cache() {
    track_cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// `{"text", "enough"}`: how moves like `r` turned out.
pub fn track_info(r: &Recommendation, league: &str, data_dir: &Path, today: NaiveDate) -> Value {
    let tr = rec_record(&track_book(data_dir, league, today), r);
    json!({"text": record_text(&tr), "enough": tr.enough})
}

/// Per-kind summary for the overview's "Model track record" strip.
pub fn track_strip(league: &str, data_dir: &Path, today: NaiveDate) -> Value {
    Value::Array(strip(&track_book(data_dir, league, today), &STRIP_KINDS))
}

/// A trade's acceptance view; null for other kinds and for trade recs without a P_ACCEPT reason.
pub fn trade_info(r: &Recommendation) -> Value {
    let Some(p) = (r.kind == RecKind::Trade).then(|| r.reason("P_ACCEPT").and_then(|x| x.value)).flatten() else {
        return Value::Null;
    };
    let edge = r.reason("MY_EDGE");
    let edge_v = if edge.is_some() { edge.and_then(|e| e.value) } else { r.predicted_gain };
    let mv = r.reason("MARKET_VIEW");
    let week = r.reason("GAIN_WEEK").and_then(|x| x.value);
    let season = r.reason("GAIN_SEASON").and_then(|x| x.value);
    let units: Vec<String> = [
        week.map(|w| format!("{} pts/week", fixed(w, 1, true))),
        season.map(|s| format!("{} pts rest of season", fixed(s, 0, true))),
    ]
    .into_iter()
    .flatten()
    .collect();
    let units = units.join(" · ");
    let per_game = r.reason("DELTA_ME").and_then(|x| x.value);
    let gain: Vec<String> = [per_game.map(|g| format!("{} pts/game", fixed(g, 2, true))), (!units.is_empty()).then(|| units.clone())]
        .into_iter()
        .flatten()
        .collect();
    let gain_text = gain.join(" · ");
    let exploit = r.reason("EXPLOIT");
    json!({"p": p, "p_text": format!("~{}%", 5 * py_round_i64(p * 20.0)), "ev": edge_v.map(|e| e * p),
           "edge": edge_v, "market": mv.and_then(|m| m.value), "market_text": mv.map(|m| m.text.clone()),
           "sweet": r.reason("SWEET_SPOT").is_some(), "week": week, "season": season,
           "units_text": (!units.is_empty()).then_some(units), "gain_text": (!gain_text.is_empty()).then_some(gain_text),
           "exploit": exploit.map(|e| e.text.clone())})
}

// --------------------------------------------------------------------------- memoised engine sections

/// The Moves page's "Exploit" section.
pub fn exploits(res: &LoadResult, limit: usize) -> Arc<Vec<Value>> {
    res.memo.get_or(&format!("exploits:{limit}"), || {
        let opts = TradeOpts { dynasty_values: res.dynasty.as_ref(), ..TradeOpts::default() };
        match exploit_opportunities(&res.ctx, &res.values, &opts, limit, EXPLOIT_PER_TEAM) {
            Ok(recs) => recs
                .iter()
                .map(|r| json!({"rec": ser(r), "text": exploit_text(r), "trade": trade_info(r)}))
                .collect(),
            Err(e) => {
                res.warn(format!("Exploit trades unavailable: {}: {}", err_type(&e), err_text(&e)));
                vec![]
            }
        }
    })
}

fn ts_asset(a: &fm_engine::analysis::outlook::Asset<'_>) -> Value {
    json!({"player": ser(a.player), "owner": a.owner, "now": a.now, "future": a.future, "age": a.age, "why": a.why})
}

fn team_outlook_value(t: &fm_engine::analysis::outlook::TeamOutlook<'_>) -> Value {
    json!({"team": ser(t.team), "record": ser(&t.record), "win_pct": t.win_pct, "now": t.now, "future": t.future,
           "now_rank": t.now_rank, "future_rank": t.future_rank, "playoff_odds": t.playoff_odds, "label": t.label,
           "future_avg": t.future_avg(), "mine": t.mine()})
}

/// The `Outlook` dataclass as a template context.
pub fn outlook_value(o: &Outlook<'_>) -> Value {
    json!({"teams": o.teams.iter().map(team_outlook_value).collect::<Vec<_>>(),
           "me": o.me().map(team_outlook_value), "dynasty": o.dynasty, "recommended": o.recommended,
           "current": o.current, "reasons": o.reasons, "sell": o.sell.iter().map(ts_asset).collect::<Vec<_>>(),
           "buy": o.buy.iter().map(ts_asset).collect::<Vec<_>>(), "weeks_left": o.weeks_left,
           "playoff_teams": o.playoff_teams, "notes": o.notes})
}

/// `analysis.outlook.build_outlook` on the loaded result (memoised); null when it fails.
pub fn outlook_section(res: &LoadResult, provider: Option<&dyn LeagueProvider>) -> Arc<Value> {
    res.memo.get_or("outlook", || {
        let o = build_outlook(&res.ctx, &res.values, res.dynasty.as_ref(), provider, SIMS);
        outlook_value(&o)
    })
}

/// "Trades for your needs" section.
pub fn need_trades(res: &LoadResult, limit: usize) -> Arc<Vec<Value>> {
    res.memo.get_or(&format!("need_trades:{limit}"), || {
        let outlook = build_outlook(&res.ctx, &res.values, res.dynasty.as_ref(), None, SIMS);
        let opts = TradeOpts { dynasty_values: res.dynasty.as_ref(), ..TradeOpts::default() };
        let view: &dyn OutlookView = &outlook;
        match recommend_need_trades(&res.ctx, &res.values, &opts, Some(view), limit, NEED_PER_TEAM, None) {
            Ok(recs) => recs
                .iter()
                .map(|r| {
                    json!({"rec": ser(r), "need": r.reason("NEED_MATCH").map(|x| x.text.clone()),
                           "star": r.reason("STAR_CONSOLIDATION").is_some(), "pitch": pitch_of(r), "trade": trade_info(r)})
                })
                .collect(),
            Err(e) => {
                res.warn(format!("Need trades unavailable: {}: {}", err_type(&e), err_text(&e)));
                vec![]
            }
        }
    })
}

/// `recommend.claims.plan_claims` on the loaded result (memoised); None when it fails.
pub fn claim_plan(res: &LoadResult, provider: Option<&dyn LeagueProvider>) -> Arc<Option<ClaimPlan>> {
    res.memo.get_or("claims", || {
        match plan_claims(&res.ctx, &res.values, res.dynasty.as_ref(), provider, &res.now, CLAIM_LIMIT, &res.sched) {
            Ok(p) => Some(p),
            Err(e) => {
                res.warn(format!("Claims unavailable: {}: {}", match &e { CoreError::Lookup(_) => "LookupError", CoreError::Value(_) => "ValueError" }, e));
                None
            }
        }
    })
}

/// The Moves page's "Claims to place" section; null on failure.
pub fn claims_section(res: &LoadResult, provider: Option<&dyn LeagueProvider>) -> Value {
    let cp = claim_plan(res, provider);
    let Some(cp) = cp.as_ref() else { return Value::Null };
    let mut rows = Vec::new();
    for c in &cp.claims {
        let gain = match (c.gain, c.gain_units.as_deref()) {
            (None, _) => Value::Null,
            (Some(g), Some("matchup_pts")) => json!(format!("+{} pts", fixed(g, 1, false))),
            (Some(g), Some("season_fpg")) => json!(format!("+{} FPG", fixed(g, 2, false))),
            (Some(g), _) => json!(format!("+{}", fixed(g, 1, false))),
        };
        let typ = match (c.kind.as_str(), c.clears) {
            ("claim", Some(d)) => format!("claim, clears {}", strftime_date(d, "%a")),
            _ => c.kind.clone(),
        };
        rows.push(json!({
            "order": c.order, "add": ser(&c.add), "drop": ser(&c.drop), "to_ir": ser(&c.to_ir), "type": typ,
            "matchup": c.horizon == "matchup", "effective": ds(c.effective),
            "games": c.games.iter().map(|d| ds(*d)).collect::<Vec<_>>(),
            "games_text": c.games.iter().map(|d| strftime_date(*d, "%a")).collect::<Vec<_>>().join(", "),
            "games_7d": c.games_7d, "gain": gain, "headline": c.headline, "market": c.market, "bid": c.bid,
        }));
    }
    json!({"rows": rows, "rules": cp.rules, "priority": cp.priority, "moves_left": cp.moves_left, "notes": cp.notes,
           "has_bid": cp.claims.iter().any(|c| c.bid.is_some())})
}

/// Absolute strength (0-10) when the engine provides it, else the within-kind rank score; plus
/// "1 of 4 waivers". `recs` is searched by identity for `r`.
pub fn strength_info(r: &Recommendation, recs: &[Recommendation]) -> Value {
    let s = r.strength;
    let is_rank = s.is_none();
    let value = s.unwrap_or(r.score);
    let level = if value < 4.0 { "low" } else if value <= 7.0 { "mid" } else { "high" };
    let (mut rank, mut total) = (r.rank_in_kind, r.kind_total);
    if rank.is_none() || total.is_none() {
        let mut same: Vec<&Recommendation> = recs.iter().filter(|x| x.kind == r.kind).collect();
        same.sort_by(|a, b| cmp_f64(-a.score, -b.score));
        rank = same.iter().position(|x| std::ptr::eq(*x, r)).map(|i| i as i64 + 1);
        total = rank.map(|_| same.len() as i64);
    }
    let mut plural = kind_plural(r.kind.as_str()).to_string();
    if total == Some(1) && plural.ends_with('s') {
        plural.pop();
    }
    let rank_text = match (rank, total) {
        (Some(rk), Some(t)) if rk != 0 && t != 0 => Some(format!("{rk} of {t} {plural}")),
        _ => None,
    };
    json!({"value": value, "is_rank": is_rank, "level": level,
           "pct": py_round_i64(value * 10.0).clamp(0, 100), "rank_text": rank_text})
}

pub fn involves_position(r: &Recommendation, pos: &str) -> bool {
    for p in r.add.iter().chain(r.drop.iter()) {
        let has = |x: &str| p.positions.iter().any(|q| q == x);
        if pos == "F" && (has("C") || has("LW") || has("RW") || has("F")) {
            return true;
        }
        if has(pos) {
            return true;
        }
    }
    false
}

// --------------------------------------------------------------------------- lineups, standings

/// `team_lineup(res, team, horizon)`.
#[derive(Clone, Debug)]
pub struct TeamLineup {
    pub slots: Vec<String>,
    /// cid -> slot of the best lineup
    pub opt_slot: Map<String, String>,
    pub optimal: f64,
    pub current: f64,
    /// daily-lineup leagues: last day of the plan
    pub end: Option<NaiveDate>,
    pub playing_today: Option<Vec<String>>,
    pub locked: Option<Vec<String>>,
}

pub fn team_lineup(res: &LoadResult, team: &FantasyTeam, horizon: Horizon) -> Arc<TeamLineup> {
    res.memo.get_or(&format!("lineup:{}:{}", team.team_id, horizon.as_str()), || {
        let shape = &res.ctx.roster_shape;
        let slots = starting_slots(shape);
        let plan = if horizon == Horizon::Week { daily_plan(&res.ctx, team, &res.values, None, &res.now, &res.sched) } else { None };
        if let Some(plan) = plan {
            let d0 = &plan.days[0];
            return TeamLineup {
                opt_slot: d0.assign.iter().map(|(i, cid)| (cid.clone(), slots[*i].clone())).collect(),
                slots,
                optimal: plan.optimal(),
                current: plan.current(),
                end: plan.days.last().map(|d| d.day),
                playing_today: Some(d0.playing.iter().cloned().collect()),
                locked: Some(d0.locked.iter().cloned().collect()),
            };
        }
        let (assign, opt_total) = optimal_lineup(team, &res.values, shape, horizon);
        let cur_total = current_total(team, &res.values, horizon);
        TeamLineup {
            opt_slot: assign.iter().map(|(i, cid)| (cid.clone(), slots[*i].clone())).collect(),
            slots,
            optimal: opt_total,
            current: cur_total,
            end: None,
            playing_today: None,
            locked: None,
        }
    })
}

/// cid -> {"change": "start"|"sit", "gain", "other": player} from lineup recs.
pub fn lineup_changes(res: &LoadResult) -> Map<String, Value> {
    let mut out: Map<String, Value> = Map::new();
    for r in res.recs.iter().filter(|r| r.kind == RecKind::Lineup) {
        let g = r.reason("LINEUP_GAIN").and_then(|x| x.value);
        if let Some(a) = r.add.first() {
            out.entry(a.cid.clone()).or_insert_with(|| json!({"change": "start", "gain": g, "other": r.drop.first().map(ser)}));
        }
        if let Some(d) = r.drop.first() {
            out.entry(d.cid.clone()).or_insert_with(|| json!({"change": "sit", "gain": g, "other": r.add.first().map(ser)}));
        }
    }
    out
}

fn sorted_slots(team: &FantasyTeam) -> Vec<(&str, &Player)> {
    let mut v: Vec<(&str, &Player)> = team.slots.iter().filter_map(|s| s.player.as_ref().map(|p| (s.slot.as_str(), p))).collect();
    v.sort_by(|a, b| (slot_rank(a.0), &a.1.name).cmp(&(slot_rank(b.0), &b.1.name)));
    v
}

/// "This week": current vs optimal lineup with projected week points, games per player and
/// per-player start/sit changes.
pub fn week_panel(res: &LoadResult, team: Option<&FantasyTeam>) -> Value {
    let Some(team) = team.or_else(|| my_team(&res.ctx)) else { return Value::Null };
    let lu = team_lineup(res, team, Horizon::Week);
    let (start, days, preseason) = week_start_memo(res);
    let gains = if team.owner_is_me { lineup_changes(res) } else { Map::new() };
    let mut rows: Vec<Value> = Vec::new();
    let starting: HashMap<&str, bool> = team.slots.iter().filter_map(|s| s.player.as_ref().map(|p| (p.cid.as_str(), s.starting))).collect();
    for (slot, p) in sorted_slots(team) {
        let mut row = player_row(res, p, Some(slot), None);
        let now = *starting.get(p.cid.as_str()).unwrap_or(&false) && slot != "IR";
        let best = lu.opt_slot.contains_key(&p.cid);
        let plays = lu.playing_today.as_ref().is_none_or(|t| t.contains(&p.cid));
        let locked = lu.locked.as_ref().is_some_and(|l| l.contains(&p.cid));
        let mut change: Option<&str> = if best && !now { Some("start") } else if now && !best && plays { Some("sit") } else { None };
        if locked {
            change = None;
        }
        let o = row.as_object_mut().expect("row object");
        o.insert("now_start".into(), json!(now));
        o.insert("opt_start".into(), json!(best));
        o.insert("opt_slot".into(), json!(lu.opt_slot.get(&p.cid)));
        o.insert("no_game".into(), json!(!plays && slot != "IR"));
        o.insert("locked".into(), json!(locked));
        o.insert("change".into(), json!(change));
        o.insert("change_gain".into(), gains.get(&p.cid).cloned().unwrap_or(Value::Null));
        rows.push(row);
    }
    let has_proj = rows.iter().any(|r| !r["proj_week"].is_null());
    let starts: Vec<Value> = rows
        .iter()
        .filter(|r| r["goalie"].as_bool() == Some(true) && !r["confirmed_start"].is_null())
        .map(|r| json!({"p": r["p"], "starting": r["confirmed_start"].as_bool().unwrap_or(false), "source": r["p"]["start_source"]}))
        .collect();
    let sum_i = |key: &str| -> i64 {
        rows.iter().filter(|r| r["now_start"].as_bool() == Some(true)).map(|r| r[key].as_i64().unwrap_or(0)).sum()
    };
    let daily = lu.end.is_some();
    json!({
        "team": ser(team), "rows": rows, "current": lu.current, "optimal": lu.optimal, "starts": starts,
        "delta": lu.optimal - lu.current, "daily": daily,
        "units": if daily { "pts" } else if has_proj { "pts/wk" } else { "FPG" },
        "change_units": if daily { "pts today" } else if has_proj { "pts/wk" } else { "FPG" },
        "start": ds(start), "end": ds(lu.end.unwrap_or(start + Duration::days(days - 1))), "preseason": preseason,
        "games": sum_i("games_n"), "offnights": sum_i("offnight"),
        "changes": rows.iter().filter(|r| !r["change"].is_null()).count(),
    })
}

/// A trade counterparty's weakest slots and its roster rows; null for non-trades.
pub fn counterparty(res: &LoadResult, r: &Recommendation) -> Value {
    let name = r.counterparty.as_deref().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return Value::Null;
    }
    let Some(team) = res.ctx.teams.iter().find(|t| t.name.trim() == name) else { return Value::Null };
    let lineup = team_lineup(res, team, Horizon::Season);
    let mut by_type: Map<String, Vec<String>> = Map::new();
    for (cid, slot) in &lineup.opt_slot {
        by_type.entry(slot.clone()).or_default().push(cid.clone());
    }
    let shape = &res.ctx.roster_shape;
    let vorp = |c: &str| res.values.get(c).map(|v| v.vorp);
    let mut weak: Vec<(bool, f64, Value)> = Vec::new();
    let mut seen: Vec<&String> = Vec::new();
    for slot in &lineup.slots {
        if seen.contains(&slot) {
            continue;
        }
        seen.push(slot);
        if slot == "UTIL" {
            continue;
        }
        let cids = by_type.get(slot).cloned().unwrap_or_default();
        let need = shape.get(slot).copied().unwrap_or(0);
        if (cids.len() as i64) < need {
            weak.push((false, -99.0, json!({"slot": slot, "player": null, "vorp": null, "empty": need - cids.len() as i64})));
            continue;
        }
        let key = |c: &String| vorp(c).filter(|v| *v != 0.0).unwrap_or(0.0);
        let worst = first_min_by(cids.iter(), |a, b| cmp_f64(key(a), key(b))).cloned();
        let Some(worst) = worst else { continue };
        let player = team.players_iter().find(|p| p.cid == worst);
        let v = vorp(&worst);
        weak.push((player.is_some(), v.unwrap_or(-99.0), json!({"slot": slot, "player": player.map(ser), "vorp": v, "empty": 0})));
    }
    weak.sort_by(|a, b| a.0.cmp(&b.0).then(cmp_f64(a.1, b.1)));
    let rows: Vec<Value> = sorted_slots(team).into_iter().map(|(slot, p)| player_row(res, p, Some(slot), None)).collect();
    json!({"team": ser(team), "weakest": weak.into_iter().take(3).map(|w| w.2).collect::<Vec<_>>(), "rows": rows})
}

/// Every team: record, win %, best-lineup projections, VORP and dynasty totals; sorted by win %.
pub fn standings(res: &LoadResult) -> Arc<Vec<Value>> {
    res.memo.get_or("standings", || {
        struct Row {
            pct: Option<f64>,
            proj_week: f64,
            v: serde_json::Map<String, Value>,
        }
        let mut rows: Vec<Row> = Vec::new();
        for t in &res.ctx.teams {
            let rec = t.record;
            let gp = rec.map_or(0, |r| r.0 + r.1 + r.2);
            let pct = rec.filter(|_| gp != 0).map(|r| (r.0 as f64 + 0.5 * r.2 as f64) / gp as f64);
            let wk = team_lineup(res, t, Horizon::Week);
            let szn = team_lineup(res, t, Horizon::Season);
            let vorp = py_sum(t.players_iter().filter_map(|p| res.values.get(&p.cid).map(|v| v.vorp)));
            let dyn_ = res.dynasty.as_ref().map(|d| py_sum(t.players_iter().filter_map(|p| d.get(&p.cid).map(|v| v.value))));
            let mut v = serde_json::Map::new();
            v.insert("team".into(), ser(t));
            v.insert("record".into(), ser(&rec));
            v.insert("pct".into(), json!(pct));
            v.insert("proj_week".into(), json!(wk.optimal));
            v.insert("lineup_fpg".into(), json!(szn.optimal));
            v.insert("vorp".into(), json!(vorp));
            v.insert("dyn".into(), json!(dyn_));
            v.insert("injured".into(), json!(t.players_iter().filter(|p| ALERT_STATUSES.contains(&p.status.as_str())).count()));
            v.insert("players".into(), json!(t.players_iter().count()));
            v.insert("mine".into(), json!(t.owner_is_me));
            rows.push(Row { pct, proj_week: wk.optimal, v });
        }
        let mut by_proj: Vec<usize> = (0..rows.len()).collect();
        by_proj.sort_by(|a, b| cmp_f64(-rows[*a].proj_week, -rows[*b].proj_week));
        for (i, idx) in by_proj.iter().enumerate() {
            rows[*idx].v.insert("proj_rank".into(), json!(i + 1));
        }
        rows.sort_by(|a, b| {
            a.pct.is_none().cmp(&b.pct.is_none()).then(cmp_f64(-a.pct.unwrap_or(0.0), -b.pct.unwrap_or(0.0))).then(cmp_f64(-a.proj_week, -b.proj_week))
        });
        rows.into_iter().map(|r| Value::Object(r.v)).collect()
    })
}

/// `team`'s draft picks with their dynasty value, by year and round.
pub fn draft_picks(res: &LoadResult, team: Option<&FantasyTeam>) -> Vec<Value> {
    let Some(team) = team else { return vec![] };
    if res.dynasty.is_none() || res.ctx.draft_picks.is_empty() {
        return vec![];
    }
    let by_holder = res.memo.get_or("draft_picks", || picks_by_holder(value_picks(&res.ctx, &res.values, res.dynasty.as_ref())));
    let mut held = by_holder.get(&team.team_id).cloned().unwrap_or_default();
    held.sort_by_key(|a| (a.year, a.round));
    held.iter()
        .map(|a| json!({"label": a.short(Some(&team.team_id)), "slot": a.expected_slot, "overall": a.overall,
                        "value": a.value, "net": a.net, "inferred": a.inferred}))
        .collect()
}

fn fa_pos_ok(p: &Player, pos: Option<&str>) -> bool {
    let Some(pos) = pos else { return true };
    let has = |x: &str| p.positions.iter().any(|q| q == x);
    if pos == "F" { has("C") || has("LW") || has("RW") || has("F") } else { has(pos) }
}

/// The best `limit` free agents (position filter, name search, sort).
pub fn free_agents(res: &LoadResult, limit: usize, pos: Option<&str>, q: Option<&str>, sort: &str) -> Vec<Value> {
    let pos = pos.filter(|p| POSITION_FILTERS.contains(p));
    let sort = if FA_SORTS.iter().any(|(k, _)| *k == sort) { sort } else { "vorp" };
    let needle = match q.filter(|q| !q.is_empty()) {
        Some(q) => fm_core::normalize::normalize_name(Some(q)),
        None => String::new(),
    };
    let numv = |p: &Player, attr: &str| -> f64 {
        match res.values.get(&p.cid) {
            None => -99.0,
            Some(v) => if attr == "fpg_week" { v.fpg_week } else { v.vorp },
        }
    };
    let mut fas: Vec<&Player> = res
        .ctx
        .free_agents
        .iter()
        .filter(|p| fa_pos_ok(p, pos) && (needle.is_empty() || fm_core::normalize::normalize_name(Some(&p.name)).contains(&needle)))
        .collect();
    if sort == "games" {
        let mut rows: Vec<(Value, f64)> = fas.iter().map(|p| (player_row(res, p, None, Some("FA")), numv(p, "vorp"))).collect();
        let gn = |v: &Value| v["games_n"].as_f64().unwrap_or(-1.0);
        rows.sort_by(|a, b| cmp_f64(-gn(&a.0), -gn(&b.0)).then(cmp_f64(-a.1, -b.1)));
        return rows.into_iter().take(limit).map(|r| r.0).collect();
    }
    let attr = if sort == "fpg_week" { "fpg_week" } else { "vorp" };
    fas.sort_by(|a, b| cmp_f64(-numv(a, attr), -numv(b, attr)).then(cmp_f64(-numv(a, "vorp"), -numv(b, "vorp"))));
    fas.into_iter().take(limit).map(|p| player_row(res, p, None, Some("FA"))).collect()
}

/// Injured / suspended players on the other fantasy teams, most valuable first.
pub fn league_injuries(res: &LoadResult, limit: usize) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for t in res.ctx.teams.iter().filter(|t| !t.owner_is_me) {
        for s in &t.slots {
            if let Some(p) = &s.player {
                if ALERT_STATUSES.contains(&p.status.as_str()) {
                    rows.push(player_row(res, p, Some(&s.slot), Some(&t.name)));
                }
            }
        }
    }
    rows.sort_by(|a, b| cmp_f64(-a["fpg"].as_f64().unwrap_or(0.0), -b["fpg"].as_f64().unwrap_or(0.0)));
    rows.truncate(limit);
    rows
}

// --------------------------------------------------------------------------- trade evaluator

fn side_rows(res: &LoadResult, team: &FantasyTeam, chosen: &[String]) -> Vec<Value> {
    let mut slots: Vec<(&str, &Player)> = team.slots.iter().filter_map(|s| s.player.as_ref().map(|p| (s.slot.as_str(), p))).collect();
    let key = |p: &Player| res.values.get(&p.cid).map(|v| v.fpg_season).filter(|v| *v != 0.0).unwrap_or(-99.0);
    slots.sort_by(|a, b| cmp_f64(-key(a.1), -key(b.1)));
    slots
        .into_iter()
        .map(|(slot, p)| {
            let pv = res.values.get(&p.cid);
            json!({"cid": p.cid, "name": p.name, "pos": pos_str(p), "team": p.team.clone().unwrap_or_else(|| "FA".into()),
                   "slot": slot, "status": p.status.as_str(), "fpg": pv.map(|v| v.fpg_season), "vorp": pv.map(|v| v.vorp),
                   "dyn": res.dynasty.as_ref().and_then(|d| d.get(&p.cid)).map(|d| d.value), "checked": chosen.contains(&p.cid)})
        })
        .collect()
}

/// The Trade evaluator page.
#[allow(clippy::too_many_arguments)]
pub fn trade_eval_page(
    res: &LoadResult,
    team_id: Option<&str>,
    give: &[String],
    get: &[String],
    give_picks: &[String],
    get_picks: &[String],
    provider: Option<&dyn LeagueProvider>,
    evaluate: bool,
) -> Value {
    let ctx = &res.ctx;
    let me = ctx.teams.iter().find(|t| t.owner_is_me);
    let others: Vec<&FantasyTeam> = ctx.teams.iter().filter(|t| !t.owner_is_me).collect();
    let give: Vec<String> = give.iter().filter(|c| !c.is_empty()).cloned().collect();
    let mut get: Vec<String> = get.iter().filter(|c| !c.is_empty()).cloned().collect();
    let give_picks: Vec<String> = give_picks.iter().filter(|c| !c.is_empty()).cloned().collect();
    let mut get_picks: Vec<String> = get_picks.iter().filter(|c| !c.is_empty()).cloned().collect();
    let mut team = others.iter().find(|t| Some(t.team_id.as_str()) == team_id).copied();
    if team.is_none() && !get.is_empty() {
        team = others.iter().find(|t| t.players_iter().any(|p| get.contains(&p.cid))).copied();
    }
    let team = team.or_else(|| others.first().copied());
    let mut out = serde_json::Map::new();
    out.insert("teams".into(), Value::Array(others.iter().map(|t| json!({"team_id": t.team_id, "name": t.name})).collect()));
    out.insert("team".into(), team.map_or(Value::Null, ser));
    for k in ["my_rows", "their_rows", "my_picks", "their_picks"] {
        out.insert(k.into(), json!([]));
    }
    out.insert("result".into(), Value::Null);
    out.insert("rec".into(), Value::Null);
    out.insert("error".into(), Value::Null);
    let (Some(me), Some(team)) = (me, team) else {
        out.insert(
            "error".into(),
            json!(if me.is_some() { "No other teams loaded." } else { "Your team was not found in the league." }),
        );
        return Value::Object(out);
    };
    get.retain(|c| team.players_iter().any(|p| &p.cid == c));
    out.insert("my_rows".into(), Value::Array(side_rows(res, me, &give)));
    out.insert("their_rows".into(), Value::Array(side_rows(res, team, &get)));
    let picks = res.memo.get_or("trade_eval_picks", || pick_assets(ctx, &res.values, res.dynasty.as_ref()));
    let held_theirs: Vec<String> = picks.get(&team.team_id).map(|v| v.iter().map(|a| a.key()).collect()).unwrap_or_default();
    get_picks.retain(|k| held_theirs.contains(k));
    let pick_rows = |holder: &FantasyTeam, chosen: &[String]| -> Value {
        let mut v = picks.get(&holder.team_id).cloned().unwrap_or_default();
        v.sort_by_key(|a| (a.year, a.round));
        Value::Array(
            v.iter()
                .map(|a| json!({"key": a.key(), "label": a.short(Some(&holder.team_id)), "value": a.value, "net": a.net,
                                "checked": chosen.contains(&a.key())}))
                .collect(),
        )
    };
    out.insert("my_picks".into(), pick_rows(me, &give_picks));
    out.insert("their_picks".into(), pick_rows(team, &get_picks));
    if !evaluate || (give.is_empty() && get.is_empty() && give_picks.is_empty() && get_picks.is_empty()) {
        return Value::Object(out);
    }
    let blocks = provider.map(|p| p.extras().trade_blocks.clone()).unwrap_or_default();
    let wants = trade_block_wants(ctx, &blocks);
    let offered = trade_block_offered(&blocks);
    let g: Vec<&str> = give.iter().map(String::as_str).collect();
    let t: Vec<&str> = get.iter().map(String::as_str).collect();
    let gp: Vec<PickRef> = give_picks.iter().map(|k| PickRef::Key(k.clone())).collect();
    let tp: Vec<PickRef> = get_picks.iter().map(|k| PickRef::Key(k.clone())).collect();
    let opts = EngineOpts { wants: wants.as_ref(), offered: offered.as_ref(), ..EngineOpts::default() };
    match evaluate_offer(ctx, &res.values, &g, &t, Some(&team.team_id), res.dynasty.as_ref(), &gp, &tp, &opts) {
        Ok(o) => {
            out.insert("result".into(), offer_summary(&o));
            out.insert("rec".into(), ser(&o.rec));
        }
        Err(e) => match e.downcast_ref::<CoreError>() {
            Some(c) => {
                let (CoreError::Lookup(m) | CoreError::Value(m)) = c;
                out.insert("error".into(), json!(m));
            }
            None => panic!("trade evaluation failed: {e:#}"),
        },
    }
    Value::Object(out)
}

// --------------------------------------------------------------------------- player card: game log

/// Columns of the game log, in order, when the player has them.
const LOG_SKATER: [&str; 8] = ["G", "A", "PM", "SOG", "PPP", "HIT", "BLK", "PIM"];
const LOG_GOALIE: [&str; 5] = ["SA", "SV", "GA", "SO", "W"];

/// This season's games, newest first: opponent, home/away and ice time from the NHL game log, the
/// full stat line from the harness ledger's nightly NHL results when it has that day (the NHL game
/// log has no hits or blocks), and fantasy points under the league's scoring (points leagues).
/// Empty when the player has no NHL id or nothing could be read.
pub fn game_log(res: &LoadResult, p: &Player) -> Vec<Value> {
    let Some(nhl_id) = p.nhl_id() else { return Vec::new() };
    let entries = res.memo.get_or(&format!("game_log:{nhl_id}"), || -> Vec<fm_sources::nhl::NhlGameLogEntry> {
        let fetch = || -> anyhow::Result<Vec<fm_sources::nhl::NhlGameLogEntry>> {
            let cache = std::sync::Arc::new(fm_http::HttpCache::open(&res.data_dir, false)?);
            let client = fm_sources::nhl::NhlClient::new(fm_sources::nhl::cached_nhl_fetch(cache), None, res.today);
            client.game_log(nhl_id, None, 2)
        };
        fetch().unwrap_or_else(|e| {
            tracing::warn!("game log for {nhl_id}: {e:#}");
            Vec::new()
        })
    });
    let ledger_rows: std::collections::HashMap<NaiveDate, Map<String, f64>> = (|| -> anyhow::Result<_> {
        let led = fm_harness::ledger::Ledger::open(&res.data_dir)?;
        let mut out = std::collections::HashMap::new();
        for r in led.query("SELECT game_date, stats_json FROM realized_daily WHERE nhl_id=?", &[&nhl_id])? {
            let (Some(d), Some(s)) = (r.get("game_date").and_then(|v| v.as_str()), r.get("stats_json").and_then(|v| v.as_str())) else {
                continue;
            };
            let (Ok(day), Ok(stats)) = (NaiveDate::parse_from_str(d, "%Y-%m-%d"), serde_json::from_str::<Map<String, f64>>(s)) else {
                continue;
            };
            out.insert(day, stats);
        }
        Ok(out)
    })()
    .unwrap_or_default();
    let scoring = (res.ctx.scoring.kind.as_str() == "points").then(|| {
        fm_engine::scoring::PointsScoring::new(res.ctx.scoring.weights.clone(), Some(res.ctx.scoring.goalie_weights.clone()))
    });
    let cols: &[&str] = if p.is_goalie() { &LOG_GOALIE } else { &LOG_SKATER };
    entries
        .iter()
        .map(|e| {
            let full = if e.is_goalie { None } else { ledger_rows.get(&e.date) };
            let stats = full.unwrap_or(&e.stats);
            let toi = e.toi_seconds.map(|s| format!("{}:{:02}", (s as i64) / 60, (s as i64) % 60));
            let line: Map<String, Value> = cols.iter().map(|c| (c.to_string(), stats.get(*c).map_or(Value::Null, |v| json!(v)))).collect();
            json!({
                "date": e.date.format("%Y-%m-%d").to_string(),
                "opp": e.opponent, "home": e.home, "toi": toi, "decision": e.decision,
                "stats": line,
                "fpts": scoring.as_ref().map(|s| s.value(stats)),
                // without the ledger's line the points miss stats the game log lacks (hits, blocks)
                "partial": full.is_none() && !e.is_goalie,
            })
        })
        .collect()
}

/// Fantasy points per game over this season's games (oldest first) with the model's FPG as a
/// dashed reference line; None for categories leagues or fewer than 2 games.
pub fn game_log_chart(p: &Player, log: &[Value], model_fpg: Option<f64>) -> Option<Value> {
    let pts: Vec<(String, f64)> =
        log.iter().rev().filter_map(|g| Some((g["date"].as_str()?.to_string(), g["fpts"].as_f64()?))).collect();
    if pts.len() < 2 {
        return None;
    }
    let x: Vec<String> = pts.iter().map(|(d, _)| d.clone()).collect();
    let mut lines = vec![crate::charts::Line::new("fpts", "Fantasy points", pts.iter().map(|(_, v)| Some(*v)).collect(), "main", "")];
    if let Some(f) = model_fpg {
        lines.push(crate::charts::Line::new("model", "Model FPG", vec![Some(f); pts.len()], "base", crate::charts::DASHES[1]));
    }
    let fmt = |v: f64| fm_pycompat::fmt::fixed(v, 1, false);
    let opts = crate::charts::ChartOpts { y_fmt: &fmt, zero_line: true, ..Default::default() };
    let id = format!("gl-{}", p.cid);
    let svg = crate::charts::line_chart(&id, &x, &lines, &format!("{} fantasy points per game", p.name), "One point per game this season; the dashed line is the model's points per game.", &opts);
    Some(json!({"svg": svg, "legend": crate::charts::legend(&lines, None), "n": pts.len()}))
}

// --------------------------------------------------------------------------- player page

/// Stat splits as rows with per-game columns and totals; FPG from the league weights.
pub fn split_table(p: &Player, weights: Option<&Map<String, f64>>) -> (Vec<String>, Vec<Value>) {
    let order: &[&str] = if p.is_goalie() { GOALIE_STATS } else { SKATER_STATS };
    let mut present: Vec<String> = Vec::new();
    for line in p.lines.values() {
        for k in line.stats.keys() {
            if !present.contains(k) {
                present.push(k.clone());
            }
        }
    }
    let mut cols: Vec<String> = order.iter().filter(|k| **k != "GP" && present.iter().any(|x| x == *k)).map(|k| k.to_string()).collect();
    let mut rest: Vec<String> = present.iter().filter(|k| !cols.contains(k) && *k != "GP").cloned().collect();
    rest.sort();
    cols.extend(rest);
    let mut splits: Vec<&String> = SPLIT_ORDER.iter().filter_map(|s| p.lines.keys().find(|k| k.as_str() == *s)).collect();
    let mut others: Vec<&String> = p.lines.keys().filter(|k| !SPLIT_ORDER.contains(&k.as_str())).collect();
    others.sort();
    splits.extend(others);
    let mut rows = Vec::new();
    for split in splits {
        let line = &p.lines[split];
        let pg = line.per_game();
        let fpg = match weights {
            Some(w) if !w.is_empty() && line.gp > 0 => Some(py_sum(w.iter().map(|(k, w)| w * pg.get(k).copied().unwrap_or(0.0)))),
            _ => None,
        };
        let mut pgv = serde_json::Map::new();
        let mut tot = serde_json::Map::new();
        for k in &cols {
            pgv.insert(k.clone(), fv(pg.get(k).copied()));
            tot.insert(k.clone(), fv(line.stats.get(k).copied()));
        }
        rows.push(json!({"label": split_label(split), "gp": line.gp, "fpg": fpg, "pg": pgv, "tot": tot}));
    }
    (cols, rows)
}

pub fn dynasty_breakdown(d: Option<&DynastyValue>) -> Value {
    let Some(d) = d else { return Value::Null };
    let (weights, terminal) = match mode_base(&d.mode) {
        Some((w, t)) => (Some(w.to_vec()), Some(t)),
        None => (None, None),
    };
    json!({"value": d.value, "age": d.age, "age_mult": d.age_mult, "upside": d.upside, "pedigree": d.pedigree,
           "model": d.model_value, "market": d.market_value, "mode": d.mode, "horizon": d.horizon_years,
           "weights": weights, "terminal": terminal})
}

fn isoformat_local(seen: f64) -> Value {
    let secs = seen.floor() as i64;
    let micros = ((seen - seen.floor()) * 1e6).round() as u32;
    match chrono::Local.timestamp_opt(secs, micros * 1000).single() {
        Some(t) => {
            let n = t.naive_local();
            let base = n.format("%Y-%m-%dT%H:%M:%S").to_string();
            json!(if micros != 0 { format!("{base}.{micros:06}") } else { base })
        }
        None => Value::Null,
    }
}

/// Recorded status changes of a player, newest first.
pub fn status_history(res: &LoadResult, cid: &str) -> Vec<Value> {
    let mut out: Vec<Value> = res
        .status_log
        .get(cid)
        .map(|v| v.iter().map(|s| json!({"when": isoformat_local(s.seen_at), "status": s.status, "note": s.note})).collect())
        .unwrap_or_default();
    out.reverse();
    out
}

// --------------------------------------------------------------------------- schedule grid + matchup

fn period_value(p: &Period) -> Value {
    let mut v = ser(p);
    if let Value::Object(o) = &mut v {
        o.insert("days".into(), json!(p.days()));
        o.insert("label".into(), json!(p.label()));
    }
    v
}

fn with_periods(mut v: Value, periods: &[Period]) -> Value {
    if let Value::Object(o) = &mut v {
        if o.contains_key("periods") {
            o.insert("periods".into(), Value::Array(periods.iter().map(period_value).collect()));
        }
    }
    v
}

pub fn week_plan_value(p: &WeekPlan) -> Value {
    let mut v = ser(p);
    if let Value::Object(o) = &mut v {
        o.insert("lineup_gain".into(), json!(p.lineup_gain()));
        o.insert("moves_gain".into(), json!(p.moves_gain()));
        if let (Some(Value::Object(s)), Some(since)) = (o.get_mut("since"), p.since.as_ref()) {
            s.insert("empty".into(), json!(since.empty()));
        }
    }
    v
}

pub fn matchup_value(m: &Matchup) -> Value {
    let mut v = ser(m);
    if let Value::Object(o) = &mut v {
        if let Some(Value::Object(g)) = o.get_mut("gap_by_position") {
            for (k, gap) in g.iter_mut() {
                if let (Value::Object(go), Some(pg)) = (gap, m.gap_by_position.get(k)) {
                    go.insert("gap".into(), json!(pg.gap()));
                }
            }
        }
        o.insert("my_projected_total".into(), json!(m.my_projected_total()));
        o.insert("their_projected_total".into(), json!(m.their_projected_total()));
        o.insert("margin".into(), json!(m.margin()));
    }
    v
}

/// `/schedule`: the week's team x day grid, streaming targets, playoff tables, season counts.
pub fn schedule_page(res: &LoadResult, week: Option<&str>, provider: Option<&dyn LeagueProvider>, limit: usize) -> Value {
    let ctx = &res.ctx;
    let day = week.filter(|w| !w.is_empty()).and_then(|w| date_fromisoformat(fm_pycompat::text::py_strip(w).chars().take(10).collect::<String>().as_str()));
    let start = day.map_or_else(|| default_week(ctx), fm_core::calendar::monday);
    let view = res.memo.get_or(&format!("sched:week:{start}"), || week_rows(ctx, start, OFFNIGHT_THRESHOLD, 7));
    let plan = res.memo.get_or(&format!("sched:stream:{start}"), || {
        streaming_targets(ctx, &res.values, start, &StreamingOpts { limit, ..StreamingOpts::default() })
    });
    let po: Arc<PlayoffWeeks> = res.memo.get_or("sched:playoffs", || playoff_weeks(ctx, provider, None, OFFNIGHT_THRESHOLD));
    let pplan: Arc<PlayoffPlan> = res.memo.get_or("sched:playoff_plan", || playoff_plan(ctx, &res.values, provider, Some((*po).clone()), 5, OFFNIGHT_THRESHOLD));
    let grid = res.memo.get_or("sched:season", || season_grid(ctx, None, None, OFFNIGHT_THRESHOLD));
    let week_idx = if grid.weeks.is_empty() { None } else { grid.week_index(start) };
    let mut view_v = ser(&*view);
    if let Value::Object(o) = &mut view_v {
        o.insert("label".into(), json!(view.label()));
    }
    json!({
        "view": view_v, "plan": ser(&*plan), "playoffs": with_periods(ser(&*po), &po.periods),
        "pplan": with_periods(ser(&*pplan), &pplan.periods), "grid": ser(&*grid), "start": ds(start),
        "prev": ds(start - Duration::days(7)), "next": ds(start + Duration::days(7)),
        "this_week": ds(default_week(ctx)), "week_idx": week_idx, "notes": Vec::<String>::new(),
        "has_schedule": !ctx.schedule.is_empty(),
    })
}

fn matchup_of(res: &LoadResult, provider: Option<&dyn LeagueProvider>) -> Arc<Result<Matchup, String>> {
    res.memo.get_or("matchup", || {
        let opts = MatchupOpts { seed: res.matchup_seed, ..MatchupOpts::default() };
        current_matchup(&res.ctx, provider, Some(&res.values), opts, &res.now, &res.sched).map_err(|e| e.to_string())
    })
}

/// The overview's "This week's matchup" strip; null without an opponent or on error.
pub fn matchup_strip(res: &LoadResult, provider: Option<&dyn LeagueProvider>) -> Value {
    let Ok(page) = matchup_page(res, provider) else { return Value::Null };
    if page["m"]["opponent_team"].as_str().is_none_or(str::is_empty) {
        return Value::Null;
    }
    json!({"m": page["m"], "pct": page["pct"]})
}

/// `/matchup`: the current head-to-head preview.
pub fn matchup_page(res: &LoadResult, provider: Option<&dyn LeagueProvider>) -> Result<Value, String> {
    let m = matchup_of(res, provider);
    let m = m.as_ref().as_ref().map_err(|e| e.clone())?;
    let pct = m.win_probability.map(|w| py_round_i64(w * 100.0));
    let top = m.gap_by_position.values().map(|g| g.gap().abs()).fold(1.0_f64, f64::max);
    let gaps: Vec<Value> = m
        .gap_by_position
        .values()
        .map(|g| {
            json!({"group": g.group, "mine": g.mine, "theirs": g.theirs, "gap": g.gap(),
                   "width": round_to((g.gap().abs() / top * 50.0).min(50.0), 1), "side": if g.gap() >= 0.0 { "pos" } else { "neg" }})
        })
        .collect();
    Ok(json!({"m": matchup_value(m), "pct": pct, "gaps": gaps, "goalies": goalie_section(res)}))
}

/// `/plan`: the weekly action plan.
pub fn plan_page(res: &LoadResult, provider: Option<&dyn LeagueProvider>) -> Value {
    let p: Arc<WeekPlan> = res.memo.get_or("plan", || {
        let m = matchup_of(res, provider);
        let m = m.as_ref().as_ref().ok();
        let opts = PlanOpts {
            dynasty_values: res.dynasty.as_ref(),
            recs: Some(&res.recs),
            matchup: m,
            provider,
            data_dir: Some(&res.data_dir),
            ..PlanOpts::default()
        };
        build_week_plan(&res.ctx, &res.values, &opts, &res.now, &res.sched)
    });
    let pct = |x: Option<f64>| x.map(|x| py_round_i64(x * 100.0));
    let today = p.days.first().map(|d| d.day);
    let snap = latest_snapshot(&res.data_dir, &res.ctx.provider).unwrap_or_else(|| json!({}));
    let mut today_moves: Vec<&fm_engine::recommend::plan::PlanMove> = p.moves.iter().filter(|m| Some(m.day) == today).collect();
    today_moves.sort_by_key(|m| m.drop.is_some() || m.to_ir.is_some());
    let later: Vec<Value> = p.moves.iter().filter(|m| Some(m.day) != today).map(ser).collect();
    json!({"plan": week_plan_value(&p), "win_as_set": pct(p.win_as_set), "win_lineup": pct(p.win_lineup), "win_plan": pct(p.win_plan),
           "today_moves": today_moves.iter().map(|m| ser(*m)).collect::<Vec<_>>(), "later_moves": later,
           "since_ref": snap.get("created").and_then(|c| c.as_str()).unwrap_or("")})
}

// NHL team primary colours for the small team chips (presentation only).
const TEAM_COLOR: [(&str, &str); 32] = [
    ("ANA", "#F47A38"), ("BOS", "#FFB81C"), ("BUF", "#003087"), ("CGY", "#C8102E"), ("CAR", "#CE1126"), ("CHI", "#CF0A2C"),
    ("COL", "#6F263D"), ("CBJ", "#002654"), ("DAL", "#006847"), ("DET", "#CE1126"), ("EDM", "#FF4C00"), ("FLA", "#C8102E"),
    ("LAK", "#111111"), ("MIN", "#154734"), ("MTL", "#AF1E2D"), ("NSH", "#FFB81C"), ("NJD", "#CE1126"), ("NYI", "#00539B"),
    ("NYR", "#0038A8"), ("OTT", "#C52032"), ("PHI", "#F74902"), ("PIT", "#FCB514"), ("SJS", "#006D75"), ("SEA", "#99D9D9"),
    ("STL", "#002F87"), ("TBL", "#002868"), ("TOR", "#00205B"), ("UTA", "#71AFE5"), ("VAN", "#00205B"), ("VGK", "#B4975A"),
    ("WSH", "#C8102E"), ("WPG", "#041E42"),
];
const TEAM_ALIASES: [(&str, &str); 13] = [
    ("LA", "LAK"), ("NJ", "NJD"), ("SJ", "SJS"), ("TB", "TBL"), ("UTAH", "UTA"), ("UTH", "UTA"), ("WAS", "WSH"),
    ("MON", "MTL"), ("NAS", "NSH"), ("CLB", "CBJ"), ("VEG", "VGK"), ("WIN", "WPG"), ("CAL", "CGY"),
];

/// Primary colour for an NHL team abbreviation (any provider's spelling), else None.
pub fn team_color(abbr: Option<&str>) -> Option<&'static str> {
    let key = fm_pycompat::text::py_upper(fm_pycompat::text::py_strip(abbr.filter(|a| !a.is_empty())?));
    let key = TEAM_ALIASES.iter().find(|(a, _)| *a == key).map_or(key.as_str(), |(_, b)| b);
    TEAM_COLOR.iter().find(|(k, _)| *k == key).map(|(_, c)| *c)
}

fn goalie_week_of(res: &LoadResult) -> Arc<GoalieWeek> {
    res.memo.get_or("goalie_week", || {
        let opts = GoalieWeekOpts { streams: false, data_dir: Some(res.data_dir.clone()), ..GoalieWeekOpts::default() };
        goalie_week(&res.ctx, &res.values, &opts, &res.now, &res.sched)
    })
}

/// The matchup page's goalie plan; null when there are no days.
pub fn goalie_section(res: &LoadResult) -> Value {
    let gw = goalie_week_of(res);
    if gw.days.is_empty() {
        return Value::Null;
    }
    let who = |cid: &str, day: NaiveDate| -> Value {
        let row = gw.mine.iter().find(|r| r.cid == cid);
        let g = row.and_then(|r| r.games.iter().find(|x| x.day == day));
        match (row, g) {
            (Some(r), Some(g)) => json!({"name": r.name, "prob": py_round_i64(g.start_prob * 100.0), "pts": g.exp_pts,
                                          "opp": g.opponent, "why": g.start_source}),
            _ => Value::Null,
        }
    };
    let days: Vec<Value> = gw
        .plan
        .iter()
        .map(|d| {
            json!({"day": ds(d.day), "start": d.start.iter().map(|c| who(c, d.day)).collect::<Vec<_>>(),
                   "sit": d.sit.iter().map(|c| who(c, d.day)).collect::<Vec<_>>(), "open": d.open_slots})
        })
        .collect();
    let fas: Vec<Value> = gw
        .free_agents
        .iter()
        .map(|r| json!({"name": r.name, "team": r.team, "games": r.games.len(), "starts": round_to(r.starts(), 1),
                        "pts": round_to(r.exp_total(), 1)}))
        .collect();
    json!({"days": days, "total": round_to(gw.exp_total(), 1), "free_agents": fas, "notes": gw.notes, "g_slots": gw.g_slots})
}

/// `g` formatting helper kept for W10 filters (`{x:g}`).
pub fn fmt_g(x: f64) -> String {
    general(x, None, false)
}
