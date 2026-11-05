//! Rust port of `fantasy_manager/analysis/goalie_plan.py`: the goalie start planner for the rest
//! of the matchup (`fm goalies --week`, the matchup page and the weekly plan). Owner: work package
//! `W6-lineup-waivers`. The team factors, `second_night_prob` and the odds source live in
//! [`crate::analysis::team_factors`] (needed by valuation) and are re-exported here.
//!
//! Deviations: the clock (`now`, for the streaming pickups' daily plan) and the schedule memo are
//! explicit; `odds` None reads the archive in `data_dir` (`team_factors::odds_fn`). Streaming
//! suggestions cannot raise in Rust except for a missing "my team" (`CoreError`), which becomes
//! the same note Python wrote for any exception.

use chrono::{Duration, NaiveDate};
use fm_core::models::{LeagueContext, Player, Recommendation, Status};
use fm_core::normalize::normalize_team;
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::{Cid, Map, PyDateTime};
use fm_pycompat::fmt::percent;
use fm_pycompat::num::{py_sum, round_to};
use fm_pycompat::order::{cmp_f64, sort_desc_by};
use serde::{Deserialize, Serialize};

pub use crate::analysis::team_factors::{
    odds_fn, second_night_prob, second_night_prob_default, shrunk_team_factors, team_ga_factors, team_gf_factors, DayOdds,
    OddsFn, TeamFactors, BACKUP_REPEAT_P, GA_MIN_COVERAGE, GF_MIN_GAMES, GF_SHRINK_K, RESTED_STARTER_P,
};
use crate::recommend::lineup::{available_on, confirmed_start_probability, plan_days, starting_slots};
use crate::recommend::strength::apply_ranks;
use crate::recommend::waivers::recommend_matchup_waivers;
use crate::scoring::{from_config, PointsScoring, Stats};
use crate::valuation::schedule::{night_start_probs, ScheduleMemo, BACKUP_MIN_SHARE, B2B_SECOND_NIGHT_P, SEASON_B2B_FRAC};

