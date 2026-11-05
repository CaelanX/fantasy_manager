//! Rust port of the parts of `fantasy_manager/analysis/goalie_plan.py` that `valuation.schedule`
//! needs (moved here to break the analysis <-> valuation cycle): `team_gf_factors`,
//! `team_ga_factors`, `_shrunk_team_factors`, `second_night_prob`, `_odds_fn` and the goalie-plan
//! constants they use. `analysis::goalie_plan` (W6) re-exports them.
//! Owner: work package `W3-valuation-schedule` (wave 3).
//!
//! Deviation: `_odds_fn(None)` read the odds archive of the settings data dir; here the data dir is
//! explicit (`odds_fn(None)` is a source without odds, every day `{}`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::NaiveDate;
use fm_core::models::LeagueContext;
use fm_core::normalize::normalize_team;
use fm_core::Map;
use fm_pycompat::num::py_sum;
use fm_sources::odds::{team_context, TeamOddsContext, LOOKBACK_DAYS};

use crate::valuation::schedule::{BACKUP_MIN_SHARE, B2B_SECOND_NIGHT_P, STARTER_SHARE};

/// a #1 who sat the first night of a back-to-back starts the second
pub const RESTED_STARTER_P: f64 = 0.85;
/// a backup who started the first night starts the second again
pub const BACKUP_REPEAT_P: f64 = 0.15;
/// games of shrinkage of an opponent's goals per game toward the average
pub const GF_SHRINK_K: f64 = 10.0;
/// fewer team games than this: neutral opponent
pub const GF_MIN_GAMES: i64 = 3;
/// the pool's goalies must cover this share of a team's games for its GA rate
pub const GA_MIN_COVERAGE: f64 = 0.8;

/// `{team: (factor shrunk toward 1.0, team games)}` (`team_gf_factors` / `team_ga_factors`).
pub type TeamFactors = Map<String, (f64, i64)>;

/// One day's betting-market context per team (`providers.odds.team_context` output).
pub type DayOdds = Map<String, TeamOddsContext>;

/// `OddsFn = Callable[[date], Mapping[str, Mapping[str, Any]]]`: a day -> that day's team context.
/// Errors are the source's business (Python's callers caught them and used `{}`).
pub type OddsFn = Arc<dyn Fn(NaiveDate) -> DayOdds + Send + Sync>;

/// `_odds_fn(data_dir)`: `team_context` over the odds archive in `<data_dir>/archive`, cached per
/// day. `None` (no data dir known) gives a source without odds.
pub fn odds_fn(data_dir: Option<PathBuf>) -> OddsFn {
    let cache: Mutex<HashMap<NaiveDate, DayOdds>> = Mutex::new(HashMap::new());
    Arc::new(move |d: NaiveDate| -> DayOdds {
        if let Some(hit) = cache.lock().ok().and_then(|c| c.get(&d).cloned()) {
            return hit;
        }
        let got = match &data_dir {
            Some(dir) => team_context(d, dir, LOOKBACK_DAYS),
            None => Map::new(),
        };
        if let Ok(mut c) = cache.lock() {
            c.insert(d, got.clone());
        }
        got
    })
}

/// Games each team played before `ctx.as_of` (`sum(1 for d in ds if d < ctx.as_of)`).
fn played_before(ctx: &LeagueContext) -> HashMap<&str, i64> {
    ctx.schedule
        .iter()
        .map(|(t, ds)| (t.as_str(), ds.iter().filter(|d| **d < ctx.as_of).count() as i64))
        .collect()
}

