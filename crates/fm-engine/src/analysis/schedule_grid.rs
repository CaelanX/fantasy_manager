//! Rust port of `fantasy_manager/analysis/schedule_grid.py`: season schedule grid, weekly
//! streaming planner and fantasy-playoff schedule strength. Owner: work package `W5-recommend-base`.
//!
//! Everything is computed from the NHL schedule already loaded on the context (`ctx.schedule`,
//! `ctx.games_per_day`, `ctx.opponents`); only the league-calendar readers talk to a provider
//! (`&dyn LeagueProvider`; best effort: failures become notes and a Monday-Sunday calendar is used).
//!
//! `monday`, `per_day`, `season_bounds` and `calendar_weeks` live in `fm_core::calendar` (shared
//! with fm-providers) and are re-exported here. Deviations from Python:
//! * default arguments are explicit (`threshold`, `days`, ...); `streaming_targets` takes a
//!   [`StreamingOpts`] (with `Default`) instead of eight keyword arguments;
//! * the provider's ESPN `League` object is `provider.espn_league()` (`EspnLeagueMeta`), Fantrax's
//!   `FxpaClient.call` is `provider.fantrax_call`;
//! * a failing provider call is reported as `CalendarError { kind, message }` (the Python
//!   exception type name is only cosmetic in the note text).

use std::collections::HashMap;

use chrono::{Duration, NaiveDate};
use fm_core::models::{LeagueContext, Player};
use fm_core::provider::LeagueProvider;
use fm_core::snapshot::EspnLeagueMeta;
use fm_core::values::PlayerValue;
use fm_core::{Cid, Map};
use fm_pycompat::num::py_sum;
use fm_pycompat::order::cmp_f64;
use fm_pycompat::time::{strftime_date, strptime};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::recommend::base::{cap_text, capped_positions, moves_left, moves_note, no_moves_text};
use crate::valuation::params;

pub use fm_core::calendar::{calendar_weeks, monday, per_day, season_bounds, LIGHT_WEEK_SHARE};

pub const OFFNIGHT_THRESHOLD: i64 = 8;
pub const UNAVAILABLE: [&str; 4] = ["out", "ir", "ltir", "suspended"];
/// Playoff start period when the provider does not say (Fantrax: this league's Rules page says 25).
pub fn default_playoff_start(provider: &str) -> Option<i64> {
    (provider == "fantrax").then_some(25)
}
pub const PLAYOFF_ENV: &str = "FM_PLAYOFF_START_PERIOD";
pub const FALLBACK_PLAYOFF_WEEKS: i64 = 3;
pub const FANTRAX_PLAYOFF_LABEL: &str = "Playoffs will begin in this Scoring Period";

// --------------------------------------------------------------------------- basics

pub fn is_offnight(n_games: i64, threshold: i64) -> bool {
    0 < n_games && n_games < threshold
}

/// Monday of the current week; the opening week's Monday before the season starts.
pub fn default_week(ctx: &LeagueContext) -> NaiveDate {
    let mut day = ctx.as_of;
    if let Some(s) = ctx.season_start {
        if s > day {
            day = s;
        }
    }
    monday(day)
}

/// (games, off-night games, back-to-backs) for one team's dates in [start, end]. A back-to-back is
/// counted in the window of its second game.
pub fn window_counts(
    dates: &[NaiveDate],
    games_per_day: &Map<NaiveDate, i64>,
    start: NaiveDate,
    end: NaiveDate,
    threshold: i64,
) -> (i64, i64, i64) {
    let mut all: Vec<NaiveDate> = dates.to_vec();
    all.sort_unstable();
    all.dedup();
    let (mut games, mut off, mut b2b) = (0, 0, 0);
    for d in all.iter().filter(|d| start <= **d && **d <= end) {
        games += 1;
        if is_offnight(games_per_day.get(d).copied().unwrap_or(0), threshold) {
            off += 1;
        }
        if all.binary_search(&(*d - Duration::days(1))).is_ok() {
            b2b += 1;
        }
    }
    (games, off, b2b)
}

