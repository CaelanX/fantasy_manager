//! Trade finder by need (`fm trades --need`): `find_needs`, `recommend_need_trades`,
//! `pitch_text`.

use fm_core::models::{LeagueContext, Player, Reason, Recommendation, Status};
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::{Cid, Map};
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::py_sum;
use fm_pycompat::order::{cmp_f64, first_max_by, first_min_by, sort_desc_by};
use rayon::prelude::*;
use smallvec::SmallVec;

use super::engine::{TradeData, TradeEngine, TradeEval};
use super::exploit::median;
use super::greedy::{eligible, greedy_assignment, Pid, SlotSpec};
use super::rec::{diverse, to_rec};
use super::search::{refine_many, top_by, TradeOpts};
use super::*;
use crate::analysis::outlook_values::future_values;
use crate::recommend::base::{keeper_k, keeper_set, player_age};
use crate::recommend::strength::{apply_ranks, apply_strength};
use crate::valuation::dynasty::position_group;

/// What need trades read from `analysis.outlook.Outlook` (implemented by W8's Outlook).
pub trait OutlookView: Sync {
    /// `outlook.recommended`: contend / balanced / rebuild (dynasty) or a label (redraft)
    fn recommended(&self) -> Option<&str>;
    /// `outlook.direction(team_id)`: a team's recommended mode / label
    fn direction(&self, team_id: &str) -> Option<&str>;
}

/// A starting slot type I should trade for (`gap` = my starters' mean VORP there minus the
/// league median; -inf: an empty slot; `long_term`: from future value).
#[derive(Clone, Debug, PartialEq)]
pub struct Need {
    pub slot: String,
    pub gap: f64,
    pub text: String,
    pub long_term: bool,
}

/// `_GROUP_SLOTS`.
fn group_slots(g: &str) -> &'static [&'static str] {
    match g {
        "G" => &["G"],
        "D" => &["D"],
        _ => &["F", "C", "LW", "RW", "UTIL"],
    }
}

/// `_slot_vorps(players, values, shape)`: slot type -> mean VORP of the starters there (-inf when
/// a slot of that type is empty); greedy lineup over `players` in the given order.
pub(crate) fn slot_vorps(players: &[&Player], values: &Map<Cid, PlayerValue>, ctx: &LeagueContext) -> Map<String, f64> {
    let spec = SlotSpec::new(&ctx.roster_shape);
    let fpg: Vec<f64> = players.iter().map(|p| values.get(&p.cid).map_or(0.0, |pv| pv.fpg_season)).collect();
    let elig: Vec<u16> = players.iter().map(|p| spec.elig_mask(p)).collect();
    let ids: Vec<Pid> = (0..players.len() as Pid).collect();
    let held = greedy_assignment(&ids, &spec, &fpg, &elig);
    let mut out: Map<String, f64> = Map::new();
    for (ti, slot) in spec.types.iter().enumerate() {
        if slot == "UTIL" {
            continue;
        }
        let cids = &held[ti];
        if (cids.len() as i64) < ctx.roster_shape.get(slot).copied().unwrap_or(0) {
            out.insert(slot.clone(), f64::NEG_INFINITY);
        } else {
            let vs: Vec<f64> = cids.iter().filter_map(|&i| values.get(&players[i as usize].cid)).map(|pv| pv.vorp).collect();
            out.insert(slot.clone(), if vs.is_empty() { f64::NEG_INFINITY } else { py_sum(vs.iter().copied()) / vs.len() as f64 });
        }
    }
    out
}

fn recommended_dir<'o>(ctx: &LeagueContext, outlook: Option<&'o dyn OutlookView>) -> Option<String> {
    outlook
        .and_then(|o| o.recommended())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| ctx.dynasty.then(|| ctx.dynasty_mode.as_str().to_string()))
}