/// `_shrunk_team_factors(ctx, totals)`: {team: (total per game / league average, shrunk toward 1.0
/// with `GF_SHRINK_K` games; team games)}. Teams with fewer than `GF_MIN_GAMES` games are left out.
pub fn shrunk_team_factors(ctx: &LeagueContext, totals: &Map<String, f64>) -> TeamFactors {
    let played = played_before(ctx);
    let mut rates: Vec<(&String, f64, i64)> = Vec::new();
    for (t, g) in totals {
        let n = played.get(t.as_str()).copied().unwrap_or(0);
        if n >= GF_MIN_GAMES {
            rates.push((t, g / n as f64, n));
        }
    }
    if rates.is_empty() {
        return Map::new();
    }
    let avg = py_sum(rates.iter().map(|(_, r, _)| *r)) / rates.len() as f64;
    if avg <= 0.0 {
        return Map::new();
    }
    rates
        .into_iter()
        .map(|(t, r, n)| (t.clone(), ((n as f64 * (r / avg) + GF_SHRINK_K) / (n as f64 + GF_SHRINK_K), n)))
        .collect()
}

/// `team_gf_factors(ctx)`: {team: (goals per game / league average, shrunk toward 1.0; team games)}
/// from the league's skater season lines and the NHL schedule's games played before today.
pub fn team_gf_factors(ctx: &LeagueContext) -> TeamFactors {
    let mut goals: Map<String, f64> = Map::new();
    for p in ctx.all_players() {
        let line = p.lines.get("season");
        let team = normalize_team(p.team.as_deref());
        let (Some(line), Some(team)) = (line, team) else { continue };
        if p.is_goalie() {
            continue;
        }
        let g = line.stats.get("G").copied().unwrap_or(0.0);
        let e = goals.entry(team).or_insert(0.0);
        *e += g;
    }
    shrunk_team_factors(ctx, &goals)
}

/// `team_ga_factors(ctx)`: {team: (goals against per game / league average, shrunk like
/// [`team_gf_factors`]; team games)} from the league's goalie season lines (GA). A team whose
/// goalies in the pool played fewer than `GA_MIN_COVERAGE` of its games is left out.
pub fn team_ga_factors(ctx: &LeagueContext) -> TeamFactors {
    let mut ga: Map<String, f64> = Map::new();
    let mut gp: HashMap<String, f64> = HashMap::new();
    for p in ctx.all_players() {
        let line = p.lines.get("season");
        let team = normalize_team(p.team.as_deref());
        let (Some(line), Some(team)) = (line, team) else { continue };
        if !p.is_goalie() || !line.stats.contains_key("GA") {
            continue;
        }
        *ga.entry(team.clone()).or_insert(0.0) += line.stats.get("GA").copied().unwrap_or(0.0);
        *gp.entry(team).or_insert(0.0) += line.gp as f64;
    }
    let played = played_before(ctx);
    let ga: Map<String, f64> = ga
        .into_iter()
        .filter(|(t, _)| {
            let n = played.get(t.as_str()).copied().unwrap_or(0);
            n != 0 && gp.get(t).copied().unwrap_or(0.0) >= GA_MIN_COVERAGE * n as f64
        })
        .collect();
    shrunk_team_factors(ctx, &ga)
}

/// `second_night_prob(share, q, b2b_p)`: P(start) on a back-to-back's second night when the first
/// night's start probability `q` is known: a #1 (share >= 0.5) who starts the first night starts
/// again with `b2b_p`, one who rests with `RESTED_STARTER_P`; a backup (share >= 0.25) who starts
/// the first night repeats with `BACKUP_REPEAT_P`, else `1 - b2b_p`. A third goalie keeps his share.
pub fn second_night_prob(share: f64, q: f64, b2b_p: f64) -> f64 {
    if share >= STARTER_SHARE {
        return q * b2b_p + (1.0 - q) * RESTED_STARTER_P;
    }
    if share >= BACKUP_MIN_SHARE {
        return q * BACKUP_REPEAT_P + (1.0 - q) * (1.0 - b2b_p);
    }
    share
}

/// [`second_night_prob`] with the default `b2b_p` (`B2B_SECOND_NIGHT_P`).
pub fn second_night_prob_default(share: f64, q: f64) -> f64 {
    second_night_prob(share, q, B2B_SECOND_NIGHT_P)
}