// --------------------------------------------------------------------------- season grid

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamWeeks {
    pub team: String,
    pub games: Vec<i64>,
    pub offnights: Vec<i64>,
    pub b2b: Vec<i64>,
    pub total_games: i64,
    pub total_offnights: i64,
    pub total_b2b: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grid {
    /// Monday of each week
    pub weeks: Vec<NaiveDate>,
    /// Sunday (clipped to `end`)
    pub week_ends: Vec<NaiveDate>,
    /// NHL games per week
    pub league_games: Vec<i64>,
    /// nights with < threshold games per week
    pub offnight_days: Vec<i64>,
    /// alphabetical
    pub teams: Vec<TeamWeeks>,
    pub threshold: i64,
}

impl Grid {
    pub fn empty(threshold: i64) -> Grid {
        Grid { weeks: vec![], week_ends: vec![], league_games: vec![], offnight_days: vec![], teams: vec![], threshold }
    }

    pub fn team(&self, abbrev: &str) -> Option<&TeamWeeks> {
        self.teams.iter().find(|t| t.team == abbrev)
    }

    pub fn week_index(&self, day: NaiveDate) -> Option<usize> {
        let m = monday(day);
        self.weeks.iter().position(|w| *w == m)
    }
}

/// Games / off-night games / back-to-backs per NHL team per Monday-Sunday week in [start, end]
/// (default: the whole loaded regular season). Empty without a schedule.
pub fn season_grid(ctx: &LeagueContext, start: Option<NaiveDate>, end: Option<NaiveDate>, threshold: i64) -> Grid {
    let Some(bounds) = season_bounds(ctx) else { return Grid::empty(threshold) };
    let start = start.unwrap_or(bounds.0);
    let end = end.unwrap_or(bounds.1);
    if end < start {
        return Grid::empty(threshold);
    }
    let gpd = per_day(ctx);
    let mut weeks: Vec<(NaiveDate, NaiveDate)> = Vec::new();
    let mut w = monday(start);
    while w <= end {
        weeks.push((w.max(start), (w + Duration::days(6)).min(end)));
        w += Duration::days(7);
    }
    let mut names: Vec<&String> = ctx.schedule.keys().collect();
    names.sort();
    let mut teams = Vec::with_capacity(names.len());
    for team in names {
        let dates = &ctx.schedule[team];
        let cells: Vec<(i64, i64, i64)> = weeks.iter().map(|(a, b)| window_counts(dates, &gpd, *a, *b, threshold)).collect();
        let g: Vec<i64> = cells.iter().map(|c| c.0).collect();
        let o: Vec<i64> = cells.iter().map(|c| c.1).collect();
        let bb: Vec<i64> = cells.iter().map(|c| c.2).collect();
        teams.push(TeamWeeks {
            team: team.clone(),
            total_games: g.iter().sum(),
            total_offnights: o.iter().sum(),
            total_b2b: bb.iter().sum(),
            games: g,
            offnights: o,
            b2b: bb,
        });
    }
    let mut league_games = Vec::with_capacity(weeks.len());
    let mut off_days = Vec::with_capacity(weeks.len());
    for (a, b) in &weeks {
        let days: Vec<NaiveDate> = (0..=(*b - *a).num_days()).map(|i| *a + Duration::days(i)).collect();
        league_games.push(days.iter().map(|d| gpd.get(d).copied().unwrap_or(0)).sum());
        off_days.push(days.iter().filter(|d| is_offnight(gpd.get(*d).copied().unwrap_or(0), threshold)).count() as i64);
    }
    Grid {
        weeks: weeks.iter().map(|(a, _)| monday(*a)).collect(),
        week_ends: weeks.iter().map(|(_, b)| *b).collect(),
        league_games,
        offnight_days: off_days,
        teams,
        threshold,
    }
}

/// The season-long team x week matrix.
pub fn team_week_counts(ctx: &LeagueContext, threshold: i64) -> Grid {
    season_grid(ctx, None, None, threshold)
}

// --------------------------------------------------------------------------- one week

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DayCell {
    pub date: NaiveDate,
    /// "BOS" at home, "@BOS" away
    pub opp: Option<String>,
    /// off-night (fewer than threshold NHL games)
    pub off: bool,
    /// second game of a back-to-back
    pub b2b: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamWeekRow {
    pub team: String,
    pub cells: Vec<Option<DayCell>>,
    pub games: i64,
    pub offnights: i64,
    pub b2b: i64,
    /// healthy free agents on this NHL team (streaming table)
    pub free_agents: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeekView {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub days: Vec<NaiveDate>,
    pub league_games: Vec<i64>,
    pub off_days: Vec<bool>,
    /// most games first, then off-night games
    pub rows: Vec<TeamWeekRow>,
    pub threshold: i64,
}

impl WeekView {
    pub fn label(&self) -> String {
        format!("{} - {}", strftime_date(self.start, "%b %d"), strftime_date(self.end, "%b %d"))
    }
}

fn team_row(ctx: &LeagueContext, team: &str, days: &[NaiveDate], gpd: &Map<NaiveDate, i64>, threshold: i64) -> TeamWeekRow {
    let dates: &[NaiveDate] = ctx.schedule.get(team).map_or(&[], |v| v.as_slice());
    let opp = ctx.opponents.get(team);
    let mut cells: Vec<Option<DayCell>> = Vec::with_capacity(days.len());
    for d in days {
        if dates.contains(d) {
            cells.push(Some(DayCell {
                date: *d,
                opp: opp.and_then(|o| o.get(d)).cloned(),
                off: is_offnight(gpd.get(d).copied().unwrap_or(0), threshold),
                b2b: dates.contains(&(*d - Duration::days(1))),
            }));
        } else {
            cells.push(None);
        }
    }
    let played = cells.iter().flatten();
    let (mut games, mut off, mut b2b) = (0, 0, 0);
    for c in played {
        games += 1;
        off += c.off as i64;
        b2b += c.b2b as i64;
    }
    TeamWeekRow { team: team.to_string(), cells, games, offnights: off, b2b, free_agents: None }
}

fn rank_rows(mut rows: Vec<TeamWeekRow>) -> Vec<TeamWeekRow> {
    rows.sort_by(|a, b| {
        b.games
            .cmp(&a.games)
            .then(b.offnights.cmp(&a.offnights))
            .then(a.b2b.cmp(&b.b2b))
            .then_with(|| a.team.cmp(&b.team))
    });
    rows
}

/// The week starting on `week_start`'s Monday as a team x day matrix (`days`: 7 in Python).
pub fn week_rows(ctx: &LeagueContext, week_start: NaiveDate, threshold: i64, days: i64) -> WeekView {
    let start = monday(week_start);
    let ds: Vec<NaiveDate> = (0..days).map(|i| start + Duration::days(i)).collect();
    let gpd = per_day(ctx);
    let rows: Vec<TeamWeekRow> = ctx.schedule.keys().map(|t| team_row(ctx, t, &ds, &gpd, threshold)).collect();
    WeekView {
        start,
        end: *ds.last().expect("days >= 1"),
        league_games: ds.iter().map(|d| gpd.get(d).copied().unwrap_or(0)).collect(),
        off_days: ds.iter().map(|d| is_offnight(gpd.get(d).copied().unwrap_or(0), threshold)).collect(),
        days: ds,
        rows: rank_rows(rows),
        threshold,
    }
}

// --------------------------------------------------------------------------- streaming

/// `schedule_grid._group`: G / D (a D with no forward position) / F.
pub fn group_of(p: &Player) -> &'static str {
    if p.is_goalie() {
        return "G";
    }
    let has = |x: &str| p.positions.iter().any(|q| q == x);
    if has("D") && !(has("C") || has("LW") || has("RW") || has("F")) {
        return "D";
    }
    "F"
}

/// `float(valuation.params.offnight_bonus())`.
pub fn offnight_bonus() -> f64 {
    params::offnight_bonus(None)
}

/// Expected fantasy points per NHL team game over the coming days: the week projection divided by
/// its games and off-night multiplier (availability and goalie start share included), else the
/// healthy FPG (times the start share for goalies).
pub fn per_game_value(pv: Option<&PlayerValue>) -> Option<f64> {
    let pv = pv?;
    if let (Some(proj), Some(games)) = (pv.proj_week, pv.games_next7) {
        if games > 0 {
            let off = pv.offnight_next7.unwrap_or(0);
            return Some(proj / (games as f64 * (1.0 + offnight_bonus() * off as f64)));
        }
    }
    Some(pv.fpg * pv.start_share.unwrap_or(1.0))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamTarget {
    pub cid: String,
    pub name: String,
    pub team: Option<String>,
    pub pos: String,
    pub group: String,
    pub status: String,
    pub games: i64,
    pub offnights: i64,
    pub b2b: i64,
    pub per_game: f64,
    pub proj: f64,
    pub pct_owned: Option<f64>,
    pub opps: Vec<String>,
    /// Set when my roster is at a per-position maximum for him (e.g. "G limit 3"): streaming him
    /// needs a same-position drop, not just any drop.
    pub needs_drop: Option<String>,
    /// Moves-left note when the league limits moves; with none left it is also appended to
    /// `needs_drop` so every view shows it.
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamingPlan {
    pub week_start: NaiveDate,
    pub week_end: NaiveDate,
    /// first day counted (today once the week has started)
    pub from_day: NaiveDate,
    pub by_slot: Map<String, Vec<StreamTarget>>,
    /// best NHL teams to stream from
    pub teams: Vec<TeamWeekRow>,
    pub bonus: f64,
    /// "1 of 2 moves left this week" (None = unlimited)
    pub moves_note: Option<String>,
}

/// Keyword arguments of `streaming_targets`.
#[derive(Clone, Debug)]
pub struct StreamingOpts<'a> {
    pub slots: &'a [&'a str],
    pub limit: usize,
    pub today: Option<NaiveDate>,
    pub team_limit: usize,
    pub end: Option<NaiveDate>,
    pub threshold: i64,
}

impl Default for StreamingOpts<'_> {
    fn default() -> Self {
        StreamingOpts { slots: &["F", "D", "G"], limit: 10, today: None, team_limit: 8, end: None, threshold: OFFNIGHT_THRESHOLD }
    }
}

fn lower_first(s: &str) -> String {
    let mut it = s.chars();
    match it.next() {
        Some(c) => c.to_lowercase().chain(it).collect(),
        None => String::new(),
    }
}

/// Free agents ranked by projected points over the rest of the week starting at `week_start`'s
/// Monday (from `today` = `ctx.as_of` once the week has begun): per-game value x games x
/// (1 + bonus x off-night games). Injured / suspended free agents are skipped. `teams` ranks the
/// NHL teams with the most games left (then off-night games). Targets whose position is at a
/// league roster maximum on my team get `needs_drop`. When the league limits moves every target
/// gets `note`, appended to `needs_drop` when there is one; with no moves left `needs_drop` says so.
pub fn streaming_targets(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    week_start: NaiveDate,
    opts: &StreamingOpts<'_>,
) -> StreamingPlan {
    let mut budget_note = moves_note(ctx);
    let exhausted = budget_note.is_some() && moves_left(ctx) == Some(0);
    if exhausted {
        budget_note = Some(no_moves_text(ctx).split(';').next().unwrap_or("").to_string());
    }
    let my_team = if ctx.position_limits.is_empty() { None } else { ctx.my_team().ok() };
    let start = monday(week_start);
    let week_end = opts.end.unwrap_or(start + Duration::days(6));
    let today = opts.today.unwrap_or(ctx.as_of);
    let from_day = if today <= week_end { start.max(today) } else { start };
    let gpd = per_day(ctx);
    let bonus = offnight_bonus();
    let days: Vec<NaiveDate> = (0..(week_end - from_day).num_days() + 1).map(|i| from_day + Duration::days(i)).collect();
    let mut fa_by_team: HashMap<&str, i64> = HashMap::new();
    let mut by_slot: Map<String, Vec<StreamTarget>> = opts.slots.iter().map(|s| (s.to_string(), Vec::new())).collect();
    for p in &ctx.free_agents {
        if UNAVAILABLE.contains(&p.status.as_str()) {
            continue;
        }
        let g = group_of(p);
        let team = p.team.as_deref().filter(|t| !t.is_empty());
        if let Some(t) = team {
            *fa_by_team.entry(t).or_insert(0) += 1;
        }
        let Some(team) = team else { continue };
        if !by_slot.contains_key(g) || !ctx.schedule.contains_key(team) {
            continue;
        }
        let Some(pg) = per_game_value(values.get(&p.cid)) else { continue };
        let sched = &ctx.schedule[team];
        let (games, off, b2b) = window_counts(sched, &gpd, from_day, week_end, opts.threshold);
        if games == 0 {
            continue;
        }
        let opp = ctx.opponents.get(team);
        let opps: Vec<String> = days
            .iter()
            .filter(|d| sched.contains(d))
            .map(|d| match opp.and_then(|o| o.get(d)).filter(|s| !s.is_empty()) {
                Some(s) => s.clone(),
                None => strftime_date(*d, "%a"),
            })
            .collect();
        let capped: Vec<&str> = my_team.map_or_else(Vec::new, |t| capped_positions(t, ctx, p));
        let mut drop_txt: Option<String> = if capped.is_empty() {
            None
        } else {
            let cts = capped.iter().map(|k| cap_text(ctx, k, None)).collect::<Vec<_>>().join(", ");
            Some(format!("{cts} reached: drop a {}", capped.join("/")))
        };
        if let Some(bn) = budget_note.as_deref().filter(|b| !b.is_empty()) {
            if drop_txt.as_deref().is_some_and(|d| !d.is_empty()) || exhausted {
                let second = if exhausted { lower_first(bn) } else { bn.to_string() };
                let parts: Vec<&str> =
                    [drop_txt.as_deref().unwrap_or(""), second.as_str()].into_iter().filter(|x| !x.is_empty()).collect();
                drop_txt = Some(parts.join("; "));
            }
        }
        let pos: String = p.positions.iter().filter(|x| *x != "F").cloned().collect::<Vec<_>>().join("/");
        by_slot[g].push(StreamTarget {
            cid: p.cid.clone(),
            name: p.name.clone(),
            team: p.team.clone(),
            pos: if pos.is_empty() { g.to_string() } else { pos },
            group: g.to_string(),
            status: p.status.as_str().to_string(),
            games,
            offnights: off,
            b2b,
            per_game: pg,
            proj: pg * games as f64 * (1.0 + bonus * off as f64),
            pct_owned: p.pct_owned,
            opps,
            needs_drop: drop_txt,
            note: budget_note.clone(),
        });
    }
    for v in by_slot.values_mut() {
        v.sort_by(|a, b| cmp_f64(b.proj, a.proj).then_with(|| a.name.cmp(&b.name)));
        v.truncate(opts.limit);
    }
    let mut rows: Vec<TeamWeekRow> = if days.is_empty() {
        vec![]
    } else {
        ctx.schedule.keys().map(|t| team_row(ctx, t, &days, &gpd, opts.threshold)).collect()
    };
    for r in rows.iter_mut() {
        r.free_agents = Some(fa_by_team.get(r.team.as_str()).copied().unwrap_or(0));
    }
    let mut teams = rank_rows(rows);
    teams.truncate(opts.team_limit);
    StreamingPlan { week_start: start, week_end, from_day, by_slot, teams, bonus, moves_note: budget_note }
}

// --------------------------------------------------------------------------- league calendar

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Period {
    pub number: i64,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub playoffs: bool,
}

impl Period {
    pub fn new(number: i64, start: NaiveDate, end: NaiveDate) -> Period {
        Period { number, start, end, playoffs: false }
    }
    pub fn contains(&self, d: NaiveDate) -> bool {
        self.start <= d && d <= self.end
    }
    pub fn days(&self) -> i64 {
        (self.end - self.start).num_days() + 1
    }
    pub fn label(&self) -> String {
        format!("{} - {}", strftime_date(self.start, "%b %d"), strftime_date(self.end, "%b %d"))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeriodCalendar {
    pub periods: Vec<Period>,
    pub playoff_start: Option<i64>,
    pub source: String,
    pub notes: Vec<String>,
}

impl Default for PeriodCalendar {
    fn default() -> Self {
        PeriodCalendar { periods: vec![], playoff_start: None, source: "calendar weeks".to_string(), notes: vec![] }
    }
}

impl PeriodCalendar {
    /// The period containing `today`, else the next one, else the last.
    pub fn current(&self, today: NaiveDate) -> Option<&Period> {
        if let Some(p) = self.periods.iter().find(|p| p.contains(today)) {
            return Some(p);
        }
        if let Some(p) = self.periods.iter().find(|p| p.start > today) {
            return Some(p);
        }
        self.periods.last()
    }

    pub fn get(&self, number: i64) -> Option<&Period> {
        self.periods.iter().find(|p| p.number == number)
    }

    pub fn playoff_periods(&self) -> Vec<Period> {
        self.periods.iter().filter(|p| p.playoffs).cloned().collect()
    }
}

fn mark_playoffs(mut periods: Vec<Period>, playoff_start: Option<i64>) -> Vec<Period> {
    for p in periods.iter_mut() {
        p.playoffs = playoff_start.is_some_and(|s| p.number >= s);
    }
    periods
}

/// A provider / calendar problem; `kind` is the Python exception class name used in notes.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct CalendarError {
    pub kind: &'static str,
    pub message: String,
}

impl CalendarError {
    fn value(msg: &str) -> Self {
        CalendarError { kind: "ValueError", message: msg.to_string() }
    }
    fn other(e: anyhow::Error) -> Self {
        CalendarError { kind: "Error", message: format!("{e}") }
    }
}

fn fx_period_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\(?\s*([A-Za-z]{3} \d{1,2}/\d{2})\s*-\s*([A-Za-z]{3} \d{1,2}/\d{2})\s*\)?").expect("static regex")
    })
}

