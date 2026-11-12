//! Rust port of `fantasy_manager/analysis/outlook.py`: contention window and mode advisor
//! (`fm outlook`, the dashboard's Outlook section). Owner: work package `W8-outlook-playoffs-calib`.
//!
//! * `future_values` / `age` live in [`super::outlook_values`] (W7-trades) and are re-exported.
//! * `_lineup` calls [`crate::recommend::trades::greedy_lineup_value`].
//! * Python's `_greedy_assignment` (the set of starters, used by the sell/buy lists) is the trade
//!   module's [`greedy_assignment`] ([`greedy_starters`] wraps it).
//! * `playoff_odds` reproduces Python's random draw order exactly: `Random(7)`, per simulated
//!   week a `shuffle` then one `random()` per pair, then one `random()` per team (in team order)
//!   as the third element of the sort key.
//! * `math.erf` is `libm::erf` (the same fdlibm algorithm family).
//! * `Outlook` implements [`OutlookView`] for the need-trade search.

use std::collections::HashMap;

use chrono::Duration;
use fm_core::models::{FantasyTeam, LeagueContext, Player};
use fm_core::provider::LeagueProvider;
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::{Cid, Map};
use fm_pycompat::fmt::{fixed, percent};
use fm_pycompat::num::{py_sum, round_to};
use fm_pycompat::order::cmp_f64;
use fm_pycompat::random::PyRandom;
use serde_json::{json, Value};

use super::luck::{league_weeks, luck_table, LeagueWeeks, TeamLuck};
use super::schedule_grid::{playoff_weeks, OFFNIGHT_THRESHOLD};
pub use super::outlook_values::{age, future_values, future_values_with, FUTURE_YEARS};
use crate::recommend::base::{keeper_k, keeper_set};
use crate::recommend::started::{roster_value, StartWindow};
use crate::recommend::trades::{greedy_assignment, greedy_lineup_value, OutlookView, Pid, SlotSpec};
use crate::valuation::dynasty::ProductionCurves;

pub const SIMS: usize = 2000;
pub const SEED: i64 = 7;
/// a starter's NHL games in a fantasy week (league-average)
pub const GAMES_PER_WEEK: f64 = 3.4;
/// sd of a team's weekly points as a share of its mean
pub const WEEKLY_CV: f64 = 0.20;
pub const CONTEND_ODDS: f64 = 0.55;
pub const REBUILD_ODDS: f64 = 0.15;
pub const LONGSHOT_ODDS: f64 = 0.30;
/// redraft: "pretender" at or below this
pub const PRETENDER_ODDS: f64 = 0.20;
pub const BALANCED_SELL_AGE: f64 = 30.0;
pub const REBUILD_BUY_MAX_AGE: f64 = 25.0;
pub const PRIME_AGES: (f64, f64) = (23.0, 29.0);
pub const LIST_N: usize = 3;
/// fantasy playoffs ~ the NHL season's last 3 weeks
pub const DEFAULT_REG_SEASON_TAIL_DAYS: i64 = 21;

#[derive(Clone, Debug)]
pub struct TeamOutlook<'a> {
    pub team: &'a FantasyTeam,
    pub record: Option<(i64, i64, i64)>,
    pub win_pct: Option<f64>,
    /// rest-of-season FPG: the started-games value in a daily-lineup league, else the best lineup
    pub now: f64,
    /// best lineup FPG 1..FUTURE_YEARS seasons ahead (of the keepers only in a keeper league). It
    /// does not count bench starts, so it is not on the same basis as `now`: compare ranks, not values
    pub future: Vec<f64>,
    pub now_rank: usize,
    pub future_rank: Option<usize>,
    pub playoff_odds: Option<f64>,
    /// recommended mode (dynasty) or contender / bubble / pretender
    pub label: String,
    /// points for, all-play record and expected wins so far (None when the weekly scores are not
    /// available)
    pub luck: Option<TeamLuck>,
}

impl TeamOutlook<'_> {
    pub fn future_avg(&self) -> Option<f64> {
        if self.future.is_empty() {
            None
        } else {
            Some(py_sum(self.future.iter().copied()) / self.future.len() as f64)
        }
    }

    pub fn mine(&self) -> bool {
        self.team.owner_is_me
    }
}

#[derive(Clone, Debug)]
pub struct Asset<'a> {
    pub player: &'a Player,
    pub owner: Option<String>,
    pub now: f64,
    pub future: Option<f64>,
    pub age: Option<f64>,
    pub why: String,
}

#[derive(Clone, Debug)]
pub struct Outlook<'a> {
    /// best now first
    pub teams: Vec<TeamOutlook<'a>>,
    /// index into `teams`
    pub me: Option<usize>,
    pub dynasty: bool,
    /// contend / balanced / rebuild (dynasty) or label (redraft)
    pub recommended: Option<String>,
    /// the mode in use (dynasty)
    pub current: Option<String>,
    pub reasons: Vec<String>,
    pub sell: Vec<Asset<'a>>,
    pub buy: Vec<Asset<'a>>,
    pub weeks_left: i64,
    pub playoff_teams: i64,
    /// my playoff odds if I win this week's matchup and if I lose it
    pub this_week: Option<(f64, f64)>,
    pub notes: Vec<String>,
}

