//! The Trades page's "Shop a player" (`mode=shop&cid=<mine>`) and "Get a player"
//! (`mode=get&cid=<theirs>`) views: pickers plus the ranked deals of `shop_player` /
//! `acquire_player`, each with a link that opens it in the evaluator (`mode=eval`).

use std::time::Instant;

use fm_core::models::{FantasyTeam, Player};
use fm_core::provider::LeagueProvider;
use fm_core::values::PickAsset;
use fm_core::CoreError;
use fm_engine::recommend::base::gain_week;
use fm_engine::recommend::trades::{
    acquire_player, shop_player, trade_block_offered, trade_block_wants, EngineOpts, OfferEval, DEFAULT_RATE,
};
use fm_pycompat::num::round_to;
use fm_pycompat::order::cmp_f64;
use serde_json::{json, Value};

use crate::app::urlencode;
use crate::views::{pos_str, LoadResult};

/// deals listed on the page
const LIMIT: usize = 10;

fn slot_of<'t>(team: &'t FantasyTeam, cid: &str) -> &'t str {
    team.slots.iter().find(|s| s.player.as_ref().is_some_and(|p| p.cid == cid)).map_or("", |s| s.slot.as_str())
}

/// A player row as the template's `deal_side` and pickers read it.
fn row(res: &LoadResult, team: &FantasyTeam, p: &Player) -> Value {
    let pv = res.values.get(&p.cid);
    json!({"cid": p.cid, "name": p.name, "pos": pos_str(p), "team": p.team.clone().unwrap_or_else(|| "FA".into()),
           "slot": slot_of(team, &p.cid), "status": p.status.as_str(), "fpg": pv.map(|v| v.fpg_season), "vorp": pv.map(|v| v.vorp),
           "dyn": res.dynasty.as_ref().and_then(|d| d.get(&p.cid)).map(|d| d.value)})
}

/// A roster for a picker, best season FPG first.
fn picker_rows(res: &LoadResult, team: &FantasyTeam, chosen: Option<&str>) -> Vec<Value> {
    let mut ps: Vec<&Player> = team.players_iter().collect();
    let key = |p: &Player| res.values.get(&p.cid).map_or(-99.0, |v| v.fpg_season);
    ps.sort_by(|a, b| cmp_f64(key(b), key(a)));
    ps.into_iter()
        .map(|p| {
            let mut r = row(res, team, p);
            r["selected"] = json!(chosen == Some(p.cid.as_str()));
            r
        })
        .collect()
}

fn pick_row(a: &PickAsset, holder: &str) -> Value {
    json!({"key": a.key(), "label": a.short(Some(holder)), "value": a.value, "net": a.net})
}

fn deal(res: &LoadResult, me: &FantasyTeam, o: &OfferEval<'_>) -> Value {
    let ev = &o.ev;
    let tid = ev.team.team_id.as_str();
    let mut q: Vec<(String, String)> = vec![("mode".into(), "eval".into()), ("team".into(), tid.to_string())];
    q.extend(ev.give.iter().map(|p| ("give".to_string(), p.cid.clone())));
    q.extend(ev.get.iter().map(|p| ("get".to_string(), p.cid.clone())));
    q.extend(ev.give_picks.iter().map(|a| ("give_pick".to_string(), a.key())));
    q.extend(ev.get_picks.iter().map(|a| ("get_pick".to_string(), a.key())));
    let rate = ev.rate.unwrap_or(DEFAULT_RATE);
    json!({
        "team": ev.team.name, "team_id": tid,
        "give": ev.give.iter().map(|p| row(res, me, p)).collect::<Vec<_>>(),
        "get": ev.get.iter().map(|p| row(res, ev.team, p)).collect::<Vec<_>>(),
        "give_picks": ev.give_picks.iter().map(|a| pick_row(a, &a.holder)).collect::<Vec<_>>(),
        "get_picks": ev.get_picks.iter().map(|a| pick_row(a, tid)).collect::<Vec<_>>(),
        "my_fpg": round_to(ev.delta_me, 3), "my_week": round_to(gain_week(ev.delta_me, &rate), 2),
        "their_fpg": round_to(ev.delta_them, 3), "my_edge": round_to(ev.my_edge(), 3), "dyn_mode": ev.mode,
        "p": round_to(ev.p(), 3), "verdict": o.verdict, "why": o.why, "counter": o.counter,
        "strength": o.rec.strength,
        "eval_q": urlencode(&q),
    })
}