/// "(Mar 22/27 - Mar 28/27)" -> (2027-03-22, 2027-03-28).
pub fn parse_fantrax_period(name: &str) -> Option<(NaiveDate, NaiveDate)> {
    let m = fx_period_re().captures(name)?;
    let a = strptime(&m[1], "%b %d/%y")?.date();
    let b = strptime(&m[2], "%b %d/%y")?.date();
    Some((a, b))
}

/// Python `int(x)` for the JSON values a Fantrax period list can hold.
fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().replace('_', "").parse::<i64>().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

/// Fantrax `displayedLists.scoringPeriodList` -> periods ("Full Season" is skipped).
pub fn fantrax_periods(scoring_period_list: &[Value]) -> Vec<Period> {
    let mut out: Vec<Period> = Vec::new();
    for item in scoring_period_list {
        let Some(num) = item.get("value").and_then(py_int) else { continue };
        let name = match item.get("name") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        };
        let Some(rng) = parse_fantrax_period(&name) else { continue };
        if num >= 9999 {
            continue;
        }
        out.push(Period::new(num, rng.0, rng.1));
    }
    out.sort_by_key(|p| p.number);
    out
}

fn env_playoff_start() -> Option<i64> {
    let raw = std::env::var(PLAYOFF_ENV).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        None
    } else {
        raw.parse::<i64>().ok()
    }
}

