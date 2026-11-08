//! Targeted trade searches around one player: `shop_player` (what each team would give for one of
//! mine; `fm trades --shop`, the Trades page's "Shop a player") and `acquire_player` (what it
//! takes to get one of theirs; `fm trades --target`, "Get a player"). Both enumerate small
//! packages, score them with the regular evaluator and keep what the engine itself would offer.

use fm_core::models::{LeagueContext, Recommendation};
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::{Cid, CoreError, Map};
use fm_pycompat::order::{cmp_f64, sort_desc_by};
use rayon::prelude::*;
use smallvec::{smallvec, SmallVec};

use super::engine::{EngineOpts, TradeData, TradeEngine, TradeEval};
use super::greedy::Pid;
use super::offer::{counter_pick, verdict, OfferEval};
use super::rec::{diverse, to_rec};
use super::search::{refine_many, top_by};
use super::*;
use crate::recommend::strength::{apply_ranks, apply_strength};

/// packages per team scored by the fast (greedy, screened) evaluator, closest to even first
pub const TARGET_EVALS: usize = 300;
/// deals listed per counterparty when shopping a player
pub const TARGET_PER_TEAM: usize = 2;
/// "they'd likely accept": acquire packages at or above this chance rank ahead of the rest
pub const TARGET_LIKELY_P: f64 = 0.5;

type Shape = (SmallVec<[Pid; 2]>, SmallVec<[Pid; 2]>);

fn lookup(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CoreError::Lookup(msg.into()))
}

/// Shapes whose market gap could still reach MIN_P_ACCEPT with every bonus (`candidates`' window),
/// smallest and closest to even first, capped at TARGET_EVALS.
fn bounded(eng: &TradeEngine<'_, '_>, shapes: Vec<Shape>) -> Vec<Shape> {
    let floor = eng.params.perceived_for(MIN_P_ACCEPT) - NEED_PTS - BLOCK_PTS;
    let mut out: Vec<(Shape, f64)> = shapes
        .into_iter()
        .filter_map(|s| {
            let base = eng.pkg(&s.0) - eng.pkg(&s.1);
            (floor - 1e-9 <= base && base <= OVERPAY_CAP + 1e-9).then_some((s, base))
        })
        .collect();
    out.sort_by(|a, b| (a.0 .0.len() + a.0 .1.len()).cmp(&(b.0 .0.len() + b.0 .1.len())).then(cmp_f64(a.1.abs(), b.1.abs())));
    out.truncate(TARGET_EVALS);
    out.into_iter().map(|(s, _)| s).collect()
}

fn roster(eng: &TradeEngine<'_, '_>, si: usize) -> Vec<Pid> {
    let s = &eng.sides[si];
    s.active.iter().chain(s.ir.iter()).copied().collect()
}

/// Drop a deal when a smaller one inside it (same team, a subset of its players and picks) is
/// listed and does at least as well by `as_good`.
fn drop_padded<'a>(evs: Vec<TradeEval<'a>>, as_good: impl Fn(&TradeEval<'a>, &TradeEval<'a>) -> bool) -> Vec<TradeEval<'a>> {
    let size = |e: &TradeEval<'_>| e.give_ix.len() + e.get_ix.len() + e.give_picks.len() + e.get_picks.len();
    let inside = |small: &TradeEval<'_>, big: &TradeEval<'_>| {
        std::ptr::eq(small.team, big.team)
            && size(small) < size(big)
            && small.give_ix.iter().all(|p| big.give_ix.contains(p))
            && small.get_ix.iter().all(|p| big.get_ix.contains(p))
            && small.give_picks.iter().all(|a| big.give_picks.iter().any(|b| b.key() == a.key()))
            && small.get_picks.iter().all(|a| big.get_picks.iter().any(|b| b.key() == a.key()))
    };
    let keep: Vec<bool> = evs.iter().map(|e| !evs.iter().any(|s| inside(s, e) && as_good(s, e))).collect();
    evs.into_iter().zip(keep).filter(|(_, k)| *k).map(|(e, _)| e).collect()
}