/// My weakest starting slot types this season (vs the league median, at most NEED_SLOTS) and, in
/// dynasty leagues heading to rebuild / balanced, the position group whose future value ranks in
/// the league's bottom third. Keeper leagues seed the groups in (F, D, G) order (Python iterates a
/// set there, whose order is not deterministic).
pub fn find_needs(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    outlook: Option<&dyn OutlookView>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
) -> Vec<Need> {
    let per_team: Vec<(String, Map<String, f64>)> =
        ctx.teams.iter().map(|t| (t.team_id.clone(), slot_vorps(&t.players(), values, ctx))).collect();
    let my_id = ctx.my_team().map(|t| t.team_id.clone()).unwrap_or_default();
    let mine = per_team.iter().find(|(tid, _)| *tid == my_id).map(|(_, m)| m.clone()).unwrap_or_default();
    let mut needs: Vec<Need> = Vec::new();
    for (slot, &v) in &mine {
        let med = median(per_team.iter().map(|(_, x)| x.get(slot).copied().unwrap_or(f64::NEG_INFINITY))).unwrap_or(0.0);
        let gap = v - med;
        let text = if v == f64::NEG_INFINITY {
            format!("an empty {slot} slot")
        } else {
            format!("{slot}: your starters' VORP {} vs the league median {}", fixed(v, 2, true), fixed(med, 2, true))
        };
        needs.push(Need { slot: slot.clone(), gap, text, long_term: false });
    }
    needs.sort_by(|a, b| cmp_f64(a.gap, b.gap));
    let mut out: Vec<Need> = needs.iter().filter(|n| n.gap < 0.0).take(NEED_SLOTS).cloned().collect();
    if out.is_empty() {
        out = needs.iter().take(1).cloned().collect();
    }
    let direction = recommended_dir(ctx, outlook);
    let dv = dynasty_values.filter(|d| !d.is_empty());
    let (Some(dv), true) = (dv, ctx.dynasty && matches!(direction.as_deref(), Some("rebuild") | Some("balanced"))) else {
        return out;
    };
    let mut totals: Map<&'static str, Map<String, f64>> = Map::new();
    let k = keeper_k(ctx, Some(dv));
    if k.is_some() {
        let present: Vec<&'static str> = ctx.teams.iter().flat_map(|t| t.players_iter()).map(position_group).collect();
        for g in ["F", "D", "G"] {
            if present.contains(&g) {
                totals.insert(g, ctx.teams.iter().map(|t| (t.team_id.clone(), 0.0)).collect());
            }
        }
    }
    for t in &ctx.teams {
        let players: Vec<&Player> = match k {
            None => t.players(),
            Some(k) => keeper_set(t.players_iter(), dv, k),
        };
        for p in players {
            let f = future_values(p, ctx, values, Some(dv), None);
            let g = position_group(p);
            *totals.entry(g).or_default().entry(t.team_id.clone()).or_insert(0.0) += py_sum(f.iter().copied()) / f.len() as f64;
        }
    }
    for (g, by_team) in &totals {
        let mut ranked: Vec<(&String, f64)> = by_team.iter().map(|(t, v)| (t, *v)).collect();
        ranked.sort_by(|a, b| cmp_f64(-a.1, -b.1));
        let Some(mine_v) = by_team.get(&my_id).copied() else { continue };
        let pos = match k {
            None => ranked.iter().position(|(t, _)| **t == my_id).unwrap_or(0),
            Some(_) => by_team.values().filter(|v| **v > mine_v).count(),
        };
        if (pos as f64) < ranked.len() as f64 * (1.0 - LONG_TERM_BOTTOM) {
            continue;
        }
        let Some(slot) = group_slots(g).iter().find(|s| ctx.roster_shape.contains_key(**s)) else { continue };
        if out.iter().any(|n| n.slot == *slot) {
            continue;
        }
        let what = match k {
            None => "future value".to_string(),
            Some(k) => format!("keepers' future value (top {k})"),
        };
        out.push(Need {
            slot: slot.to_string(),
            gap: -1.0,
            text: format!("{g} long term: your {what} ranks #{} of {}", pos + 1, ranked.len()),
            long_term: true,
        });
    }
    out
}

fn direction_of(team_id: &str, outlook: Option<&dyn OutlookView>, standing: &Map<String, String>) -> Option<String> {
    let d = outlook.and_then(|o| o.direction(team_id)).filter(|s| !s.is_empty());
    d.map(str::to_string).or_else(|| match standing.get(team_id).map(|s| s.as_str()) {
        Some("contender") => Some("contend".to_string()),
        Some("rebuilder") => Some("rebuild".to_string()),
        _ => None,
    })
}

fn complementary(a: Option<&str>, b: Option<&str>) -> bool {
    let win = |x: &str| x == "contend" || x == "contender";
    let later = |x: &str| x == "rebuild" || x == "pretender";
    match (a, b) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => (win(a) && later(b)) || (later(a) && win(b)),
        _ => false,
    }
}