/// (playoff start period, where it came from): env override, official API, Rules page, raw settings,
/// default. Departs from the Python order, where the Rules page came before the env override.
pub fn fantrax_playoff_start(provider: &dyn LeagueProvider) -> (Option<i64>, String) {
    if let Some(env) = env_playoff_start().filter(|e| *e != 0) {
        return (Some(env), PLAYOFF_ENV.to_string());
    }
    if let Some(v) = provider.official_calendar().ok().and_then(|c| c.playoff_start).filter(|v| *v != 0) {
        return (Some(v), "Fantrax official API".to_string());
    }
    if let Some(rules) = provider.rules() {
        if let Some(v) = rules.get_int(FANTRAX_PLAYOFF_LABEL).filter(|v| *v != 0) {
            return (Some(v), "Fantrax Rules page".to_string());
        }
    }
    if let Ok(raw) = provider.raw_settings() {
        if let Some(sections) = raw.get("league_rules").and_then(|v| v.as_object()) {
            for sect in sections.values() {
                let Some(items) = sect.as_object() else { continue };
                for (k, v) in items {
                    if k.to_lowercase().contains("playoffs will begin") {
                        let s = match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        let t = s.trim_start();
                        let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
                        if let Ok(n) = digits.parse::<i64>() {
                            return (Some(n), "Fantrax Rules page".to_string());
                        }
                    }
                }
            }
        }
    }
    (default_playoff_start("fantrax"), "default for this league (period 25)".to_string())
}