impl Outlook<'_> {
    /// A team's recommended mode / label.
    pub fn direction(&self, team_id: &str) -> Option<&str> {
        self.teams.iter().find(|t| t.team.team_id == team_id).map(|t| t.label.as_str())
    }

    pub fn me(&self) -> Option<&TeamOutlook<'_>> {
        self.me.map(|i| &self.teams[i])
    }
}

impl OutlookView for Outlook<'_> {
    fn recommended(&self) -> Option<&str> {
        self.recommended.as_deref()
    }
    fn direction(&self, team_id: &str) -> Option<&str> {
        Outlook::direction(self, team_id)
    }
}

// --------------------------------------------------------------------------- per-player values

/// `_now(values, p)`: rest-of-season per-game value, floored at 0.
fn now_of(values: &Map<Cid, PlayerValue>, p: &Player) -> f64 {
    values.get(&p.cid).map_or(0.0, |pv| pv.fpg_season.max(0.0))
}

fn lineup(players: &[&Player], vals: &HashMap<&str, f64>, ctx: &LeagueContext) -> f64 {
    greedy_lineup_value(players, |p| vals.get(p.cid.as_str()).copied(), &ctx.roster_shape)
}

/// A team's strength now: its started-games value (`recommend::started`) in a daily-lineup league
/// with the schedule loaded, else the best lineup's sum as in Python.
fn lineup_now(players: &[&Player], vals: &HashMap<&str, f64>, ctx: &LeagueContext, win: Option<&StartWindow>) -> f64 {
    match win {
        Some(w) => roster_value(w, &SlotSpec::new(&ctx.roster_shape), players, |p| vals.get(p.cid.as_str()).copied().unwrap_or(0.0)),
        None => lineup(players, vals, ctx),
    }
}

/// The cids of the best greedy starting lineup (`_greedy_assignment`, season FPG): the trade
/// engine's [`greedy_assignment`] over the players' season values.
fn greedy_starters<'a>(players: &[&'a Player], values: &Map<Cid, PlayerValue>, ctx: &LeagueContext) -> Vec<&'a str> {
    let spec = SlotSpec::new(&ctx.roster_shape);
    let vals: Vec<f64> = players.iter().map(|p| values.get(&p.cid).map_or(0.0, |pv| pv.fpg_season)).collect();
    let elig: Vec<u16> = players.iter().map(|p| spec.elig_mask(p)).collect();
    let ids: Vec<Pid> = (0..players.len() as Pid).collect();
    let held = greedy_assignment(&ids, &spec, &vals, &elig);
    held.iter().flat_map(|h| h.iter().map(|&i| players[i as usize].cid.as_str())).collect()
}

// --------------------------------------------------------------------------- season length

/// (fantasy regular-season weeks left, how it was found).
pub fn weeks_left(ctx: &LeagueContext, provider: Option<&dyn LeagueProvider>) -> (i64, String) {
    weeks_until_playoffs(ctx, provider, false)
}

/// [`weeks_left`] counting the matchup in progress: a part week before the first playoff period is
/// the current matchup, which no record or completed week includes yet. Python floored to whole
/// weeks, which leaves that matchup out of the simulation from Tuesday to Sunday.
pub fn weeks_to_play(ctx: &LeagueContext, provider: Option<&dyn LeagueProvider>) -> (i64, String) {
    weeks_until_playoffs(ctx, provider, true)
}

fn weeks_until_playoffs(ctx: &LeagueContext, provider: Option<&dyn LeagueProvider>, part_week: bool) -> (i64, String) {
    let po = playoff_weeks(ctx, provider, None, OFFNIGHT_THRESHOLD);
    let (start, src) = match po.periods.first() {
        Some(p) => (p.start, "until the league's first playoff period".to_string()),
        None => {
            let last = ctx.schedule.values().flat_map(|ds| ds.iter().copied()).max();
            match last {
                None => return (0, "no schedule loaded".to_string()),
                Some(l) => (l - Duration::days(DEFAULT_REG_SEASON_TAIL_DAYS), "NHL season end minus 3 weeks (assumed)".to_string()),
            }
        }
    };
    let days = (start - ctx.as_of).num_days() + if part_week { 6 } else { 0 };
    (days.div_euclid(7).max(0), src)
}

/// Playoff spots: ESPN `settings.playoff_team_count`, the Fantrax Rules page "Number of teams
/// qualifying for playoffs", else half the league.
pub fn playoff_teams(ctx: &LeagueContext, provider: Option<&dyn LeagueProvider>) -> (i64, String) {
    let n = ctx.teams.len() as i64;
    let mut candidates: Vec<Option<String>> = Vec::new();
    candidates.push(provider.and_then(|p| p.espn_league()).and_then(|l| l.settings.playoff_team_count).map(|v| v.to_string()));
    candidates.push(provider.and_then(|p| p.rules()).and_then(|r| r.get("Number of teams qualifying for playoffs")).map(str::to_string));
    for v in candidates.into_iter().flatten() {
        if v.is_empty() {
            continue;
        }
        if let Ok(k) = v.trim().parse::<i64>() {
            if 0 < k && k <= n {
                return (k, "league setting".to_string());
            }
        }
    }
    (1.max(n / 2), "assumed half the league".to_string())
}

