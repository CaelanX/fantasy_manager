//! Rust port of `fantasy_manager/analysis/matchup.py`: head-to-head matchup preview (score so far,
//! projected remaining points with both teams' best lineup each remaining day, Monte Carlo win
//! probability, chase / protect / even advice).
//!
//! Owner: work package `W7-injuries-flags-alerts-matchup`.
//!
//! Deviations from Python (behaviour-preserving unless noted):
//! * The provider is `Option<&dyn LeagueProvider>`; scores come from `espn_league_get`
//!   (`mMatchupScore`) / `espn_scoreboard` / `fantrax_call(getStandings SCHEDULE)`.
//! * `now` (Python default: the current time) and the schedule memo are explicit.
//! * `win_probability` draws `PyRandom(seed).gauss` in the same order as Python (mine, then theirs,
//!   per draw); `seed = None` seeds from the system clock (Python: OS randomness).
//! * The "Live matchup scores unavailable (<Type>: <msg>)" warning carries the Rust error text
//!   (`Error: <msg>`), not the Python exception class name.

use chrono::{Duration, NaiveDate};
use fm_core::models::{FantasyTeam, LeagueContext, Player, ScoringKind};
use fm_core::provider::LeagueProvider;
use fm_core::values::{Horizon, PlayerValue};
use fm_core::{Cid, Map, PyDateTime};
use fm_pycompat::fmt::{fixed, percent};
use fm_pycompat::num::py_sum;
use fm_pycompat::order::{cmp_f64, first_min_by, sort_desc_by};
use fm_pycompat::random::PyRandom;
use fm_pycompat::time::strptime;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

use crate::analysis::schedule_grid::{
    group_of as group, league_calendar, per_game_value, streaming_targets, PeriodCalendar, StreamingOpts, UNAVAILABLE,
};
use crate::recommend::lineup::{daily_plan, opp_game, optimal_lineup, started, starting_slots, week_opp, DailyPlan};
use crate::recommend::lineup_lock::lineup_lock_for;
use crate::scoring::{from_config, PointsScoring};
use crate::valuation::valuate::fit_and_valuate;
use crate::valuation::schedule::ScheduleMemo;

pub const DRAWS: usize = 2000;
pub const SKATER_SD_K: f64 = 1.3;
pub const GOALIE_SD_K: f64 = 2.5;
pub const CHASE_BELOW: f64 = 0.40;
pub const PROTECT_ABOVE: f64 = 0.60;
pub const HOT_RATIO: f64 = 1.3;
pub const HOT_MIN_GP: i64 = 3;
pub const KEY_PLAYERS: usize = 6;
pub const NON_STARTING: [&str; 4] = ["BN", "IR", "MIN", "TAXI"];
pub const ALERT: [&str; 5] = ["dtd", "out", "ir", "ltir", "suspended"];