fn opt_str(v: Option<i64>) -> String {
    v.map_or("None".to_string(), |x| x.to_string())
}

/// Scoring periods from the official API when the provider has it, else from `getTeamRosterInfo`
/// (view GAMES_PER_POS) via the provider's Fantrax client; playoff start per `fantrax_playoff_start`.
pub fn fantrax_calendar(_ctx: &LeagueContext, provider: &dyn LeagueProvider) -> Result<PeriodCalendar, CalendarError> {
    if let Ok(cal) = provider.official_calendar() {
        let periods: Vec<Period> = cal.periods.iter().map(|&(n, a, b)| Period::new(n, a, b)).collect();
        if !periods.is_empty() {
            let (start, src) = fantrax_playoff_start(provider);
            return Ok(PeriodCalendar {
                periods: mark_playoffs(periods, start),
                playoff_start: start,
                source: "Fantrax official scoring periods".to_string(),
                notes: vec![format!("Playoffs start period {} ({src})", opt_str(start))],
            });
        }
    }
    let mut params: Map<String, Value> = Map::new();
    params.insert("view".to_string(), Value::String("GAMES_PER_POS".to_string()));
    let out = provider
        .fantrax_call(&[("getTeamRosterInfo".to_string(), params)], false)
        .map_err(CalendarError::other)?;
    let data = out.into_iter().next().unwrap_or(Value::Null);
    let list: Vec<Value> = data
        .get("displayedLists")
        .and_then(|d| d.get("scoringPeriodList"))
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default();
    let periods = fantrax_periods(&list);
    if periods.is_empty() {
        return Err(CalendarError::value("Fantrax returned no scoring periods"));
    }
    let (start, src) = fantrax_playoff_start(provider);
    Ok(PeriodCalendar {
        periods: mark_playoffs(periods, start),
        playoff_start: start,
        source: "Fantrax scoring periods".to_string(),
        notes: vec![format!("Playoffs start period {} ({src})", opt_str(start))],
    })
}