/// `pitch_text(ev, need, mine_dir, their_dir, ctx, values, dynasty_values)`: a short message to
/// send the other manager. It never quotes our model's edge.
pub fn pitch_text(
    ev: &TradeEval<'_>,
    need: Option<&Need>,
    mine_dir: Option<&str>,
    their_dir: Option<&str>,
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
) -> String {
    let tid = ev.team.team_id.as_str();
    let give: Vec<String> = ev
        .give
        .iter()
        .map(|p| p.name.clone())
        .chain(ev.give_picks.iter().map(|a| format!("my {} pick", a.short(Some(&a.holder)))))
        .collect();
    let get: Vec<String> =
        ev.get.iter().map(|p| p.name.clone()).chain(ev.get_picks.iter().map(|a| format!("your {} pick", a.short(Some(tid))))).collect();
    let fpg = |p: &Player| values.get(&p.cid).map_or(0.0, |pv| pv.fpg_season);
    let lead: Option<&Player> = first_max_by(ev.give.iter().copied(), |a, b| cmp_f64(fpg(a), fpg(b)));
    let mut parts = vec![format!("Hi {}, I'd send {} for {}.", ev.team.name, give.join(" + "), get.join(" + "))];
    let their = their_dir.unwrap_or("");
    if (their == "contend" || their == "contender") && lead.is_some() && ev.delta_them > 0.0 {
        let l = lead.expect("lead");
        parts.push(format!(
            "You're in the race this season: {} ({} pts/game) makes your lineup better right now.",
            l.name,
            fixed(fpg(l), 1, false)
        ));
    } else if (their == "rebuild" || their == "pretender") && !ev.give.is_empty() {
        let age_or = |p: &Player| match player_age(p, ctx, dynasty_values) {
            Some(a) if a != 0.0 => a,
            _ => 99.0,
        };
        let young = first_min_by(ev.give.iter().copied(), |a, b| cmp_f64(age_or(a), age_or(b))).expect("give");
        let age = player_age(young, ctx, dynasty_values);
        let mid = age.map_or(String::new(), |a| format!(" is {} and", fixed(a, 0, false)));
        parts.push(format!("You're building for later: {}{mid} fits your timeline.", young.name));
    } else if ev.give.len() > ev.get.len() && !ev.give.is_empty() {
        let ns: Vec<&str> = ev.give.iter().map(|p| p.name.as_str()).collect();
        parts.push(format!("Two everyday players for one: {} give you depth across your lineup.", ns.join(" and ")));
    } else if ev.give.len() < ev.get.len() && lead.is_some() {
        parts.push(format!("You get the best player in the deal ({}) and a free roster spot.", lead.expect("lead").name));
    } else if let Some(l) = lead {
        parts.push(format!("{} has been good for {} pts/game.", l.name, fixed(fpg(l), 1, false)));
    }
    if let Some(nt) = &ev.need_text {
        let slot: Option<&str> = nt.split_once("weakest slot ").map(|(_, rest)| rest.split(' ').next().unwrap_or(rest));
        let fits = ev.give.iter().copied().find(|p| slot.is_some_and(|s| !s.is_empty() && eligible(&p.positions, s))).or(lead);
        if let (Some(f), Some(s)) = (fits, slot.filter(|s| !s.is_empty())) {
            parts.push(format!("{} slots right into your weakest spot ({s}).", f.name));
        }
    }
    if !ev.give_picks.is_empty() {
        parts.push("The pick is there to make it worth your while.".to_string());
    }
    if let Some(n) = need {
        let what = if n.gap == f64::NEG_INFINITY { "an empty spot" } else { "a hole" };
        let when = if n.long_term { " for the next few seasons" } else { "" };
        match mine_dir {
            Some("contend") | Some("contender") => parts.push(format!("I'm going for it this season and have {what} at {}.", n.slot)),
            Some("rebuild") => parts.push(format!("I'm building for the next few seasons and want to shore up {}.", n.slot)),
            _ => parts.push(format!("I'm looking to shore up {}{when}.", n.slot)),
        }
    }
    parts.join(" ")
}

/// The PITCH reason's text of a need trade, else None.
pub fn pitch_of(r: &Recommendation) -> Option<&str> {
    r.reasons.iter().find(|x| x.code == "PITCH").map(|x| x.text.as_str())
}

type Shape = (SmallVec<[Pid; 2]>, SmallVec<[Pid; 2]>);