/// Where the matchup stands according to the provider.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ScoreInfo {
    pub period: Option<i64>,
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
    pub opponent_id: Option<String>,
    pub my_points: f64,
    pub their_points: f64,
    pub playoffs: bool,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlayerProjection {
    pub cid: String,
    pub name: String,
    pub team: Option<String>,
    pub pos: String,
    pub group: String,
    pub slot: Option<String>,
    pub status: String,
    pub note: Option<String>,
    pub games_left: i64,
    pub proj_remaining: f64,
    pub sd: f64,
    pub fpg: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KeyPlayer {
    pub cid: String,
    pub name: String,
    pub team: Option<String>,
    pub pos: String,
    /// "injured" | "hot" | "top"
    pub tag: String,
    pub status: String,
    pub note: Option<String>,
    pub games_left: i64,
    pub proj_remaining: f64,
    pub fpg: Option<f64>,
    pub recent_fpg: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PositionGap {
    pub group: String,
    pub mine: f64,
    pub theirs: f64,
}

impl PositionGap {
    pub fn gap(&self) -> f64 {
        self.mine - self.theirs
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Matchup {
    pub league: String,
    pub my_team: String,
    pub opponent_team: Option<String>,
    pub opponent_id: Option<String>,
    pub period: Option<i64>,
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
    pub from_day: Option<NaiveDate>,
    pub days_left: i64,
    pub playoffs: bool,
    pub my_points_so_far: f64,
    pub their_points_so_far: f64,
    pub my_projected_remaining: f64,
    pub their_projected_remaining: f64,
    pub my_games_left: i64,
    pub their_games_left: i64,
    pub my_sd: f64,
    pub their_sd: f64,
    pub win_probability: Option<f64>,
    /// "chase" | "protect" | "even"
    pub stance: Option<String>,
    pub key_players_theirs: Vec<KeyPlayer>,
    pub gap_by_position: Map<String, PositionGap>,
    pub advice: Vec<String>,
    pub my_players: Vec<PlayerProjection>,
    pub their_players: Vec<PlayerProjection>,
    pub source: String,
    pub lineup_basis: String,
    pub warnings: Vec<String>,
    pub draws: usize,
}

impl Matchup {
    pub fn my_projected_total(&self) -> f64 {
        self.my_points_so_far + self.my_projected_remaining
    }
    pub fn their_projected_total(&self) -> f64 {
        self.their_points_so_far + self.their_projected_remaining
    }
    pub fn margin(&self) -> f64 {
        self.my_projected_total() - self.their_projected_total()
    }

    /// `model_dump(mode="json")` + the totals, margin and each position gap.
    pub fn to_json(&self) -> Value {
        let mut d = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Value::Object(o) = &mut d {
            o.insert("my_projected_total".into(), json!(self.my_projected_total()));
            o.insert("their_projected_total".into(), json!(self.their_projected_total()));
            o.insert("margin".into(), json!(self.margin()));
            if let Some(Value::Object(g)) = o.get_mut("gap_by_position") {
                for (k, x) in &self.gap_by_position {
                    if let Some(Value::Object(e)) = g.get_mut(k) {
                        e.insert("gap".into(), json!(x.gap()));
                    }
                }
            }
        }
        d
    }
}

// --------------------------------------------------------------------------- projections

/// A player's projected points over `games_left` games: the week projection apportioned by games
/// (`proj_week * games_left / games_next7`), else per-game value x games.
pub fn apportion(pv: Option<&PlayerValue>, games_left: i64) -> f64 {
    let Some(pv) = pv.filter(|_| games_left > 0) else { return 0.0 };
    if let (Some(proj), Some(g7)) = (pv.proj_week, pv.games_next7) {
        if g7 > 0 {
            return proj * games_left as f64 / g7 as f64;
        }
    }
    per_game_value(Some(pv)).unwrap_or(0.0) * games_left as f64
}

/// Per-game SD of fantasy points: 1.3 x sqrt(FPG) skaters, 2.5 x sqrt(FPG) goalies.
pub fn game_sd(per_game: f64, goalie: bool) -> f64 {
    (if goalie { GOALIE_SD_K } else { SKATER_SD_K }) * per_game.max(0.0).sqrt()
}

fn pos(p: &Player) -> String {
    let a: Vec<&str> = p.positions.iter().map(|s| s.as_str()).filter(|x| *x != "F").collect();
    if !a.is_empty() {
        return a.join("/");
    }
    if !p.positions.is_empty() {
        return p.positions.join("/");
    }
    "-".into()
}

/// (starters with their slot, basis): the current starters when lineups are locked for the
/// period, else the optimal week lineup (falls back to the current starters).
pub fn lineup_players<'t>(
    team: &'t FantasyTeam,
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    locked: bool,
) -> (Vec<(&'t Player, Option<String>)>, &'static str) {
    let current: Vec<(&Player, Option<String>)> = team
        .slots
        .iter()
        .filter(|s| s.starting && !NON_STARTING.contains(&s.slot.as_str()))
        .filter_map(|s| s.player.as_ref().map(|p| (p, Some(s.slot.clone()))))
        .collect();
    if locked {
        return (current, "current lineup (locked for the period)");
    }
    let slots = starting_slots(&ctx.roster_shape);
    let (assign, _) = optimal_lineup(team, values, &ctx.roster_shape, Horizon::Week);
    let chosen: Vec<(&Player, Option<String>)> = assign
        .iter()
        .filter_map(|(i, cid)| team.players_iter().find(|p| &p.cid == cid).map(|p| (p, slots.get(*i).cloned())))
        .collect();
    if !chosen.is_empty() {
        return (chosen, "optimal week lineup");
    }
    (current, "current lineup")
}

fn projection(p: &Player, slot: Option<String>, games: i64, proj: f64, sd: f64, pv: Option<&PlayerValue>) -> PlayerProjection {
    PlayerProjection {
        cid: p.cid.clone(),
        name: p.name.clone(),
        team: p.team.clone(),
        pos: pos(p),
        group: group(p).to_string(),
        slot,
        status: p.status.as_str().to_string(),
        note: p.status_note.clone(),
        games_left: games,
        proj_remaining: proj,
        sd,
        fpg: pv.map(|v| v.fpg),
    }
}

fn sort_proj(out: &mut [PlayerProjection]) {
    sort_desc_by(out, |a, b| cmp_f64(a.proj_remaining, b.proj_remaining));
}

/// Each starter's games, projected points and SD over [start, end] (see the Python docstring).
#[allow(clippy::too_many_arguments)]
pub fn project_team(
    ctx: &LeagueContext,
    team: &FantasyTeam,
    values: &Map<Cid, PlayerValue>,
    start: NaiveDate,
    end: NaiveDate,
    locked: bool,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> (Vec<PlayerProjection>, String) {
    if !locked && start <= end {
        let days: Vec<NaiveDate> = (0..=(end - start).num_days()).map(|k| start + Duration::days(k)).collect();
        if let Some(plan) = daily_plan(ctx, team, values, Some(&days), now, memo) {
            return (project_daily(team, values, &plan), "best lineup each day".into());
        }
    }
    let (starters, basis) = lineup_players(team, ctx, values, locked);
    let mut out = Vec::new();
    for (p, slot) in starters {
        let mut days: Vec<NaiveDate> = Vec::new();
        if start <= end {
            if let Some(sched) = p.team.as_ref().and_then(|t| ctx.schedule.get(t)) {
                let mut ds = sched.clone();
                ds.sort();
                ds.dedup();
                days = ds
                    .into_iter()
                    .filter(|d| start <= *d && *d <= end && !(*d == ctx.as_of && started(ctx, p, *d, now)))
                    .collect();
            }
        }
        let games = days.len() as i64;
        let pv = values.get(&p.cid);
        let mut proj = apportion(pv, games);
        if games > 0 && !p.is_goalie() {
            if let Some(pv) = pv {
                let opp = py_sum(days.iter().map(|d| opp_game(ctx, memo, p, *d).mult)) / games as f64;
                proj *= opp / week_opp(pv);
            }
        }
        let pg = if games > 0 { proj / games as f64 } else { 0.0 };
        let sd = if games > 0 { (games as f64).sqrt() * game_sd(pg, p.is_goalie()) } else { 0.0 };
        out.push(projection(p, slot, games, proj, sd, pv));
    }
    sort_proj(&mut out);
    (out, basis.to_string())
}

/// Projections for daily lineups: each player counts only the days the plan starts him, less
/// today's games that have already started.
fn project_daily(team: &FantasyTeam, values: &Map<Cid, PlayerValue>, plan: &DailyPlan) -> Vec<PlayerProjection> {
    let mut out = Vec::new();
    for p in team.players_iter() {
        let games = plan.remaining_starts(&p.cid);
        if games == 0 {
            continue;
        }
        let proj = plan.points(&p.cid);
        let pv = values.get(&p.cid);
        let slot = team.slots.iter().find(|s| s.player.as_ref().is_some_and(|x| x.cid == p.cid)).map(|s| s.slot.clone());
        let sd = (games as f64).sqrt() * game_sd(proj / games as f64, p.is_goalie());
        out.push(projection(p, slot, games, proj, sd, pv));
    }
    sort_proj(&mut out);
    out
}

pub fn team_sd(players: &[PlayerProjection]) -> f64 {
    py_sum(players.iter().map(|p| p.sd.powf(2.0))).sqrt()
}

/// Monte Carlo P(my final > their final); ties count half. `mine` / `theirs` are (mean, sd) of
/// each starter's remaining points.
pub fn win_probability(
    my_so_far: f64,
    their_so_far: f64,
    mine: &[(f64, f64)],
    theirs: &[(f64, f64)],
    draws: usize,
    seed: Option<i64>,
) -> f64 {
    if !mine.iter().chain(theirs).any(|(_, sd)| *sd > 0.0) {
        let a = my_so_far + py_sum(mine.iter().map(|(m, _)| *m));
        let b = their_so_far + py_sum(theirs.iter().map(|(m, _)| *m));
        return if a > b + 1e-9 {
            1.0
        } else if b > a + 1e-9 {
            0.0
        } else {
            0.5
        };
    }
    let seed = seed.unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
    });
    let mut rng = PyRandom::new(seed);
    let n = draws.max(1);
    let mut wins = 0.0;
    let mut buf: Vec<f64> = Vec::with_capacity(mine.len().max(theirs.len()));
    for _ in 0..n {
        buf.clear();
        buf.extend(mine.iter().map(|&(m, sd)| if sd > 0.0 { rng.gauss(m, sd) } else { m }));
        let a = my_so_far + py_sum(buf.iter().copied());
        buf.clear();
        buf.extend(theirs.iter().map(|&(m, sd)| if sd > 0.0 { rng.gauss(m, sd) } else { m }));
        let b = their_so_far + py_sum(buf.iter().copied());
        wins += if a > b {
            1.0
        } else if a == b {
            0.5
        } else {
            0.0
        };
    }
    wins / n as f64
}

// --------------------------------------------------------------------------- key players

fn recent_fpg(ctx: &LeagueContext, p: &Player) -> Option<f64> {
    if ctx.scoring.kind != ScoringKind::Points {
        return None;
    }
    let gw = if ctx.scoring.goalie_weights.is_empty() { None } else { Some(ctx.scoring.goalie_weights.clone()) };
    let sc = PointsScoring::new(ctx.scoring.weights.clone(), gw);
    for split in ["last15", "last7", "last30"] {
        if let Some(line) = p.lines.get(split).filter(|l| l.gp >= HOT_MIN_GP) {
            return Some(sc.value(&line.per_game()));
        }
    }
    None
}

/// Their injured / suspended players (most valuable first), hot starters (recent FPG >= 1.3x the
/// model's) and top projected starters.
pub fn key_players(
    ctx: &LeagueContext,
    team: &FantasyTeam,
    projections: &[PlayerProjection],
    values: &Map<Cid, PlayerValue>,
    limit: usize,
) -> Vec<KeyPlayer> {
    let mut out: Map<String, KeyPlayer> = Map::new();
    let mut add = |p: &Player, tag: &str, recent: Option<f64>| {
        if out.contains_key(&p.cid) {
            return;
        }
        let x = projections.iter().find(|x| x.cid == p.cid);
        out.insert(
            p.cid.clone(),
            KeyPlayer {
                cid: p.cid.clone(),
                name: p.name.clone(),
                team: p.team.clone(),
                pos: pos(p),
                tag: tag.to_string(),
                status: p.status.as_str().to_string(),
                note: p.status_note.clone(),
                games_left: x.map(|x| x.games_left).unwrap_or(0),
                proj_remaining: x.map(|x| x.proj_remaining).unwrap_or(0.0),
                fpg: values.get(&p.cid).map(|v| v.fpg),
                recent_fpg: recent,
            },
        );
    };
    let fpg = |cid: &str| values.get(cid).map(|v| v.fpg);
    let mut hurt: Vec<&Player> = team.players_iter().filter(|p| p.status.is_in(&ALERT)).collect();
    hurt.sort_by(|a, b| cmp_f64(-fpg(&a.cid).unwrap_or(0.0), -fpg(&b.cid).unwrap_or(0.0)));
    for p in hurt {
        add(p, "injured", None);
    }
    let by_cid = |cid: &str| team.players_iter().find(|p| p.cid == cid);
    for x in projections {
        let Some(p) = by_cid(&x.cid) else { continue };
        let Some(base) = fpg(&x.cid).filter(|b| *b != 0.0 && *b > 0.0) else { continue };
        if let Some(recent) = recent_fpg(ctx, p) {
            if recent >= HOT_RATIO * base {
                add(p, "hot", Some(recent));
            }
        }
    }
    for x in projections.iter().take(3) {
        if let Some(p) = by_cid(&x.cid) {
            if x.proj_remaining > 0.0 {
                add(p, "top", None);
            }
        }
    }
    out.into_values().take(limit).collect()
}

// --------------------------------------------------------------------------- provider scores

/// `_num`: a number (bools excluded), or a string parsed after dropping thousands separators.
fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Bool(_) | Value::Null => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.replace(',', "").trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn id_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

fn truthy_id(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::Bool(b) => *b,
        _ => true,
    }
}

/// Current matchup period, opponent and live points from ESPN.
pub fn espn_scores(ctx: &LeagueContext, provider: &dyn LeagueProvider, calendar: &PeriodCalendar) -> anyhow::Result<Option<ScoreInfo>> {
    let mp = provider.espn_league().and_then(|l| l.current_matchup_period).filter(|m| *m != 0);
    let my_id: i64 = ctx.my_team()?.team_id.trim().parse()?;
    let mut found: Option<ScoreInfo> = None;
    if let Ok(data) = provider.espn_league_get(&[("view".to_string(), "mMatchupScore".to_string())]) {
        let empty = Value::Object(Default::default());
        if let Some(Value::Array(sched)) = data.get("schedule") {
            for m in sched {
                if m.get("matchupPeriodId").and_then(|v| v.as_i64()) != mp {
                    continue;
                }
                let home = m.get("home").filter(|v| !v.is_null()).unwrap_or(&empty);
                let away = m.get("away").filter(|v| !v.is_null()).unwrap_or(&empty);
                let tid = |side: &Value| side.get("teamId").and_then(|v| v.as_i64());
                if tid(home) != Some(my_id) && tid(away) != Some(my_id) {
                    continue;
                }
                let (me, them) = if tid(home) == Some(my_id) { (home, away) } else { (away, home) };
                let pts = |side: &Value| num(side.get("totalPointsLive")).or_else(|| num(side.get("totalPoints"))).unwrap_or(0.0);
                let them_nonempty = them.as_object().is_some_and(|o| !o.is_empty());
                let tier = match m.get("playoffTierType") {
                    Some(Value::String(s)) if !s.is_empty() => s.clone(),
                    Some(Value::Null) | None => "NONE".to_string(),
                    Some(v) => v.to_string(),
                };
                found = Some(ScoreInfo {
                    period: mp,
                    opponent_id: them.get("teamId").and_then(id_text),
                    my_points: pts(me),
                    their_points: if them_nonempty { pts(them) } else { 0.0 },
                    playoffs: tier != "NONE",
                    source: "ESPN scoreboard".into(),
                    ..ScoreInfo::default()
                });
                break;
            }
        }
    }
    if found.is_none() {
        for m in provider.espn_scoreboard(mp.unwrap_or(0))? {
            let team_id = |v: Option<&Value>| -> Value {
                match v {
                    Some(Value::Object(o)) => o.get("team_id").cloned().unwrap_or(Value::Null),
                    Some(v) => v.clone(),
                    None => Value::Null,
                }
            };
            let (hid, aid) = (team_id(m.get("home_team")), team_id(m.get("away_team")));
            if hid.as_i64() != Some(my_id) && aid.as_i64() != Some(my_id) {
                continue;
            }
            let mine_home = hid.as_i64() == Some(my_id);
            let other = if mine_home { &aid } else { &hid };
            let score = |side: &str| num(m.get(&format!("{side}_score")).or_else(|| m.get(&format!("{side}_final_score")))).unwrap_or(0.0);
            found = Some(ScoreInfo {
                period: mp,
                opponent_id: if truthy_id(other) { id_text(other) } else { None },
                my_points: score(if mine_home { "home" } else { "away" }),
                their_points: score(if mine_home { "away" } else { "home" }),
                source: "ESPN scoreboard".into(),
                ..ScoreInfo::default()
            });
            break;
        }
    }
    let Some(mut found) = found else { return Ok(None) };
    if let Some(per) = mp.and_then(|m| calendar.get(m)) {
        found.start = Some(per.start);
        found.end = Some(per.end);
        found.playoffs = found.playoffs || per.playoffs;
    }
    Ok(Some(found))
}

/// (away team, away points, home team, home points) cell indexes from the table header.
fn schedule_columns(header: Option<&Value>) -> (usize, usize, usize, usize) {
    let keys: Vec<String> = header
        .and_then(|h| h.get("cells"))
        .and_then(|c| c.as_array())
        .map(|cells| {
            cells
                .iter()
                .map(|c| match c.get("key") {
                    Some(Value::String(s)) => s.to_lowercase(),
                    Some(Value::Null) | None => String::new(),
                    Some(v) => v.to_string().to_lowercase(),
                })
                .collect()
        })
        .unwrap_or_default();
    let (Some(a), Some(h)) = (keys.iter().position(|k| k == "away"), keys.iter().position(|k| k == "home")) else {
        return (0, 1, 2, 3);
    };
    let pts = |team_i: usize, stop: usize| -> usize {
        let lo = (team_i + 1).min(keys.len());
        let hi = stop.max(lo).min(keys.len());
        let side = &keys[lo..hi];
        for k in ["total", "fpts"] {
            if let Some(i) = side.iter().position(|x| x == k) {
                return team_i + 1 + i;
            }
        }
        team_i + 1
    };
    (a, pts(a, if h > a { h } else { keys.len() }), h, pts(h, if a > h { a } else { keys.len() }))
}

/// One scoring-period table of the Fantrax SCHEDULE view.
#[derive(Clone, Debug, PartialEq)]
pub struct ScheduleTable {
    pub number: Option<i64>,
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
    pub playoffs: bool,
    /// (away id, away pts, home id, home pts)
    pub games: Vec<(Value, f64, Value, f64)>,
}

fn strip_chars<'a>(s: &'a str, chars: &[char]) -> &'a str {
    s.trim_matches(|c| chars.contains(&c))
}

