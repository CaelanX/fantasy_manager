//! Rust port of `fantasy_manager/analysis/playoffs.py`: the roster-aware fantasy-playoff planner
//! (`fm playoffs`, the dashboard's Playoffs section). Owner: work package `W8-outlook-playoffs-calib`.
//!
//! Deviations (behaviour-preserving): the per-team schedule date sets are built once in the
//! solver (Python rebuilt `set(...)` per call); the solver's per-game memo is dropped (pure and
//! cheap). `recommend.lineup._dp` is [`crate::recommend::lineup::dp_idx`], `_lock` is
//! [`crate::recommend::lineup_lock::lock`].

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Local, NaiveDate};
use fm_core::models::{LeagueContext, Player};
use fm_core::normalize::normalize_team;
use fm_core::provider::LeagueProvider;
use fm_core::values::PlayerValue;
use fm_core::{Cid, Map};
use fm_pycompat::num::{py_sum, round_to};
use fm_pycompat::order::cmp_f64;
use fm_pycompat::time::{date_fromisoformat, strptime};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use super::schedule_grid::{group_of as group, offnight_bonus, per_day, playoff_weeks, window_counts, Period, PlayoffWeeks, OFFNIGHT_THRESHOLD, UNAVAILABLE};
use crate::recommend::lineup::{dp_idx, starting_slots};
use crate::recommend::lineup_lock;