/// team_id -> P(top `spots` after `weeks` more random-opponent weeks) (module docstring of the
/// Python file).
pub fn playoff_odds(rows: &[TeamOutlook<'_>], weeks: i64, spots: i64, sims: usize, seed: i64) -> Map<String, f64> {
    playoff_odds_with(rows, weeks, spots, sims, seed, None)
}

/// [`playoff_odds`] with the first simulated week's result for team `force.0` (an index into
/// `rows`) fixed to a win (`force.1`) or a loss. The random draws are the same as without it, so
/// the two forced runs differ only by that result.
///
/// Teams level on wins are ordered by points for when every row has it: points so far plus the
/// projected weekly mean for each remaining week. Points for is the standings tiebreaker of both
/// leagues (ESPN's `playoffSeedingRule` is `TOTAL_POINTS_SCORED`, and the Fantrax rules page lists
/// "Most fantasy points for entire season" first); head-to-head among tied teams is not modelled
/// because the opponents are random. Python broke ties on projected strength alone, which is
/// still the order when no row has points for.
pub fn playoff_odds_with(
    rows: &[TeamOutlook<'_>],
    weeks: i64,
    spots: i64,
    sims: usize,
    seed: i64,
    force: Option<(usize, bool)>,
) -> Map<String, f64> {
    let mut out: Map<String, f64> = Map::new();
    if rows.is_empty() {
        return out;
    }
    let mut rng = PyRandom::new(seed);
    let n = rows.len();
    let mean: Vec<f64> = rows.iter().map(|r| r.now * GAMES_PER_WEEK).collect();
    let base: Vec<f64> = rows.iter().map(|r| r.record.map_or(0.0, |rec| rec.0 as f64 + 0.5 * rec.2 as f64)).collect();
    let pf_known = rows.iter().all(|r| r.luck.is_some());
    let tie: Vec<f64> = rows
        .iter()
        .zip(&mean)
        .map(|(r, m)| match r.luck {
            Some(l) if pf_known => l.points_for + m * weeks.max(0) as f64,
            _ => *m,
        })
        .collect();
    let p_win = |a: usize, b: usize| -> f64 {
        let h = (WEEKLY_CV * mean[a]).hypot(WEEKLY_CV * mean[b]);
        let sd = if h == 0.0 { 1.0 } else { h };
        0.5 * (1.0 + libm::erf((mean[a] - mean[b]) / (sd * 2.0_f64.sqrt())))
    };
    let mut pw = vec![0.0; n * n];
    for a in 0..n {
        for b in 0..n {
            if a != b {
                pw[a * n + b] = p_win(a, b);
            }
        }
    }
    let mut made = vec![0usize; n];
    let mut wins = vec![0.0f64; n];
    let mut order: Vec<usize> = (0..n).collect();
    let mut keys: Vec<f64> = vec![0.0; n];
    for _ in 0..sims {
        wins.copy_from_slice(&base);
        for w in 0..weeks {
            for (i, o) in order.iter_mut().enumerate() {
                *o = i;
            }
            rng.shuffle(&mut order);
            let forced = force.filter(|_| w == 0);
            let mut i = 0;
            while i + 1 < n {
                let (a, b) = (order[i], order[i + 1]);
                let a_wins = rng.random() < pw[a * n + b];
                let a_wins = match forced {
                    Some((t, won)) if t == a => won,
                    Some((t, won)) if t == b => !won,
                    _ => a_wins,
                };
                if a_wins {
                    wins[a] += 1.0;
                } else {
                    wins[b] += 1.0;
                }
                i += 2;
            }
            // odd league: the forced team may have had the bye
            if let Some((t, won)) = forced {
                if n % 2 == 1 && order[n - 1] == t && won {
                    wins[t] += 1.0;
                }
            }
        }
        // sorted(ids, key=(-wins, -tiebreak, rng.random())): the key is built once per team in list order
        for k in keys.iter_mut() {
            *k = rng.random();
        }
        let mut ranked: Vec<usize> = (0..n).collect();
        ranked.sort_by(|&a, &b| {
            cmp_f64(-wins[a], -wins[b]).then(cmp_f64(-tie[a], -tie[b])).then(cmp_f64(keys[a], keys[b]))
        });
        for &t in ranked.iter().take(spots.max(0) as usize) {
            made[t] += 1;
        }
    }
    for (i, r) in rows.iter().enumerate() {
        out.insert(r.team.team_id.clone(), made[i] as f64 / sims as f64);
    }
    out
}

// --------------------------------------------------------------------------- recommendation

/// (contend / balanced / rebuild, reasons) for my team.
pub fn recommend_mode(me: &TeamOutlook<'_>, n: usize) -> (String, Vec<String>) {
    let odds = me.playoff_odds.unwrap_or(0.0);
    let mut first = format!("Playoff odds this season about {} (lineup #{} of {} right now", percent(odds, 0, false), me.now_rank, n);
    if let Some(r) = me.record {
        first.push_str(&format!(", record {}-{}-{}", r.0, r.1, r.2));
    }
    first.push(')');
    let mut why = vec![first];
    let fr = me.future_rank;
    if let Some(fr) = fr {
        let trend = if fr < me.now_rank {
            "better"
        } else if fr > me.now_rank {
            "worse"
        } else {
            "the same"
        };
        why.push(format!("Your core ranks #{fr} of {n} over the next {FUTURE_YEARS} seasons ({trend} than now)"));
    }
    if odds >= CONTEND_ODDS {
        why.push("Good odds: trade future value for this season's points".to_string());
        return ("contend".to_string(), why);
    }
    if odds <= REBUILD_ODDS {
        why.push("A long shot this season: sell this season's points for younger value".to_string());
        return ("rebuild".to_string(), why);
    }
    if odds <= LONGSHOT_ODDS && fr.is_some_and(|f| f < me.now_rank) {
        why.push("Your window is ahead: build toward it rather than chase this season".to_string());
        return ("rebuild".to_string(), why);
    }
    why.push("On the bubble: only make trades that help both now and later".to_string());
    ("balanced".to_string(), why)
}

pub fn redraft_label(odds: f64) -> &'static str {
    if odds >= CONTEND_ODDS {
        "contender"
    } else if odds <= PRETENDER_ODDS {
        "pretender"
    } else {
        "bubble"
    }
}

fn age_prefix(age: Option<f64>) -> String {
    age.map_or(String::new(), |a| format!("age {}, ", fixed(a, 0, false)))
}

/// Fill `o.sell` / `o.buy` for the recommended direction.
fn assets<'a>(
    o: &mut Outlook<'a>,
    ctx: &'a LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dyn_: Option<&Map<Cid, DynastyValue>>,
    fut: &HashMap<&str, f64>,
) {
    let Ok(me) = ctx.my_team() else { return };
    let mine: Vec<&'a Player> = me.players();
    let starters: Vec<&str> = greedy_starters(&mine, values, ctx);
    let is_starter = |cid: &str| starters.contains(&cid);
    let dyn_nonempty = dyn_.filter(|d| !d.is_empty());
    let k = if dyn_nonempty.is_some() { keeper_k(ctx, dyn_) } else { None };

    let surplus = |p: &Player| dyn_.and_then(|d| d.get(&p.cid)).map_or(0.0, |dv| dv.keeper_surplus);

    // keeper leagues: a later season counts only for a player who would enter my top-K keepers
    let my_keepers: Vec<&Player> = match (k, dyn_) {
        (Some(k), Some(d)) => keeper_set(mine.iter().copied(), d, k),
        _ => vec![],
    };
    let keep_bar = match k {
        Some(k) if my_keepers.len() as i64 >= k => my_keepers.last().map_or(0.0, |p| surplus(p)),
        _ => 0.0,
    };
    let weakest = mine
        .iter()
        .filter(|p| is_starter(&p.cid))
        .map(|p| now_of(values, p))
        .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| if x < m { x } else { m })))
        .unwrap_or(0.0);
    let direction: HashMap<&str, &TeamOutlook<'a>> = o.teams.iter().map(|t| (t.team.team_id.as_str(), t)).collect();
    let rec = o.recommended.clone().unwrap_or_default();
    let rec_some = o.recommended.is_some();
    let rec = rec.as_str();

    let mut sell: Vec<(f64, Asset<'a>)> = Vec::new();
    for &p in &mine {
        let (now, f, ag) = (now_of(values, p), fut.get(p.cid.as_str()).copied(), age(p, ctx, dyn_));
        let dv = dyn_.and_then(|d| d.get(&p.cid)).map(|dv| dv.value);
        if rec_some && rec == "contend" && k.is_some() {
            // keeper league: his long-term value is his keeper surplus
            let s = surplus(p);
            let in_dyn = dyn_.is_some_and(|d| d.contains_key(&p.cid));
            if is_starter(&p.cid) || !in_dyn || s <= 0.0 {
                continue;
            }
            let kept = my_keepers.iter().any(|q| q.cid == p.cid);
            let kk = k.unwrap_or(0);
            sell.push((
                s,
                Asset {
                    player: p,
                    owner: None,
                    now,
                    future: f,
                    age: ag,
                    why: format!(
                        "not in your lineup this season (long-term value {}, {}): turn him into a starter",
                        fixed(s, 1, false),
                        if kept { "one of your keepers".to_string() } else { format!("not one of your top {kk} keepers") }
                    ),
                },
            ));
        } else if rec_some && rec == "contend" {
            let Some(dv) = dv else { continue };
            if is_starter(&p.cid) || dv <= 0.0 {
                continue;
            }
            sell.push((
                dv,
                Asset {
                    player: p,
                    owner: None,
                    now,
                    future: f,
                    age: ag,
                    why: format!("not in your lineup this season (dynasty value {}): turn him into a starter", fixed(dv, 1, false)),
                },
            ));
        } else if let Some(fv) = f.filter(|_| now > 0.0 && (rec == "rebuild" || ag.unwrap_or(0.0) >= BALANCED_SELL_AGE)) {
            let drop = now - fv;
            if drop <= 0.0 {
                continue;
            }
            sell.push((
                drop * now,
                Asset {
                    player: p,
                    owner: None,
                    now,
                    future: f,
                    age: ag,
                    why: format!(
                        "{}{} FPG now, about {} over the next {FUTURE_YEARS} seasons: sell while his value is high",
                        age_prefix(ag),
                        fixed(now, 2, false),
                        fixed(fv, 2, false)
                    ),
                },
            ));
        }
    }
    sell.sort_by(|a, b| cmp_f64(-a.0, -b.0));
    o.sell = sell.into_iter().take(LIST_N).map(|(_, a)| a).collect();

    let mut buy: Vec<(f64, Asset<'a>)> = Vec::new();
    for t in &ctx.teams {
        if t.owner_is_me {
            continue;
        }
        let their = direction.get(t.team_id.as_str()).copied();
        for p in t.players() {
            if matches!(p.status.as_str(), "ir" | "ltir" | "out" | "suspended") {
                continue;
            }
            let (now, f, ag) = (now_of(values, p), fut.get(p.cid.as_str()).copied(), age(p, ctx, dyn_));
            let (score, why);
            if rec_some && (rec == "contend" || rec == "contender") {
                let gain = now - weakest;
                if gain <= 0.0 {
                    continue;
                }
                let seller = their.is_some_and(|t| t.label == "rebuild" || t.label == "pretender");
                score = gain * if seller { 1.2 } else { 1.0 };
                why = format!(
                    "{} FPG now, +{} over your weakest starter{}",
                    fixed(now, 2, false),
                    fixed(gain, 2, false),
                    if seller { format!("; {} is a likely seller", t.name) } else { String::new() }
                );
            } else if rec_some && rec == "rebuild" {
                let Some(fv) = f else { continue };
                if fv <= now || ag.is_some_and(|a| a > REBUILD_BUY_MAX_AGE) {
                    continue;
                }
                if k.is_some() && surplus(p) <= keep_bar {
                    continue; // keeper league: would not be one of my keepers
                }
                let buyer = their.is_some_and(|t| t.label == "contend");
                score = fv * if buyer { 1.2 } else { 1.0 };
                why = format!(
                    "{}{} FPG now rising to about {}{}",
                    age_prefix(ag),
                    fixed(now, 2, false),
                    fixed(fv, 2, false),
                    if buyer { format!("; {} is contending and may pay in vets", t.name) } else { String::new() }
                );
            } else if rec_some && rec == "balanced" {
                let (Some(fv), Some(a)) = (f, ag) else { continue };
                if !(PRIME_AGES.0 <= a && a <= PRIME_AGES.1) {
                    continue;
                }
                if k.is_some() && surplus(p) <= keep_bar {
                    continue; // keeper league: his later value would not be mine to keep
                }
                score = now.min(fv);
                why = format!(
                    "age {}, {} FPG now and about {} later: value both ways",
                    fixed(a, 0, false),
                    fixed(now, 2, false),
                    fixed(fv, 2, false)
                );
            } else {
                continue;
            }
            buy.push((score, Asset { player: p, owner: Some(t.name.clone()), now, future: f, age: ag, why }));
        }
    }
    buy.sort_by(|a, b| cmp_f64(-a.0, -b.0));
    o.buy = buy.into_iter().take(LIST_N).map(|(_, a)| a).collect();
}

/// Every team's this-season and future lineup strength, playoff odds, and my recommended mode
/// with sell / buy lists. With a provider the league's completed weekly scores are read for points
/// for and the all-play record ([`super::luck`]); a failure there is a note, not an error.
pub fn build_outlook<'a>(
    ctx: &'a LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    provider: Option<&dyn LeagueProvider>,
    sims: usize,
) -> Outlook<'a> {
    let (weeks, err) = match provider.map(|p| league_weeks(ctx, p)) {
        Some(Ok(w)) => (w, None),
        Some(Err(e)) => (None, Some(format!("Points for and all-play record unavailable: {e}"))),
        None => (None, None),
    };
    let mut o = build_outlook_with(ctx, values, dynasty_values, provider, sims, weeks.as_ref());
    o.notes.extend(err);
    o
}