/// Date of ESPN scoring period 1 (opening night): from today's scoring period once the season is
/// under way, else the NHL opening date.
pub fn espn_first_day(ctx: &LeagueContext, league: &EspnLeagueMeta) -> Option<NaiveDate> {
    let cur = league.current_week.unwrap_or(0);
    if cur > 1 && ctx.season_start.map_or(true, |s| ctx.as_of >= s) {
        return Some(ctx.as_of - Duration::days(cur - 1));
    }
    if let Some(s) = ctx.season_start {
        return Some(s);
    }
    season_bounds(ctx).map(|b| b.0)
}

/// ESPN matchup periods (`settings.matchup_periods`, one calendar week each) mapped to dates:
/// daily scoring periods run from opening night to `finalScoringPeriod`; weeks are Monday-Sunday
/// with the All-Star break week merged into the next one (and the opening week into week 2 if there
/// are still too many) until the count matches. Playoffs start after `settings.reg_season_count`.
/// Dates are derived, not read from ESPN.
pub fn espn_calendar(
    ctx: &LeagueContext,
    provider: &dyn LeagueProvider,
    league: Option<&EspnLeagueMeta>,
) -> Result<PeriodCalendar, CalendarError> {
    let league = league
        .or_else(|| provider.espn_league())
        .ok_or_else(|| CalendarError { kind: "LookupError", message: "no ESPN league data on the provider".to_string() })?;
    let st = &league.settings;
    let n: Option<i64> = Some(st.matchup_periods.len() as i64).filter(|n| *n != 0);
    let first = espn_first_day(ctx, league)
        .ok_or_else(|| CalendarError::value("no NHL opening date to anchor ESPN scoring periods"))?;
    let last = league.final_scoring_period.filter(|f| *f > 0).map(|f| first + Duration::days(f - 1));
    let weeks = calendar_weeks(ctx, n, Some(first), last);
    let periods: Vec<Period> = weeks.iter().enumerate().map(|(i, (a, b))| Period::new(i as i64 + 1, *a, *b)).collect();
    let start = match (st.reg_season_count, n) {
        (Some(reg), Some(n)) if reg < n => Some(reg + 1),
        _ => None,
    };
    let count = n.unwrap_or(periods.len() as i64);
    let mut notes = vec![match start {
        Some(s) => format!("ESPN: {count} matchup periods, playoffs start period {s}"),
        None => format!("ESPN: {count} matchup periods"),
    }];
    if let Some(n) = n {
        if periods.len() as i64 != n {
            notes.push(format!("ESPN period dates are approximate: {} calendar weeks for {n} matchup periods", periods.len()));
        }
    }
    Ok(PeriodCalendar {
        periods: mark_playoffs(periods, start),
        playoff_start: start,
        source: "ESPN matchup periods".to_string(),
        notes,
    })
}