/// Trades aimed at my needs (`find_needs`), scored by the regular evaluator; ranking = score x
/// COMPLEMENT_MULT (partner heading the other way) x STAR_MULT (star consolidation). Python
/// defaults: `limit` 5, `per_team` NEED_PER_TEAM; `opts.limit` / `max_per_team` are not used.
pub fn recommend_need_trades(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    opts: &TradeOpts<'_>,
    outlook: Option<&dyn OutlookView>,
    limit: usize,
    per_team: usize,
    needs: Option<Vec<Need>>,
) -> anyhow::Result<Vec<Recommendation>> {
    let needs = match needs {
        Some(n) => n,
        None => find_needs(ctx, values, outlook, opts.dynasty_values),
    };
    if needs.is_empty() {
        return Ok(Vec::new());
    }
    let data = TradeData::new(ctx, values, opts.dynasty_values);
    let eng = TradeEngine::new(&data, &opts.engine)?;
    let me = &eng.sides[eng.me];
    let me_all: Vec<&Player> = me.active.iter().chain(me.ir.iter()).map(|&p| eng.p(p)).collect();
    let mine_slot = slot_vorps(&me_all, values, ctx);
    let mine_dir = recommended_dir(ctx, outlook);
    let floor = eng.params.perceived_for(MIN_P_ACCEPT) - NEED_PTS - BLOCK_PTS;
    let vorp = |p: &Player| values.get(&p.cid).map_or(0.0, |pv| pv.vorp);
    let fills = |p: &Player| -> Option<usize> {
        needs.iter().position(|n| {
            eligible(&p.positions, &n.slot)
                && !matches!(p.status, Status::Out | Status::Ir | Status::Ltir | Status::Suspended)
                && (n.long_term || vorp(p) > mine_slot.get(&n.slot).copied().unwrap_or(f64::NEG_INFINITY))
        })
    };
    let picks = !eng.picks().is_empty();
    let teams: Vec<usize> = (0..ctx.teams.len()).filter(|&i| i != eng.me).collect();
    let rows: Vec<Vec<(f64, TradeEval<'_>, usize)>> = teams
        .par_iter()
        .map(|&si| {
            let side = &eng.sides[si];
            let mut gets: Vec<Pid> =
                side.active.iter().chain(side.ir.iter()).copied().filter(|&p| eng.v(p) > 0.0 && fills(eng.p(p)).is_some()).collect();
            sort_desc_by(&mut gets, |a, b| cmp_f64(eng.v(*a), eng.v(*b)));
            gets.truncate(NEED_GET_N);
            if gets.is_empty() {
                return Vec::new();
            }
            let mut theirs: Vec<Pid> = gets.clone();
            for &p in &side.top {
                if !theirs.contains(&p) {
                    theirs.push(p);
                }
            }
            let mut shapes: Vec<Shape> = Vec::new();
            for &a in &me.top {
                for &b in &gets {
                    shapes.push((smallvec::smallvec![a], smallvec::smallvec![b]));
                }
            }
            for i in 0..me.top.len() {
                for j in i + 1..me.top.len() {
                    for &b in &gets {
                        shapes.push((smallvec::smallvec![me.top[i], me.top[j]], smallvec::smallvec![b]));
                    }
                }
            }
            for &a in &me.top {
                for i in 0..theirs.len() {
                    for j in i + 1..theirs.len() {
                        if gets.contains(&theirs[i]) || gets.contains(&theirs[j]) {
                            shapes.push((smallvec::smallvec![a], smallvec::smallvec![theirs[i], theirs[j]]));
                        }
                    }
                }
            }
            let mut cands: Vec<(Shape, f64)> = shapes
                .into_iter()
                .filter_map(|s| {
                    let base = eng.pkg(&s.0) - eng.pkg(&s.1);
                    (floor - 1e-9 <= base && base <= OVERPAY_CAP + 1e-9).then_some((s, base))
                })
                .collect();
            cands.sort_by(|a, b| (a.0 .0.len() + a.0 .1.len()).cmp(&(b.0 .0.len() + b.0 .1.len())).then(cmp_f64(a.1.abs(), b.1.abs())));
            cands.truncate(MAX_EVALS_PER_TEAM);
            let evals: Vec<TradeEval<'_>> = cands.par_iter().map(|((give, get), _)| eng.evaluate(si, give, get, true, false, None)).collect();
            let mut near: Vec<TradeEval<'_>> = Vec::new();
            let mut refused: Vec<TradeEval<'_>> = Vec::new();
            for ev in evals {
                if ev.near_plausible() {
                    near.push(ev);
                } else if picks && !ev.screened && ev.blocked.is_none() && ev.my_illegal.is_none() {
                    refused.push(ev);
                }
            }
            sort_desc_by(&mut near, |a, b| cmp_f64(a.score(), b.score()));
            let first: Vec<usize> = (0..near.len().min(REFINE_PER_TEAM)).collect();
            refine_many(&eng, &mut near, &first);
            let mut ok: Vec<TradeEval<'_>> = near.into_iter().filter(|e| (e.refined || !eng.exact()) && e.accepted()).collect();
            if picks {
                // tries: ok overpays by score, then refusals by my edge; the first PICK_TRIES_PER_TEAM
                let mut tries: Vec<(bool, usize)> = top_by(&ok, (0..ok.len()).filter(|&i| ok[i].perceived() > PICK_ASK_MIN), usize::MAX, |e| e.score())
                    .into_iter()
                    .map(|i| (true, i))
                    .collect();
                tries.extend(top_by(&refused, 0..refused.len(), usize::MAX, |e| e.my_edge()).into_iter().map(|i| (false, i)));
                tries.truncate(PICK_TRIES_PER_TEAM);
                let vars: Vec<((bool, usize), Option<TradeEval<'_>>)> = tries
                    .par_iter()
                    .map(|&(in_ok, i)| {
                        let mut e = if in_ok { ok[i].clone() } else { refused[i].clone() };
                        eng.refine(&mut e);
                        ((in_ok, i), eng.with_pick(&e))
                    })
                    .collect();
                for ((in_ok, i), var) in vars {
                    if let Some(var) = var {
                        if in_ok {
                            ok[i] = var;
                        } else {
                            ok.push(var);
                        }
                    }
                }
            }
            let their_dir = direction_of(&side.team.team_id, outlook, &eng.standing);
            let comp = complementary(mine_dir.as_deref(), their_dir.as_deref());
            let mut team_rows: Vec<(f64, TradeEval<'_>, usize)> = Vec::new();
            for e in ok {
                if !e.listable() {
                    continue;
                }
                let Some(need) = e.get.iter().find_map(|p| fills(p)) else { continue };
                let rank = e.score() * if comp { COMPLEMENT_MULT } else { 1.0 } * if is_star(&eng, &e) { STAR_MULT } else { 1.0 };
                team_rows.push((rank, e, need));
            }
            sort_desc_by(&mut team_rows, |a, b| cmp_f64(a.0, b.0));
            team_rows.truncate(per_team);
            team_rows
        })
        .collect();
    let mut picked: Vec<(f64, TradeEval<'_>, usize)> = rows.into_iter().flatten().collect();
    sort_desc_by(&mut picked, |a, b| cmp_f64(a.0, b.0));
    let evs: Vec<&TradeEval<'_>> = picked.iter().map(|(_, e, _)| e).collect();
    let chosen = diverse(&evs, limit, per_team, MAX_PER_GIVEN, MAX_PER_RECEIVED);
    let mut out: Vec<Recommendation> = Vec::new();
    for i in chosen {
        let (_, e, ni) = &picked[i];
        let need = &needs[*ni];
        let their_dir = direction_of(&e.team.team_id, outlook, &eng.standing);
        let mut r = to_rec(e, eng.v_map(), eng.units(), eng.source(), Some(&eng.market));
        let incoming = e.get.iter().copied().find(|p| eligible(&p.positions, &need.slot)).unwrap_or(e.get[0]);
        let mut extra = vec![Reason {
            code: "NEED_MATCH".to_string(),
            text: format!("Fills your need at {}: {} ({})", need.slot, incoming.name, need.text),
            value: if need.gap == f64::NEG_INFINITY { None } else { Some(need.gap) },
            baseline: None,
        }];
        if complementary(mine_dir.as_deref(), their_dir.as_deref()) {
            extra.push(Reason::new(
                "COMPLEMENT",
                format!(
                    "{} is heading the other way ({} vs your {}): you value these players differently, which makes a deal easier",
                    e.team.name,
                    their_dir.as_deref().unwrap_or("None"),
                    mine_dir.as_deref().unwrap_or("None")
                ),
            ));
        }
        if is_star(&eng, e) {
            extra.push(Reason::new(
                "STAR_CONSOLIDATION",
                format!(
                    "Star consolidation: you get the best player in the deal ({}) and free a roster spot; trade calculators tend to undervalue the star side of a 2-for-1",
                    e.get[0].name
                ),
            ));
        }
        extra.push(Reason::new(
            "PITCH",
            pitch_text(e, Some(need), mine_dir.as_deref(), their_dir.as_deref(), ctx, values, opts.dynasty_values),
        ));
        let at = 2.min(r.reasons.len());
        r.reasons.splice(at..at, extra);
        out.push(r);
    }
    apply_strength(&mut out);
    apply_ranks(&mut out);
    Ok(out)
}

/// 2-for-1 where I get the best player in the deal (by trade value V).
fn is_star(eng: &TradeEngine<'_, '_>, e: &TradeEval<'_>) -> bool {
    e.give_ix.len() >= 2
        && e.get_ix.len() == 1
        && eng.v(e.get_ix[0]) > e.give_ix.iter().map(|&p| eng.v(p)).fold(f64::NEG_INFINITY, f64::max)
}
