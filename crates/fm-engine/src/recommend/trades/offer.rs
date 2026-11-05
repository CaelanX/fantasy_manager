//! Evaluating a specific offer (`evaluate_trade`, `evaluate_offer`, `offer_summary`; `fm
//! trade-eval`, the web Trade evaluator), trade blocks, and name / team / pick resolution for
//! user-typed offers.

use std::collections::{BTreeSet, HashSet};

use fm_core::models::{FantasyTeam, LeagueContext, Player, Recommendation};
use fm_core::normalize::normalize_name;
use fm_core::snapshot::TradeBlockInfo;
use fm_core::values::{DynastyValue, PickAsset, PlayerValue};
use fm_core::{Cid, CoreError, Map};
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::round_to;
use fm_pycompat::order::cmp_f64;
use regex::Regex;
use serde_json::{json, Value};

use super::engine::{EngineOpts, TradeData, TradeEngine, TradeEval};
use super::greedy::Pid;
use super::rec::{market_label, roster_text, to_rec, DEFAULT_RATE};
use super::*;
use crate::recommend::base::{gain_season, gain_week};
use crate::recommend::strength::apply_strength;

fn lookup(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CoreError::Lookup(msg.into()))
}

fn value_err(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CoreError::Value(msg.into()))
}

/// A draft pick named by object or by its key ("pick:<year>:<round>:<original team id>").
#[derive(Clone, Debug)]
pub enum PickRef {
    Asset(PickAsset),
    Key(String),
}

impl PickRef {
    fn key(&self) -> String {
        match self {
            PickRef::Asset(a) => a.key(),
            PickRef::Key(k) => k.clone(),
        }
    }
}

/// PickAssets for `picks` (objects or keys), as the engine values them.
fn pick_assets_for(eng: &TradeEngine<'_, '_>, picks: &[PickRef]) -> anyhow::Result<Vec<PickAsset>> {
    if picks.is_empty() {
        return Ok(Vec::new());
    }
    if eng.picks().is_empty() {
        return Err(value_err("draft picks can only be traded in dynasty leagues whose provider reports picks (Fantrax)"));
    }
    let mut out = Vec::new();
    for x in picks {
        let k = x.key();
        let hit = eng.picks().values().flatten().find(|a| a.key() == k);
        match hit {
            Some(a) => out.push(a.clone()),
            None => {
                let shown = match x {
                    PickRef::Key(k) => format!("'{k}'"),
                    PickRef::Asset(a) => format!("{a:?}"),
                };
                return Err(lookup(format!("unknown draft pick {shown}")));
            }
        }
    }
    Ok(out)
}