pub fn fallback_calendar(ctx: &LeagueContext) -> PeriodCalendar {
    let weeks = calendar_weeks(ctx, None, None, None);
    let periods: Vec<Period> = weeks.iter().enumerate().map(|(i, (a, b))| Period::new(i as i64 + 1, *a, *b)).collect();
    let env = env_playoff_start().filter(|e| *e != 0);
    let mut start = env.or_else(|| default_playoff_start(&ctx.provider));
    let mut how = if env.is_some() { PLAYOFF_ENV.to_string() } else { "default".to_string() };
    if start.is_none() && !periods.is_empty() {
        start = Some((periods.len() as i64 - FALLBACK_PLAYOFF_WEEKS + 1).max(1));
        how = format!("assumed: last {FALLBACK_PLAYOFF_WEEKS} weeks");
    }
    PeriodCalendar {
        periods: mark_playoffs(periods, start),
        playoff_start: start,
        source: "NHL calendar weeks (Mon-Sun)".to_string(),
        notes: match start.filter(|s| *s != 0) {
            Some(s) => vec![format!("Playoffs start period {s} ({how})")],
            None => vec![],
        },
    }
}

/// The league's periods from the provider (ESPN / Fantrax), else Monday-Sunday weeks. Provider
/// failures become notes, never errors.
pub fn league_calendar(ctx: &LeagueContext, provider: Option<&dyn LeagueProvider>) -> PeriodCalendar {
    let mut notes: Vec<String> = Vec::new();
    if let Some(provider) = provider {
        let res = match ctx.provider.as_str() {
            "espn" => Some(espn_calendar(ctx, provider, None)),
            "fantrax" => Some(fantrax_calendar(ctx, provider)),
            _ => None,
        };
        match res {
            Some(Ok(cal)) => return cal,
            Some(Err(e)) => notes.push(format!("League periods unavailable ({}: {}); using calendar weeks", e.kind, e.message)),
            None => {}
        }
    }
    let mut cal = fallback_calendar(ctx);
    notes.extend(std::mem::take(&mut cal.notes));
    cal.notes = notes;
    cal
}

