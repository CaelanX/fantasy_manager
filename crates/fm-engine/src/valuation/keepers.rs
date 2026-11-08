//! Keeper planner (keeper leagues): every team's predicted keepers as `apply_dynasty` flags them
//! (top `keepers_per_team` by keeper surplus), the next candidates behind them, and the redraft
//! pool: everyone not predicted kept, ranked by the next-season value `keeper_slot_curve` uses, so
//! pool index i is overall pick i + 1 of the redraft.

use std::collections::HashSet;

use fm_core::models::LeagueContext;
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::Map;
use fm_pycompat::order::{cmp_f64, sort_desc_by};
use serde::Serialize;

use super::dynasty::{keeper_count, ProductionCurves};
use super::picks::{next_season_fpg_with, next_season_weight};

#[derive(Clone, Debug, Serialize)]
pub struct KeeperCandidate {
    pub cid: String,
    pub name: String,
    pub age: Option<f64>,
    /// place on his team by keeper surplus (1 = best)
    pub rank: usize,
    /// long-term value over a waiver-level roster spot (`DynastyValue.keeper_surplus`)
    pub surplus: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TeamKeepers {
    pub team_id: String,
    pub name: String,
    pub me: bool,
    /// predicted keepers, best first (fewer than k when too few players beat a waiver-level spot)
    pub keepers: Vec<KeeperCandidate>,
    /// the best non-keepers right behind them
    pub bubble: Vec<KeeperCandidate>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PoolPlayer {
    pub cid: String,
    pub name: String,
    pub age: Option<f64>,
    /// holder's team name (None: free agent)
    pub team: Option<String>,
    /// w_1 x next season's per-game value (the redraft slot curve's units)
    pub value: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct KeeperPlan {
    pub k: i64,
    /// my team first, then league order
    pub teams: Vec<TeamKeepers>,
    /// the whole redraft pool, best first
    pub pool: Vec<PoolPlayer>,
    /// rostered cid -> (holder's team name, place on that team by keeper surplus)
    #[serde(skip)]
    pub ranks: Map<String, (String, usize)>,
}

impl KeeperPlan {
    pub fn mine(&self) -> Option<&TeamKeepers> {
        self.teams.iter().find(|t| t.me)
    }

    /// The pool player at overall pick `overall` of the redraft (1-based, rounded).
    pub fn at_overall(&self, overall: f64) -> Option<&PoolPlayer> {
        let i = overall.round().max(1.0) as usize;
        self.pool.get(i - 1)
    }
}

/// The keeper plan, or None outside keeper leagues (`keepers_per_team` unset or 0). `bubble`: how
/// many non-keepers to list per team.
pub fn keeper_plan(
    ctx: &LeagueContext,
    values: &Map<String, PlayerValue>,
    dynasty_values: &Map<String, DynastyValue>,
    bubble: usize,
) -> Option<KeeperPlan> {
    let k = keeper_count(ctx).filter(|k| *k > 0)?;
    let mut seen: HashSet<&str> = HashSet::new();
    let mut holder: Map<&str, &str> = Map::new();
    let mut teams: Vec<TeamKeepers> = Vec::with_capacity(ctx.teams.len());
    let mut ranks: Map<String, (String, usize)> = Map::new();
    for t in &ctx.teams {
        // same holder and order as `apply_keepers`, so ranks match its "Keeper #n" reasons
        let mut ranked: Vec<&DynastyValue> = Vec::new();
        for p in t.players_iter() {
            if let Some(dv) = dynasty_values.get(&p.cid) {
                if seen.insert(p.cid.as_str()) {
                    ranked.push(dv);
                    holder.insert(p.cid.as_str(), t.name.as_str());
                }
            }
        }
        ranked.sort_by(|a, b| cmp_f64(-a.keeper_surplus, -b.keeper_surplus));
        for (i, dv) in ranked.iter().enumerate() {
            ranks.insert(dv.player.cid.clone(), (t.name.clone(), i + 1));
        }
        let cand = |(i, dv): (usize, &&DynastyValue)| KeeperCandidate {
            cid: dv.player.cid.clone(),
            name: dv.player.name.clone(),
            age: dv.age,
            rank: i + 1,
            surplus: dv.keeper_surplus,
        };
        let kept = ranked.iter().take_while(|dv| dv.keeper).count();
        teams.push(TeamKeepers {
            team_id: t.team_id.clone(),
            name: t.name.clone(),
            me: t.owner_is_me,
            keepers: ranked.iter().enumerate().take(kept).map(cand).collect(),
            bubble: ranked.iter().enumerate().skip(kept).take(bubble).map(cand).collect(),
        });
    }
    teams.sort_by_key(|t| !t.me);

    let curves = ProductionCurves::current();
    let w1 = next_season_weight(ctx);
    let rostered: HashSet<&str> = ctx.teams.iter().flat_map(|t| t.players_iter()).map(|p| p.cid.as_str()).collect();
    let mut pool: Vec<PoolPlayer> = Vec::new();
    for (cid, pv) in values {
        let dv = dynasty_values.get(cid);
        if rostered.contains(cid.as_str()) && dv.is_some_and(|d| d.keeper) {
            continue;
        }
        pool.push(PoolPlayer {
            cid: cid.clone(),
            name: pv.player.name.clone(),
            age: dv.and_then(|d| d.age),
            team: holder.get(cid.as_str()).map(|s| s.to_string()),
            value: w1 * next_season_fpg_with(&curves, ctx, pv, dv),
        });
    }
    sort_desc_by(&mut pool, |a, b| cmp_f64(a.value, b.value));
    Some(KeeperPlan { k, teams, pool, ranks })
}

#[cfg(test)]
mod tests {
    use super::super::dynasty::apply_dynasty;
    use super::super::dynasty::keeper_tests::std_league;
    use super::super::picks::keeper_slot_curve;
    use super::*;

    #[test]
    fn keepers_follow_the_dynasty_flags_mine_first() {
        let (c, v) = std_league(Some(3));
        let d = apply_dynasty(&v, &c, None, None);
        let plan = keeper_plan(&c, &v, &d, 2).unwrap();
        assert_eq!(plan.k, 3);
        assert_eq!(plan.teams[0].team_id, "mine");
        assert_eq!(plan.teams.len(), c.teams.len());
        for t in &plan.teams {
            let team = c.team(&t.team_id).unwrap();
            let flagged: HashSet<&str> = team.players_iter().filter(|p| d[&p.cid].keeper).map(|p| p.cid.as_str()).collect();
            let listed: HashSet<&str> = t.keepers.iter().map(|x| x.cid.as_str()).collect();
            assert_eq!(flagged, listed, "{}", t.name);
            assert!(t.keepers.len() <= 3 && t.bubble.len() <= 2);
            let ranks: Vec<usize> = t.keepers.iter().chain(&t.bubble).map(|x| x.rank).collect();
            assert_eq!(ranks, (1..=ranks.len()).collect::<Vec<_>>());
            if let (Some(last), Some(next)) = (t.keepers.last(), t.bubble.first()) {
                assert!(next.surplus <= last.surplus);
            }
            for x in &t.keepers {
                let r = d[&x.cid].reasons.iter().find(|r| r.code == "KEEPER").unwrap();
                assert!(r.text.starts_with(&format!("Keeper #{} of 3", x.rank)), "{}", r.text);
            }
        }
        assert_eq!(plan.ranks[&plan.teams[0].bubble[0].cid], ("Team mine".to_string(), 4));
        assert_eq!(plan.mine().unwrap().keepers.len(), 3);
        assert!(!plan.mine().unwrap().bubble.is_empty());
    }

    #[test]
    fn pool_is_the_redraft_curve_without_rostered_keepers() {
        let (c, v) = std_league(Some(3));
        let d = apply_dynasty(&v, &c, None, None);
        let plan = keeper_plan(&c, &v, &d, 2).unwrap();
        let curve = keeper_slot_curve(&c, &v, &d);
        assert_eq!(plan.pool.iter().map(|p| p.value).collect::<Vec<_>>(), curve);
        let kept: HashSet<&str> = plan.teams.iter().flat_map(|t| &t.keepers).map(|x| x.cid.as_str()).collect();
        assert!(plan.pool.iter().all(|p| !kept.contains(p.cid.as_str())));
        assert!(plan.pool.iter().any(|p| p.team.is_none()) && plan.pool.iter().any(|p| p.team.is_some()));
        assert_eq!(plan.at_overall(1.4).unwrap().cid, plan.pool[0].cid);
        assert_eq!(plan.at_overall(2.6).unwrap().cid, plan.pool[2].cid);
    }

    #[test]
    fn no_plan_outside_keeper_leagues() {
        for keepers in [None, Some(0)] {
            let (c, v) = std_league(keepers);
            let d = apply_dynasty(&v, &c, None, None);
            assert!(keeper_plan(&c, &v, &d, 2).is_none());
        }
    }
}