/// `_evaluate_with(eng, give, get, team_id, give_picks, get_picks)`: the one scoring path for a
/// given proposal (no screen, exact lineups, then any picks added in).
pub(crate) fn evaluate_with<'a>(
    eng: &TradeEngine<'a, '_>,
    give: &[&str],
    get: &[&str],
    team_id: Option<&str>,
    give_picks: &[PickRef],
    get_picks: &[PickRef],
) -> anyhow::Result<TradeEval<'a>> {
    let ctx = eng.ctx;
    let by_cid = |c: &str| ctx.teams.iter().flat_map(|t| t.players_iter()).filter(|p| p.cid == c).last();
    let missing: Vec<&str> = give.iter().chain(get.iter()).copied().filter(|c| by_cid(c).is_none()).collect();
    if !missing.is_empty() {
        return Err(lookup(format!("not on any roster: {}", missing.join(", "))));
    }
    let me = ctx.teams[eng.me].team_id.as_str();
    let team_id: String = match team_id {
        Some(t) => t.to_string(),
        None => {
            let owner: Vec<String> = if !get.is_empty() {
                ctx.teams
                    .iter()
                    .filter(|t| !t.owner_is_me && get.iter().all(|c| t.players_iter().any(|p| p.cid == *c)))
                    .map(|t| t.team_id.clone())
                    .collect()
            } else {
                let holders: BTreeSet<String> = pick_assets_for(eng, get_picks)?.into_iter().map(|a| a.holder).collect();
                holders.into_iter().filter(|h| h != me).collect()
            };
            owner.into_iter().next().ok_or_else(|| lookup("could not find a single team owning all requested players"))?
        }
    };
    let si = match eng.side_index(&team_id) {
        Some(si) if team_id != me => si,
        _ => return Err(lookup(format!("unknown counterparty team '{team_id}'"))),
    };
    let mine: HashSet<&str> = ctx.teams[eng.me].players_iter().map(|p| p.cid.as_str()).collect();
    let team = ctx.teams[si].clone();
    let theirs: HashSet<&str> = ctx.teams[si].players_iter().map(|p| p.cid.as_str()).collect();
    let name_of = |c: &str| by_cid(c).map(|p| p.name.clone()).unwrap_or_default();
    if !give.iter().all(|c| mine.contains(c)) {
        let bad: Vec<String> = give.iter().filter(|c| !mine.contains(**c)).map(|c| name_of(c)).collect();
        return Err(lookup(format!("not on your roster: {}", bad.join(", "))));
    }
    if !get.iter().all(|c| theirs.contains(c)) {
        let bad: Vec<String> = get.iter().filter(|c| !theirs.contains(**c)).map(|c| name_of(c)).collect();
        return Err(lookup(format!("not on {}'s roster: {}", team.name, bad.join(", "))));
    }
    let pids = |cs: &[&str]| -> Vec<Pid> { cs.iter().map(|c| eng.pid_of(c).expect("rostered")).collect() };
    let mut ev = eng.evaluate(si, &pids(give), &pids(get), false, true, None);
    let gp = pick_assets_for(eng, give_picks)?;
    let tp = pick_assets_for(eng, get_picks)?;
    for a in &gp {
        if a.holder != me {
            return Err(lookup(format!("you do not hold the {} pick", a.label())));
        }
    }
    for a in &tp {
        if a.holder != team_id {
            return Err(lookup(format!("{} does not hold the {} pick", ev.team.name, a.label())));
        }
    }
    if !gp.is_empty() || !tp.is_empty() {
        ev = eng.with_picks(&ev, &gp, &tp);
    }
    Ok(ev)
}

/// `evaluate_trade(ctx, values, give, get, team_id, ...)`: an arbitrary n-for-m proposal (cids)
/// scored exactly, with its recommendation.
pub fn evaluate_trade<'a>(
    ctx: &'a LeagueContext,
    values: &'a Map<Cid, PlayerValue>,
    give: &[&str],
    get: &[&str],
    team_id: Option<&str>,
    dynasty_values: Option<&'a Map<Cid, DynastyValue>>,
    opts: &EngineOpts<'_>,
    give_picks: &[PickRef],
    get_picks: &[PickRef],
) -> anyhow::Result<(TradeEval<'a>, Recommendation)> {
    let data = TradeData::new(ctx, values, dynasty_values);
    let eng = TradeEngine::new(&data, opts)?;
    let ev = evaluate_with(&eng, give, get, team_id, give_picks, get_picks)?;
    let rec = to_rec(&ev, eng.v_map(), eng.units(), eng.source(), Some(&eng.market));
    Ok((ev, rec))
}

/// A specific trade offer judged from my side.
#[derive(Clone, Debug)]
pub struct OfferEval<'a> {
    pub ev: TradeEval<'a>,
    pub rec: Recommendation,
    /// "accept" | "counter" | "decline"
    pub verdict: &'static str,
    pub why: Vec<String>,
    pub counter: Option<String>,
    pub units: &'static str,
}

