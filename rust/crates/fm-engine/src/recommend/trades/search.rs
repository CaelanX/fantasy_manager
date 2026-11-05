//! `recommend_trades`: the candidate search, in parallel.
//!
//! Counterparties and their candidates are evaluated with rayon (`evaluate(exact=false)` is a pure
//! function of the candidate given the shared memos), collected by index, refined in parallel and
//! then merged per team in Python's order before the same stable sorts, so the result is identical
//! to the sequential Python loop.

use fm_core::models::{GainUnits, Recommendation};
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::{Cid, LeagueContext, Map};
use fm_pycompat::order::{cmp_f64, sort_desc_by};
use rayon::prelude::*;

use super::engine::{EngineOpts, TradeData, TradeEngine, TradeEval};
use super::rec::{diverse, to_rec};
use super::*;
use crate::recommend::strength::{apply_ranks, apply_strength};

/// `recommend_trades` options (Python keyword arguments).
#[derive(Clone, Debug)]
pub struct TradeOpts<'o> {
    pub dynasty_values: Option<&'o Map<Cid, DynastyValue>>,
    pub max_per_team: usize,
    pub limit: usize,
    pub engine: EngineOpts<'o>,
    pub max_per_given: usize,
}

impl Default for TradeOpts<'_> {
    fn default() -> Self {
        TradeOpts { dynasty_values: None, max_per_team: 3, limit: 10, engine: EngineOpts::default(), max_per_given: MAX_PER_GIVEN }
    }
}

/// The per-team result of the search (Python's loop body for one counterparty).
struct TeamResult<'a> {
    /// team_evals that pass `listable` and the 2-for-1 padding check, in order
    accepted: Vec<TradeEval<'a>>,
    future: Vec<TradeEval<'a>>,
}

/// Indices of `xs` sorted by `key` descending (stable), first `n`.
pub(crate) fn top_by<T>(xs: &[T], idx: impl IntoIterator<Item = usize>, n: usize, key: impl Fn(&T) -> f64) -> Vec<usize> {
    let mut v: Vec<usize> = idx.into_iter().collect();
    sort_desc_by(&mut v, |a, b| cmp_f64(key(&xs[*a]), key(&xs[*b])));
    v.truncate(n);
    v
}

/// Refine `evs[i]` for every index in `which` (deduplicated), in parallel.
pub(crate) fn refine_many<'a>(eng: &TradeEngine<'a, '_>, evs: &mut [TradeEval<'a>], which: &[usize]) {
    let mut flags = vec![false; evs.len()];
    for &i in which {
        flags[i] = true;
    }
    evs.par_iter_mut().zip(flags.par_iter()).filter(|(_, f)| **f).for_each(|(e, _)| eng.refine(e));
}

