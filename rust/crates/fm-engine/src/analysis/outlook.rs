//! Rust port of `fantasy_manager/analysis/outlook.py`: contention window and mode advisor
//! (`fm outlook`, the dashboard's Outlook section). Owner: work package `W8-outlook-playoffs-calib`.
//!
//! * `future_values` / `age` live in [`super::outlook_values`] (W7-trades) and are re-exported.
//! * `_lineup` calls [`crate::recommend::trades::greedy_lineup_value`].
//! * Python's `_greedy_assignment` (the set of starters, used by the sell/buy lists) is not
//!   exported by the trade module, so [`greedy_starters`] is a local line-by-line copy of it.
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

use super::schedule_grid::{playoff_weeks, OFFNIGHT_THRESHOLD};
pub use super::outlook_values::{age, future_values, future_values_with, FUTURE_YEARS};
use crate::recommend::base::{keeper_k, keeper_set};
use crate::recommend::trades::{eligible, greedy_lineup_value, starting_slots, OutlookView};
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
    /// best lineup, rest-of-season FPG
    pub now: f64,
    /// best lineup FPG 1..FUTURE_YEARS seasons ahead
    pub future: Vec<f64>,
    pub now_rank: usize,
    pub future_rank: Option<usize>,
    pub playoff_odds: Option<f64>,
    /// recommended mode (dynasty) or contender / bubble / pretender
    pub label: String,
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

/// The cids of the best greedy starting lineup (`_greedy_assignment`, season FPG): greedy by value
/// with an augmenting-path feasibility check over slot types.
fn greedy_starters<'a>(players: &[&'a Player], values: &Map<Cid, PlayerValue>, ctx: &LeagueContext) -> Vec<&'a str> {
    let slots = starting_slots(&ctx.roster_shape);
    let mut types: Vec<&str> = Vec::new();
    for s in &slots {
        if !types.contains(&s.as_str()) {
            types.push(s.as_str());
        }
    }
    let cap: Vec<usize> = types.iter().map(|t| slots.iter().filter(|s| s.as_str() == *t).count()).collect();
    let elig: Vec<Vec<usize>> = players
        .iter()
        .map(|p| (0..types.len()).filter(|&t| eligible(&p.positions, types[t])).collect())
        .collect();
    let vals: Vec<f64> = players.iter().map(|p| values.get(&p.cid).map_or(0.0, |pv| pv.fpg_season)).collect();
    let mut held: Vec<Vec<usize>> = vec![vec![]; types.len()];

    fn augment(p: usize, seen: &mut Vec<bool>, held: &mut Vec<Vec<usize>>, cap: &[usize], elig: &[Vec<usize>]) -> bool {
        let ts: Vec<usize> = elig[p].iter().copied().filter(|&t| !seen[t]).collect();
        for &t in &ts {
            if held[t].len() < cap[t] {
                held[t].push(p);
                return true;
            }
        }
        for &t in &ts {
            if seen[t] {
                continue;
            }
            seen[t] = true;
            let mut i = 0;
            while i < held[t].len() {
                let q = held[t][i];
                if augment(q, seen, held, cap, elig) {
                    held[t][i] = p;
                    return true;
                }
                i += 1;
            }
        }
        false
    }

    let mut order: Vec<usize> = (0..players.len()).collect();
    order.sort_by(|&a, &b| cmp_f64(vals[b], vals[a]));
    let mut dead = vec![false; types.len()];
    let mut filled = 0usize;
    for p in order {
        if filled >= slots.len() || vals[p] <= 0.0 {
            break;
        }
        if elig[p].iter().all(|&t| dead[t]) {
            continue;
        }
        let mut seen = dead.clone();
        if augment(p, &mut seen, &mut held, &cap, &elig) {
            filled += 1;
        } else {
            dead = seen;
        }
    }
    held.iter().flat_map(|h| h.iter().map(|&i| players[i].cid.as_str())).collect()
}

// --------------------------------------------------------------------------- season length

/// (fantasy regular-season weeks left, how it was found).
pub fn weeks_left(ctx: &LeagueContext, provider: Option<&dyn LeagueProvider>) -> (i64, String) {
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
    (((start - ctx.as_of).num_days().div_euclid(7)).max(0), src)
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
    let mut out: Map<String, f64> = Map::new();
    if rows.is_empty() {
        return out;
    }
    let mut rng = PyRandom::new(seed);
    let n = rows.len();
    let mean: Vec<f64> = rows.iter().map(|r| r.now * GAMES_PER_WEEK).collect();
    let base: Vec<f64> = rows.iter().map(|r| r.record.map_or(0.0, |rec| rec.0 as f64 + 0.5 * rec.2 as f64)).collect();
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
        for _w in 0..weeks {
            for (i, o) in order.iter_mut().enumerate() {
                *o = i;
            }
            rng.shuffle(&mut order);
            let mut i = 0;
            while i + 1 < n {
                let (a, b) = (order[i], order[i + 1]);
                if rng.random() < pw[a * n + b] {
                    wins[a] += 1.0;
                } else {
                    wins[b] += 1.0;
                }
                i += 2;
            }
        }
        // sorted(ids, key=(-wins, -mean, rng.random())): the key is built once per team in list order
        for k in keys.iter_mut() {
            *k = rng.random();
        }
        let mut ranked: Vec<usize> = (0..n).collect();
        ranked.sort_by(|&a, &b| {
            cmp_f64(-wins[a], -wins[b]).then(cmp_f64(-mean[a], -mean[b])).then(cmp_f64(keys[a], keys[b]))
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
/// with sell / buy lists.
pub fn build_outlook<'a>(
    ctx: &'a LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    provider: Option<&dyn LeagueProvider>,
    sims: usize,
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
            now: lineup(&players, &now_vals, ctx),
            future: if dyn_.is_some() { (0..FUTURE_YEARS).map(|y| lineup(&core[ti], &fut_by_year[y], ctx)).collect() } else { vec![] },
            now_rank: 0,
            future_rank: None,
            playoff_odds: None,
            label: String::new(),
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
    let (weeks, wsrc) = weeks_left(ctx, provider);
    let (spots, ssrc) = playoff_teams(ctx, provider);
    let odds = playoff_odds(&rows, weeks, spots, sims, SEED);
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
        notes: vec![],
    };
    out.notes = vec![format!(
        "Playoff odds: {weeks} regular-season weeks left ({wsrc}), {spots} playoff spots ({ssrc}); random opponents, {sims} simulations (approximate)"
    )];
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
        assets(&mut out, ctx, values, dyn_, &fut_avg);
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
                   "future_rank": r.future_rank, "playoff_odds": opt3(r.playoff_odds), "label": r.label})
        })
        .collect();
    json!({"recommended": o.recommended, "current": o.current, "dynasty": o.dynasty,
           "text": recommendation_text(o), "reasons": o.reasons, "weeks_left": o.weeks_left,
           "playoff_teams": o.playoff_teams, "notes": o.notes, "teams": teams,
           "sell": o.sell.iter().map(asset).collect::<Vec<_>>(), "buy": o.buy.iter().map(asset).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(id: &str, mine: bool) -> FantasyTeam {
        FantasyTeam { team_id: id.to_string(), name: format!("T{id}"), owner_is_me: mine, slots: vec![], record: Some((0, 0, 0)) }
    }

    fn row<'a>(t: &'a FantasyTeam, now: f64) -> TeamOutlook<'a> {
        TeamOutlook { team: t, record: t.record, win_pct: None, now, future: vec![], now_rank: 1, future_rank: None, playoff_odds: None, label: String::new() }
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