/// `_verdict(ev)`: (accept / counter / decline, reasons).
pub fn verdict(ev: &TradeEval<'_>) -> (&'static str, Vec<String>) {
    let edge = ev.my_edge();
    if let Some(i) = &ev.my_illegal {
        return ("decline", vec![format!("Your roster would be illegal after it ({i})")]);
    }
    if let Some(b) = &ev.blocked {
        return ("decline", vec![format!("The deal {b}")]);
    }
    let mut why: Vec<String> = Vec::new();
    let floor_broken = ev.win_now_cost() || (ev.mode == Some("balanced") && ev.delta_me < -BALANCED_TOLERANCE);
    let mode_txt = ev.mode.unwrap_or("None");
    if floor_broken {
        let tol = if ev.mode == Some("contend") { WIN_NOW_TOLERANCE } else { BALANCED_TOLERANCE };
        why.push(format!(
            "It costs this season's lineup {} pts/game ({mode_txt} mode allows {})",
            fixed(-ev.delta_me, 2, false),
            super::engine::g(tol)
        ));
    }
    let label = match ev.mode {
        None => "your edge".to_string(),
        Some(m) => format!("your {m} edge"),
    };
    let e2 = fixed(edge, 2, true);
    if edge < EVAL_DECLINE_EDGE {
        let mut out = vec![format!("You lose: {label} {e2} pts/game-equivalent")];
        out.extend(why);
        return ("decline", out);
    }
    if floor_broken {
        why.push(format!("{label} {e2}"));
        return (if edge > 0.0 { "counter" } else { "decline" }, why);
    }
    if edge < EVAL_ACCEPT_EDGE {
        return ("counter", vec![format!("Too close to call: {label} {e2}; ask for a little more")]);
    }
    if ev.clear_overpay() && edge < OVERPAY_MIN_EDGE {
        return (
            "counter",
            vec![format!(
                "You gain ({label} {e2}) but hand over {} market worth pts: {}, so ask for more",
                fixed(ev.market_overpay(), 0, false),
                market_label(ev.market_overpay()).to_lowercase()
            )],
        );
    }
    why.insert(0, format!("You gain: {label} {e2}"));
    if ev.market_overpay() < -3.0 {
        why.push(format!("{} by market value", market_label(ev.market_overpay())));
    }
    ("accept", why)
}

/// `_counter_pick(eng, ev)`: the cheapest of their picks that turns a counter into an accept
/// they plausibly take.
fn counter_pick<'a>(eng: &TradeEngine<'a, '_>, ev: &TradeEval<'a>) -> Option<PickAsset> {
    if eng.mode.is_none() || !ev.give_picks.is_empty() || !ev.get_picks.is_empty() {
        return None;
    }
    let mut opts: Vec<&PickAsset> = eng.picks().get(&ev.team.team_id).map(|v| v.iter().collect()).unwrap_or_default();
    opts.sort_by(|a, b| cmp_f64(a.value, b.value));
    for a in opts {
        let var = eng.with_picks(ev, &[], std::slice::from_ref(a));
        if verdict(&var).0 == "accept" && var.p() >= MIN_P_ACCEPT {
            return Some(a.clone());
        }
    }
    None
}

/// `evaluate_offer(...)`: judge one specific offer from my side (scoring, reasons, verdict).
pub fn evaluate_offer<'a>(
    ctx: &'a LeagueContext,
    values: &'a Map<Cid, PlayerValue>,
    give: &[&str],
    get: &[&str],
    team_id: Option<&str>,
    dynasty_values: Option<&'a Map<Cid, DynastyValue>>,
    give_picks: &[PickRef],
    get_picks: &[PickRef],
    opts: &EngineOpts<'_>,
) -> anyhow::Result<OfferEval<'a>> {
    if give.is_empty() && get.is_empty() && give_picks.is_empty() && get_picks.is_empty() {
        return Err(value_err("an offer needs at least one player or pick on some side"));
    }
    let data = TradeData::new(ctx, values, dynasty_values);
    let eng = TradeEngine::new(&data, opts)?;
    let ev = evaluate_with(&eng, give, get, team_id, give_picks, get_picks)?;
    let mut recs = vec![to_rec(&ev, eng.v_map(), eng.units(), eng.source(), Some(&eng.market))];
    apply_strength(&mut recs);
    let rec = recs.pop().expect("one rec");
    let (v, why) = verdict(&ev);
    let mut counter = None;
    if v == "counter" {
        if let Some(a) = counter_pick(&eng, &ev) {
            counter = Some(format!("Ask {} to add their {} pick", ev.team.name, a.short(Some(&ev.team.team_id))));
        }
    }
    Ok(OfferEval { ev, rec, verdict: v, why, counter, units: eng.units() })
}