pub const FA_LIMIT: usize = 3;
pub const STREAM_LIMIT: usize = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalieGame {
    pub day: NaiveDate,
    /// "BOS" at home, "@BOS" away
    pub opponent: String,
    pub start_prob: f64,
    /// "confirmed" / "likely" / "season share 52%" / "B2B 2nd night"
    pub start_source: String,
    #[serde(default)]
    pub second_night: bool,
    /// betting market, no vig
    #[serde(default)]
    pub win_prob: Option<f64>,
    /// opponent's implied goals (odds) or goals per game (fallback)
    #[serde(default)]
    pub opp_goals: Option<f64>,
    /// "odds" / "goals per game" / "neutral"
    pub matchup_source: String,
    #[serde(default)]
    pub pts_if_starts: f64,
    #[serde(default)]
    pub exp_pts: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalieRow {
    pub cid: String,
    pub name: String,
    pub team: Option<String>,
    pub mine: bool,
    pub status: Status,
    pub start_share: Option<f64>,
    #[serde(default)]
    pub games: Vec<GoalieGame>,
}

impl GoalieRow {
    pub fn exp_total(&self) -> f64 {
        py_sum(self.games.iter().map(|g| g.exp_pts))
    }
    pub fn starts(&self) -> f64 {
        py_sum(self.games.iter().map(|g| g.start_prob))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalieDay {
    pub day: NaiveDate,
    /// cids, best first
    #[serde(default)]
    pub start: Vec<String>,
    /// mine with a game but no G slot
    #[serde(default)]
    pub sit: Vec<String>,
    /// G slots without a goalie of mine playing
    #[serde(default)]
    pub open_slots: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalieWeek {
    pub days: Vec<NaiveDate>,
    pub g_slots: i64,
    pub mine: Vec<GoalieRow>,
    pub free_agents: Vec<GoalieRow>,
    pub plan: Vec<GoalieDay>,
    #[serde(default)]
    pub streams: Vec<Recommendation>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl GoalieWeek {
    pub fn row(&self, cid: &str) -> Option<&GoalieRow> {
        self.mine.iter().chain(self.free_agents.iter()).find(|r| r.cid == cid)
    }

    /// Expected points from my goalies in their G slots over the rest of the matchup.
    pub fn exp_total(&self) -> f64 {
        let mut xs = Vec::new();
        for d in &self.plan {
            for c in &d.start {
                if let Some(r) = self.mine.iter().find(|r| &r.cid == c) {
                    xs.extend(r.games.iter().filter(|g| g.day == d.day).map(|g| g.exp_pts));
                }
            }
        }
        py_sum(xs)
    }
}

/// Expected fantasy points if he starts (see the Python module docstring); his FPG when the league
/// is not a points league or he has no rates.
pub fn start_points(scoring: Option<&PointsScoring>, pv: &PlayerValue, win_prob: Option<f64>, ga_factor: Option<f64>) -> f64 {
    let mut rates: Stats = pv.rates.clone();
    let Some(scoring) = scoring.filter(|_| !rates.is_empty()) else { return pv.fpg };
    if let (Some(ga), Some(f)) = (rates.get("GA").copied(), ga_factor.filter(|f| *f > 0.0)) {
        let new_ga = ga * f;
        let extra = new_ga - ga;
        if let Some(sv) = rates.get_mut("SV") {
            *sv = (*sv - extra).max(0.0);
        }
        if let Some(so) = rates.get_mut("SO") {
            *so *= (-extra).exp();
        }
        if let Some(gaa) = rates.get_mut("GAA") {
            *gaa *= f;
        }
        if let Some(sa) = rates.get("SA").copied().filter(|s| *s != 0.0) {
            rates.insert("SVPCT".to_string(), ((sa - new_ga) / sa).max(0.0));
        }
        rates.insert("GA".to_string(), new_ga);
    }
    if let (Some(wp), Some(w)) = (win_prob, rates.get("W").copied()) {
        let sum = w + rates.get("L").copied().unwrap_or(0.0) + rates.get("OTL").copied().unwrap_or(0.0);
        let decisions = if sum != 0.0 { sum } else { 1.0 };
        let new_w = (wp * decisions).max(0.0).min(decisions);
        if let Some(l) = rates.get_mut("L") {
            *l = (*l - (new_w - w)).max(0.0);
        }
        rates.insert("W".to_string(), new_w);
    }
    scoring.value(&rates).max(0.0)
}

/// (win prob, opponent goals shown, GA factor, source) for `team` on `day`.
fn matchup(team: &str, opp_label: &str, odds: &DayOdds, gf: &TeamFactors) -> (Option<f64>, Option<f64>, Option<f64>, &'static str) {
    let o = odds.get(team);
    let win = o.and_then(|o| o.win_prob);
    let implied: Vec<f64> = odds.values().filter_map(|v| v.implied_total).filter(|x| *x != 0.0).collect();
    if let Some(opp) = o.and_then(|o| o.opp_implied_total).filter(|x| *x != 0.0) {
        if !implied.is_empty() {
            let avg = py_sum(implied.iter().copied()) / implied.len() as f64;
            return (win, Some(opp), if avg > 0.0 { Some(opp / avg) } else { None }, "odds");
        }
    }
    let opp_team = normalize_team(Some(opp_label.trim_start_matches('@'))).unwrap_or_default();
    match gf.get(&opp_team) {
        Some(f) => (win, None, Some(f.0), "goals per game"),
        None => (win, None, None, "neutral"),
    }
}

fn games(
    ctx: &LeagueContext,
    p: &Player,
    pv: &PlayerValue,
    days: &[NaiveDate],
    odds_for: &OddsFn,
    gf: &TeamFactors,
    scoring: Option<&PointsScoring>,
) -> Vec<GoalieGame> {
    let team = normalize_team(p.team.as_deref()).unwrap_or_default();
    let dates: &[NaiveDate] = ctx.schedule.get(&team).map(|v| v.as_slice()).unwrap_or(&[]);
    let share = pv.start_share.unwrap_or(1.0);
    let (p1, p2) = night_start_probs(share, B2B_SECOND_NIGHT_P, SEASON_B2B_FRAC);
    let today_prob = confirmed_start_probability(p);
    let mut out = Vec::new();
    let mut prev_known: Option<f64> = None;
    for &d in days {
        if !dates.contains(&d) || !available_on(p, Some(pv), d) {
            prev_known = None;
            continue;
        }
        let second = dates.contains(&(d - Duration::days(1)));
        let (prob, src, known): (f64, String, Option<f64>);
        match (d == ctx.as_of, today_prob) {
            (true, Some(tp)) => {
                prob = tp;
                src = if tp == 0.0 || tp == 1.0 { "confirmed" } else { "likely" }.to_string();
                known = Some(prob);
            }
            _ => {
                if let (true, Some(q), true) = (second, prev_known, share >= BACKUP_MIN_SHARE) {
                    prob = second_night_prob(share, q, B2B_SECOND_NIGHT_P);
                    src = "B2B 2nd night, after tonight's starter".to_string();
                } else if second {
                    prob = p2;
                    src = "B2B 2nd night".to_string();
                } else {
                    prob = p1;
                    src = format!("season share {}", percent(share, 0, false));
                }
                known = None;
            }
        }
        prev_known = known;
        let opp = ctx.opponents.get(&team).and_then(|m| m.get(&d)).cloned().unwrap_or_else(|| "?".to_string());
        let day_odds = odds_for(d);
        let (win, opp_goals, ga_f, msrc) = matchup(&team, &opp, &day_odds, gf);
        let pts = start_points(scoring, pv, win, ga_f);
        out.push(GoalieGame {
            day: d,
            opponent: opp,
            start_prob: round_to(prob, 3),
            start_source: src,
            second_night: second,
            win_prob: win,
            opp_goals,
            matchup_source: msrc.to_string(),
            pts_if_starts: round_to(pts, 2),
            exp_pts: round_to(prob * pts, 2),
        });
    }
    out
}

/// Keyword arguments of `goalie_week`.
#[derive(Clone)]
pub struct GoalieWeekOpts<'a> {
    /// a day -> `providers.odds.team_context` output (None: the archive in `data_dir`)
    pub odds: Option<OddsFn>,
    pub data_dir: Option<std::path::PathBuf>,
    pub fa_limit: usize,
    pub streams: bool,
    pub dynasty_values: Option<&'a Map<Cid, DynastyValue>>,
}

impl Default for GoalieWeekOpts<'_> {
    fn default() -> Self {
        GoalieWeekOpts { odds: None, data_dir: None, fa_limit: FA_LIMIT, streams: true, dynasty_values: None }
    }
}

/// The goalie plan for the rest of the matchup (see the Python module docstring). `now` and `memo`
/// feed the streaming pickups (`waivers::recommend_matchup_waivers`).
pub fn goalie_week(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    opts: &GoalieWeekOpts<'_>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> GoalieWeek {
    let days = plan_days(ctx);
    let odds_for: OddsFn = opts.odds.clone().unwrap_or_else(|| odds_fn(opts.data_dir.clone()));
    let gf = team_gf_factors(ctx);
    let sc = from_config(&ctx.scoring);
    let scoring = sc.as_points();
    let g_slots = starting_slots(&ctx.roster_shape).iter().filter(|s| s.as_str() == "G").count();
    let mut notes: Vec<String> = Vec::new();

    let row = |p: &Player, mine: bool| -> Option<GoalieRow> {
        let pv = values.get(&p.cid)?;
        Some(GoalieRow {
            cid: p.cid.clone(),
            name: p.name.clone(),
            team: p.team.clone(),
            mine,
            status: p.status,
            start_share: pv.start_share,
            games: games(ctx, p, pv, &days, &odds_for, &gf, scoring),
        })
    };

    let mine: Vec<GoalieRow> = match ctx.my_team() {
        Ok(t) => t.players_iter().filter(|p| p.is_goalie()).filter_map(|p| row(p, true)).collect(),
        Err(_) => vec![],
    };
    let mut fas: Vec<GoalieRow> = ctx
        .free_agents
        .iter()
        .filter(|p| p.is_goalie() && normalize_team(p.team.as_deref()).is_some_and(|t| !t.is_empty()))
        .filter_map(|p| row(p, false))
        .filter(|r| !r.games.is_empty())
        .collect();
    sort_desc_by(&mut fas, |a, b| cmp_f64(a.exp_total(), b.exp_total()));

    let mut plan: Vec<GoalieDay> = Vec::new();
    for &d in &days {
        let mut today: Vec<(&GoalieRow, &GoalieGame)> = mine
            .iter()
            .flat_map(|r| r.games.iter().filter(move |g| g.day == d && g.exp_pts > 0.0).map(move |g| (r, g)))
            .collect();
        sort_desc_by(&mut today, |a, b| cmp_f64(a.1.exp_pts, b.1.exp_pts));
        let start: Vec<String> = today.iter().take(g_slots).map(|(r, _)| r.cid.clone()).collect();
        let sit: Vec<String> = today.iter().skip(g_slots).map(|(r, _)| r.cid.clone()).collect();
        let open = g_slots.saturating_sub(start.len()) as i64;
        plan.push(GoalieDay { day: d, start, sit, open_slots: open });
    }

    let mut recs: Vec<Recommendation> = Vec::new();
    if opts.streams {
        match recommend_matchup_waivers(ctx, values, 50, opts.dynasty_values, &[], now, memo) {
            Ok(rs) => {
                recs = rs.into_iter().filter(|r| r.add.first().is_some_and(|p| p.is_goalie())).take(STREAM_LIMIT).collect();
                apply_ranks(&mut recs);
            }
            Err(e) => notes.push(format!("Goalie streaming suggestions failed: {e}")),
        }
    }
    let sources: Vec<&str> = mine.iter().chain(fas.iter()).flat_map(|r| r.games.iter().map(|g| g.matchup_source.as_str())).collect();
    if !sources.contains(&"odds") {
        notes.push(if sources.contains(&"goals per game") {
            "No betting odds archived for these days (ODDS_API_KEY / `fm harness odds`): matchups use the opponent's goals per game"
                .to_string()
        } else {
            "No betting odds or opponent scoring available: matchups are neutral".to_string()
        });
    }
    fas.truncate(opts.fa_limit);
    GoalieWeek { days, g_slots: g_slots as i64, mine, free_agents: fas, plan, streams: recs, notes }
}

#[cfg(test)]
mod tests {
    //! Port of tests/test_goalie_plan.py (intent).
    use std::sync::Arc;

    use super::*;
    use crate::recommend::lineup::tests::now_at;
    use fm_core::models::{FantasyTeam, RosterSlot};
    use fm_sources::odds::TeamOddsContext;
    use serde_json::json;

    fn mon() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 26).unwrap()
    }
    fn day(k: i64) -> NaiveDate {
        mon() + Duration::days(k)
    }
    fn weights() -> Map<String, f64> {
        [("W", 2.0), ("GA", -1.5), ("SV", 0.2), ("SO", 2.0)].iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }
    fn rates() -> Map<String, f64> {
        [("W", 0.5), ("L", 0.4), ("OTL", 0.1), ("GA", 2.8), ("SV", 25.0), ("SA", 27.8), ("SO", 0.05), ("GS", 1.0)]
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect()
    }
    fn scoring() -> PointsScoring {
        PointsScoring::new(weights(), None)
    }
    fn goalie(cid: &str, team: &str) -> Player {
        serde_json::from_value(json!({"cid": cid, "name": format!("{cid} Goalie"), "name_norm": cid, "ids": {},
                                      "team": team, "positions": ["G"]}))
        .unwrap()
    }
    fn skater(cid: &str, team: &str, goals: f64) -> Player {
        serde_json::from_value(json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": team,
            "positions": ["C"], "lines": {"season": {"split": "season", "gp": 10, "stats": {"G": goals}}}}))
        .unwrap()
    }
    fn pv(p: &Player, share: f64, fpg: Option<f64>) -> PlayerValue {
        let f = fpg.unwrap_or_else(|| scoring().value(&rates()));
        serde_json::from_value(json!({"player": p, "fpg": f, "fpg_season": f * share, "fpg_week": f * share,
            "vorp": 0.0, "vorp_week": 0.0, "start_share": share, "rates": rates()}))
        .unwrap()
    }
    fn make_ctx(
        mine: &[&Player],
        fas: &[Player],
        schedule: Option<Map<String, Vec<NaiveDate>>>,
        bench: &[&Player],
        opponents: Option<Map<String, Map<NaiveDate, String>>>,
    ) -> LeagueContext {
        let sched = schedule.unwrap_or_else(|| {
            [
                ("EDM", vec![day(1), day(2), day(4)]),
                ("CGY", vec![day(1), day(3), day(5)]),
                ("VAN", (0..7).map(day).collect()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
        });
        let mut slots: Vec<RosterSlot> =
            mine.iter().map(|p| RosterSlot { slot: "G".into(), player: Some((*p).clone()), starting: true }).collect();
        slots.extend(bench.iter().map(|p| RosterSlot { slot: "BN".into(), player: Some((*p).clone()), starting: false }));
        let t = FantasyTeam { team_id: "1".into(), name: "me".into(), owner_is_me: true, slots, record: None };
        let mut ctx: LeagueContext = serde_json::from_value(json!({"provider": "t", "league_id": "1", "season": 2027,
            "name": "T", "scoring": {"kind": "points", "weights": weights()}, "roster_shape": {"G": 2, "BN": 1},
            "teams": [t], "free_agents": fas, "matchup_period": 1, "as_of": mon(), "season_start": "2026-10-07",
            "lineup_lock": "daily", "period_start": mon(), "period_end": day(6)}))
        .unwrap();
        ctx.schedule = sched;
        ctx.opponents = opponents.unwrap_or_default();
        ctx
    }
    fn no_odds() -> OddsFn {
        Arc::new(|_d| Map::new())
    }
    fn week(ctx: &LeagueContext, values: &Map<Cid, PlayerValue>, odds: OddsFn, streams: bool) -> GoalieWeek {
        let opts = GoalieWeekOpts { odds: Some(odds), streams, ..GoalieWeekOpts::default() };
        goalie_week(ctx, values, &opts, &now_at(12, 0, 1), &ScheduleMemo::new(None))
    }
    fn vals(xs: Vec<PlayerValue>) -> Map<Cid, PlayerValue> {
        xs.into_iter().map(|v| (v.player.cid.clone(), v)).collect()
    }
    fn game(gw: &GoalieWeek, cid: &str, d: NaiveDate) -> GoalieGame {
        gw.row(cid).unwrap().games.iter().find(|g| g.day == d).unwrap().clone()
    }

    #[test]
    fn back_to_back_second_night_goes_to_the_backup() {
        let (s, b) = (goalie("one", "EDM"), goalie("two", "EDM"));
        let ctx = make_ctx(&[&s, &b], &[], None, &[], None);
        let gw = week(&ctx, &vals(vec![pv(&s, 0.7, None), pv(&b, 0.3, None)]), no_odds(), false);
        let (p1, p2) = night_start_probs(0.7, B2B_SECOND_NIGHT_P, SEASON_B2B_FRAC);
        assert!((game(&gw, "one", day(1)).start_prob - p1).abs() < 1e-3);
        let w = game(&gw, "one", day(2));
        assert!((w.start_prob - p2).abs() < 1e-3 && w.second_night && w.start_source == "B2B 2nd night");
        assert!((game(&gw, "two", day(2)).start_prob - (1.0 - B2B_SECOND_NIGHT_P)).abs() < 1e-3);
        assert_eq!(gw.plan.iter().find(|x| x.day == day(2)).unwrap().start, vec!["two", "one"]);
    }

    #[test]
    fn confirmed_start_overrides_the_share_and_conditions_tomorrow() {
        let sched: Map<String, Vec<NaiveDate>> = [("EDM".to_string(), vec![mon(), day(1)])].into_iter().collect();
        let (mut s, mut b) = (goalie("one", "EDM"), goalie("two", "EDM"));
        s.confirmed_start = Some(false);
        s.start_source = Some("Daily Faceoff: Confirmed".into());
        b.confirmed_start = Some(true);
        b.start_source = Some("Daily Faceoff: Confirmed".into());
        let ctx = make_ctx(&[&s, &b], &[], Some(sched), &[], None);
        let gw = week(&ctx, &vals(vec![pv(&s, 0.7, None), pv(&b, 0.3, None)]), no_odds(), false);
        let g = game(&gw, "one", mon());
        assert_eq!((g.start_prob, g.start_source.as_str()), (0.0, "confirmed"));
        assert_eq!(game(&gw, "two", mon()).start_prob, 1.0);
        assert!((game(&gw, "one", day(1)).start_prob - RESTED_STARTER_P).abs() < 1e-9);
        assert!((game(&gw, "two", day(1)).start_prob - BACKUP_REPEAT_P).abs() < 1e-9);
    }

    #[test]
    fn odds_drive_win_and_goals_against() {
        let g = goalie("one", "EDM");
        let opp: Map<String, Map<NaiveDate, String>> =
            [("EDM".to_string(), [(day(1), "@BOS".to_string())].into_iter().collect())].into_iter().collect();
        let ctx = make_ctx(&[&g], &[], None, &[], Some(opp));
        let ctxo = |win: f64, opp: f64, imp: f64| TeamOddsContext {
            implied_total: Some(imp),
            opp_implied_total: Some(opp),
            win_prob: Some(win),
            opponent: String::new(),
            home: false,
            game_total: None,
            as_of: String::new(),
        };
        let table: DayOdds =
            [("EDM".to_string(), ctxo(0.65, 2.4, 3.6)), ("BOS".to_string(), ctxo(0.35, 3.6, 2.4))].into_iter().collect();
        let tue = day(1);
        let odds: OddsFn = Arc::new(move |d| if d == tue { table.clone() } else { Map::new() });
        let gw = week(&ctx, &vals(vec![pv(&g, 0.7, None)]), odds, false);
        let gm = game(&gw, "one", tue);
        assert_eq!((gm.matchup_source.as_str(), gm.win_prob, gm.opp_goals), ("odds", Some(0.65), Some(2.4)));
        assert_eq!(gm.opponent, "@BOS");
        assert!(gm.pts_if_starts > scoring().value(&rates()));
        let want = start_points(Some(&scoring()), &pv(&g, 0.7, None), Some(0.65), Some(2.4 / 3.0));
        assert!((gm.pts_if_starts - want).abs() < 0.01);
    }

    #[test]
    fn without_odds_falls_back_to_goals_per_game_then_neutral() {
        let g = goalie("one", "EDM");
        let past: Vec<NaiveDate> = (1..6).map(|k| day(-k)).collect();
        let mut edm = past.clone();
        edm.push(day(1));
        let sched: Map<String, Vec<NaiveDate>> =
            [("EDM".to_string(), edm), ("BOS".to_string(), past.clone()), ("CGY".to_string(), past)].into_iter().collect();
        let shooters = vec![skater("b1", "BOS", 30.0), skater("c1", "CGY", 10.0), skater("e1", "EDM", 20.0)];
        let opp: Map<String, Map<NaiveDate, String>> =
            [("EDM".to_string(), [(day(1), "BOS".to_string())].into_iter().collect())].into_iter().collect();
        let ctx = make_ctx(&[&g], &shooters, Some(sched), &[], Some(opp));
        let f = team_gf_factors(&ctx);
        assert!(f["BOS"].0 > 1.0 && 1.0 > f["CGY"].0);
        let gw = week(&ctx, &vals(vec![pv(&g, 0.7, None)]), no_odds(), false);
        let gm = &gw.row("one").unwrap().games[0];
        assert_eq!((gm.matchup_source.as_str(), gm.win_prob), ("goals per game", None));
        assert!(gm.pts_if_starts < scoring().value(&rates()));
        assert!(gw.notes.iter().any(|n| n.contains("goals per game")));
        let ctx2 = make_ctx(&[&g], &[], None, &[], None);
        let gw2 = week(&ctx2, &vals(vec![pv(&g, 0.7, None)]), no_odds(), false);
        let g2 = &gw2.row("one").unwrap().games[0];
        assert_eq!(g2.matchup_source, "neutral");
        assert!((g2.pts_if_starts - scoring().value(&rates())).abs() < 0.01);
    }

    #[test]
    fn open_goalie_nights_and_stream_suggestion_respect_the_move_budget() {
        let (m1, m2, weak, fa) = (goalie("one", "EDM"), goalie("two", "EDM"), goalie("weak", "CGY"), goalie("fa", "VAN"));
        let v = vals(vec![pv(&m1, 0.6, None), pv(&m2, 0.4, None), pv(&weak, 0.2, Some(0.5)), pv(&fa, 0.9, None)]);
        let ctx = make_ctx(&[&m1, &m2], &[fa.clone()], None, &[&weak], None);
        let gw = week(&ctx, &v, no_odds(), true);
        assert!(gw.plan.iter().any(|d| d.open_slots > 0));
        assert_eq!(gw.free_agents[0].cid, "fa");
        assert_eq!(gw.streams[0].add[0].cid, "fa");
        assert_eq!((gw.streams[0].rank_in_kind, gw.streams[0].kind_total), (Some(1), Some(gw.streams.len() as i64)));
        let mut spent = ctx.clone();
        spent.moves_limit_per_period = Some(2);
        spent.moves_used_this_period = Some(2);
        assert!(week(&spent, &v, no_odds(), true).streams.is_empty());
    }

    #[test]
    fn out_goalie_has_no_games_and_categories_keep_fpg() {
        let mut g = goalie("one", "EDM");
        g.status = Status::Out;
        let ctx = make_ctx(&[&g], &[], None, &[], None);
        let gw = week(&ctx, &vals(vec![pv(&g, 0.7, None)]), no_odds(), false);
        assert!(gw.row("one").unwrap().games.is_empty() && gw.plan.iter().all(|d| d.start.is_empty()));
        assert_eq!(start_points(None, &pv(&g, 0.7, Some(3.0)), Some(0.9), Some(0.5)), 3.0);
        assert!((second_night_prob(0.7, 1.0, B2B_SECOND_NIGHT_P) - B2B_SECOND_NIGHT_P).abs() < 1e-12);
    }
}