/// getStandings view SCHEDULE -> one table per scoring period.
pub fn parse_fantrax_schedule(data: &Value) -> Vec<ScheduleTable> {
    let cap_re = Regex::new(r"(\d+)\s*$").expect("static regex");
    let mut out = Vec::new();
    let Some(tables) = data.get("tableList").and_then(|t| t.as_array()) else { return out };
    let text = |v: Option<&Value>| match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    };
    for t in tables {
        let cap = text(t.get("caption"));
        let number = cap_re.captures(&cap).and_then(|m| m[1].parse::<i64>().ok());
        let (mut start, mut end) = (None, None);
        let sub_raw = text(t.get("subCaption"));
        let sub = strip_chars(&sub_raw, &['(', ')', ' ']);
        if let Some((a, b)) = sub.split_once(" - ") {
            let pa = strptime(a.trim(), "%a %b %d, %Y");
            let pb = strptime(b.trim(), "%a %b %d, %Y");
            if let (Some(pa), Some(pb)) = (pa, pb) {
                start = Some(pa.date());
                end = Some(pb.date());
            }
        }
        let (away_i, away_pts, home_i, home_pts) = schedule_columns(t.get("header"));
        let mut games = Vec::new();
        for row in t.get("rows").and_then(|r| r.as_array()).into_iter().flatten() {
            let cells: Vec<Value> = row.get("cells").and_then(|c| c.as_array()).cloned().unwrap_or_default();
            if cells.len() <= away_pts.max(home_pts) {
                continue;
            }
            let id = |i: usize| cells[i].get("teamId").cloned().unwrap_or(Value::Null);
            let p = |i: usize| num(cells[i].get("content")).unwrap_or(0.0);
            games.push((id(away_i), p(away_pts), id(home_i), p(home_pts)));
        }
        out.push(ScheduleTable { number, start, end, playoffs: cap.to_lowercase().starts_with("playoff"), games });
    }
    out
}

