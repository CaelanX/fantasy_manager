//! Points for, all-play record and expected wins from the league's completed weekly scores (gap
//! C2 of `docs/knowledge/09-engine-gap-analysis.md`).
//!
//! A head-to-head record mixes team strength with schedule luck. The all-play record is the record
//! a team would have if it had played every other team every completed week; expected wins is the
//! same thing as a sum, each week counting the share of the other teams it outscored (a tie counts
//! half). Points for is the season total. Research file 08, section 7, uses these to tell a bad
//! record from a weak roster.
//!
//! ESPN's scores come from the `mMatchupScore` view (a matchup counts once ESPN has named its
//! winner), Fantrax's from the standings SCHEDULE view (a scoring period counts once its last day
//! is before today). Playoff matchups are left out of both.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use fm_core::models::LeagueContext;
use fm_core::provider::LeagueProvider;
use fm_core::Map;
use fm_pycompat::order::cmp_f64;
use serde_json::Value;

use super::matchup::{parse_fantrax_schedule, ScheduleTable};

/// One completed regular-season scoring period: team id -> points scored.
pub type WeekScores = Map<String, f64>;

/// The completed regular-season weeks of a league and where they were read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LeagueWeeks {
    pub weeks: Vec<WeekScores>,
    pub source: String,
}

/// One team's season-to-date points and all-play numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TeamLuck {
    pub points_for: f64,
    /// 1 = most points for
    pub pf_rank: usize,
    /// (wins, losses, ties) against every other team every completed week
    pub all_play: (i64, i64, i64),
    pub expected_wins: f64,
}

fn id_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn points_of(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().replace(',', "").parse().ok(),
        _ => None,
    }
}

/// Completed regular-season weeks from ESPN's league JSON (`schedule` of the `mMatchupScore` view),
/// in matchup-period order.
pub fn espn_weeks(data: &Value) -> Vec<WeekScores> {
    let mut by_period: BTreeMap<i64, WeekScores> = BTreeMap::new();
    for m in data.get("schedule").and_then(Value::as_array).into_iter().flatten() {
        let regular = matches!(m.get("playoffTierType"), None | Some(Value::Null)) || m.get("playoffTierType").and_then(Value::as_str) == Some("NONE");
        let decided = m.get("winner").and_then(Value::as_str).is_some_and(|w| !w.is_empty() && w != "UNDECIDED");
        let Some(period) = m.get("matchupPeriodId").and_then(Value::as_i64) else { continue };
        if !regular || !decided {
            continue;
        }
        let week = by_period.entry(period).or_default();
        for side in ["home", "away"] {
            let Some(s) = m.get(side) else { continue };
            if let (Some(id), Some(pts)) = (id_of(s.get("teamId")), points_of(s.get("totalPoints"))) {
                week.insert(id, pts);
            }
        }
    }
    by_period.into_values().filter(|w| !w.is_empty()).collect()
}