/// The listed deals as offers: recommendation (ranked in this order), verdict, counter.
fn offers<'a>(eng: &TradeEngine<'a, '_>, evs: Vec<TradeEval<'a>>) -> Vec<OfferEval<'a>> {
    let mut recs: Vec<Recommendation> = evs.iter().map(|e| to_rec(e, eng.v_map(), eng.units(), eng.source(), Some(&eng.market))).collect();
    apply_strength(&mut recs);
    apply_ranks(&mut recs);
    evs.into_iter()
        .zip(recs)
        .map(|(ev, rec)| {
            let (v, why) = verdict(&ev);
            let counter = if v == "counter" {
                counter_pick(eng, &ev).map(|a| format!("Ask {} to add their {} pick", ev.team.name, a.short(Some(&ev.team.team_id))))
            } else {
                None
            };
            OfferEval { ev, rec, verdict: v, why, counter, units: eng.units() }
        })
        .collect()
}

fn shop_team<'a>(eng: &TradeEngine<'a, '_>, si: usize, x: Pid) -> Vec<TradeEval<'a>> {
    let theirs = roster(eng, si);
    let xw = eng.pkg(&[x]);
    let addons: Vec<Pid> = roster(eng, eng.me).into_iter().filter(|&a| a != x && eng.pkg(&[a]) < xw).collect();
    let mut shapes: Vec<Shape> = Vec::new();
    for (i, &b) in theirs.iter().enumerate() {
        shapes.push((smallvec![x], smallvec![b]));
        for &c in &theirs[i + 1..] {
            shapes.push((smallvec![x], smallvec![b, c]));
        }
        for &a in &addons {
            shapes.push((smallvec![x, a], smallvec![b]));
        }
    }
    let cands = bounded(eng, shapes);
    let evals: Vec<TradeEval<'a>> = cands.par_iter().map(|(give, get)| eng.evaluate(si, give, get, true, false, None)).collect();
    let mut near: Vec<TradeEval<'a>> = evals.into_iter().filter(|e| e.near_plausible()).collect();
    sort_desc_by(&mut near, |a, b| cmp_f64(a.score(), b.score()));
    let first: Vec<usize> = (0..near.len().min(REFINE_PER_TEAM)).collect();
    refine_many(eng, &mut near, &first);
    let mut ok: Vec<TradeEval<'a>> = near.into_iter().filter(|e| (e.refined || !eng.exact()) && e.accepted()).collect();
    if !eng.picks().is_empty() {
        // they would still say yes with more: ask for one of their picks on top
        let asks = top_by(&ok, (0..ok.len()).filter(|&i| ok[i].perceived() > PICK_ASK_MIN), PICK_TRIES_PER_TEAM, |e| e.score());
        let vars: Vec<(usize, Option<TradeEval<'a>>)> = asks.par_iter().map(|&i| (i, eng.with_pick(&ok[i]))).collect();
        for (i, var) in vars {
            if let Some(var) = var {
                ok[i] = var;
            }
        }
    }
    ok.retain(|e| e.listable());
    let mut ok = drop_padded(ok, |s, e| s.score() >= e.score() - 1e-12);
    sort_desc_by(&mut ok, |a, b| cmp_f64(a.score(), b.score()));
    ok.truncate(TARGET_PER_TEAM);
    ok
}

/// What each other team would give for my player `cid`: 1 or 2 of their players (plus one of their
/// draft picks where they would still say yes) for him, or for him and one smaller add-on of mine.
/// Kept: what the trade finder would list (`accepted` and `listable`); ranked by its score (my edge x
/// the chance they accept, less any market overpay), at most TARGET_PER_TEAM per team, `limit` overall.
pub fn shop_player<'a>(
    ctx: &'a LeagueContext,
    values: &'a Map<Cid, PlayerValue>,
    cid: &str,
    dynasty_values: Option<&'a Map<Cid, DynastyValue>>,
    opts: &EngineOpts<'_>,
    limit: usize,
) -> anyhow::Result<Vec<OfferEval<'a>>> {
    let data = TradeData::new(ctx, values, dynasty_values);
    let eng = TradeEngine::new(&data, opts)?;
    let Some(x) = eng.pid_of(cid).filter(|p| roster(&eng, eng.me).contains(p)) else {
        return Err(lookup(format!("'{cid}' is not on your roster")));
    };
    let teams: Vec<usize> = (0..ctx.teams.len()).filter(|&i| i != eng.me).collect();
    let mut all: Vec<TradeEval<'_>> = teams.par_iter().map(|&si| shop_team(&eng, si, x)).flatten().collect();
    sort_desc_by(&mut all, |a, b| cmp_f64(a.score(), b.score()));
    all.truncate(limit);
    Ok(offers(&eng, all))
}