/// `offer_summary(o)`: plain-data view of an OfferEval (`fm trade-eval --json`, the web evaluator).
pub fn offer_summary(o: &OfferEval<'_>) -> Value {
    let ev = &o.ev;
    let r = &o.rec;
    let rate = ev.rate.unwrap_or(DEFAULT_RATE);
    let acc = ev.acceptance.as_ref();
    let team_id = ev.team.team_id.as_str();
    let pl = |ps: &[&Player]| -> Value {
        Value::Array(
            ps.iter().map(|p| json!({"cid": p.cid, "name": p.name, "positions": p.positions, "team": p.team})).collect(),
        )
    };
    let pk = |a: &PickAsset, holder: &str| {
        json!({"key": a.key(), "label": a.short(Some(holder)), "value": round_to(a.value, 2), "net": round_to(a.net, 2)})
    };
    let fair_dev = if ev.fair_dev == f64::INFINITY { Value::Null } else { json!(round_to(ev.fair_dev, 3)) };
    json!({
        "team": ev.team.name, "team_id": team_id,
        "give": pl(&ev.give), "get": pl(&ev.get),
        "give_picks": ev.give_picks.iter().map(|a| pk(a, &a.holder)).collect::<Vec<_>>(),
        "get_picks": ev.get_picks.iter().map(|a| pk(a, team_id)).collect::<Vec<_>>(),
        "verdict": o.verdict, "verdict_reasons": o.why, "counter": o.counter,
        "my_gain": {"fpg": round_to(ev.delta_me, 3), "week": round_to(gain_week(ev.delta_me, &rate), 2),
                    "season": round_to(gain_season(ev.delta_me, &rate), 1)},
        "their_gain": {"fpg": round_to(ev.delta_them, 3)},
        "my_edge": round_to(ev.my_edge(), 3), "mode": ev.mode, "units": o.units,
        "dynasty_delta": match ev.mode {
            None => Value::Null,
            Some(_) => json!({"value": round_to(ev.delta_dyn, 2), "fpg_equiv": round_to(ev.delta_dyn_n, 3)}),
        },
        "fairness": {"value_in": round_to(ev.v_in, 2), "value_out": round_to(ev.v_out, 2), "model_gap": fair_dev,
                     "model_fair": ev.fair,
                     "market_to_them": acc.map(|_| round_to(ev.market_overpay(), 1)),
                     "market_label": acc.map(|_| market_label(ev.market_overpay())),
                     "p_accept": acc.map(|a| round_to(a.p, 3))},
        "roster": roster_text(ev),
        "market_check": r.reasons.iter().filter(|x| x.code == "MARKET_CHECK").map(|x| x.text.clone()).collect::<Vec<_>>(),
        "reasons": r.reasons.iter().map(|x| serde_json::to_value(x).expect("reason json")).collect::<Vec<_>>(),
    })
}

// -- trade blocks -------------------------------------------------------------------------

/// `trade_block_wants(ctx, provider)`: team_id -> positions wanted (sorted) from the provider's
/// trade blocks; None without any.
pub fn trade_block_wants(ctx: &LeagueContext, blocks: &[TradeBlockInfo]) -> Option<Map<String, Vec<String>>> {
    let mut by_cid: Map<&str, &Player> = Map::new();
    for p in ctx.all_players() {
        by_cid.entry(p.cid.as_str()).or_insert(p);
    }
    let mut wants: Map<String, Vec<String>> = Map::new();
    for b in blocks {
        let tid = b.team_id.as_str();
        let mut pos: BTreeSet<String> =
            b.positions_wanted.iter().filter(|x| !x.is_empty() && !["BN", "IR", "UTIL"].contains(&x.as_str())).cloned().collect();
        for cid in &b.players_wanted {
            if let Some(p) = by_cid.get(cid.as_str()) {
                pos.extend(p.positions.iter().filter(|x| *x != "F").cloned());
            }
        }
        if !tid.is_empty() && !pos.is_empty() {
            wants.insert(tid.to_string(), pos.into_iter().collect());
        }
    }
    (!wants.is_empty()).then_some(wants)
}

