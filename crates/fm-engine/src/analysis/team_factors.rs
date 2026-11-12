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
/// starts of shrinkage of a team's goalie wins per start toward the league average. A team's
/// first 10 games' points explain 14.2% of its final points (04-projection-and-regression.md,
/// SI 2015). Taking that R-squared as the reliability of a 10-game sample, the weight on 10 games
/// is 0.142 = 10 / (10 + k), so k = 10 x (1 - 0.142) / 0.142 = 60.4, rounded to 60 starts.
pub const GOALIE_WIN_SHRINK_K: f64 = 60.0;
/// starts of shrinkage of a team's goalie shots against per start toward the league average. A
/// team's 10-game shot differential explains 28.9% of its final points (04, SI 2015), so by the
/// same rule k = 10 x (1 - 0.289) / 0.289 = 24.6, rounded to 25 starts.
pub const GOALIE_SHOTS_SHRINK_K: f64 = 25.0;

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

/// A team's goalie wins and shots against per goalie start this season, shrunk toward the league
/// average, and the goalie starts (`starts`) behind them. Not in the Python.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TeamGoalieRates {
    pub wins_per_start: f64,
    pub shots_per_start: f64,
    pub starts: f64,
}

/// team -> (wins, shots against, starts) summed over its goalies' lines.
pub type GoalieTotals = Map<String, (f64, f64, f64)>;

/// Add one goalie line (season totals, `gp` games) to `team`'s totals. Starts are GS (GP when the
/// provider has no GS), shots against SA (SV + GA without it); a line without wins or shots is
/// skipped, so a team's totals never mix lines that carry different stats.
pub fn add_goalie_line(totals: &mut GoalieTotals, team: String, stats: &Map<String, f64>, gp: i64) {
    let shots = stats.get("SA").copied().or_else(|| Some(stats.get("SV")? + stats.get("GA")?));
    let (Some(w), Some(shots)) = (stats.get("W").copied(), shots) else { return };
    let starts = stats.get("GS").copied().unwrap_or(gp as f64);
    if starts <= 0.0 {
        return;
    }
    let e = totals.entry(team).or_insert((0.0, 0.0, 0.0));
    e.0 += w;
    e.1 += shots;
    e.2 += starts;
}

/// {team: wins per start shrunk toward the league's wins per start with `GOALIE_WIN_SHRINK_K`
/// starts, shots against per start shrunk toward the league's with `GOALIE_SHOTS_SHRINK_K`}. The league
/// averages are over `totals`; empty when no team has starts.
pub fn shrunk_goalie_team_rates(totals: &GoalieTotals) -> Map<String, TeamGoalieRates> {
    let starts = py_sum(totals.values().map(|t| t.2));
    if starts <= 0.0 {
        return Map::new();
    }
    let lw = py_sum(totals.values().map(|t| t.0)) / starts;
    let lsa = py_sum(totals.values().map(|t| t.1)) / starts;
    let (kw, ks) = (GOALIE_WIN_SHRINK_K, GOALIE_SHOTS_SHRINK_K);
    totals
        .iter()
        .map(|(team, (w, sa, n))| {
            let r = TeamGoalieRates {
                wins_per_start: (w + kw * lw) / (n + kw),
                shots_per_start: (sa + ks * lsa) / (n + ks),
                starts: *n,
            };
            (team.clone(), r)
        })
        .collect()
}

/// {team: this season's goalie wins and shots against per start} from the league's goalie season
/// lines, grouped by each goalie's current team, so a goalie who changed teams is counted with his
/// new team. Teams with fewer than `GF_MIN_GAMES` games, or whose pool goalies started fewer than
/// `GA_MIN_COVERAGE` of their games, are left out (the coverage rule of [`team_ga_factors`]); a
/// team's starts are capped at its games played, and the rest are shrunk as in
/// [`shrunk_goalie_team_rates`]. Not in the Python.
pub fn team_goalie_rates(ctx: &LeagueContext) -> Map<String, TeamGoalieRates> {
    let mut totals = GoalieTotals::new();
    for p in ctx.all_players() {
        let (Some(line), Some(team)) = (p.lines.get("season"), normalize_team(p.team.as_deref())) else { continue };
        if p.is_goalie() && line.gp > 0 {
            add_goalie_line(&mut totals, team, &line.stats, line.gp);
        }
    }
    let played = played_before(ctx);
    totals.retain(|t, v| {
        let n = played.get(t.as_str()).copied().unwrap_or(0);
        // a team game has one starter: GP counted as starts (no GS) includes relief appearances
        v.2 = v.2.min(n as f64);
        n >= GF_MIN_GAMES && v.2 >= GA_MIN_COVERAGE * n as f64
    });
    shrunk_goalie_team_rates(&totals)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(xs: &[(&str, f64)]) -> Map<String, f64> {
        xs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn goalie_lines_sum_by_team_with_fallbacks_for_missing_gs_and_sa() {
        let mut t = GoalieTotals::new();
        add_goalie_line(&mut t, "NJD".into(), &stats(&[("GS", 9.0), ("W", 5.0), ("SA", 250.0)]), 10);
        // a Fantrax line: no GS (GP counts) and no SA (SV + GA counts)
        add_goalie_line(&mut t, "NJD".into(), &stats(&[("W", 5.0), ("SV", 105.0), ("GA", 10.0)]), 5);
        // no wins: skipped
        add_goalie_line(&mut t, "NJD".into(), &stats(&[("GS", 3.0), ("SA", 90.0)]), 3);
        assert_eq!(t["NJD"], (10.0, 365.0, 14.0));
    }

    #[test]
    fn team_goalie_rates_shrink_toward_the_league_per_start() {
        let mut t = GoalieTotals::new();
        t.insert("A".into(), (12.0, 600.0, 20.0));
        t.insert("B".into(), (8.0, 500.0, 20.0));
        let r = shrunk_goalie_team_rates(&t);
        // league: 20 W / 40 starts = 0.5, 1100 SA / 40 = 27.5
        assert!((r["A"].wins_per_start - (12.0 + 60.0 * 0.5) / (20.0 + 60.0)).abs() < 1e-12);
        assert!((r["A"].wins_per_start - 0.525).abs() < 1e-12);
        assert!((r["A"].shots_per_start - (600.0 + 25.0 * 27.5) / 45.0).abs() < 1e-12);
        assert!((r["B"].wins_per_start - 38.0 / 80.0).abs() < 1e-12);
        assert_eq!(r["B"].starts, 20.0);
        assert!(shrunk_goalie_team_rates(&GoalieTotals::new()).is_empty());
    }
}