/// This scoring period's opponent and FPts from Fantrax's schedule view.
pub fn fantrax_scores(
    ctx: &LeagueContext,
    provider: &dyn LeagueProvider,
    calendar: &PeriodCalendar,
    today: NaiveDate,
) -> anyhow::Result<Option<ScoreInfo>> {
    let mut params = Map::new();
    params.insert("view".to_string(), Value::String("SCHEDULE".into()));
    let data = provider.fantrax_call(&[("getStandings".to_string(), params)], false)?;
    let data = data.into_iter().next().ok_or_else(|| anyhow::anyhow!("empty getStandings response"))?;
    let tables = parse_fantrax_schedule(&data);
    let per = if calendar.source.starts_with("Fantrax") { calendar.current(today) } else { None };
    let mut table = per.and_then(|p| tables.iter().find(|t| t.number == Some(p.number)));
    if table.is_none() {
        let mut dated: Vec<&ScheduleTable> = tables.iter().filter(|t| t.start.is_some() && t.end.is_some()).collect();
        table = dated.iter().copied().find(|t| t.start.unwrap() <= today && today <= t.end.unwrap());
        if table.is_none() {
            dated.sort_by_key(|t| t.start);
            table = dated.into_iter().find(|t| t.start.unwrap() > today);
        }
    }
    let Some(table) = table else { return Ok(None) };
    let my_id = Value::String(ctx.my_team()?.team_id.clone());
    for (away, apts, home, hpts) in &table.games {
        if *away != my_id && *home != my_id {
            continue;
        }
        let mine_away = *away == my_id;
        let other = if mine_away { home } else { away };
        return Ok(Some(ScoreInfo {
            period: table.number,
            start: table.start.or(per.map(|p| p.start)),
            end: table.end.or(per.map(|p| p.end)),
            opponent_id: if truthy_id(other) { id_text(other) } else { None },
            my_points: if mine_away { *apts } else { *hpts },
            their_points: if mine_away { *hpts } else { *apts },
            playoffs: table.playoffs || per.is_some_and(|p| p.playoffs),
            source: "Fantrax schedule".into(),
        }));
    }
    Ok(None)
}

pub fn fetch_scores(
    ctx: &LeagueContext,
    provider: Option<&dyn LeagueProvider>,
    calendar: &PeriodCalendar,
    today: NaiveDate,
    warnings: &mut Vec<String>,
) -> Option<ScoreInfo> {
    let Some(provider) = provider else {
        warnings.push("Live matchup scores unavailable: no provider handle".into());
        return None;
    };
    let res = match ctx.provider.as_str() {
        "espn" => espn_scores(ctx, provider, calendar),
        "fantrax" => fantrax_scores(ctx, provider, calendar, today),
        other => {
            warnings.push(format!("Live matchup scores are not supported for provider '{other}'"));
            return None;
        }
    };
    match res {
        Ok(s) => s,
        Err(e) => {
            warnings.push(format!("Live matchup scores unavailable (Error: {e})"));
            None
        }
    }
}

// --------------------------------------------------------------------------- advice

pub fn stance_for(p: Option<f64>) -> Option<&'static str> {
    let p = p?;
    Some(if p < CHASE_BELOW {
        "chase"
    } else if p > PROTECT_ABOVE {
        "protect"
    } else {
        "even"
    })
}