fn search_team<'a>(eng: &TradeEngine<'a, '_>, si: usize, want_future: bool) -> TeamResult<'a> {
    // candidates never repeat a (give, get) set pair: the `seen` set of Python is a no-op here
    let cands = eng.candidates(si);
    let evals: Vec<TradeEval<'a>> =
        cands.par_iter().map(|(give, get, _)| eng.evaluate(si, give, get, true, false, None)).collect();
    let mut near: Vec<TradeEval<'a>> = Vec::new();
    let mut refused: Vec<TradeEval<'a>> = Vec::new();
    let picks = !eng.picks().is_empty();
    for ev in evals {
        if ev.near_plausible() {
            near.push(ev);
        } else if picks && !ev.screened && ev.blocked.is_none() && ev.my_illegal.is_none() {
            refused.push(ev);
        }
    }
    // the exact solver only runs where it can change what is shown
    let mut refine = top_by(&near, 0..near.len(), REFINE_PER_TEAM, |e| e.score());
    refine.extend(top_by(
        &near,
        (0..near.len()).filter(|&i| near[i].perceived().abs() <= SWEET_MAX_PERCEIVED + 2.0),
        REFINE_PER_TEAM,
        |e| e.sweet_spot_score(),
    ));
    if want_future {
        refine.extend(top_by(&near, (0..near.len()).filter(|&i| near[i].win_now_cost()), REFINE_PER_TEAM, |e| e.delta_dyn_n * e.p()));
    }
    refine_many(eng, &mut near, &refine);
    let mut team_evals: Vec<TradeEval<'a>> = Vec::new();
    let mut future: Vec<TradeEval<'a>> = Vec::new();
    for ev in near {
        if eng.exact() && !ev.refined {
            continue;
        }
        if ev.accepted() {
            team_evals.push(ev);
        } else if want_future && ev.accepted_future() {
            future.push(ev);
        }
    }
    if picks {
        // draft picks as add-ins: ask for one back from overpays, sweeten refusals
        let asks = top_by(
            &team_evals,
            (0..team_evals.len()).filter(|&i| team_evals[i].perceived() > PICK_ASK_MIN),
            PICK_TRIES_PER_TEAM,
            |e| e.score(),
        );
        let sweeten = top_by(&refused, 0..refused.len(), PICK_TRIES_PER_TEAM, |e| e.my_edge());
        let ask_vars: Vec<(usize, Option<TradeEval<'a>>)> = asks
            .par_iter()
            .map(|&i| {
                let mut e = team_evals[i].clone();
                eng.refine(&mut e);
                (i, eng.with_pick(&e))
            })
            .collect();
        let sweet_vars: Vec<Option<TradeEval<'a>>> = sweeten
            .par_iter()
            .map(|&i| {
                let mut e = refused[i].clone();
                eng.refine(&mut e);
                eng.with_pick(&e)
            })
            .collect();
        for (i, var) in ask_vars {
            if let Some(var) = var {
                team_evals[i] = var;
            }
        }
        team_evals.extend(sweet_vars.into_iter().flatten());
    }
    // a 2-for-1 that merely pads an accepted 1-for-1 (same core, no better EV) is redundant
    let mut singles: Vec<((u32, u32), f64)> = Vec::new();
    for e in &team_evals {
        if e.give_ix.len() == 1 && e.get_ix.len() == 1 {
            let k = (e.give_ix[0], e.get_ix[0]);
            match singles.iter_mut().find(|(c, _)| *c == k) {
                Some(slot) => slot.1 = e.score(),
                None => singles.push((k, e.score())),
            }
        }
    }
    let mut accepted: Vec<TradeEval<'a>> = Vec::new();
    for e in team_evals {
        if !e.listable() {
            continue;
        }
        if e.give_ix.len() + e.get_ix.len() > 2 {
            let score = e.score();
            let padded = e.give_ix.iter().any(|&gv| {
                e.get_ix.iter().any(|&r| singles.iter().any(|(c, s)| *c == (gv, r) && *s >= score))
            });
            if padded {
                continue;
            }
        }
        accepted.push(e);
    }
    TeamResult { accepted, future }
}

/// Trade proposals ranked by score = EV less the market overpay cost (module docs), at most
/// `max_per_team` per counterparty and `max_per_given` per player given away / received.
/// `future_only` (contend mode) collects trades that gain dynasty value but cost this season's
/// lineup; `sweet_spot` collects the top sweet-spot deals (else up to half the returned slots are
/// reserved for them).
pub fn recommend_trades(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    opts: &TradeOpts<'_>,
    future_only: Option<&mut Vec<Recommendation>>,
    sweet_spot: Option<&mut Vec<Recommendation>>,
) -> anyhow::Result<Vec<Recommendation>> {
    let data = TradeData::new(ctx, values, opts.dynasty_values);
    let eng = TradeEngine::new(&data, &opts.engine)?;
    let want_future = future_only.is_some();
    let me = eng.me;
    let teams: Vec<usize> = (0..ctx.teams.len()).filter(|&i| i != me).collect();
    let results: Vec<TeamResult<'_>> = teams.par_iter().map(|&si| search_team(&eng, si, want_future)).collect();
    let mut accepted: Vec<TradeEval<'_>> = Vec::new();
    let mut future: Vec<TradeEval<'_>> = Vec::new();
    for r in results {
        accepted.extend(r.accepted);
        future.extend(r.future);
    }
    sort_desc_by(&mut accepted, |a, b| cmp_f64(a.score(), b.score()));
    let (mpt, mpg) = (opts.max_per_team, opts.max_per_given);
    let mut sweet_order: Vec<&TradeEval<'_>> = accepted.iter().filter(|e| e.sweet_spot()).collect();
    sort_desc_by(&mut sweet_order, |a, b| cmp_f64(a.sweet_spot_score(), b.sweet_spot_score()));
    let sweet: Vec<&TradeEval<'_>> = diverse(&sweet_order, SWEET_SPOT_N, mpt, mpg, mpg).into_iter().map(|i| sweet_order[i]).collect();
    let all: Vec<&TradeEval<'_>> = accepted.iter().collect();
    let chosen: Vec<&TradeEval<'_>> = if sweet_spot.is_some() {
        diverse(&all, opts.limit, mpt, mpg, mpg).into_iter().map(|i| all[i]).collect()
    } else {
        let reserved: Vec<&TradeEval<'_>> = sweet.iter().copied().take(SWEET_SPOT_N.min(opts.limit / 2)).collect();
        let mut pool: Vec<&TradeEval<'_>> = reserved.clone();
        pool.extend(all.iter().copied().filter(|e| !reserved.iter().any(|r| std::ptr::eq(*r, *e))));
        let mut chosen: Vec<&TradeEval<'_>> = diverse(&pool, opts.limit, mpt, mpg, mpg).into_iter().map(|i| pool[i]).collect();
        sort_desc_by(&mut chosen, |a, b| cmp_f64(a.score(), b.score()));
        chosen
    };
    let source = eng.source();
    let rec = |e: &TradeEval<'_>| to_rec(e, eng.v_map(), eng.units(), source, Some(&eng.market));
    if let Some(fo) = future_only {
        sort_desc_by(&mut future, |a, b| cmp_f64(a.delta_dyn_n * a.p(), b.delta_dyn_n * b.p()));
        for e in future.iter().take(opts.limit) {
            let mut r = rec(e);
            r.score = e.delta_dyn_n * e.p();
            r.predicted_gain = Some(e.delta_dyn_n);
            r.gain_units = Some(GainUnits::Dynasty);
            fo.push(r);
        }
        apply_strength(fo);
        apply_ranks(fo);
    }
    if let Some(ss) = sweet_spot {
        let mut recs: Vec<Recommendation> = sweet.iter().map(|e| rec(e)).collect();
        apply_strength(&mut recs);
        apply_ranks(&mut recs);
        ss.extend(recs);
    }
    let mut out: Vec<Recommendation> = chosen.iter().map(|e| rec(e)).collect();
    apply_strength(&mut out);
    apply_ranks(&mut out);
    Ok(out)
}