/// `trade_block_offered(provider)`: team_id -> player cids offered (sorted); None without blocks.
pub fn trade_block_offered(blocks: &[TradeBlockInfo]) -> Option<Map<String, Vec<String>>> {
    let mut out: Map<String, Vec<String>> = Map::new();
    for b in blocks {
        let cids: BTreeSet<String> = b.players_offered.iter().filter(|c| !c.is_empty()).cloned().collect();
        if !b.team_id.is_empty() && !cids.is_empty() {
            out.insert(b.team_id.clone(), cids.into_iter().collect());
        }
    }
    (!out.is_empty()).then_some(out)
}

// -- name resolution ------------------------------------------------------------------------

/// `find_players(queries, pool)`: cid or exact normalized name, else every query token in the
/// name, else the fuzzy matcher. Errors name the unmatched or ambiguous query.
pub fn find_players<'p>(queries: &[&str], pool: &[&'p Player]) -> anyhow::Result<Vec<&'p Player>> {
    use fm_sources::matching::matcher::{Candidate, Confidence, Key, PlayerIndex};
    // {p.cid: p for p in pool}.values(): first position, last object
    let mut uniq: Vec<&'p Player> = Vec::new();
    for p in pool {
        match uniq.iter().position(|q| q.cid == p.cid) {
            Some(i) => uniq[i] = p,
            None => uniq.push(p),
        }
    }
    let norm: Vec<String> = uniq.iter().map(|p| normalize_name(Some(&p.name))).collect();
    let mut index: Option<PlayerIndex> = None;
    let mut out = Vec::new();
    for raw in queries {
        let q = raw.trim();
        if q.is_empty() {
            continue;
        }
        let nq = normalize_name(Some(q));
        let mut hits: Vec<usize> = (0..uniq.len()).filter(|&i| uniq[i].cid == q).collect();
        if hits.is_empty() {
            hits = (0..uniq.len()).filter(|&i| norm[i] == nq).collect();
        }
        if hits.is_empty() && !nq.is_empty() {
            let toks: Vec<&str> = nq.split_whitespace().collect();
            hits = (0..uniq.len())
                .filter(|&i| {
                    toks.iter().all(|t| {
                        norm[i].split_whitespace().any(|w| w == *t || (t.chars().count() == 1 && w.starts_with(t)))
                    })
                })
                .collect();
        }
        if hits.is_empty() && !nq.is_empty() {
            let idx = index.get_or_insert_with(|| {
                PlayerIndex::new(uniq.iter().map(|p| Candidate::new(p.cid.clone(), &p.name, p.team.as_deref(), None)))
            });
            let m = idx.match_(q, None, None, 8);
            if m.confidence != Confidence::None {
                if let Some(Key::Str(k)) = &m.key {
                    hits = (0..uniq.len()).filter(|&i| &uniq[i].cid == k).collect();
                }
            }
        }
        if hits.is_empty() {
            return Err(lookup(format!("no player matching '{q}'")));
        }
        if hits.len() > 1 {
            let shown: Vec<String> =
                hits.iter().take(6).map(|&i| format!("{} ({})", uniq[i].name, uniq[i].team.as_deref().filter(|t| !t.is_empty()).unwrap_or("?"))).collect();
            return Err(lookup(format!("'{q}' is ambiguous: {}", shown.join(", "))));
        }
        out.push(uniq[hits[0]]);
    }
    Ok(out)
}