/// The view of `mode` ("shop" | "get") for `cid` (None or not found: the picker alone).
pub fn trades_mode_page(res: &LoadResult, mode: &str, cid: Option<&str>, provider: Option<&dyn LeagueProvider>) -> Value {
    let ctx = &res.ctx;
    let shop = mode == "shop";
    let mut out = serde_json::Map::new();
    out.insert("cid".into(), json!(cid));
    for k in ["target", "deals", "error", "seconds"] {
        out.insert(k.into(), Value::Null);
    }
    let Some(me) = ctx.teams.iter().find(|t| t.owner_is_me) else {
        out.insert("error".into(), json!("Your team was not found in the league."));
        return Value::Object(out);
    };
    let others: Vec<&FantasyTeam> = ctx.teams.iter().filter(|t| !t.owner_is_me).collect();
    if shop {
        out.insert("my_players".into(), Value::Array(picker_rows(res, me, cid)));
    } else {
        let groups: Vec<Value> =
            others.iter().map(|t| json!({"team": t.name, "players": picker_rows(res, t, cid)})).collect();
        out.insert("groups".into(), Value::Array(groups));
    }
    let owner = |c: &str| -> Option<(&FantasyTeam, &Player)> {
        let pool: Vec<&FantasyTeam> = if shop { vec![me] } else { others.clone() };
        pool.into_iter().find_map(|t| t.players_iter().find(|p| p.cid == c).map(|p| (t, p)))
    };
    let Some((team, p)) = cid.filter(|c| !c.is_empty()).and_then(owner) else {
        if cid.is_some_and(|c| !c.is_empty()) {
            out.insert("error".into(), json!(if shop { "That player is not on your roster." } else { "That player is not on another team's roster." }));
        }
        return Value::Object(out);
    };
    let mut target = row(res, team, p);
    target["owner"] = json!(team.name);
    out.insert("target".into(), target);
    let key = format!("trades_{mode}:{}", p.cid);
    let built = res.memo.get_or(&key, || -> Result<(Vec<Value>, f64), String> {
        let blocks = provider.map(|p| p.extras().trade_blocks.clone()).unwrap_or_default();
        let wants = trade_block_wants(ctx, &blocks);
        let offered = trade_block_offered(&blocks);
        let opts = EngineOpts { wants: wants.as_ref(), offered: offered.as_ref(), ..EngineOpts::default() };
        let t = Instant::now();
        let found = if shop {
            shop_player(ctx, &res.values, &p.cid, res.dynasty.as_ref(), &opts, LIMIT)
        } else {
            acquire_player(ctx, &res.values, &p.cid, res.dynasty.as_ref(), &opts, LIMIT)
        };
        match found {
            Ok(offers) => Ok((offers.iter().map(|o| deal(res, me, o)).collect(), t.elapsed().as_secs_f64())),
            Err(e) => match e.downcast_ref::<CoreError>() {
                Some(CoreError::Lookup(m)) | Some(CoreError::Value(m)) => Err(m.clone()),
                None => panic!("trade search failed: {e:#}"),
            },
        }
    });
    match built.as_ref() {
        Ok((deals, secs)) => {
            out.insert("deals".into(), Value::Array(deals.clone()));
            out.insert("seconds".into(), json!(round_to(*secs, 2)));
        }
        Err(m) => {
            out.insert("error".into(), json!(m));
        }
    }
    Value::Object(out)
}