pub fn build_advice(m: &Matchup, locked: bool, stream_names: &[String], stream_teams: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let (Some(p), Some(_)) = (m.win_probability, m.opponent_team.as_ref()) else {
        return vec!["No opponent found for this period; the projection covers your lineup only.".into()];
    };
    let lead = format!(
        "{} vs {} projected ({} to win)",
        fixed(m.my_projected_total(), 1, false),
        fixed(m.their_projected_total(), 1, false),
        percent(p, 0, false)
    );
    if m.days_left <= 0 {
        out.push(format!(
            "The period is over: {} - {}.",
            fixed(m.my_points_so_far, 1, false),
            fixed(m.their_points_so_far, 1, false)
        ));
        return out;
    }
    let extra = if stream_names.is_empty() { String::new() } else { format!(" Best adds for the days left: {}.", stream_names.join(", ")) };
    let teams = if stream_teams.is_empty() {
        String::new()
    } else {
        format!(" Teams with the most games left: {}.", stream_teams.join(", "))
    };
    match m.stance.as_deref() {
        Some("chase") if locked => out.push(format!(
            "Chase: you trail, {lead}. Your lineup is set for this period, so the gap closes only with the players you started; line up extra games (and goalie starts) for the next period."
        )),
        Some("chase") => out.push(format!(
            "Chase: you trail, {lead}. Add games - stream free agents who play the most remaining nights (off-nights first) and start every goalie with a game; you need variance.{extra}{teams}"
        )),
        Some("protect") => out.push(format!(
            "Protect: you lead, {lead}. Take the safe floor - start confirmed goalies only, keep steady starters over one-game streamers and avoid risky adds."
        )),
        _ => out.push(format!(
            "Even: {lead}. Maximize games played - fill every open slot on game nights and favour off-night starts.{extra}{teams}"
        )),
    }
    let hurt: Vec<&PlayerProjection> = m.my_players.iter().filter(|x| UNAVAILABLE.contains(&x.status.as_str())).collect();
    if !hurt.is_empty() {
        out.push(format!(
            "In your lineup but unavailable: {} - replace them{}",
            hurt.iter().map(|x| format!("{} ({})", x.name, x.status.to_uppercase())).collect::<Vec<_>>().join(", "),
            if locked { " when lineups unlock." } else { "." }
        ));
    }
    let idle: Vec<&PlayerProjection> =
        m.my_players.iter().filter(|x| x.games_left == 0 && !UNAVAILABLE.contains(&x.status.as_str())).collect();
    if !idle.is_empty() && !locked {
        out.push(format!(
            "No games left for {}: swap in players who still play.",
            idle.iter().take(4).map(|x| x.name.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    if let Some(worst) = first_min_by(m.gap_by_position.values(), |a, b| cmp_f64(a.gap(), b.gap())) {
        if worst.gap() < -1.0 {
            out.push(format!(
                "Biggest gap: {} ({} projected) - target that position when streaming.",
                worst.group,
                fixed(worst.gap(), 1, true)
            ));
        }
    }
    let out_theirs: Vec<&KeyPlayer> = m.key_players_theirs.iter().filter(|k| k.tag == "injured").collect();
    if !out_theirs.is_empty() {
        out.push(format!(
            "Their unavailable players: {}.",
            out_theirs.iter().take(3).map(|k| format!("{} ({})", k.name, k.status.to_uppercase())).collect::<Vec<_>>().join(", ")
        ));
    }
    if locked {
        out.push("Lineups lock for the scoring period: adds and swaps count from the next period.".into());
    }
    out
}

/// Whether lineups are locked for the whole period (`lineup_lock_for` is "weekly").
pub fn lineups_locked(ctx: &LeagueContext, provider: Option<&dyn LeagueProvider>) -> bool {
    lineup_lock_for(ctx, provider) == "weekly"
}

// --------------------------------------------------------------------------- entry point

/// Keyword arguments of `current_matchup`.
pub struct MatchupOpts<'a> {
    pub today: Option<NaiveDate>,
    pub draws: usize,
    pub seed: Option<i64>,
    /// skips the provider request (tests)
    pub scores: Option<ScoreInfo>,
    /// defaults to `league_calendar(ctx, provider)`
    pub calendar: Option<&'a PeriodCalendar>,
    pub streaming: bool,
}

impl Default for MatchupOpts<'_> {
    fn default() -> Self {
        MatchupOpts { today: None, draws: DRAWS, seed: None, scores: None, calendar: None, streaming: true }
    }
}

/// The current matchup preview for my team (see the Python docstring). `values` None = valuate
/// the league (fitted scoring).
pub fn current_matchup(
    ctx: &LeagueContext,
    provider: Option<&dyn LeagueProvider>,
    values: Option<&Map<Cid, PlayerValue>>,
    opts: MatchupOpts<'_>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Result<Matchup, fm_core::CoreError> {
    let today = opts.today.unwrap_or(ctx.as_of);
    let owned;
    let values: &Map<Cid, PlayerValue> = match values {
        Some(v) => v,
        None => {
            owned = fit_and_valuate(&mut ctx.clone(), &mut from_config(&ctx.scoring), memo);
            &owned
        }
    };
    let mut warnings: Vec<String> = Vec::new();
    let cal_owned;
    let cal: &PeriodCalendar = match opts.calendar {
        Some(c) => c,
        None => {
            cal_owned = league_calendar(ctx, provider);
            &cal_owned
        }
    };
    let scores = match opts.scores {
        Some(s) => Some(s),
        None => fetch_scores(ctx, provider, cal, today, &mut warnings),
    };
    let per = scores.as_ref().and_then(|s| s.period).filter(|p| *p != 0).and_then(|p| cal.get(p)).or_else(|| cal.current(today));
    let start = scores.as_ref().and_then(|s| s.start).or(per.map(|p| p.start)).unwrap_or(today);
    let end = scores.as_ref().and_then(|s| s.end).or(per.map(|p| p.end)).unwrap_or(today + Duration::days(6));
    let from_day = today.max(start);
    let days_left = ((end - from_day).num_days() + 1).max(0);
    let locked = lineups_locked(ctx, provider);
    let me = ctx.my_team()?;
    let mut opp: Option<&FantasyTeam> = None;
    if let Some(oid) = scores.as_ref().and_then(|s| s.opponent_id.as_ref()) {
        opp = ctx.teams.iter().find(|t| &t.team_id == oid);
        if opp.is_none() {
            warnings.push(format!("Opponent team {oid} is not in the loaded league"));
        }
    }
    let (mine, basis) = project_team(ctx, me, values, from_day, end, locked, now, memo);
    let theirs = match opp {
        Some(o) => project_team(ctx, o, values, from_day, end, locked, now, memo).0,
        None => vec![],
    };
    if !cal.source.starts_with("ESPN") && !cal.source.starts_with("Fantrax") {
        warnings.extend(cal.notes.iter().cloned());
    }
    let mut m = Matchup {
        league: ctx.provider.clone(),
        my_team: me.name.clone(),
        opponent_team: opp.map(|o| o.name.clone()),
        opponent_id: opp.map(|o| o.team_id.clone()),
        period: match &scores {
            Some(s) => s.period,
            None => per.map(|p| p.number),
        },
        start: Some(start),
        end: Some(end),
        from_day: Some(from_day),
        days_left,
        playoffs: scores.as_ref().is_some_and(|s| s.playoffs) || per.is_some_and(|p| p.playoffs),
        my_points_so_far: scores.as_ref().map(|s| s.my_points).unwrap_or(0.0),
        their_points_so_far: scores.as_ref().map(|s| s.their_points).unwrap_or(0.0),
        my_projected_remaining: py_sum(mine.iter().map(|x| x.proj_remaining)),
        their_projected_remaining: py_sum(theirs.iter().map(|x| x.proj_remaining)),
        my_games_left: mine.iter().map(|x| x.games_left).sum(),
        their_games_left: theirs.iter().map(|x| x.games_left).sum(),
        my_sd: team_sd(&mine),
        their_sd: team_sd(&theirs),
        win_probability: None,
        stance: None,
        key_players_theirs: vec![],
        gap_by_position: Map::new(),
        advice: vec![],
        my_players: mine,
        their_players: theirs,
        source: scores.as_ref().map(|s| s.source.clone()).unwrap_or_else(|| "projection only".into()),
        lineup_basis: basis,
        warnings,
        draws: opts.draws,
    };
    if let Some(o) = opp {
        let a: Vec<(f64, f64)> = m.my_players.iter().map(|x| (x.proj_remaining, x.sd)).collect();
        let b: Vec<(f64, f64)> = m.their_players.iter().map(|x| (x.proj_remaining, x.sd)).collect();
        let wp = win_probability(m.my_points_so_far, m.their_points_so_far, &a, &b, opts.draws, opts.seed);
        m.win_probability = Some(wp);
        m.stance = stance_for(Some(wp)).map(|s| s.to_string());
        m.key_players_theirs = key_players(ctx, o, &m.their_players, values, KEY_PLAYERS);
        for g in ["F", "D", "G"] {
            let a = py_sum(m.my_players.iter().filter(|x| x.group == g).map(|x| x.proj_remaining));
            let b = py_sum(m.their_players.iter().filter(|x| x.group == g).map(|x| x.proj_remaining));
            let any = m.my_players.iter().chain(m.their_players.iter()).any(|x| x.group == g);
            if a != 0.0 || b != 0.0 || any {
                m.gap_by_position.insert(g.to_string(), PositionGap { group: g.to_string(), mine: a, theirs: b });
            }
        }
    }
    let mut names: Vec<String> = Vec::new();
    let mut teams: Vec<String> = Vec::new();
    if opts.streaming
        && !locked
        && matches!(m.stance.as_deref(), Some("chase") | Some("even"))
        && days_left > 0
        && !ctx.schedule.is_empty()
    {
        let sopts = StreamingOpts { limit: 3, today: Some(from_day), end: Some(end), team_limit: 3, ..StreamingOpts::default() };
        let plan = streaming_targets(ctx, values, start, &sopts);
        let mut best: Vec<_> = plan.by_slot.values().flatten().collect();
        best.sort_by(|a, b| cmp_f64(-a.proj, -b.proj));
        names = best
            .into_iter()
            .take(3)
            .map(|t| {
                format!(
                    "{} ({}, {} GP, {} pts{})",
                    t.name,
                    t.team.as_deref().unwrap_or("None"),
                    t.games,
                    fixed(t.proj, 1, false),
                    t.needs_drop.as_deref().filter(|s| !s.is_empty()).map(|d| format!("; {d}")).unwrap_or_default()
                )
            })
            .collect();
        teams = plan.teams.iter().filter(|r| r.games > 0).map(|r| format!("{} {}", r.team, r.games)).collect();
    }
    m.advice = build_advice(&m, locked, &names, &teams);
    Ok(m)
}

#[cfg(test)]
mod tests {
    //! Port of tests/test_matchup.py (intent).
    use super::*;
    use crate::analysis::schedule_grid::Period;
    use crate::recommend::lineup::tests::now_at;
    use chrono::{TimeZone, Utc};
    use fm_core::snapshot::{EspnLeagueMeta, LeagueRules, ProviderExtras};

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
    }

    fn today() -> NaiveDate {
        d(8)
    }

    fn pl(cid: &str, team: &str, pos: &[&str], status: &str) -> Player {
        serde_json::from_value(json!({"cid": cid, "name": cid.to_uppercase(), "name_norm": cid, "ids": {}, "team": team,
            "positions": pos, "status": status}))
        .unwrap()
    }

    fn make_league(provider: &str) -> (LeagueContext, Map<Cid, PlayerValue>) {
        let slot = |s: &str, p: Player| json!({"slot": s, "player": p, "starting": !["BN", "IR"].contains(&s)});
        let mine = vec![
            slot("C", pl("m1", "AAA", &["C"], "healthy")),
            slot("C", pl("m2", "BBB", &["C"], "healthy")),
            slot("D", pl("m3", "CCC", &["D"], "healthy")),
            slot("G", pl("mg", "AAA", &["G"], "healthy")),
            slot("BN", pl("mb", "CCC", &["C"], "healthy")),
        ];
        let mut t1 = pl("t1", "AAA", &["C"], "healthy");
        t1.lines.insert("last15".into(), serde_json::from_value(json!({"split": "last15", "gp": 5, "stats": {"G": 10.0}})).unwrap());
        let theirs = vec![
            slot("C", t1),
            slot("C", pl("t2", "BBB", &["C"], "healthy")),
            slot("D", pl("t3", "BBB", &["D"], "healthy")),
            slot("G", pl("tg", "BBB", &["G"], "healthy")),
            slot("IR", pl("tir", "AAA", &["C"], "ir")),
        ];
        let sched = json!({
            "AAA": ["2026-10-05", "2026-10-06", "2026-10-08", "2026-10-10", "2026-10-12"],
            "BBB": ["2026-10-06", "2026-10-10", "2026-10-11", "2026-10-12"],
            "CCC": ["2026-10-07", "2026-10-08", "2026-10-09"]});
        let gpd = json!({"2026-10-05": 3, "2026-10-06": 12, "2026-10-07": 9, "2026-10-08": 4, "2026-10-09": 10,
                         "2026-10-10": 13, "2026-10-11": 5, "2026-10-12": 5});
        let ctx: LeagueContext = serde_json::from_value(json!({"provider": provider, "league_id": "1", "season": 2027,
            "name": "T", "scoring": {"kind": "points", "weights": {"G": 1.0}},
            "roster_shape": {"C": 2, "D": 1, "G": 1, "BN": 2, "IR": 1},
            "teams": [{"team_id": "1", "name": "Mine", "owner_is_me": true, "slots": mine},
                      {"team_id": "2", "name": "Theirs", "owner_is_me": false, "slots": theirs}],
            "free_agents": [pl("fa1", "AAA", &["C"], "healthy"), pl("fa2", "BBB", &["D"], "healthy")],
            "matchup_period": 1, "as_of": "2026-10-08", "schedule": sched, "games_per_day": gpd,
            "season_start": "2026-10-05"}))
        .unwrap();
        let fpg: Map<&str, f64> = [("m1", 2.0), ("m2", 1.5), ("m3", 1.0), ("mg", 3.0), ("mb", 0.5), ("t1", 1.0), ("t2", 2.0),
            ("t3", 1.2), ("tg", 2.5), ("tir", 4.0), ("fa1", 1.8), ("fa2", 1.1)]
        .into_iter()
        .collect();
        let mut values = Map::new();
        for p in ctx.all_players() {
            let g7 = p.team.as_ref().and_then(|t| ctx.schedule.get(t)).map_or(0, |ds| ds.iter().filter(|x| today() <= **x && **x <= d(14)).count());
            let f = fpg[p.cid.as_str()];
            let pv: PlayerValue = serde_json::from_value(json!({"player": p, "fpg": f, "fpg_season": f, "fpg_week": f,
                "vorp": 0.0, "proj_week": f * g7 as f64, "games_next7": g7, "offnight_next7": 0}))
            .unwrap();
            values.insert(p.cid.clone(), pv);
        }
        (ctx, values)
    }

    fn cal() -> PeriodCalendar {
        let mut p2 = Period::new(2, d(12), d(18));
        p2.playoffs = true;
        PeriodCalendar { periods: vec![Period::new(1, d(5), d(11)), p2], playoff_start: Some(2), source: "test".into(), notes: vec![] }
    }

    fn scores(mine: f64, theirs: f64) -> ScoreInfo {
        ScoreInfo {
            period: Some(1),
            start: Some(d(5)),
            end: Some(d(11)),
            opponent_id: Some("2".into()),
            my_points: mine,
            their_points: theirs,
            playoffs: false,
            source: "fake".into(),
        }
    }

    struct Fake {
        extras: ProviderExtras,
        fantrax: Option<Value>,
        espn: Option<Value>,
        fail: bool,
    }

    impl Fake {
        fn new() -> Fake {
            Fake { extras: ProviderExtras::default(), fantrax: None, espn: None, fail: false }
        }
        fn rules(mut self, lock: &str) -> Fake {
            let mut r = LeagueRules::default();
            r.settings.insert("Misc".into(), [(crate::recommend::lineup_lock::LINEUP_RULE.to_string(), lock.to_string())].into_iter().collect());
            self.extras.rules = Some(r);
            self
        }
    }

    impl LeagueProvider for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        fn load(&mut self) -> anyhow::Result<LeagueContext> {
            anyhow::bail!("not loadable")
        }
        fn extras(&self) -> &ProviderExtras {
            &self.extras
        }
        fn fantrax_call(&self, calls: &[(String, Map<String, Value>)], _fresh: bool) -> anyhow::Result<Vec<Value>> {
            assert_eq!(calls[0].0, "getStandings");
            Ok(vec![self.fantrax.clone().ok_or_else(|| anyhow::anyhow!("no fantrax"))?])
        }
        fn espn_league_get(&self, params: &[(String, String)]) -> anyhow::Result<Value> {
            if self.fail {
                anyhow::bail!("espn down");
            }
            assert_eq!(params, &[("view".to_string(), "mMatchupScore".to_string())]);
            self.espn.clone().ok_or_else(|| anyhow::anyhow!("no espn"))
        }
        fn espn_scoreboard(&self, _mp: i64) -> anyhow::Result<Vec<Map<String, Value>>> {
            anyhow::bail!("espn down")
        }
    }

    fn run(ctx: &LeagueContext, prov: Option<&dyn LeagueProvider>, values: &Map<Cid, PlayerValue>, sc: Option<ScoreInfo>, seed: i64, streaming: bool, now: &PyDateTime) -> Matchup {
        let c = cal();
        let opts = MatchupOpts { today: Some(today()), seed: Some(seed), scores: sc, calendar: Some(&c), streaming, ..MatchupOpts::default() };
        current_matchup(ctx, prov, Some(values), opts, now, &ScheduleMemo::new(None)).unwrap()
    }

    fn noon() -> PyDateTime {
        now_at(12, 0, 8)
    }

    fn by_cid(xs: &[PlayerProjection]) -> Map<String, PlayerProjection> {
        xs.iter().map(|x| (x.cid.clone(), x.clone())).collect()
    }

    #[test]
    fn projection_counts_remaining_games_only() {
        let (ctx, values) = make_league("espn");
        let m = run(&ctx, None, &values, Some(scores(10.0, 12.0)), 1, true, &noon());
        assert_eq!(m.opponent_team.as_deref(), Some("Theirs"));
        assert_eq!((m.period, m.days_left, m.from_day), (Some(1), 4, Some(today())));
        let mine = by_cid(&m.my_players);
        assert_eq!(mine["m1"].games_left, 2);
        let pv = &values["m1"];
        assert!((mine["m1"].proj_remaining - pv.proj_week.unwrap() * 2.0 / pv.games_next7.unwrap() as f64).abs() < 1e-9);
        assert_eq!(mine["m3"].games_left, 2);
        assert_eq!(mine["mb"].games_left, 2);
        assert!((mine["mb"].proj_remaining - 1.0).abs() < 1e-9);
        assert_eq!(m.lineup_basis, "best lineup each day");
        assert!(!m.their_players.iter().any(|x| x.cid == "tir"));
        assert_eq!(m.my_points_so_far, 10.0);
        assert_eq!(m.gap_by_position.keys().cloned().collect::<Vec<_>>(), vec!["F", "D", "G"]);
        assert!((m.gap_by_position["G"].mine - mine["mg"].proj_remaining).abs() < 1e-9);
        let js = m.to_json();
        assert!((js["gap_by_position"]["F"]["gap"].as_f64().unwrap() - m.gap_by_position["F"].gap()).abs() < 1e-12);
        assert!((js["margin"].as_f64().unwrap() - m.margin()).abs() < 1e-12);
    }

    #[test]
    fn win_probability_monotonic() {
        let mine = [(10.0, 4.0), (6.0, 3.0)];
        let theirs = [(12.0, 4.5), (5.0, 2.0)];
        let probs: Vec<f64> = (0..=40).step_by(4).map(|x| win_probability(x as f64, 20.0, &mine, &theirs, 2000, Some(7))).collect();
        assert!(probs.windows(2).all(|w| w[0] <= w[1]), "{probs:?}");
        assert!(probs[0] < 0.1 && probs[probs.len() - 1] > 0.9);
        assert!((win_probability(0.0, 0.0, &[(10.0, 3.0)], &[(10.0, 3.0)], 2000, Some(3)) - 0.5).abs() < 0.05);
        assert_eq!(win_probability(5.0, 4.0, &[], &[], 2000, None), 1.0);
        assert_eq!(win_probability(4.0, 5.0, &[], &[], 2000, None), 0.0);
        assert_eq!(win_probability(4.0, 4.0, &[(0.0, 0.0)], &[], 2000, None), 0.5);
        let (ctx, values) = make_league("espn");
        let ps: Vec<f64> = [0.0, 20.0, 30.0, 40.0, 60.0]
            .iter()
            .map(|x| run(&ctx, None, &values, Some(scores(*x, 30.0)), 11, false, &noon()).win_probability.unwrap())
            .collect();
        assert!(ps.windows(2).all(|w| w[0] <= w[1]) && ps[0] < ps[4], "{ps:?}");
    }

    #[test]
    fn advice_chase_protect_and_key_players() {
        let (ctx, values) = make_league("espn");
        let behind = run(&ctx, None, &values, Some(scores(0.0, 60.0)), 1, true, &noon());
        assert_eq!(behind.stance.as_deref(), Some("chase"));
        assert!(behind.advice[0].starts_with("Chase") && behind.advice[0].contains("stream") && behind.advice[0].contains("FA1"), "{:?}", behind.advice);
        assert!(behind.advice.iter().any(|a| a.starts_with("Their unavailable players: TIR")));
        let ahead = run(&ctx, None, &values, Some(scores(60.0, 0.0)), 1, true, &noon());
        assert_eq!(ahead.stance.as_deref(), Some("protect"));
        assert!(ahead.advice[0].starts_with("Protect"));
        let m = run(&ctx, None, &values, Some(scores(0.0, 0.0)), 1, true, &noon());
        let tags: Map<String, String> = m.key_players_theirs.iter().map(|k| (k.cid.clone(), k.tag.clone())).collect();
        assert_eq!(tags["tir"], "injured");
        assert_eq!(tags["t1"], "hot");
        let hot = m.key_players_theirs.iter().find(|k| k.cid == "t1").unwrap();
        assert!((hot.recent_fpg.unwrap() - 2.0).abs() < 1e-9);
        assert!(tags.values().any(|t| t == "top"));
    }

    #[test]
    fn no_scores_is_projection_only() {
        let (ctx, values) = make_league("espn");
        let m = run(&ctx, None, &values, None, 1, true, &noon());
        assert!(m.opponent_team.is_none() && m.win_probability.is_none());
        assert_eq!(m.period, Some(1));
        assert!(m.my_projected_remaining > 0.0);
        assert!(m.warnings.iter().any(|w| w.contains("no provider handle")));
        assert!(m.advice[0].starts_with("No opponent found"));
    }

    #[test]
    fn lineups_locked_by_rules() {
        let (ctx, _) = make_league("fantrax");
        assert!(lineups_locked(&ctx, Some(&Fake::new())));
        assert!(!lineups_locked(&ctx, Some(&Fake::new().rules("Daily"))));
        assert!(lineups_locked(&ctx, Some(&Fake::new().rules("Weekly"))));
        let (espn, _) = make_league("espn");
        assert!(!lineups_locked(&espn, None));
    }

    fn started_aaa(ctx: &mut LeagueContext) -> (PyDateTime, PyDateTime, PyDateTime) {
        let at = |dd: u32, h: u32, mi: u32| PyDateTime::from_utc(Utc.with_ymd_and_hms(2026, 10, dd, h, mi, 0).unwrap());
        ctx.game_starts.insert("AAA".into(), [(today(), at(8, 23, 0))].into_iter().collect());
        ctx.game_starts.insert("CCC".into(), [(today(), at(9, 1, 0))].into_iter().collect());
        (at(8, 22, 0), at(8, 23, 30), at(9, 6, 0))
    }

    #[test]
    fn started_games_today_are_not_projected_again() {
        let (mut ctx, values) = make_league("espn");
        let (before, live, over) = started_aaa(&mut ctx);
        let pre = run(&ctx, None, &values, Some(scores(10.0, 12.0)), 1, false, &before);
        let mid = run(&ctx, None, &values, Some(scores(10.0, 12.0)), 1, false, &live);
        let (pm, mm) = (by_cid(&pre.my_players), by_cid(&mid.my_players));
        let per_game = values["m1"].proj_week.unwrap() / values["m1"].games_next7.unwrap() as f64;
        assert_eq!((pm["m1"].games_left, mm["m1"].games_left), (2, 1));
        assert!((mm["m1"].proj_remaining - (pm["m1"].proj_remaining - per_game)).abs() < 1e-9);
        assert_eq!(mm["mg"].games_left, pm["mg"].games_left - 1);
        assert_eq!((mm["m3"].games_left, pm["m3"].games_left), (2, 2));
        assert!(mid.my_projected_remaining < pre.my_projected_remaining);
        let end = run(&ctx, None, &values, Some(scores(10.0, 12.0)), 1, false, &over);
        let em = by_cid(&end.my_players);
        assert_eq!((em["m3"].games_left, em["m1"].games_left), (1, 1));
    }

    #[test]
    fn started_games_today_weekly_lock_path() {
        let (mut ctx, values) = make_league("fantrax");
        let weekly = Fake::new().rules("Weekly");
        let (before, live, _) = started_aaa(&mut ctx);
        let pre = by_cid(&run(&ctx, Some(&weekly), &values, Some(scores(0.0, 0.0)), 1, true, &before).my_players);
        let mid = by_cid(&run(&ctx, Some(&weekly), &values, Some(scores(0.0, 0.0)), 1, true, &live).my_players);
        assert_eq!((pre["m1"].games_left, mid["m1"].games_left), (2, 1));
        assert!((mid["m1"].proj_remaining - values["m1"].proj_week.unwrap() / values["m1"].games_next7.unwrap() as f64).abs() < 1e-9);
        assert_eq!(mid["m3"].games_left, 2);
    }

    #[test]
    fn locked_lineup_uses_current_starters_and_no_streaming() {
        let (ctx, values) = make_league("fantrax");
        let weekly = Fake::new().rules("Weekly");
        let m = run(&ctx, Some(&weekly), &values, Some(scores(0.0, 60.0)), 1, true, &noon());
        assert!(m.lineup_basis.starts_with("current lineup"));
        let mut cids: Vec<&str> = m.my_players.iter().map(|x| x.cid.as_str()).collect();
        cids.sort();
        assert_eq!(cids, vec!["m1", "m2", "m3", "mg"]);
        assert!(m.advice[0].contains("Your lineup is set") && !m.advice[0].contains("FA1"));
        assert!(m.advice.last().unwrap().starts_with("Lineups lock"));
    }

    fn fx_schedule() -> Value {
        json!({"tableList": [
            {"caption": "Scoring Period 1", "subCaption": "(Mon Oct 5, 2026 - Sun Oct 11, 2026)", "rows": [
                {"cells": [{"content": "Theirs", "teamId": "2"}, {"content": "41.5"},
                           {"content": "Mine", "teamId": "1"}, {"content": "1,012.25"}]},
                {"cells": [{"content": "X", "teamId": "3"}, {"content": "0"}, {"content": "Y", "teamId": "4"},
                           {"content": "0"}]}]},
            {"caption": "Scoring Period 2", "subCaption": "(Mon Oct 12, 2026 - Sun Oct 18, 2026)", "rows": [
                {"cells": [{"content": "Mine", "teamId": "1"}, {"content": "0"}, {"content": "X", "teamId": "3"},
                           {"content": "0"}]}]}]})
    }

    #[test]
    fn fantrax_scores_with_fake_client() {
        let (ctx, values) = make_league("fantrax");
        let tables = parse_fantrax_schedule(&fx_schedule());
        assert_eq!(tables.iter().map(|t| t.number).collect::<Vec<_>>(), vec![Some(1), Some(2)]);
        assert_eq!(tables[0].games[0], (json!("2"), 41.5, json!("1"), 1012.25));
        let mut prov = Fake::new().rules("Daily");
        prov.fantrax = Some(fx_schedule());
        let s = fantrax_scores(&ctx, &prov, &cal(), today()).unwrap().unwrap();
        assert_eq!((s.period, s.opponent_id.as_deref(), s.my_points, s.their_points), (Some(1), Some("2"), 1012.25, 41.5));
        let m = run(&ctx, Some(&prov), &values, None, 2, true, &noon());
        assert_eq!(m.opponent_team.as_deref(), Some("Theirs"));
        assert_eq!(m.source, "Fantrax schedule");
        assert_eq!(m.lineup_basis, "optimal week lineup");
        assert_eq!(m.win_probability, Some(1.0));
    }

    #[test]
    fn espn_scores_with_fake_league() {
        let (ctx, values) = make_league("espn");
        let mut prov = Fake::new();
        prov.extras.espn_league = Some(EspnLeagueMeta { current_matchup_period: Some(1), ..EspnLeagueMeta::default() });
        prov.espn = Some(json!({"schedule": [
            {"matchupPeriodId": 1, "playoffTierType": "NONE",
             "home": {"teamId": 2, "totalPoints": 20.0, "totalPointsLive": 22.5},
             "away": {"teamId": 1, "totalPoints": 30.0, "totalPointsLive": null}},
            {"matchupPeriodId": 2, "home": {"teamId": 1}, "away": {"teamId": 3}}]}));
        let s = espn_scores(&ctx, &prov, &cal()).unwrap().unwrap();
        assert_eq!((s.period, s.opponent_id.as_deref(), s.my_points, s.their_points), (Some(1), Some("2"), 30.0, 22.5));
        assert_eq!((s.start, s.end), (Some(d(5)), Some(d(11))));
        let m = run(&ctx, Some(&prov), &values, None, 3, true, &noon());
        assert_eq!(m.opponent_team.as_deref(), Some("Theirs"));
        assert_eq!(m.my_points_so_far, 30.0);
        assert_eq!(m.source, "ESPN scoreboard");
    }

    #[test]
    fn provider_failure_becomes_warning() {
        let (ctx, values) = make_league("espn");
        let mut prov = Fake::new();
        prov.fail = true;
        let m = run(&ctx, Some(&prov), &values, None, 1, true, &noon());
        assert!(m.opponent_team.is_none());
        assert!(m.warnings.iter().any(|w| w.contains("espn down")), "{:?}", m.warnings);
    }

    #[test]
    fn apportion_and_sd() {
        let pv: PlayerValue = serde_json::from_value(json!({"player": {"cid": "a", "name": "a", "name_norm": "a", "ids": {},
            "team": "EDM", "positions": ["C"]}, "fpg": 2.0, "fpg_season": 2.0, "fpg_week": 2.0, "vorp": 1.0,
            "proj_week": 9.0, "games_next7": 3}))
        .unwrap();
        assert_eq!(apportion(Some(&pv), 2), 6.0);
        assert_eq!(apportion(Some(&pv), 0), 0.0);
        assert_eq!(apportion(None, 3), 0.0);
        assert!((game_sd(4.0, false) - 2.6).abs() < 1e-12);
        assert!((game_sd(4.0, true) - 5.0).abs() < 1e-12);
        assert_eq!(game_sd(-1.0, true), 0.0);
    }

    #[test]
    fn win_probability_deterministic_and_seeded() {
        assert_eq!(win_probability(10.0, 5.0, &[(1.0, 0.0)], &[(1.0, 0.0)], 100, None), 1.0);
        assert_eq!(win_probability(5.0, 5.0, &[(1.0, 0.0)], &[(1.0, 0.0)], 100, None), 0.5);
        let a = win_probability(0.0, 0.0, &[(10.0, 3.0)], &[(10.0, 3.0)], 2000, Some(7));
        let b = win_probability(0.0, 0.0, &[(10.0, 3.0)], &[(10.0, 3.0)], 2000, Some(7));
        assert_eq!(a, b);
        assert!((a - 0.5).abs() < 0.05);
        let lead = win_probability(20.0, 0.0, &[(10.0, 3.0)], &[(10.0, 3.0)], 2000, Some(7));
        assert!(lead > 0.99);
    }

    #[test]
    fn stance_thresholds() {
        assert_eq!(stance_for(Some(0.2)), Some("chase"));
        assert_eq!(stance_for(Some(0.5)), Some("even"));
        assert_eq!(stance_for(Some(0.7)), Some("protect"));
        assert_eq!(stance_for(None), None);
    }

    #[test]
    fn fantrax_schedule_parsing() {
        let data = json!({"tableList": [
            {"caption": "Scoring Period 6", "subCaption": "(Mon Nov 2, 2026 - Sun Nov 8, 2026)",
             "header": {"cells": [{"key": "away"}, {"key": "fpts"}, {"key": "adj"}, {"key": "total"},
                                  {"key": "home"}, {"key": "fpts"}, {"key": "adj"}, {"key": "total"}]},
             "rows": [{"cells": [{"teamId": "a"}, {"content": "10.5"}, {"content": "0"}, {"content": "11.5"},
                                 {"teamId": "b"}, {"content": "1,020.25"}, {"content": "0"}, {"content": "20"}]}]},
            {"caption": "Playoffs Round 1", "rows": [{"cells": [{"teamId": "a"}, {"content": "1"}, {"teamId": "b"}, {"content": "2"}]}]}
        ]});
        let t = parse_fantrax_schedule(&data);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].number, Some(6));
        assert_eq!(t[0].start, NaiveDate::from_ymd_opt(2026, 11, 2));
        assert_eq!(t[0].end, NaiveDate::from_ymd_opt(2026, 11, 8));
        assert_eq!(t[0].games, vec![(json!("a"), 11.5, json!("b"), 20.0)]);
        assert!(!t[0].playoffs);
        assert!(t[1].playoffs && t[1].number == Some(1));
        assert_eq!(t[1].games, vec![(json!("a"), 1.0, json!("b"), 2.0)]);
    }
}