/// Completed regular-season weeks from Fantrax's SCHEDULE tables: those whose last day is before
/// `today`, in date order.
pub fn fantrax_weeks(tables: &[ScheduleTable], today: NaiveDate) -> Vec<WeekScores> {
    let mut done: Vec<&ScheduleTable> = tables.iter().filter(|t| !t.playoffs && t.end.is_some_and(|e| e < today)).collect();
    done.sort_by_key(|t| t.start);
    done.into_iter()
        .map(|t| {
            let mut week = WeekScores::new();
            for (away, apts, home, hpts) in &t.games {
                for (id, pts) in [(away, apts), (home, hpts)] {
                    if let Some(id) = id_of(Some(id)) {
                        week.insert(id, *pts);
                    }
                }
            }
            week
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// The league's completed regular-season weeks from the provider, or None for a provider without
/// weekly scores.
pub fn league_weeks(ctx: &LeagueContext, provider: &dyn LeagueProvider) -> anyhow::Result<Option<LeagueWeeks>> {
    match ctx.provider.as_str() {
        "espn" => {
            let data = provider.espn_league_get(&[("view".to_string(), "mMatchupScore".to_string())])?;
            Ok(Some(LeagueWeeks { weeks: espn_weeks(&data), source: "ESPN scoreboard".to_string() }))
        }
        "fantrax" => {
            let mut params = Map::new();
            params.insert("view".to_string(), Value::String("SCHEDULE".into()));
            let data = provider.fantrax_call(&[("getStandings".to_string(), params)], false)?;
            let data = data.into_iter().next().ok_or_else(|| anyhow::anyhow!("empty getStandings response"))?;
            Ok(Some(LeagueWeeks { weeks: fantrax_weeks(&parse_fantrax_schedule(&data), ctx.as_of), source: "Fantrax schedule".to_string() }))
        }
        _ => Ok(None),
    }
}

/// Points for, its rank, all-play record and expected wins for each of `team_ids` over `weeks`.
/// A team missing from a week scores nothing that week and is left out of that week's all-play.
/// Points-for ties keep the order of `team_ids`.
pub fn luck_table(weeks: &[WeekScores], team_ids: &[&str]) -> Map<String, TeamLuck> {
    let mut pf: Vec<f64> = vec![0.0; team_ids.len()];
    let mut ap: Vec<(i64, i64, i64)> = vec![(0, 0, 0); team_ids.len()];
    let mut xw: Vec<f64> = vec![0.0; team_ids.len()];
    for week in weeks {
        for (i, id) in team_ids.iter().enumerate() {
            let Some(&mine) = week.get(*id) else { continue };
            pf[i] += mine;
            let (mut w, mut l, mut t) = (0i64, 0i64, 0i64);
            for (other, &theirs) in week.iter() {
                if other == id {
                    continue;
                }
                if mine > theirs {
                    w += 1;
                } else if mine < theirs {
                    l += 1;
                } else {
                    t += 1;
                }
            }
            ap[i] = (ap[i].0 + w, ap[i].1 + l, ap[i].2 + t);
            let opponents = w + l + t;
            if opponents > 0 {
                xw[i] += (w as f64 + 0.5 * t as f64) / opponents as f64;
            }
        }
    }
    let mut order: Vec<usize> = (0..team_ids.len()).collect();
    order.sort_by(|&a, &b| cmp_f64(-pf[a], -pf[b]));
    let mut out = Map::new();
    for (rank, &i) in order.iter().enumerate() {
        out.insert(team_ids[i].to_string(), TeamLuck { points_for: pf[i], pf_rank: rank + 1, all_play: ap[i], expected_wins: xw[i] });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn week(xs: &[(&str, f64)]) -> WeekScores {
        xs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// Four teams, two weeks. Week 1: a 100, b 90, c 80, d 70. Week 2: a 60, b 90, c 90, d 120.
    /// a: week 1 beats all three (3-0-0), week 2 loses to all three (0-3-0): all-play 3-3-0,
    /// expected wins 1 + 0 = 1, points 160.
    /// b: week 1 2-1-0, week 2 beats a, ties c, loses to d (1-1-1): all-play 3-2-1, expected wins
    /// 2/3 + 1.5/3 = 7/6, points 180.
    /// d: week 1 0-3-0, week 2 3-0-0: 3-3-0, expected wins 1, points 190 (rank 1).
    #[test]
    fn all_play_and_points_for_by_hand() {
        let weeks = vec![
            week(&[("a", 100.0), ("b", 90.0), ("c", 80.0), ("d", 70.0)]),
            week(&[("a", 60.0), ("b", 90.0), ("c", 90.0), ("d", 120.0)]),
        ];
        let t = luck_table(&weeks, &["a", "b", "c", "d"]);
        assert_eq!(t["a"].all_play, (3, 3, 0));
        assert_eq!(t["a"].points_for, 160.0);
        assert!((t["a"].expected_wins - 1.0).abs() < 1e-12);
        assert_eq!(t["b"].all_play, (3, 2, 1));
        assert!((t["b"].expected_wins - 7.0 / 6.0).abs() < 1e-12);
        assert_eq!(t["c"].all_play, (2, 3, 1));
        assert_eq!(t["d"].points_for, 190.0);
        assert_eq!((t["d"].pf_rank, t["b"].pf_rank, t["c"].pf_rank, t["a"].pf_rank), (1, 2, 3, 4));
    }

    #[test]
    fn espn_counts_only_decided_regular_season_matchups() {
        let data = json!({"schedule": [
            {"matchupPeriodId": 1, "winner": "AWAY", "playoffTierType": "NONE",
             "home": {"teamId": 1, "totalPoints": 94.2}, "away": {"teamId": 10, "totalPoints": 199.2}},
            {"matchupPeriodId": 2, "winner": "HOME",
             "home": {"teamId": 10, "totalPoints": 120.0}, "away": {"teamId": 1, "totalPoints": 110.5}},
            {"matchupPeriodId": 3, "winner": "UNDECIDED", "playoffTierType": "NONE",
             "home": {"teamId": 1, "totalPoints": 22.4}, "away": {"teamId": 10, "totalPoints": 13.1}},
            {"matchupPeriodId": 25, "winner": "HOME", "playoffTierType": "WINNERS_BRACKET",
             "home": {"teamId": 1, "totalPoints": 150.0}, "away": {"teamId": 10, "totalPoints": 100.0}}
        ]});
        let w = espn_weeks(&data);
        assert_eq!(w, vec![week(&[("1", 94.2), ("10", 199.2)]), week(&[("10", 120.0), ("1", 110.5)])]);
    }

    #[test]
    fn fantrax_counts_periods_that_ended_before_today() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day);
        let table = |n, s, e, playoffs| ScheduleTable {
            number: Some(n),
            start: s,
            end: e,
            playoffs,
            games: vec![(json!("x"), 50.0, json!("y"), 60.0)],
        };
        let tables = vec![
            table(2, d(10, 13), d(10, 19), false),
            table(1, d(10, 6), d(10, 12), false),
            table(3, d(10, 20), d(10, 26), false),
            table(25, d(10, 1), d(10, 2), true),
        ];
        // today 2026-10-26: period 3 ends today and is still being played
        let w = fantrax_weeks(&tables, d(10, 26).unwrap());
        assert_eq!(w.len(), 2);
        assert_eq!(w[0], week(&[("x", 50.0), ("y", 60.0)]));
    }
}
