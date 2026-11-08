//! Keeper planner views (keeper leagues): the Roster page's keeper panel, the /keepers page and
//! [`keeper_status`] for the player card. Everything comes from `fm_engine::valuation::keepers`;
//! outside keeper leagues the plan is None and the views say so.

use std::sync::Arc;

use fm_core::models::FantasyTeam;
use fm_core::values::PickAsset;
use fm_core::Map;
use fm_engine::valuation::keepers::{keeper_plan, KeeperCandidate, KeeperPlan};
use fm_engine::valuation::picks::{picks_by_holder, value_picks};
use serde_json::{json, Value};

use crate::views::LoadResult;

/// non-keepers listed behind each team's keepers
pub const BUBBLE: usize = 2;
/// draft pool players shown
pub const POOL_SHOWN: usize = 30;

/// The memoised keeper plan (None outside keeper leagues or without dynasty values).
pub fn plan(res: &LoadResult) -> Arc<Option<KeeperPlan>> {
    res.memo.get_or("keepers:plan", || res.dynasty.as_ref().and_then(|d| keeper_plan(&res.ctx, &res.values, d, BUBBLE)))
}

fn cand(c: &KeeperCandidate) -> Value {
    json!({"cid": c.cid, "name": c.name, "age": c.age, "rank": c.rank, "surplus": c.surplus})
}

/// One team's keepers and bubble; each bubble player carries `gap` (keeper surplus behind the
/// team's last keeper) and `behind` (that keeper's name).
fn team_value(plan: &KeeperPlan, team_id: &str) -> Value {
    let Some(t) = plan.teams.iter().find(|t| t.team_id == team_id) else { return Value::Null };
    let last = t.keepers.last();
    let bubble: Vec<Value> = t
        .bubble
        .iter()
        .map(|c| {
            let mut v = cand(c);
            v["gap"] = json!(last.map(|l| l.surplus - c.surplus));
            v["behind"] = json!(last.map(|l| &l.name));
            v
        })
        .collect();
    json!({"team_id": t.team_id, "name": t.name, "me": t.me, "keepers": t.keepers.iter().map(cand).collect::<Vec<_>>(),
           "bubble": bubble, "cids": t.keepers.iter().map(|c| &c.cid).collect::<Vec<_>>()})
}

/// The Roster page panel for `team` (null outside keeper leagues): `k` and the team's keepers.
pub fn roster_panel(res: &LoadResult, team: Option<&FantasyTeam>) -> Value {
    let p = plan(res);
    let (Some(plan), Some(team)) = (p.as_ref(), team) else { return Value::Null };
    let mut v = team_value(plan, &team.team_id);
    v["k"] = json!(plan.k);
    v
}

/// The /keepers page: `plan` null outside keeper leagues, else {k, teams (mine first), pool (top
/// [`POOL_SHOWN`], with overall pick), pool_size, picks (mine, with the pool player at each
/// next-draft pick's expected overall)}.
pub fn keepers_page(res: &LoadResult) -> Value {
    let p = plan(res);
    let Some(plan) = p.as_ref() else { return json!({"plan": null}) };
    let teams: Vec<Value> = plan.teams.iter().map(|t| team_value(plan, &t.team_id)).collect();
    let pool: Vec<Value> = plan
        .pool
        .iter()
        .take(POOL_SHOWN)
        .enumerate()
        .map(|(i, x)| json!({"overall": i + 1, "cid": x.cid, "name": x.name, "age": x.age, "team": x.team, "value": x.value}))
        .collect();
    let picks: Vec<Value> = my_picks(res)
        .iter()
        .map(|a| {
            let land = if a.years_ahead == 0 { plan.at_overall(a.overall) } else { None };
            json!({"label": a.short(Some(&a.holder)), "year": a.year, "overall": a.overall, "value": a.value, "net": a.net,
                   "inferred": a.inferred, "land": land.map(|x| json!({"cid": x.cid, "name": x.name}))})
        })
        .collect();
    json!({"plan": {"k": plan.k, "teams": teams, "pool": pool, "pool_size": plan.pool.len(), "picks": picks}})
}

/// My draft picks by year and round (the Roster page's memoised pick values).
fn my_picks(res: &LoadResult) -> Vec<PickAsset> {
    let Some(me) = crate::views::my_team(&res.ctx) else { return vec![] };
    if res.dynasty.is_none() || res.ctx.draft_picks.is_empty() {
        return vec![];
    }
    let by_holder: Arc<Map<String, Vec<PickAsset>>> =
        res.memo.get_or("draft_picks", || picks_by_holder(value_picks(&res.ctx, &res.values, res.dynasty.as_ref())));
    let mut held = by_holder.get(&me.team_id).cloned().unwrap_or_default();
    held.sort_by_key(|a| (a.year, a.round));
    held
}

/// Keeper status of one player for the player card, None outside keeper leagues or for an
/// unvalued player: {"keeper", "rank" (place on his team by keeper surplus), "of" (keepers per
/// team), "team", "surplus"}. Free agents: rank and team null, keeper = he would crack my top K.
pub fn keeper_status(res: &LoadResult, cid: &str) -> Option<Value> {
    let p = plan(res);
    let plan = p.as_ref().as_ref()?;
    let dv = res.dynasty.as_ref()?.get(cid)?;
    let (team, rank) = match plan.ranks.get(cid) {
        Some((t, r)) => (Some(t), Some(*r)),
        None => (None, None),
    };
    Some(json!({"keeper": dv.keeper, "rank": rank, "of": plan.k, "team": team, "surplus": dv.keeper_surplus}))
}