/// [`build_outlook`] with the completed weekly scores given. With them the simulation also plays
/// the matchup in progress ([`weeks_to_play`]); None gives Python's outlook: no points for or
/// all-play, whole weeks left only, and ties in the simulated standings on projected strength.
pub fn build_outlook_with<'a>(
    ctx: &'a LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    provider: Option<&dyn LeagueProvider>,
    sims: usize,
    league_weeks: Option<&LeagueWeeks>,
) -> Outlook<'a> {
    let dyn_: Option<&Map<Cid, DynastyValue>> = if ctx.dynasty { dynasty_values } else { None };
    let k = if dyn_.is_some_and(|d| !d.is_empty()) { keeper_k(ctx, dyn_) } else { None };
    // keeper leagues: next season a team has only its top-K keepers (the rest are redrafted)
    let core: Vec<Vec<&Player>> = ctx
        .teams
        .iter()
        .map(|t| match (k, dyn_) {
            (Some(k), Some(d)) => keeper_set(t.players_iter(), d, k),
            _ => t.players(),
        })
        .collect();
    let mut now_vals: HashMap<&str, f64> = HashMap::new();
    for t in &ctx.teams {
        for p in t.players_iter() {
            now_vals.insert(p.cid.as_str(), now_of(values, p));
        }
    }
    let mut fut_by_year: Vec<HashMap<&str, f64>> = vec![HashMap::new(); FUTURE_YEARS];
    let mut fut_avg: HashMap<&str, f64> = HashMap::new();
    if dyn_.is_some() {
        let curves = ProductionCurves::current();
        for t in &ctx.teams {
            for p in t.players_iter() {
                let fv = future_values_with(&curves, p, ctx, values, dyn_, None);
                for (y, v) in fv.iter().enumerate() {
                    fut_by_year[y].insert(p.cid.as_str(), *v);
                }
                fut_avg.insert(p.cid.as_str(), py_sum(fv.iter().copied()) / fv.len() as f64);
            }
        }
    }
    let win = StartWindow::of(ctx);
    let team_ids: Vec<&str> = ctx.teams.iter().map(|t| t.team_id.as_str()).collect();
    let luck = league_weeks.filter(|w| !w.weeks.is_empty()).map(|w| luck_table(&w.weeks, &team_ids));
    let mut rows: Vec<TeamOutlook<'a>> = Vec::new();
    for (ti, t) in ctx.teams.iter().enumerate() {
        let rec = t.record;
        let gp = rec.map_or(0, |r| r.0 + r.1 + r.2);
        let players = t.players();
        rows.push(TeamOutlook {
            team: t,
            record: rec,
            win_pct: match rec {
                Some(r) if gp != 0 => Some((r.0 as f64 + 0.5 * r.2 as f64) / gp as f64),
                _ => None,
            },
            now: lineup_now(&players, &now_vals, ctx, win.as_ref()),
            future: if dyn_.is_some() { (0..FUTURE_YEARS).map(|y| lineup(&core[ti], &fut_by_year[y], ctx)).collect() } else { vec![] },
            now_rank: 0,
            future_rank: None,
            playoff_odds: None,
            label: String::new(),
            luck: luck.as_ref().and_then(|l| l.get(&t.team_id)).copied(),
        });
    }
    let mut by_now: Vec<usize> = (0..rows.len()).collect();
    by_now.sort_by(|&a, &b| cmp_f64(-rows[a].now, -rows[b].now));
    for (i, &r) in by_now.iter().enumerate() {
        rows[r].now_rank = i + 1;
    }
    if dyn_.is_some() {
        let mut by_future: Vec<usize> = (0..rows.len()).collect();
        by_future.sort_by(|&a, &b| cmp_f64(-rows[a].future_avg().unwrap_or(0.0), -rows[b].future_avg().unwrap_or(0.0)));
        for (i, &r) in by_future.iter().enumerate() {
            rows[r].future_rank = Some(i + 1);
        }
    }
    // with the season's results the matchup in progress is simulated too, so "this week" is that matchup
    let (weeks, wsrc) = if league_weeks.is_some() { weeks_to_play(ctx, provider) } else { weeks_left(ctx, provider) };
    let (spots, ssrc) = playoff_teams(ctx, provider);
    let odds = playoff_odds(&rows, weeks, spots, sims, SEED);
    // the leverage of this week's matchup: my odds with its result fixed either way
    let this_week = match rows.iter().position(|r| r.mine()) {
        Some(mi) if weeks >= 1 => {
            let id = &rows[mi].team.team_id;
            let won = playoff_odds_with(&rows, weeks, spots, sims, SEED, Some((mi, true))).get(id).copied();
            let lost = playoff_odds_with(&rows, weeks, spots, sims, SEED, Some((mi, false))).get(id).copied();
            won.zip(lost)
        }
        _ => None,
    };
    let n = rows.len();
    for r in rows.iter_mut() {
        r.playoff_odds = odds.get(&r.team.team_id).copied();
        r.label = if dyn_.is_some() { recommend_mode(r, n).0 } else { redraft_label(r.playoff_odds.unwrap_or(0.0)).to_string() };
    }
    rows.sort_by_key(|r| r.now_rank);
    let me = rows.iter().position(|r| r.mine());
    let mut out = Outlook {
        teams: rows,
        me,
        dynasty: dyn_.is_some(),
        recommended: None,
        current: if ctx.dynasty { Some(ctx.dynasty_mode.as_str().to_string()) } else { None },
        reasons: vec![],
        sell: vec![],
        buy: vec![],
        weeks_left: weeks,
        playoff_teams: spots,
        this_week,
        notes: vec![],
    };
    out.notes = vec![format!(
        "Playoff odds: {weeks} regular-season weeks left ({wsrc}), {spots} playoff spots ({ssrc}); random opponents, {sims} simulations (approximate)"
    )];
    if let Some(w) = league_weeks.filter(|w| !w.weeks.is_empty()) {
        out.notes.push(format!(
            "Points for and all-play record: {} completed regular-season week{} ({}); ties in the simulated standings go to the team with more points for (so far plus projected)",
            w.weeks.len(),
            if w.weeks.len() != 1 { "s" } else { "" },
            w.source
        ));
    }
    if let Some(k) = k {
        out.notes.push(format!(
            "Keeper league: next seasons count only each team's top {k} keeper{} (everyone else is redrafted), so the future lineups hold just those players",
            if k != 1 { "s" } else { "" }
        ));
    }
    if let Some(mi) = me {
        if dyn_.is_some() {
            let (rec, reasons) = recommend_mode(&out.teams[mi], n);
            out.recommended = Some(rec);
            out.reasons = reasons;
        } else {
            let m = &out.teams[mi];
            let odds_me = m.playoff_odds.unwrap_or(0.0);
            let tail = match m.label.as_str() {
                "contender" => "Buy this season's points: every week counts",
                "bubble" => "Make moves that raise your weekly floor; avoid long shots",
                _ => "Chase upside: hot streaks and high-variance adds",
            };
            out.reasons = vec![
                format!("Playoff odds about {} (lineup #{} of {})", percent(odds_me, 0, false), m.now_rank, n),
                tail.to_string(),
            ];
            out.recommended = Some(m.label.clone());
        }
        let extra = season_lines(&out.teams[mi], n, out.this_week);
        out.reasons.splice(1..1, extra);
        assets(&mut out, ctx, values, dyn_, &fut_avg);
    }
    out
}