/// What it takes to get another team's player `cid`: 1 or 2 of my players (plus one of my draft
/// picks when that is what gets a yes) for him. Kept: not a decline by `verdict`, my edge above 0 and
/// at least MIN_P_ACCEPT to be accepted. Ranked cheapest-likely first: deals at TARGET_LIKELY_P or
/// better ahead, then by my edge (the less I pay, the more I keep), then by the chance; at most
/// MAX_PER_GIVEN per player of mine, `limit` overall.
pub fn acquire_player<'a>(
    ctx: &'a LeagueContext,
    values: &'a Map<Cid, PlayerValue>,
    cid: &str,
    dynasty_values: Option<&'a Map<Cid, DynastyValue>>,
    opts: &EngineOpts<'_>,
    limit: usize,
) -> anyhow::Result<Vec<OfferEval<'a>>> {
    let data = TradeData::new(ctx, values, dynasty_values);
    let eng = TradeEngine::new(&data, opts)?;
    let found = eng.pid_of(cid).and_then(|y| (0..ctx.teams.len()).find(|&i| i != eng.me && roster(&eng, i).contains(&y)).map(|si| (y, si)));
    let Some((y, si)) = found else {
        return Err(lookup(format!("'{cid}' is not on another team's roster")));
    };
    let mine = roster(&eng, eng.me);
    let mut shapes: Vec<Shape> = Vec::new();
    for (i, &a) in mine.iter().enumerate() {
        shapes.push((smallvec![a], smallvec![y]));
        for &b in &mine[i + 1..] {
            shapes.push((smallvec![a, b], smallvec![y]));
        }
    }
    let cands = bounded(&eng, shapes);
    // unscreened: a fair price can leave me less than the trade finder's minimum gain
    let mut evals: Vec<TradeEval<'_>> = cands.par_iter().map(|(give, get)| eng.evaluate(si, give, get, false, false, None)).collect();
    let usable = |e: &TradeEval<'_>| e.blocked.is_none() && e.my_illegal.is_none() && e.my_edge() > 0.0;
    let near: Vec<usize> = (0..evals.len()).filter(|&i| usable(&evals[i]) && evals[i].near_plausible()).collect();
    let mut refine = top_by(&evals, near.iter().copied(), 2 * REFINE_PER_TEAM, |e| e.my_edge());
    refine.extend(top_by(&evals, near.iter().copied(), REFINE_PER_TEAM, |e| e.p()));
    refine_many(&eng, &mut evals, &refine);
    let keep = |e: &TradeEval<'_>| {
        (e.refined || !eng.exact()) && verdict(e).0 != "decline" && e.p() >= MIN_P_ACCEPT - 1e-9 && e.my_edge() > 0.0
    };
    let mut ok: Vec<TradeEval<'_>> = evals.iter().filter(|e| keep(e)).cloned().collect();
    if !eng.picks().is_empty() {
        // a yes that needs a sweetener: add my cheapest pick that gets it (`with_pick`)
        let refused: Vec<usize> = (0..evals.len()).filter(|&i| usable(&evals[i]) && evals[i].p() < MIN_P_ACCEPT).collect();
        let tries = top_by(&evals, refused, PICK_TRIES_PER_TEAM, |e| e.my_edge());
        let vars: Vec<Option<TradeEval<'_>>> = tries
            .par_iter()
            .map(|&i| {
                let mut e = evals[i].clone();
                eng.refine(&mut e);
                eng.with_pick(&e)
            })
            .collect();
        ok.extend(vars.into_iter().flatten().filter(|e| keep(e)));
    }
    let likely = |e: &TradeEval<'_>| e.p() >= TARGET_LIKELY_P - 1e-9;
    let mut ok = drop_padded(ok, |s, e| s.p() >= e.p() - 1e-9 && s.my_edge() >= e.my_edge() - 1e-9);
    ok.sort_by(|a, b| likely(b).cmp(&likely(a)).then(cmp_f64(b.my_edge(), a.my_edge())).then(cmp_f64(b.p(), a.p())));
    let refs: Vec<&TradeEval<'_>> = ok.iter().collect();
    let chosen = diverse(&refs, limit, usize::MAX, MAX_PER_GIVEN, usize::MAX);
    let picked: Vec<TradeEval<'_>> = chosen.into_iter().map(|i| ok[i].clone()).collect();
    Ok(offers(&eng, picked))
}