pub const TARGETS: usize = 10;
/// acquisition candidates re-solved in my lineup (best naive playoff points)
pub const CANDIDATES: usize = 60;
/// playoff games below the average NHL team that make a player a sell candidate
pub const SELL_GAMES_BELOW: i64 = 2;
pub const GROUPS: [&str; 3] = ["F", "D", "G"];
pub const DEADLINE_LABEL: &str = "Trade Deadline Date";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayoffPlayer {
    pub cid: String,
    pub name: String,
    pub team: Option<String>,
    pub pos: String,
    pub group: String,
    /// fantasy team (None: free agent)
    pub owner: Option<String>,
    pub status: String,
    /// per playoff period
    pub games: Vec<i64>,
    pub total: i64,
    pub offnights: i64,
    pub b2b: i64,
    /// playoff games minus the average NHL team's
    pub vs_avg: f64,
    pub per_game: f64,
    /// per-game x games x (1 + bonus x off-nights)
    pub naive: f64,
    /// my lineup's playoff points with him minus without him
    pub points: f64,
    /// playoff starts in my lineup (my players)
    pub starts: Option<i64>,
    /// sell: points the weak schedule costs vs an average one
    pub cost: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupStarts {
    pub group: String,
    pub mine: f64,
    pub league_avg: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayoffPlan {
    pub periods: Vec<Period>,
    pub source: String,
    pub notes: Vec<String>,
    pub days: i64,
    pub avg_team_games: f64,
    pub lock: String,
    pub my_team: String,
    pub my_points: f64,
    pub league_avg_points: f64,
    pub rank: Option<i64>,
    pub n_teams: i64,
    pub groups: Vec<GroupStarts>,
    pub hold: Vec<PlayoffPlayer>,
    pub sell: Vec<PlayoffPlayer>,
    pub acquire_fa: Vec<PlayoffPlayer>,
    pub acquire_trade: Vec<PlayoffPlayer>,
    pub deadline: Option<NaiveDate>,
    pub deadline_source: Option<String>,
    pub days_to_deadline: Option<i64>,
}

impl PlayoffPlan {
    /// `plan.model_dump(mode="json")`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

// --------------------------------------------------------------------------- values

/// Expected points per NHL team game in the fantasy playoffs: healthy FPG x start share.
pub fn playoff_per_game(pv: Option<&PlayerValue>) -> f64 {
    let Some(pv) = pv else { return 0.0 };
    (pv.fpg * pv.start_share.unwrap_or(1.0)).max(0.0)
}

/// An injured player counts from his estimated return date; without one he is assumed back by
/// then, except on LTIR.
pub fn available_in_playoffs(p: &Player, pv: Option<&PlayerValue>, day: NaiveDate) -> bool {
    if !UNAVAILABLE.contains(&p.status.as_str()) {
        return true;
    }
    match pv.and_then(|v| v.return_date) {
        Some(ret) => day >= ret,
        None => p.status.as_str() != "ltir",
    }
}

fn period_days(per: &Period) -> Vec<NaiveDate> {
    (0..=(per.end - per.start).num_days()).map(|i| per.start + Duration::days(i)).collect()
}

fn days_of(periods: &[Period]) -> Vec<NaiveDate> {
    periods.iter().flat_map(period_days).collect()
}

/// Best playoff lineup per day (or per period in weekly-lock leagues) for a pool of players.
pub struct Solver<'a> {
    values: &'a Map<Cid, PlayerValue>,
    slots: Vec<String>,
    /// a "unit" is a day (daily lineups) or a period (weekly lock); each unit -> its days
    units: Vec<Vec<NaiveDate>>,
    games: HashMap<&'a str, HashSet<NaiveDate>>,
}

impl<'a> Solver<'a> {
    pub fn new(ctx: &'a LeagueContext, values: &'a Map<Cid, PlayerValue>, periods: &[Period], lock: &str) -> Self {
        let units: Vec<Vec<NaiveDate>> =
            if lock == "daily" { days_of(periods).into_iter().map(|d| vec![d]).collect() } else { periods.iter().map(period_days).collect() };
        let games = ctx.schedule.iter().map(|(t, ds)| (t.as_str(), ds.iter().copied().collect())).collect();
        Solver { values, slots: starting_slots(&ctx.roster_shape), units, games }
    }

    pub fn per_game(&self, p: &Player) -> f64 {
        playoff_per_game(self.values.get(&p.cid))
    }

    fn team_games(&self, p: &Player) -> Option<&HashSet<NaiveDate>> {
        let t = normalize_team(p.team.as_deref()).unwrap_or_default();
        self.games.get(t.as_str())
    }

    /// His games in `unit` that he is available for.
    pub fn games_in(&self, p: &Player, unit: &[NaiveDate]) -> i64 {
        let Some(games) = self.team_games(p) else { return 0 };
        let pv = self.values.get(&p.cid);
        unit.iter().filter(|d| games.contains(d) && available_in_playoffs(p, pv, **d)).count() as i64
    }

    pub fn unit_value(&self, p: &Player, unit: &[NaiveDate]) -> f64 {
        self.games_in(p, unit) as f64 * self.per_game(p)
    }

    /// (points, indices into `pool` of the chosen starters).
    fn solve_unit(&self, pool: &[&Player], unit: &[NaiveDate]) -> (f64, Vec<usize>) {
        let vals: Vec<f64> = pool.iter().map(|p| self.unit_value(p, unit)).collect();
        let live_idx: Vec<usize> = (0..pool.len()).filter(|&i| vals[i] > 0.0).collect();
        if live_idx.is_empty() {
            return (0.0, vec![]);
        }
        let live: Vec<&Player> = live_idx.iter().map(|&i| pool[i]).collect();
        let (total, assign) = dp_idx(&live, &self.slots, |i, _| vals[live_idx[i]]);
        (total, assign.into_iter().map(|(_, i)| live_idx[i]).collect())
    }

    /// (total points, per-unit points, playoff starts per cid).
    pub fn solve(&self, pool: &[&Player]) -> (f64, Vec<f64>, HashMap<String, i64>) {
        let mut per_unit: Vec<f64> = Vec::new();
        let mut starts: HashMap<String, i64> = HashMap::new();
        for unit in &self.units {
            let (total, chosen) = self.solve_unit(pool, unit);
            per_unit.push(total);
            for i in chosen {
                *starts.entry(pool[i].cid.clone()).or_insert(0) += self.games_in(pool[i], unit);
            }
        }
        (py_sum(per_unit.iter().copied()), per_unit, starts)
    }

    fn plays_in(&self, p: &Player, unit: &[NaiveDate]) -> bool {
        self.team_games(p).is_some_and(|g| unit.iter().any(|d| g.contains(d)))
    }

    /// Total change from adding `add` / removing `remove`: only the units he plays in.
    pub fn delta(&self, pool: &[&Player], per_unit: &[f64], add: Option<&Player>, remove: Option<&Player>) -> f64 {
        let who = add.or(remove).expect("add or remove");
        let mut new_pool: Vec<&Player> = pool.iter().copied().filter(|p| remove.is_none_or(|r| p.cid != r.cid)).collect();
        if let Some(a) = add {
            new_pool.push(a);
        }
        let mut out = 0.0;
        for (i, unit) in self.units.iter().enumerate() {
            if self.plays_in(who, unit) {
                out += self.solve_unit(&new_pool, unit).0 - per_unit[i];
            }
        }
        out
    }
}

// --------------------------------------------------------------------------- trade deadline

const DATE_FORMATS: [&str; 9] =
    ["%b %d, %Y", "%B %d, %Y", "%b %d %Y", "%B %d %Y", "%Y-%m-%d", "%m/%d/%Y", "%m/%d/%y", "%b %d/%y", "%d %b %Y"];

/// Best-effort date out of a Rules-page value such as "Thu Mar 4, 2027 12:00PM EST".
pub fn parse_deadline(text: Option<&str>) -> Option<NaiveDate> {
    let text = text.filter(|t| !t.is_empty())?;
    let dow = RegexBuilder::new(r"^\s*(mon|tue|wed|thu|fri|sat|sun)[a-z]*\.?,?\s+").case_insensitive(true).build().ok()?;
    let clock = Regex::new(r"\s+\d{1,2}:\d{2}.*$").ok()?;
    let s0 = dow.replace(text.trim(), "").into_owned();
    let s1 = clock.replace(&s0, "").into_owned();
    let s = s1.trim().trim_end_matches(',');
    for fmt in DATE_FORMATS {
        if let Some(dt) = strptime(s, fmt) {
            return Some(dt.date());
        }
    }
    let iso = Regex::new(r"\d{4}-\d{2}-\d{2}").ok()?;
    iso.find(text).and_then(|m| date_fromisoformat(m.as_str()))
}

/// (deadline, where it came from).
pub fn trade_deadline(ctx: &LeagueContext, provider: Option<&dyn LeagueProvider>) -> (Option<NaiveDate>, Option<String>) {
    let Some(provider) = provider else { return (None, None) };
    match ctx.provider.as_str() {
        "espn" => {
            if let Some(ms) = provider.espn_league().and_then(|l| l.settings.trade_deadline) {
                if ms > 0 {
                    if let Some(dt) = DateTime::from_timestamp_millis(ms) {
                        return (Some(dt.with_timezone(&Local).date_naive()), Some("ESPN league settings".to_string()));
                    }
                }
            }
        }
        "fantrax" => {
            let raw = provider.rules().and_then(|r| r.get(DEADLINE_LABEL));
            if let Some(d) = parse_deadline(raw) {
                return (Some(d), Some("Fantrax Rules page".to_string()));
            }
            if let Some(raw) = raw.filter(|r| !r.is_empty()) {
                // e.g. "Not Used": no deadline in this league
                return (None, Some(format!("Fantrax Rules page: {raw}")));
            }
        }
        _ => {}
    }
    (None, None)
}

// --------------------------------------------------------------------------- the plan

fn row(
    ctx: &LeagueContext,
    p: &Player,
    po: &PlayoffWeeks,
    gpd: &Map<NaiveDate, i64>,
    solver: &Solver<'_>,
    owner: Option<&str>,
    threshold: i64,
) -> Option<PlayoffPlayer> {
    let team = normalize_team(p.team.as_deref());
    let dates = ctx.schedule.get(team.as_deref().unwrap_or(""))?;
    if dates.is_empty() {
        return None;
    }
    let cells: Vec<(i64, i64, i64)> = po.periods.iter().map(|per| window_counts(dates, gpd, per.start, per.end, threshold)).collect();
    let games: Vec<i64> = cells.iter().map(|c| c.0).collect();
    let total: i64 = games.iter().sum();
    let off: i64 = cells.iter().map(|c| c.1).sum();
    let b2b: i64 = cells.iter().map(|c| c.2).sum();
    let pg = solver.per_game(p);
    let pos = p.positions.iter().filter(|x| x.as_str() != "F").cloned().collect::<Vec<_>>().join("/");
    Some(PlayoffPlayer {
        cid: p.cid.clone(),
        name: p.name.clone(),
        team,
        pos: if pos.is_empty() { group(p).to_string() } else { pos },
        group: group(p).to_string(),
        owner: owner.map(str::to_string),
        status: p.status.as_str().to_string(),
        games,
        total,
        offnights: off,
        b2b,
        vs_avg: round_to(total as f64 - po.avg_total, 2),
        per_game: round_to(pg, 3),
        naive: round_to(pg * total as f64 * (1.0 + offnight_bonus() * off as f64), 2),
        points: 0.0,
        starts: None,
        cost: None,
    })
}

/// The roster-aware fantasy-playoff plan (see the Python module docstring).
pub fn playoff_plan(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    provider: Option<&dyn LeagueProvider>,
    weeks: Option<PlayoffWeeks>,
    limit: usize,
    threshold: i64,
) -> PlayoffPlan {
    let po = weeks.unwrap_or_else(|| playoff_weeks(ctx, provider, None, threshold));
    let mut plan = PlayoffPlan {
        periods: po.periods.clone(),
        source: po.source.clone(),
        notes: po.notes.clone(),
        days: 0,
        avg_team_games: po.avg_total,
        lock: "daily".to_string(),
        my_team: String::new(),
        my_points: 0.0,
        league_avg_points: 0.0,
        rank: None,
        n_teams: 0,
        groups: vec![],
        hold: vec![],
        sell: vec![],
        acquire_fa: vec![],
        acquire_trade: vec![],
        deadline: None,
        deadline_source: None,
        days_to_deadline: None,
    };
    let (deadline, src) = trade_deadline(ctx, provider);
    plan.deadline = deadline;
    plan.deadline_source = src;
    if let Some(d) = deadline {
        plan.days_to_deadline = Some((d - ctx.as_of).num_days());
    }
    if po.periods.is_empty() || ctx.schedule.is_empty() {
        return plan;
    }
    let Ok(mine) = ctx.my_team() else {
        plan.notes.push("No team flagged as mine".to_string());
        return plan;
    };
    plan.lock = lineup_lock::lock(ctx).to_string();
    plan.days = days_of(&po.periods).len() as i64;
    plan.my_team = mine.name.clone();
    let solver = Solver::new(ctx, values, &po.periods, &plan.lock);
    let gpd = per_day(ctx);

    // every team's projected playoff points and starts by position
    let mut totals: Map<String, f64> = Map::new();
    let mut starts_by: Map<String, HashMap<&'static str, f64>> = Map::new();
    let mut my_units: Vec<f64> = Vec::new();
    let mut my_starts: HashMap<String, i64> = HashMap::new();
    for t in &ctx.teams {
        let players = t.players();
        let (total, units, starts) = solver.solve(&players);
        totals.insert(t.team_id.clone(), total);
        let by_cid: HashMap<&str, &Player> = players.iter().map(|p| (p.cid.as_str(), *p)).collect();
        let mut per_group: HashMap<&'static str, f64> = HashMap::new();
        for g in GROUPS {
            let n: i64 = starts.iter().filter(|(c, _)| group(by_cid[c.as_str()]) == g).map(|(_, n)| *n).sum();
            per_group.insert(g, n as f64);
        }
        starts_by.insert(t.team_id.clone(), per_group);
        if t.team_id == mine.team_id {
            my_units = units;
            my_starts = starts;
        }
    }
    let my_total = totals.get(&mine.team_id).copied().unwrap_or(0.0);
    plan.my_points = round_to(my_total, 1);
    plan.n_teams = totals.len() as i64;
    plan.league_avg_points = round_to(py_sum(totals.values().copied()) / totals.len().max(1) as f64, 1);
    plan.rank = Some(1 + totals.iter().filter(|(k, v)| **k != mine.team_id && **v > my_total).count() as i64);
    for g in GROUPS {
        if starts_by.values().any(|s| s.get(g).copied().unwrap_or(0.0) != 0.0) {
            plan.groups.push(GroupStarts {
                group: g.to_string(),
                mine: starts_by.get(&mine.team_id).and_then(|s| s.get(g)).copied().unwrap_or(0.0),
                league_avg: round_to(py_sum(starts_by.values().map(|s| s.get(g).copied().unwrap_or(0.0))) / starts_by.len().max(1) as f64, 1),
            });
        }
    }

    // hold / sell: my players
    let pool: Vec<&Player> = mine.players();
    let mut hold: Vec<PlayoffPlayer> = Vec::new();
    let mut sell: Vec<PlayoffPlayer> = Vec::new();
    for &p in &pool {
        let Some(mut r) = row(ctx, p, &po, &gpd, &solver, Some(&mine.name), threshold) else { continue };
        r.starts = Some(my_starts.get(&p.cid).copied().unwrap_or(0));
        r.points = round_to(-solver.delta(&pool, &my_units, None, Some(p)), 2);
        hold.push(r.clone());
        if r.vs_avg <= -(SELL_GAMES_BELOW as f64) && r.per_game > 0.0 {
            let mut s = r;
            s.cost = Some(round_to(s.per_game * -s.vs_avg, 2));
            sell.push(s);
        }
    }
    hold.sort_by(|a, b| cmp_f64(-a.points, -b.points).then(cmp_f64(-a.naive, -b.naive)).then_with(|| a.name.cmp(&b.name)));
    plan.hold = hold;
    sell.sort_by(|a, b| cmp_f64(-a.cost.unwrap_or(0.0), -b.cost.unwrap_or(0.0)).then_with(|| a.name.cmp(&b.name)));
    sell.truncate(limit);
    plan.sell = sell;

    // acquire: free agents and other teams' players, by the playoff points they add to my lineup
    let mut owners: HashMap<&str, &str> = HashMap::new();
    for t in ctx.teams.iter().filter(|t| t.team_id != mine.team_id) {
        for p in t.players_iter() {
            owners.insert(p.cid.as_str(), t.name.as_str());
        }
    }
    let mine_ids: HashSet<&str> = pool.iter().map(|p| p.cid.as_str()).collect();
    let mut cands: Vec<(&Player, PlayoffPlayer)> = Vec::new();
    let others = ctx.teams.iter().filter(|t| t.team_id != mine.team_id).flat_map(|t| t.players_iter());
    for p in ctx.free_agents.iter().chain(others) {
        if mine_ids.contains(p.cid.as_str()) || (p.status.as_str() == "ltir" && values.get(&p.cid).and_then(|v| v.return_date).is_none()) {
            continue;
        }
        if let Some(r) = row(ctx, p, &po, &gpd, &solver, owners.get(p.cid.as_str()).copied(), threshold) {
            if r.naive > 0.0 {
                cands.push((p, r));
            }
        }
    }
    let by_naive = |a: &&(&Player, PlayoffPlayer), b: &&(&Player, PlayoffPlayer)| cmp_f64(-a.1.naive, -b.1.naive);
    let mut fa: Vec<&(&Player, PlayoffPlayer)> = cands.iter().filter(|c| c.1.owner.is_none()).collect();
    fa.sort_by(by_naive);
    fa.truncate(CANDIDATES / 2);
    let mut tr: Vec<&(&Player, PlayoffPlayer)> = cands.iter().filter(|c| c.1.owner.is_some()).collect();
    tr.sort_by(by_naive);
    tr.truncate(CANDIDATES / 2);
    let finish = |picked: Vec<&(&Player, PlayoffPlayer)>| -> Vec<PlayoffPlayer> {
        let mut rows: Vec<PlayoffPlayer> = picked
            .into_iter()
            .map(|(p, r)| {
                let mut r = r.clone();
                r.points = round_to(solver.delta(&pool, &my_units, Some(p), None), 2);
                r
            })
            .collect();
        rows.sort_by(|a, b| cmp_f64(-a.points, -b.points).then(cmp_f64(-a.naive, -b.naive)).then_with(|| a.name.cmp(&b.name)));
        rows.retain(|r| r.points > 0.0);
        rows.truncate(limit);
        rows
    };
    plan.acquire_fa = finish(fa);
    plan.acquire_trade = finish(tr);
    plan
}

/// Default-argument form: `playoff_plan(ctx, values, provider, limit=TARGETS)`.
pub fn playoff_plan_default(ctx: &LeagueContext, values: &Map<Cid, PlayerValue>, provider: Option<&dyn LeagueProvider>) -> PlayoffPlan {
    playoff_plan(ctx, values, provider, None, TARGETS, OFFNIGHT_THRESHOLD)
}

pub fn plan_json(plan: &PlayoffPlan) -> serde_json::Value {
    plan.to_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn parse_deadline_formats() {
        assert_eq!(parse_deadline(Some("Thu Mar 4, 2027 12:00PM EST")), Some(d(2027, 3, 4)));
        assert_eq!(parse_deadline(Some("March 4, 2027")), Some(d(2027, 3, 4)));
        assert_eq!(parse_deadline(Some("2027-03-04")), Some(d(2027, 3, 4)));
        assert_eq!(parse_deadline(Some("3/4/2027")), Some(d(2027, 3, 4)));
        assert_eq!(parse_deadline(Some("Not Used")), None);
        assert_eq!(parse_deadline(None), None);
        assert_eq!(parse_deadline(Some("")), None);
    }

    #[test]
    fn per_game_uses_start_share() {
        assert_eq!(playoff_per_game(None), 0.0);
    }
}