// --------------------------------------------------------------------------- fantasy playoffs

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayoffTeam {
    pub team: String,
    pub games: Vec<i64>,
    pub offnights: Vec<i64>,
    pub b2b: Vec<i64>,
    pub total: i64,
    pub total_offnights: i64,
    pub total_b2b: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayoffWeeks {
    pub periods: Vec<Period>,
    pub playoff_start: Option<i64>,
    pub source: String,
    pub notes: Vec<String>,
    /// most playoff games first
    pub teams: Vec<PlayoffTeam>,
    pub avg_total: f64,
}

/// Fantasy-playoff periods and each NHL team's games / off-night games / back-to-backs in them,
/// most games first.
pub fn playoff_weeks(
    ctx: &LeagueContext,
    provider: Option<&dyn LeagueProvider>,
    calendar: Option<&PeriodCalendar>,
    threshold: i64,
) -> PlayoffWeeks {
    let owned;
    let cal: &PeriodCalendar = match calendar {
        Some(c) => c,
        None => {
            owned = league_calendar(ctx, provider);
            &owned
        }
    };
    let periods = cal.playoff_periods();
    let mut out = PlayoffWeeks {
        periods: periods.clone(),
        playoff_start: cal.playoff_start,
        source: cal.source.clone(),
        notes: cal.notes.clone(),
        teams: vec![],
        avg_total: 0.0,
    };
    if periods.is_empty() || ctx.schedule.is_empty() {
        if periods.is_empty() {
            out.notes.push("No fantasy-playoff periods found".to_string());
        }
        return out;
    }
    let gpd = per_day(ctx);
    let mut teams: Vec<PlayoffTeam> = Vec::with_capacity(ctx.schedule.len());
    for (team, dates) in &ctx.schedule {
        let cells: Vec<(i64, i64, i64)> = periods.iter().map(|p| window_counts(dates, &gpd, p.start, p.end, threshold)).collect();
        let g: Vec<i64> = cells.iter().map(|c| c.0).collect();
        let o: Vec<i64> = cells.iter().map(|c| c.1).collect();
        let b: Vec<i64> = cells.iter().map(|c| c.2).collect();
        teams.push(PlayoffTeam {
            team: team.clone(),
            total: g.iter().sum(),
            total_offnights: o.iter().sum(),
            total_b2b: b.iter().sum(),
            games: g,
            offnights: o,
            b2b: b,
        });
    }
    teams.sort_by(|a, b| {
        b.total
            .cmp(&a.total)
            .then(b.total_offnights.cmp(&a.total_offnights))
            .then(a.total_b2b.cmp(&b.total_b2b))
            .then_with(|| a.team.cmp(&b.team))
    });
    out.avg_total = py_sum(teams.iter().map(|t| t.total as f64)) / teams.len() as f64;
    out.teams = teams;
    out
}
