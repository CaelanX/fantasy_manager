//! `fm keepers`: the keeper planner (keeper leagues). My predicted keepers and the players on the
//! bubble behind them, every team's predicted keepers, the redraft pool (everyone not predicted
//! kept, by the next-season value the pick model uses) and what my picks could land there.
//! Registered in `cmd_league`'s command list.

use anyhow::Result;
use clap::Args;
use fm_core::values::PickAsset;
use fm_engine::valuation::keepers::{keeper_plan, KeeperCandidate, KeeperPlan, TeamKeepers};
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::round_to;
use serde_json::{json, Value};

use crate::cmd_league::{pick_json, picks_by_holder_of};
use crate::common::*;

/// non-keepers listed behind each team's keepers
const BUBBLE: usize = 2;
/// draft pool players shown
const POOL_SHOWN: usize = 30;

#[derive(Args, Clone, Debug, Default)]
pub struct KeepersArgs {
    #[command(flatten)]
    pub lo: LeagueOpts,
    #[command(flatten)]
    pub mode: ModeOpt,
}

fn age_text(a: Option<f64>) -> String {
    a.map(|x| (x.trunc() as i64).to_string()).unwrap_or_else(|| "-".into())
}

fn cand_json(c: &KeeperCandidate) -> Value {
    json!({"cid": c.cid, "name": c.name, "age": c.age.map(|a| round_to(a, 1)), "rank": c.rank,
           "surplus": round_to(c.surplus, 2)})
}

/// A team's keepers and bubble; bubble players carry `gap` behind the team's last keeper.
fn team_json(t: &TeamKeepers) -> Value {
    let last = t.keepers.last();
    let bubble: Vec<Value> = t
        .bubble
        .iter()
        .map(|c| {
            let mut v = cand_json(c);
            v["gap"] = json!(last.map(|l| round_to(l.surplus - c.surplus, 2)));
            v
        })
        .collect();
    json!({"team": t.name, "team_id": t.team_id, "me": t.me,
           "keepers": t.keepers.iter().map(cand_json).collect::<Vec<_>>(), "bubble": bubble})
}

/// The pool player a next-draft pick would land (later drafts: None).
fn land<'a>(plan: &'a KeeperPlan, a: &PickAsset) -> Option<&'a fm_engine::valuation::keepers::PoolPlayer> {
    if a.years_ahead == 0 {
        plan.at_overall(a.overall)
    } else {
        None
    }
}

pub fn keepers(cli: &mut Cli, a: &KeepersArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let mut d = cli.load_all(&league, false, a.mode.val())?;
    let by_holder = picks_by_holder_of(&mut d.lc, &d.values, d.dyn_values.as_ref());
    let lc = &d.lc;
    let plan = d.dyn_values.as_ref().and_then(|dv| keeper_plan(lc, &d.values, dv, BUBBLE));
    let me = lc.my_team()?;
    let mut picks: Vec<&PickAsset> = by_holder.get(&me.team_id).map(|v| v.iter().collect()).unwrap_or_default();
    picks.sort_by_key(|a| (a.year, a.round));
    let Some(plan) = plan else {
        if as_json {
            dump(&json!({"k": null, "mine": null, "teams": [], "pool": [], "pool_size": 0, "picks": [], "errors": lc.warnings}));
        } else {
            echo(format!("{} is not a keeper league, so there are no keepers to plan.", lc.name));
            footer(lc, &[]);
        }
        return Ok(());
    };
    let pool = plan.pool.iter().take(POOL_SHOWN).enumerate();
    if as_json {
        let picks_v: Vec<Value> = picks
            .iter()
            .map(|a| {
                let mut v = pick_json(a);
                v["label"] = json!(a.short(Some(&me.team_id)));
                v["land"] = json!(land(&plan, a).map(|x| json!({"cid": x.cid, "name": x.name})));
                v
            })
            .collect();
        let pool_v: Vec<Value> = pool
            .map(|(i, x)| json!({"overall": i + 1, "cid": x.cid, "name": x.name, "age": x.age.map(|a| round_to(a, 1)),
                                 "team": x.team, "value": round_to(x.value, 2)}))
            .collect();
        dump(&json!({"k": plan.k, "mine": plan.mine().map(team_json),
                     "teams": plan.teams.iter().map(team_json).collect::<Vec<_>>(),
                     "pool": pool_v, "pool_size": plan.pool.len(), "picks": picks_v, "errors": lc.warnings}));
        return Ok(());
    }
    if let Some(mine) = plan.mine() {
        let mut tbl = Table::new(format!("Your keepers ({}, {} per team)", mine.name, plan.k));
        tbl.cols(&[("#", R), ("Player", L), ("Age", R), ("Keeper surplus", R)]);
        for c in &mine.keepers {
            tbl.row([c.rank.to_string(), c.name.clone(), age_text(c.age), fixed(c.surplus, 1, false)]);
        }
        tbl.print();
        if mine.keepers.is_empty() {
            echo("Nobody on your roster is worth more long term than a waiver-level player.");
        }
        if !mine.bubble.is_empty() {
            let last = mine.keepers.last();
            let parts: Vec<String> = mine
                .bubble
                .iter()
                .map(|c| match last {
                    Some(l) => format!("{} ({} behind {})", c.name, fixed(l.surplus - c.surplus, 1, false), l.name),
                    None => c.name.clone(),
                })
                .collect();
            echo(format!("On the bubble: {}", parts.join("; ")));
        }
    }
    let mut teams = Table::new("Predicted keepers by team");
    teams.cols(&[("Team", L), ("Keepers (surplus)", L), ("Next", L)]);
    for t in &plan.teams {
        let names = |xs: &[KeeperCandidate]| {
            xs.iter().map(|c| format!("{} {}", c.name, fixed(c.surplus, 1, false))).collect::<Vec<_>>().join(", ")
        };
        teams.row([
            format!("{}{}", t.name, if t.me { " (me)" } else { "" }),
            names(&t.keepers),
            t.bubble.first().map(|c| format!("{} {}", c.name, fixed(c.surplus, 1, false))).unwrap_or_default(),
        ]);
    }
    teams.print();
    let mut pt = Table::new(format!("Draft pool preview (best {} of {} not predicted kept)", POOL_SHOWN.min(plan.pool.len()), plan.pool.len()));
    pt.cols(&[("Rank", R), ("Player", L), ("Now", L), ("Age", R), ("Next-season value", R)]);
    for (i, x) in pool {
        pt.row([(i + 1).to_string(), x.name.clone(), x.team.clone().unwrap_or_else(|| "Free agent".into()), age_text(x.age), fixed(x.value, 1, false)]);
    }
    pt.print();
    if !picks.is_empty() {
        let mut kt = Table::new("Your draft picks");
        kt.cols(&[("Pick", L), ("Overall", R), ("Value", R), ("Could land", L)]);
        for a in &picks {
            kt.row([
                format!("{}{}", a.short(Some(&me.team_id)), if a.inferred { " *" } else { "" }),
                fixed(a.overall, 0, false),
                fixed(a.value, 1, false),
                land(&plan, a).map(|x| x.name.clone()).unwrap_or_else(|| "-".into()),
            ]);
        }
        kt.print();
    }
    echo(format!(
        "Predictions from the model's long-term values: keeper surplus is a player's value over the seasons after this \
         one above a waiver-level roster spot; each team keeps its {} highest and everyone else goes into the redraft, \
         ranked by next season's value (the units of `fm picks`). Could land: the pool player at the pick's expected \
         overall spot in the next draft.",
        plan.k
    ));
    footer(lc, &[]);
    Ok(())
}