/// The reasons that put my record in context: points-for rank and all-play record beside the real
/// record, and how far this week's result moves my playoff odds.
fn season_lines(me: &TeamOutlook<'_>, n: usize, this_week: Option<(f64, f64)>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(l) = me.luck {
        let (w, lo, t) = l.all_play;
        let actual = me.record.map_or(String::new(), |r| {
            format!(" against {} actual (record {}-{}-{})", fixed(r.0 as f64 + 0.5 * r.2 as f64, 1, false), r.0, r.1, r.2)
        });
        out.push(format!(
            "Points for #{} of {n} ({}); all-play record {w}-{lo}-{t}, about {} expected wins{actual}",
            l.pf_rank,
            fixed(l.points_for, 1, false),
            fixed(l.expected_wins, 1, false)
        ));
    }
    if let Some((won, lost)) = this_week {
        out.push(format!(
            "This week's matchup: a win puts your playoff odds at about {}, a loss at about {}",
            percent(won, 0, false),
            percent(lost, 0, false)
        ));
    }
    out
}

/// "recommended: rebuild (currently balanced)" / "contender".
pub fn recommendation_text(o: &Outlook<'_>) -> String {
    let Some(rec) = &o.recommended else {
        return "no recommendation (your team was not found)".to_string();
    };
    if !o.dynasty {
        return rec.clone();
    }
    let cur = o.current.as_ref().map_or(String::new(), |c| format!(" (currently {c})"));
    let same = if o.current.as_ref() == Some(rec) { " - matches your setting" } else { "" };
    format!("recommended: {rec}{cur}{same}")
}