/// `find_team(ctx, query)`: by id, exact name, or a unique case-insensitive name substring.
pub fn find_team<'c>(ctx: &'c LeagueContext, query: &str) -> anyhow::Result<&'c FantasyTeam> {
    let q = query.trim().to_lowercase();
    let teams = &ctx.teams;
    let mut hits: Vec<&FantasyTeam> = teams.iter().filter(|t| t.team_id.to_lowercase() == q).collect();
    if hits.is_empty() {
        hits = teams.iter().filter(|t| t.name.to_lowercase() == q).collect();
    }
    if hits.is_empty() {
        hits = teams.iter().filter(|t| !q.is_empty() && t.name.to_lowercase().contains(&q)).collect();
    }
    if hits.len() != 1 {
        let list: Vec<&str> = if hits.is_empty() { teams.iter().map(|t| t.name.as_str()).collect() } else { hits.iter().map(|t| t.name.as_str()).collect() };
        return Err(lookup(format!(
            "{} team matching '{query}' ({})",
            if hits.is_empty() { "no" } else { "ambiguous" },
            list.join(", ")
        )));
    }
    Ok(hits[0])
}

/// `find_pick(picks, spec, holder_id)`: "2027 R1", "2027 1", "2027 round 1", optionally naming the
/// original team, or a pick key. Without a team, the holder's own pick wins a tie.
pub fn find_pick<'p>(picks: &[&'p PickAsset], spec: &str, holder_id: &str) -> anyhow::Result<&'p PickAsset> {
    let s = spec.trim();
    let held: Vec<&PickAsset> = picks.iter().copied().filter(|a| a.holder == holder_id).collect();
    if s.starts_with("pick:") {
        return held.into_iter().find(|a| a.key() == s).ok_or_else(|| lookup(format!("no held pick '{s}'")));
    }
    let re = Regex::new(r"(?i)^\s*(\d{4})\s*(?:r|rd|round)?\s*(\d+)\s*(.*)$").expect("regex");
    let Some(m) = re.captures(s) else {
        return Err(lookup(format!("bad pick '{spec}': use e.g. \"2027 R1\" or \"2027 R2 (from Team X)\"")));
    };
    let year: i64 = m[1].parse().map_err(|_| lookup(format!("bad pick '{spec}'")))?;
    let rnd: i64 = m[2].parse().map_err(|_| lookup(format!("bad pick '{spec}'")))?;
    let strip = Regex::new(r"(?i)^\(?\s*(from\s+)?|\)\s*$").expect("regex");
    let rest = strip.replace_all(m[3].trim(), "").trim().to_lowercase();
    let mut hits: Vec<&PickAsset> = held.iter().copied().filter(|a| a.year == year && a.round == rnd).collect();
    if !rest.is_empty() {
        hits.retain(|a| a.original_name.to_lowercase().contains(&rest) || rest == a.original.to_lowercase());
    } else if hits.len() > 1 {
        let own: Vec<&PickAsset> = hits.iter().copied().filter(|a| a.original == holder_id).collect();
        if !own.is_empty() {
            hits = own;
        }
    }
    if hits.is_empty() {
        let mut sorted = held.clone();
        sorted.sort_by_key(|a| (a.year, a.round));
        let avail = sorted.iter().map(|a| a.short(Some(holder_id))).collect::<Vec<_>>().join(", ");
        let avail = if avail.is_empty() { "none".to_string() } else { avail };
        return Err(lookup(format!("no pick matching '{spec}' (held: {avail})")));
    }
    if hits.len() > 1 {
        let list = hits.iter().map(|a| a.short(Some(holder_id))).collect::<Vec<_>>().join(", ");
        return Err(lookup(format!("'{spec}' is ambiguous: {list}")));
    }
    Ok(hits[0])
}

/// `pick_assets(ctx, values, dynasty_values)`: valued draft picks by holder, exactly as the trade
/// engine values them ({} outside dynasty leagues with pick data).
pub fn pick_assets(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
) -> Map<String, Vec<PickAsset>> {
    let (v, units, _) = super::engine::trade_values(ctx, values, dynasty_values);
    if units != "dynasty" || ctx.draft_picks.is_empty() {
        return Map::new();
    }
    crate::valuation::picks::picks_by_holder(super::engine::value_picks_v(ctx, values, &v))
}