fn opt3(x: Option<f64>) -> Value {
    x.map_or(Value::Null, |v| json!(round_to(v, 3)))
}

/// `outlook_json(o)`.
pub fn outlook_json(o: &Outlook<'_>) -> Value {
    let asset = |a: &Asset<'_>| {
        json!({"cid": a.player.cid, "name": a.player.name, "owner": a.owner, "now_fpg": round_to(a.now, 3),
               "future_fpg": opt3(a.future), "age": a.age.map_or(Value::Null, |v| json!(round_to(v, 1))), "why": a.why})
    };
    let teams: Vec<Value> = o
        .teams
        .iter()
        .map(|r| {
            json!({"team_id": r.team.team_id, "name": r.team.name, "mine": r.mine(),
                   "record": r.record.map_or(Value::Null, |x| json!([x.0, x.1, x.2])),
                   "now_fpg": round_to(r.now, 3), "now_rank": r.now_rank,
                   "future_fpg": r.future.iter().map(|x| round_to(*x, 3)).collect::<Vec<_>>(),
                   "future_rank": r.future_rank, "playoff_odds": opt3(r.playoff_odds), "label": r.label,
                   "points_for": r.luck.map(|l| round_to(l.points_for, 1)), "points_for_rank": r.luck.map(|l| l.pf_rank),
                   "all_play": r.luck.map(|l| [l.all_play.0, l.all_play.1, l.all_play.2]),
                   "expected_wins": r.luck.map(|l| round_to(l.expected_wins, 2))})
        })
        .collect();
    json!({"recommended": o.recommended, "current": o.current, "dynasty": o.dynasty,
           "text": recommendation_text(o), "reasons": o.reasons, "weeks_left": o.weeks_left,
           "playoff_teams": o.playoff_teams, "notes": o.notes, "teams": teams,
           "this_week": o.this_week.map(|(w, l)| json!({"if_win": round_to(w, 3), "if_loss": round_to(l, 3)})),
           "sell": o.sell.iter().map(asset).collect::<Vec<_>>(), "buy": o.buy.iter().map(asset).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(id: &str, mine: bool) -> FantasyTeam {
        FantasyTeam { team_id: id.to_string(), name: format!("T{id}"), owner_is_me: mine, slots: vec![], record: Some((0, 0, 0)) }
    }

    fn row<'a>(t: &'a FantasyTeam, now: f64) -> TeamOutlook<'a> {
        TeamOutlook { team: t, record: t.record, win_pct: None, now, future: vec![], now_rank: 1, future_rank: None, playoff_odds: None, label: String::new(), luck: None }
    }

    #[test]
    fn odds_follow_strength() {
        let ts: Vec<FantasyTeam> = (0..8).map(|i| team(&i.to_string(), i == 0)).collect();
        let rows: Vec<TeamOutlook> = ts.iter().enumerate().map(|(i, t)| row(t, 20.0 + 3.0 * (7 - i) as f64)).collect();
        let odds = playoff_odds(&rows, 10, 4, 500, SEED);
        assert!(odds["0"] > 0.9, "{odds:?}");
        assert!(odds["7"] < 0.1, "{odds:?}");
        // deterministic with the seed
        assert_eq!(odds, playoff_odds(&rows, 10, 4, 500, SEED));
    }

    fn luck(points_for: f64) -> Option<TeamLuck> {
        Some(TeamLuck { points_for, pf_rank: 0, all_play: (0, 0, 0), expected_wins: 0.0 })
    }

    /// No weeks left, so the standings are the records. B (2-2, 30 FPG) and C (2-2, 25 FPG) tie
    /// for the second of two spots. On projected strength (Python) B gets it; with points for 400
    /// for B and 500 for C, C does.
    #[test]
    fn ties_go_to_points_for() {
        let mk = |id: &str, rec: (i64, i64, i64)| FantasyTeam { record: Some(rec), ..team(id, false) };
        let ts = [mk("A", (3, 1, 0)), mk("B", (2, 2, 0)), mk("C", (2, 2, 0)), mk("D", (1, 3, 0))];
        let mut rows: Vec<TeamOutlook> = ts.iter().zip([20.0, 30.0, 25.0, 20.0]).map(|(t, now)| row(t, now)).collect();
        let odds = playoff_odds(&rows, 0, 2, 50, SEED);
        assert_eq!((odds["A"], odds["B"], odds["C"]), (1.0, 1.0, 0.0));
        for (r, pf) in rows.iter_mut().zip([450.0, 400.0, 500.0, 300.0]) {
            r.luck = luck(pf);
        }
        let odds = playoff_odds(&rows, 0, 2, 50, SEED);
        assert_eq!((odds["A"], odds["B"], odds["C"]), (1.0, 0.0, 1.0));
    }

    /// Two equal teams, one week left, one spot: the forced result decides it.
    #[test]
    fn forced_result_this_week() {
        let ts = [team("A", true), team("B", false)];
        let rows: Vec<TeamOutlook> = ts.iter().map(|t| row(t, 20.0)).collect();
        let won = playoff_odds_with(&rows, 1, 1, 200, SEED, Some((0, true)));
        let lost = playoff_odds_with(&rows, 1, 1, 200, SEED, Some((0, false)));
        assert_eq!((won["A"], won["B"]), (1.0, 0.0));
        assert_eq!((lost["A"], lost["B"]), (0.0, 1.0));
        let free = playoff_odds(&rows, 1, 1, 200, SEED);
        assert!(free["A"] > 0.3 && free["A"] < 0.7, "{free:?}");
    }

    /// With no league calendar the default Mon-Sun calendar puts the first playoff period on Mon
    /// 2027-04-12. Through the week of Mon 03-22 the matchup in progress is one of three left to
    /// play every day; whole weeks (Python) count it only on the Monday.
    #[test]
    fn weeks_to_play_counts_the_part_week() {
        let mut ctx: LeagueContext = serde_json::from_value(serde_json::json!({
            "provider": "test", "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {"G": 1.0}}, "roster_shape": {"C": 12},
            "teams": [], "free_agents": [], "matchup_period": 1, "as_of": "2027-03-22",
            "schedule": {"BOS": ["2027-03-01", "2027-04-26"]},
        }))
        .unwrap();
        for day in 0..7 {
            ctx.as_of = chrono::NaiveDate::from_ymd_opt(2027, 3, 22 + day).unwrap();
            assert_eq!(weeks_to_play(&ctx, None).0, 3, "{}", ctx.as_of);
            assert_eq!(weeks_left(&ctx, None).0, if day == 0 { 3 } else { 2 }, "{}", ctx.as_of);
        }
    }

    #[test]
    fn season_lines_text() {
        let t = FantasyTeam { record: Some((1, 5, 0)), ..team("a", true) };
        let mut r = row(&t, 20.0);
        r.luck = Some(TeamLuck { points_for: 612.34, pf_rank: 4, all_play: (40, 38, 0), expected_wins: 3.08 });
        let lines = season_lines(&r, 14, Some((0.312, 0.181)));
        assert_eq!(lines[0], "Points for #4 of 14 (612.3); all-play record 40-38-0, about 3.1 expected wins against 1.0 actual (record 1-5-0)");
        assert_eq!(lines[1], "This week's matchup: a win puts your playoff odds at about 31%, a loss at about 18%");
        r.luck = None;
        assert_eq!(season_lines(&r, 14, None), Vec::<String>::new());
    }

    #[test]
    fn mode_thresholds() {
        let t = team("a", true);
        let mut r = row(&t, 20.0);
        r.now_rank = 3;
        r.future_rank = Some(8);
        r.playoff_odds = Some(0.7);
        assert_eq!(recommend_mode(&r, 12).0, "contend");
        r.playoff_odds = Some(0.10);
        assert_eq!(recommend_mode(&r, 12).0, "rebuild");
        r.playoff_odds = Some(0.5);
        assert_eq!(recommend_mode(&r, 12).0, "balanced");
        r.playoff_odds = Some(0.25);
        r.future_rank = Some(1);
        assert_eq!(recommend_mode(&r, 12).0, "rebuild"); // window ahead
        assert_eq!(redraft_label(0.6), "contender");
        assert_eq!(redraft_label(0.1), "pretender");
        assert_eq!(redraft_label(0.3), "bubble");
    }

    #[test]
    fn reasons_text() {
        let t = team("a", true);
        let mut r = row(&t, 20.0);
        r.playoff_odds = Some(0.7);
        let (_, why) = recommend_mode(&r, 12);
        assert_eq!(why[0], "Playoff odds this season about 70% (lineup #1 of 12 right now, record 0-0-0)");
    }
}
