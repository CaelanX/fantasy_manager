//! `_TradeEngine`, `_Side` and `TradeEval`: per-league search state, roster settlement and the
//! scoring of one proposal.
//!
//! Built once per league (single-threaded), then shared read-only across rayon workers: every
//! cache is a pure function of its key (`greedy` module docs) held in a concurrent map, so the
//! results do not depend on evaluation order or thread count.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use fm_core::models::{FantasyTeam, LeagueContext, Player, ScoringKind, Status};
use fm_core::values::{DynastyValue, Horizon, PickAsset, PlayerValue};
use fm_core::{Cid, Map};
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::py_sum;
use fm_pycompat::order::{cmp_f64, first_min_by, sort_desc_by};
use smallvec::SmallVec;

use super::accept::{accept_params_for, python_acceptance, market_worth, package_value, pick_worth, team_standing, AcceptParams, Acceptance};
use super::greedy::{greedy_assignment, key_of, summarize, GreedySummary, Memo, Pid, RosterKey, SlotSpec};
use super::*;
use crate::recommend::base::{
    cap_text, dynasty_change, dynasty_objects, game_rate, has_data, keeper_k, league_unit, limit_positions, player_age, protected_reason,
    roster_legal_after, DynastyChange, GameRate, PROTECT_OVERRIDE,
};
use crate::recommend::lineup::{self, Entry, RosterView};
use crate::recommend::market::MarketPool;
use crate::recommend::started::{self, StartWindow};
use crate::scoring::RotoScoring;
use crate::valuation::dynasty::{apply_dynasty, keeper_count, mode_weights};
use crate::valuation::picks::{
    elapsed, expected_slots, holdings, keeper_slot_curve, keeper_spot_curve, keeper_spot_value, pick_spot_cost, pick_value,
    picks_by_holder, roster_spot_value, slot_curve,
};
use crate::valuation::valuate::replacement_for;

pub(crate) type Pl<'a> = SmallVec<[&'a Player; 4]>;

// -- trade values ------------------------------------------------------------------------

/// `dynasty_scale(ctx)`: dynasty value of 1 FPG held flat over the horizon.
pub fn dynasty_scale(ctx: &LeagueContext) -> f64 {
    let horizon = ctx.keeper_horizon_years.max(1);
    let (w, terminal) = mode_weights(Some(ctx.dynasty_mode.as_str()), horizon);
    py_sum(w.iter().copied()) + DYNASTY_DISCOUNT.powf(horizon as f64) * terminal
}

/// `season_weight(ctx)`: the dynasty mode's weight on this season.
pub fn season_weight(ctx: &LeagueContext) -> f64 {
    mode_weights(Some(ctx.dynasty_mode.as_str()), 1).0[0]
}

/// `trade_values(ctx, values, dynasty_values)`: (V by cid, units label, value of one freed roster
/// spot in V units). `dynasty_values` None in a dynasty league: computed (`apply_dynasty`).
pub fn trade_values(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
) -> (Map<Cid, f64>, &'static str, f64) {
    let computed;
    let dv = match dynasty_values {
        None if ctx.dynasty => {
            computed = apply_dynasty(values, ctx, None, None);
            Some(&computed)
        }
        other => other,
    };
    if let Some(dv) = dv.filter(|d| !d.is_empty()) {
        let mut v: Map<Cid, f64> = dv.iter().map(|(c, d)| (c.clone(), d.value)).collect();
        for cid in values.keys() {
            v.entry(cid.clone()).or_insert(0.0);
        }
        let levels = replacement_for(ctx, values, Horizon::Season);
        let mut repl = 0.0;
        if !levels.is_empty() {
            let per = if keeper_k(ctx, Some(dv)).is_some() { season_weight(ctx) } else { dynasty_scale(ctx) };
            let lo = levels.values().copied().fold(f64::INFINITY, f64::min);
            repl = lo * per;
        }
        return (v, "dynasty", repl);
    }
    (values.iter().map(|(c, pv)| (c.clone(), pv.vorp)).collect(), "VORP", 0.0)
}

/// `valuation.picks.value_picks(ctx, values, V)` as the trade engine calls it: with the float trade
/// values V (zeros included) and, in keeper leagues, the keeper flags of a fresh
/// `apply_dynasty(values, ctx)` (floats carry none).
pub(crate) fn value_picks_v(ctx: &LeagueContext, values: &Map<Cid, PlayerValue>, v: &Map<Cid, f64>) -> Vec<PickAsset> {
    if !ctx.dynasty || ctx.draft_picks.is_empty() {
        return Vec::new();
    }
    let rows = holdings(ctx);
    if rows.is_empty() {
        return Vec::new();
    }
    let names: HashMap<&str, &str> = ctx.teams.iter().map(|t| (t.team_id.as_str(), t.name.as_str())).collect();
    let n = ctx.teams.len().max(1) as i64;
    let first = rows.iter().map(|r| r.1).min().expect("rows");
    let (curve, spot, spot_curve) = if keeper_count(ctx).is_none() {
        (slot_curve(ctx, v), roster_spot_value(ctx, values), None)
    } else {
        let full = apply_dynasty(values, ctx, None, None);
        (keeper_slot_curve(ctx, values, &full), keeper_spot_value(ctx, values), keeper_spot_curve(ctx, values, &full))
    };
    let el = elapsed(ctx);
    let mut slots: HashMap<i64, Map<String, f64>> = HashMap::new();
    let mut out: Vec<PickAsset> = Vec::with_capacity(rows.len());
    for (holder, year, rnd, orig, inferred) in rows {
        let ahead = year - first;
        let slot_map = slots.entry(ahead).or_insert_with(|| expected_slots(ctx, values, v, ahead));
        let slot = slot_map.get(&orig).copied().unwrap_or((n as f64 + 1.0) / 2.0);
        let value = pick_value(&curve, n, rnd, slot, ahead, el);
        out.push(PickAsset {
            year,
            round: rnd,
            holder,
            original_name: names.get(orig.as_str()).map(|s| s.to_string()).unwrap_or_else(|| orig.clone()),
            original: orig,
            years_ahead: ahead,
            expected_slot: slot,
            overall: (rnd - 1) as f64 * n as f64 + slot,
            value,
            inferred,
            net: (value - pick_spot_cost(spot_curve.as_deref(), spot, n, rnd, slot, ahead, el)).max(0.0),
        });
    }
    out.sort_by(|a, b| cmp_f64(-a.value, -b.value).then(a.year.cmp(&b.year)).then(a.round.cmp(&b.round)));
    out
}

/// `fairness(v_in, v_out, repl)`: (passes, deviation) of the legacy model-value band.
pub fn fairness(v_in: &[f64], v_out: &[f64], repl: f64) -> (bool, f64) {
    let (s_in, s_out) = (py_sum(v_in.iter().copied()), py_sum(v_out.iter().copied()));
    if v_in.len() == v_out.len() {
        let denom = s_in.max(s_out);
        if denom <= 0.0 {
            return (false, f64::INFINITY);
        }
        let dev = (s_in - s_out).abs() / denom;
        return (dev <= FAIR_BAND + 1e-9, dev);
    }
    let (few, many) = if v_in.len() < v_out.len() { (s_in, s_out) } else { (s_out, s_in) };
    let target = many - (v_in.len() as f64 - v_out.len() as f64).abs() * repl;
    if target <= 0.0 || few <= 0.0 {
        return (false, f64::INFINITY);
    }
    let ratio = few / target;
    (UNEVEN_BAND.0 - 1e-9 <= ratio && ratio <= UNEVEN_BAND.1 + 1e-9, (ratio - 1.0).abs())
}

/// `" + ".join(p.name for p in ps)`.
pub(crate) fn names<'p>(ps: impl IntoIterator<Item = &'p &'p Player>) -> String {
    ps.into_iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(" + ")
}

/// The owned per-league inputs of the engine (dynasty objects, trade values, picks); the engine
/// and its market pool borrow them.
pub struct TradeData<'a> {
    pub ctx: &'a LeagueContext,
    pub values: &'a Map<Cid, PlayerValue>,
    /// the dynasty values as passed (`p_accept`'s ages, `protected_reason`)
    pub dynasty_values: Option<&'a Map<Cid, DynastyValue>>,
    /// the DynastyValue objects (computed once when not supplied)
    pub dyn_: Option<Cow<'a, Map<Cid, DynastyValue>>>,
    pub v: Map<Cid, f64>,
    pub units: &'static str,
    pub repl: f64,
    pub picks: Map<String, Vec<PickAsset>>,
}

impl<'a> TradeData<'a> {
    pub fn new(ctx: &'a LeagueContext, values: &'a Map<Cid, PlayerValue>, dynasty_values: Option<&'a Map<Cid, DynastyValue>>) -> Self {
        let dyn_ = match dynasty_values {
            Some(d) => Some(Cow::Borrowed(d)),
            None => dynasty_objects(ctx, values, None),
        };
        let (v, units, repl) = trade_values(ctx, values, dyn_.as_deref());
        let picks = if units == "dynasty" && !ctx.draft_picks.is_empty() {
            picks_by_holder(value_picks_v(ctx, values, &v))
        } else {
            Map::new()
        };
        TradeData { ctx, values, dynasty_values, dyn_, v, units, repl, picks }
    }
}

/// Engine options (`lineup_fn`, trade blocks, the acceptance curve).
#[derive(Clone, Debug, Default)]
pub struct EngineOpts<'o> {
    /// trade-block positions wanted, by team id
    pub wants: Option<&'o Map<String, Vec<String>>>,
    /// trade-block player cids offered, by team id
    pub offered: Option<&'o Map<String, Vec<String>>>,
    /// `lineup_fn=False`: force the greedy solver (no exact re-solve)
    pub greedy_only: bool,
    /// None: the league's installed parameters ([`accept_params_for`]), else the prior
    pub accept_params: Option<AcceptParams>,
}

/// Per-team state (`_Side`).
#[derive(Clone, Debug)]
pub(crate) struct Side<'a> {
    pub team: &'a FantasyTeam,
    pub active: Vec<Pid>,
    pub ir: Vec<Pid>,
    pub capacity: usize,
    pub free_ir: usize,
    pub weakest: Option<(String, f64)>,
    pub top: Vec<Pid>,
}

/// One scored proposal (`TradeEval`). `give` goes to `team`, `get` comes from it.
#[derive(Clone, Debug)]
pub struct TradeEval<'a> {
    pub team: &'a FantasyTeam,
    pub give: Pl<'a>,
    pub get: Pl<'a>,
    pub(crate) give_ix: SmallVec<[Pid; 4]>,
    pub(crate) get_ix: SmallVec<[Pid; 4]>,
    pub v_in: f64,
    pub v_out: f64,
    pub fair: bool,
    pub fair_dev: f64,
    pub delta_me: f64,
    pub delta_them: f64,
    pub need_text: Option<String>,
    pub my_drops: Pl<'a>,
    pub their_drops: Pl<'a>,
    pub my_fill: Option<&'a Player>,
    pub their_fill: Option<&'a Player>,
    pub roto_adj: f64,
    pub my_ir: Pl<'a>,
    pub their_ir: Pl<'a>,
    pub mode: Option<&'static str>,
    pub delta_dyn: f64,
    pub delta_dyn_n: f64,
    pub keeper_change: Option<DynastyChange<'a>>,
    pub spot_cost: f64,
    pub blocked: Option<String>,
    pub my_cap: Option<String>,
    pub their_cap: Option<String>,
    pub block_text: Option<String>,
    pub my_illegal: Option<String>,
    pub acceptance: Option<Acceptance>,
    pub screened: bool,
    pub pressure_text: Option<String>,
    pub hole_text: Option<String>,
    pub hole_slots: i64,
    pub rate: Option<GameRate>,
    pub refined: bool,
    pub(crate) me_after: Arc<Vec<Vec<Pid>>>,
    pub(crate) them_after: Arc<Vec<Vec<Pid>>>,
    pub(crate) me_base: Arc<Vec<Vec<Pid>>>,
    pub(crate) them_base: Arc<Vec<Vec<Pid>>>,
    pub free_fill: Option<&'a Player>,
    pub give_picks: Vec<PickAsset>,
    pub get_picks: Vec<PickAsset>,
    /// the league unit ([`league_unit`]) the FPG thresholds below are multiplied by
    pub unit: f64,
    /// half-width of the range of `my_edge()` from projection error ([`TradeEngine::edge_error`])
    pub edge_error: f64,
}

pub fn me_weight(mode: &str) -> f64 {
    if mode == "rebuild" {
        0.5
    } else {
        1.0
    }
}

pub fn dyn_weight(mode: &str) -> f64 {
    if mode == "contend" {
        0.5
    } else {
        1.0
    }
}

impl<'a> TradeEval<'a> {
    pub fn need_bonus(&self) -> f64 {
        if self.need_text.is_some() {
            NEED_PTS
        } else {
            0.0
        }
    }
    /// ΔMe, or the mode's blend of ΔMe and ΔDyn in dynasty leagues.
    pub fn core(&self) -> f64 {
        match self.mode {
            None => self.delta_me,
            Some(m) => me_weight(m) * self.delta_me + dyn_weight(m) * self.delta_dyn_n,
        }
    }
    pub fn my_edge(&self) -> f64 {
        self.core() - self.roto_adj
    }
    pub fn p(&self) -> f64 {
        self.acceptance.as_ref().map_or(0.0, |a| a.p)
    }
    pub fn perceived(&self) -> f64 {
        self.acceptance.as_ref().map_or(0.0, |a| a.perceived)
    }
    pub fn ev(&self) -> f64 {
        self.my_edge() * self.p()
    }
    /// Market worth points I hand over (they get - they give; bonuses excluded).
    pub fn market_overpay(&self) -> f64 {
        self.acceptance.as_ref().map_or(0.0, |a| a.gets - a.gives)
    }
    pub fn overpay_cost(&self) -> f64 {
        OVERPAY_COST_FPG * self.unit * (self.market_overpay() - OVERPAY_FREE_PTS).max(0.0)
    }
    pub fn clear_overpay(&self) -> bool {
        self.market_overpay() > CLEAR_FAVOR_PTS
    }
    pub fn listable(&self) -> bool {
        self.score() > 0.0 && (!self.clear_overpay() || self.my_edge() >= OVERPAY_MIN_EDGE * self.unit - 1e-9)
    }
    pub fn score(&self) -> f64 {
        self.ev() - self.overpay_cost()
    }
    pub fn sweet_spot_score(&self) -> f64 {
        self.my_edge() - MARKET_PTS_TO_FPG * self.unit * (-self.perceived()).max(0.0)
    }
    pub fn sweet_spot(&self) -> bool {
        self.accepted() && self.perceived().abs() <= SWEET_MAX_PERCEIVED + 1e-9 && self.my_edge() >= SWEET_MIN_EDGE * self.unit - 1e-9
    }
    pub fn win_now_cost(&self) -> bool {
        self.mode == Some("contend") && self.delta_me < -WIN_NOW_TOLERANCE * self.unit
    }
    /// Our own model would tell them to decline: in their seat the deal fails `verdict`'s decline
    /// test (lineup change below EVAL_DECLINE_EDGE league units, and below zero even at the top of
    /// the projection-error range). Such offers are never proposed (docs/knowledge/06, implication
    /// 14: lowballs cause reactive devaluation). Their future value is not modelled, so a deal that
    /// costs me future value (`delta_dyn_n < 0`, which they may be buying) or gives them draft
    /// picks (compensation outside their lineup) is not judged here.
    /// The error range is the lineup part of `edge_error` (which a dynasty mode scales by the sum of
    /// its weights). Python had no such test.
    pub fn they_would_decline(&self) -> bool {
        let lineup_error = self.edge_error / self.mode.map_or(1.0, |m| me_weight(m) + dyn_weight(m));
        !python_acceptance()
            && self.delta_dyn_n >= 0.0
            && self.give_picks.is_empty()
            && self.delta_them < EVAL_DECLINE_EDGE * self.unit
            && self.delta_them + lineup_error < 0.0
    }
    fn plausible(&self) -> bool {
        !self.screened
            && self.blocked.is_none()
            && self.my_illegal.is_none()
            && self.p() >= MIN_P_ACCEPT - 1e-9
            && !self.they_would_decline()
    }
    pub fn near_plausible(&self) -> bool {
        !self.screened && self.blocked.is_none() && self.my_illegal.is_none() && self.p() >= MIN_P_ACCEPT - 0.05
    }
    pub fn accepted_future(&self) -> bool {
        self.plausible() && self.win_now_cost() && self.delta_dyn_n > MIN_GAIN_ME * self.unit
    }
    pub fn accepted(&self) -> bool {
        if !self.plausible() {
            return false;
        }
        if self.mode.is_some() {
            if self.win_now_cost() {
                return false;
            }
            if self.mode == Some("balanced") && self.delta_me < -BALANCED_TOLERANCE * self.unit {
                return false;
            }
        }
        self.my_edge() > MIN_GAIN_ME * self.unit
    }
    /// `len(ev.me_base)` (DELTA_ME text).
    pub fn me_base_len(&self) -> usize {
        self.me_base.len()
    }
}

/// Per-player data the hot loops read (index = `Pid`).
struct PlayerRow {
    fpg: f64,
    v: f64,
    elig: u16,
    /// bit per `ctx.position_limits` key (limits order)
    limit: u32,
    /// `_drop_tier` = (protected, no data)
    tier: (bool, bool),
}

/// `_TradeEngine`.
pub struct TradeEngine<'a, 'd> {
    pub(crate) d: &'d TradeData<'a>,
    pub ctx: &'a LeagueContext,
    pub values: &'a Map<Cid, PlayerValue>,
    pub market: MarketPool<'d>,
    pub params: AcceptParams,
    pub standing: Map<String, String>,
    pub rate: GameRate,
    pub mode: Option<&'static str>,
    pub keepers: Option<i64>,
    pub scale: f64,
    /// [`league_unit`]: league points per ESPN point, for the FPG thresholds
    pub unit: f64,
    pub(crate) protected: HashMap<Pid, String>,
    wants: HashMap<String, Vec<String>>,
    offered: HashMap<String, Vec<String>>,
    exact: bool,
    pub(crate) spec: SlotSpec,
    lineup_slots: Vec<String>,
    pub(crate) pl: Vec<&'a Player>,
    pub(crate) pid: HashMap<&'a str, Pid>,
    rows: Vec<PlayerRow>,
    fpg: Vec<f64>,
    elig: Vec<u16>,
    worth: Vec<f64>,
    /// game days in the started-games window per player ([`StartWindow::mask`]); empty without one
    masks: Vec<u32>,
    pub(crate) sides: Vec<Side<'a>>,
    pub(crate) side_of: HashMap<&'a str, usize>,
    pub(crate) me: usize,
    pub(crate) fas: Vec<Pid>,
    fas_by_slot: Vec<Vec<Pid>>,
    limit_keys: Vec<(String, i64)>,
    roto: Option<RotoScoring>,
    totals: Vec<Map<String, f64>>,
    // memos
    greedy_memo: Memo<RosterKey, Arc<GreedySummary>>,
    exact_memo: Memo<RosterKey, f64>,
    started_memo: Memo<RosterKey, f64>,
    free_memo: Memo<(RosterKey, RosterKey, usize), Option<(Pid, Arc<Vec<Pid>>)>>,
    base_memo: Memo<(usize, Pid), Arc<Vec<Vec<Pid>>>>,
}

/// Settled roster of one side (`_settle_roster`'s tuple).
struct Settled {
    players: Vec<Pid>,
    drops: Vec<Pid>,
    fill: Option<Pid>,
    to_ir: Vec<Pid>,
    blocked: Option<String>,
    cap_note: Option<String>,
}

impl<'a, 'd> TradeEngine<'a, 'd> {
    /// Half-width of the range of a trade's edge: the moved players' projection errors
    /// (`PlayerValue::fpg_error`) added in quadrature, sqrt(sum of squares), as independent errors
    /// combine (for normal errors the mean absolute error scales like the standard deviation, so
    /// this is the typical error of the summed values). In a dynasty mode the same per-game error
    /// enters both the lineup part and the dynasty part of the edge, so it is times the sum of the
    /// mode's two weights. Every moved player counts, started or not, so the range is a little wide.
    pub fn edge_error(&self, give: &[Pid], get: &[Pid]) -> f64 {
        let ss: f64 = give
            .iter()
            .chain(get.iter())
            .map(|&p| self.values.get(&self.p(p).cid).map_or(0.0, |v| v.fpg_error).powi(2))
            .sum();
        self.mode.map_or(1.0, |m| me_weight(m) + dyn_weight(m)) * ss.sqrt()
    }

    pub fn new(d: &'d TradeData<'a>, opts: &EngineOpts<'_>) -> anyhow::Result<Self> {
        let ctx = d.ctx;
        let values = d.values;
        let mode = if ctx.dynasty && d.units == "dynasty" { Some(ctx.dynasty_mode.as_str()) } else { None };
        let keepers = if d.units == "dynasty" { keeper_k(ctx, d.dyn_.as_deref()) } else { None };
        let scale = dynasty_scale(ctx).max(1e-9);
        let market = MarketPool::new(ctx, Some(values), if d.units == "dynasty" { Some(&d.v) } else { None });
        let rate = game_rate(ctx, None)?;
        let spec = SlotSpec::new(&ctx.roster_shape);
        let limit_keys: Vec<(String, i64)> = ctx.position_limits.iter().map(|(k, v)| (k.clone(), *v)).collect();

        // the player table: rostered players (team and slot order), then free agents
        let mut pl: Vec<&'a Player> = Vec::new();
        let mut pid: HashMap<&'a str, Pid> = HashMap::new();
        for t in &ctx.teams {
            for p in t.players_iter() {
                if !pid.contains_key(p.cid.as_str()) {
                    pid.insert(p.cid.as_str(), pl.len() as Pid);
                    pl.push(p);
                }
            }
        }
        for p in &ctx.free_agents {
            if !pid.contains_key(p.cid.as_str()) {
                pid.insert(p.cid.as_str(), pl.len() as Pid);
                pl.push(p);
            }
        }
        let mut protected: HashMap<Pid, String> = HashMap::new();
        if d.units == "dynasty" {
            for t in &ctx.teams {
                for p in t.players_iter() {
                    if let Some(why) = protected_reason(p, ctx, d.dynasty_values, d.dyn_.as_deref()) {
                        protected.insert(pid[p.cid.as_str()], why);
                    }
                }
            }
        }
        let dynasty_units = d.units == "dynasty";
        let rows: Vec<PlayerRow> = pl
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let pv = values.get(&p.cid);
                let mut limit = 0u32;
                if !limit_keys.is_empty() {
                    for k in limit_positions(p, &ctx.position_limits) {
                        if let Some(j) = ctx.position_limits.get_index_of(k) {
                            limit |= 1 << j;
                        }
                    }
                }
                PlayerRow {
                    fpg: pv.map_or(0.0, |pv| pv.fpg_for(Horizon::Season)),
                    v: d.v.get(&p.cid).copied().unwrap_or(0.0),
                    elig: spec.elig_mask(p),
                    limit,
                    tier: if dynasty_units { (protected.contains_key(&(i as Pid)), !has_data(pv)) } else { (false, false) },
                }
            })
            .collect();
        let fpg: Vec<f64> = rows.iter().map(|r| r.fpg).collect();
        let elig: Vec<u16> = rows.iter().map(|r| r.elig).collect();
        let worth: Vec<f64> = pl.iter().map(|p| market_worth(market.value(p))).collect();
        let masks: Vec<u32> = StartWindow::of(ctx).map_or_else(Vec::new, |w| pl.iter().map(|p| w.mask(p)).collect());

        let mut cap: i64 = ctx.roster_shape.iter().filter(|(s, _)| !IR_SLOTS.contains(&s.as_str())).map(|(_, n)| *n).sum();
        if let Some(m) = ctx.max_roster_size.filter(|m| *m != 0) {
            cap = cap.min(m);
        }
        let ir_cap: i64 = ctx.roster_shape.iter().filter(|(s, _)| IR_SLOTS.contains(&s.as_str())).map(|(_, n)| *n).sum();
        let mut eng = TradeEngine {
            d,
            ctx,
            values,
            market,
            params: opts.accept_params.clone().unwrap_or_else(|| accept_params_for(&ctx.provider)),
            standing: team_standing(ctx),
            rate,
            mode,
            keepers,
            scale,
            unit: league_unit(ctx),
            protected,
            wants: opts.wants.map(|w| w.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default(),
            offered: opts.offered.map(|w| w.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default(),
            exact: !opts.greedy_only,
            lineup_slots: lineup::starting_slots(&ctx.roster_shape),
            spec,
            pl,
            pid,
            rows,
            fpg,
            elig,
            worth,
            masks,
            sides: Vec::new(),
            side_of: HashMap::new(),
            me: 0,
            fas: Vec::new(),
            fas_by_slot: Vec::new(),
            limit_keys,
            roto: None,
            totals: Vec::new(),
            greedy_memo: Memo::default(),
            exact_memo: Memo::default(),
            started_memo: Memo::default(),
            free_memo: Memo::default(),
            base_memo: Memo::default(),
        };
        let mut sides = Vec::with_capacity(ctx.teams.len());
        for t in &ctx.teams {
            let active: Vec<Pid> = t
                .slots
                .iter()
                .filter(|s| !IR_SLOTS.contains(&s.slot.as_str()))
                .filter_map(|s| s.player.as_ref())
                .map(|p| eng.pid[p.cid.as_str()])
                .collect();
            let ir: Vec<Pid> = t
                .slots
                .iter()
                .filter(|s| IR_SLOTS.contains(&s.slot.as_str()))
                .filter_map(|s| s.player.as_ref())
                .map(|p| eng.pid[p.cid.as_str()])
                .collect();
            let all: Vec<Pid> = active.iter().chain(ir.iter()).copied().collect();
            let weakest = eng.weakest_slot(&all);
            let mut top: Vec<Pid> = all.iter().copied().filter(|&p| eng.v(p) > 0.0).collect();
            sort_desc_by(&mut top, |a, b| cmp_f64(eng.v(*a), eng.v(*b)));
            top.truncate(TOP_N);
            sides.push(Side {
                team: t,
                capacity: (cap.max(active.len() as i64)) as usize,
                free_ir: (ir_cap - ir.len() as i64).max(0) as usize,
                active,
                ir,
                weakest,
                top,
            });
        }
        eng.side_of = ctx.teams.iter().enumerate().map(|(i, t)| (t.team_id.as_str(), i)).collect();
        eng.me = ctx.my_team_index().ok_or_else(|| fm_core::CoreError::Lookup("no team flagged as mine".into()))?;
        eng.sides = sides;
        let mut fas: Vec<Pid> = ctx
            .free_agents
            .iter()
            .filter(|p| values.contains_key(&p.cid) && matches!(p.status, Status::Healthy | Status::Unknown))
            .map(|p| eng.pid[p.cid.as_str()])
            .collect();
        sort_desc_by(&mut fas, |a, b| cmp_f64(eng.fpg(*a), eng.fpg(*b)));
        eng.fas_by_slot = (0..eng.spec.types.len())
            .map(|t| fas.iter().copied().filter(|&fa| eng.elig[fa as usize] & (1 << t) != 0).collect())
            .collect();
        eng.fas = fas;
        if ctx.scoring.kind == ScoringKind::Roto && !ctx.scoring.categories.is_empty() {
            eng.roto = Some(RotoScoring::new(ctx.scoring.categories.clone(), Some(ctx.scoring.weights.clone())));
            eng.totals = eng.sides.iter().map(|s| eng.totals_of(&s.active)).collect();
        }
        Ok(eng)
    }

    // -- small accessors -------------------------------------------------------------------

    #[inline]
    pub(crate) fn fpg(&self, p: Pid) -> f64 {
        self.fpg[p as usize]
    }
    #[inline]
    pub(crate) fn v(&self, p: Pid) -> f64 {
        self.rows[p as usize].v
    }
    #[inline]
    pub(crate) fn p(&self, p: Pid) -> &'a Player {
        self.pl[p as usize]
    }
    #[inline]
    pub(crate) fn eligible_at(&self, p: Pid, t: usize) -> bool {
        self.elig[p as usize] & (1 << t) != 0
    }
    pub(crate) fn pid_of(&self, cid: &str) -> Option<Pid> {
        self.pid.get(cid).copied()
    }
    pub(crate) fn players(&self, ps: &[Pid]) -> Pl<'a> {
        ps.iter().map(|&p| self.p(p)).collect()
    }
    pub fn picks(&self) -> &Map<String, Vec<PickAsset>> {
        &self.d.picks
    }
    pub fn v_map(&self) -> &Map<Cid, f64> {
        &self.d.v
    }
    pub fn units(&self) -> &'static str {
        self.d.units
    }
    pub fn exact(&self) -> bool {
        self.exact
    }
    /// `eng.lineup.source`
    pub fn source(&self) -> &'static str {
        if self.exact {
            "optimal_lineup"
        } else {
            "greedy"
        }
    }
    pub(crate) fn side_index(&self, team_id: &str) -> Option<usize> {
        self.side_of.get(team_id).copied()
    }

    fn totals_of(&self, players: &[Pid]) -> Map<String, f64> {
        let roto = self.roto.as_ref().expect("roto");
        let empty: Map<String, f64> = Map::new();
        let rows: Vec<&Map<String, f64>> =
            players.iter().map(|&p| self.values.get(&self.p(p).cid).map_or(&empty, |pv| &pv.rates)).collect();
        roto.team_totals(rows.into_iter())
    }

    fn counts(&self, players: impl IntoIterator<Item = Pid>) -> Vec<i64> {
        let mut out = vec![0i64; self.limit_keys.len()];
        for p in players {
            let m = self.rows[p as usize].limit;
            if m == 0 {
                continue;
            }
            for (j, c) in out.iter_mut().enumerate() {
                if m & (1 << j) != 0 {
                    *c += 1;
                }
            }
        }
        out
    }

    // -- lineups -------------------------------------------------------------------------

    pub(crate) fn summary_key(&self, key: RosterKey) -> Arc<GreedySummary> {
        if let Some(hit) = self.greedy_memo.get(&key) {
            return hit.clone();
        }
        let held = greedy_assignment(&key, &self.spec, &self.fpg, &self.elig);
        let s = Arc::new(summarize(&held, &self.fpg));
        self.greedy_memo.insert(key, s.clone());
        s
    }

    pub(crate) fn summary(&self, parts: &[&[Pid]]) -> Arc<GreedySummary> {
        self.summary_key(key_of(parts.iter().copied()))
    }

    /// `_greedy_lineup_value` (cached by roster).
    pub(crate) fn greedy(&self, parts: &[&[Pid]]) -> f64 {
        self.summary(parts).value
    }

    /// The exact solver (`optimal_lineup` on a hypothetical all-bench team), cached by roster.
    fn exact_value(&self, roster: &[Pid]) -> f64 {
        let key = key_of([roster]);
        if let Some(hit) = self.exact_memo.get(&key) {
            return *hit;
        }
        let view = RosterView { entries: key.iter().map(|&p| Entry { slot: "BN", player: self.p(p), starting: false }).collect() };
        let vals = |c: &str| self.values.get(c).map_or(0.0, |pv| pv.lineup_value(Horizon::Season));
        let (_, v) = lineup::best(&view, &self.lineup_slots, &vals, &Default::default());
        self.exact_memo.insert(key, v);
        v
    }

    /// The started-games value of a roster ([`started::started_value`]), cached by roster.
    fn started_value(&self, roster: &[Pid]) -> f64 {
        let key = key_of([roster]);
        if let Some(hit) = self.started_memo.get(&key) {
            return *hit;
        }
        let v = started::started_value(&key, &self.spec, &self.fpg, &self.elig, &self.masks);
        self.started_memo.insert(key, v);
        v
    }

    /// A roster's rest-of-season value per team game. In a daily-lineup league with the schedule
    /// loaded it is the started-games value, which credits bench players and a third goalie for the
    /// games they would start; Python, and every other league, use the best starting lineup's sum.
    /// The started-games total does not depend on `exact`: its daily greedy solve is already exact.
    fn lineup_value(&self, roster: &[Pid], exact: bool) -> f64 {
        if !self.masks.is_empty() {
            return self.started_value(roster);
        }
        if exact && self.exact {
            self.exact_value(roster)
        } else {
            self.greedy(&[roster])
        }
    }

    /// `_delta`: optimal(after) - optimal(before), each the best of its rosters.
    fn delta(&self, afters: &[Vec<Pid>], bases: &[Vec<Pid>], exact: bool) -> f64 {
        let best = |rs: &[Vec<Pid>]| rs.iter().map(|r| self.lineup_value(r, exact)).fold(f64::NEG_INFINITY, f64::max);
        best(afters) - best(bases)
    }

    /// Per-type starters of a roster (uncached; `_weakest_slot` / `_slot_vorps`).
    pub(crate) fn assignment(&self, roster: &[Pid]) -> super::greedy::Held {
        greedy_assignment(roster, &self.spec, &self.fpg, &self.elig)
    }

    fn weakest_slot(&self, roster: &[Pid]) -> Option<(String, f64)> {
        let held = self.assignment(roster);
        let mut worst: Option<(String, f64)> = None;
        for (ti, slot) in self.spec.types.iter().enumerate() {
            if slot == "UTIL" {
                continue;
            }
            let need = self.ctx.roster_shape.get(slot).copied().unwrap_or(0);
            let cids = &held[ti];
            let v = if (cids.len() as i64) < need {
                f64::NEG_INFINITY
            } else {
                cids.iter()
                    .filter_map(|&p| self.values.get(&self.p(p).cid))
                    .map(|pv| pv.vorp)
                    .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.min(x))))
                    .unwrap_or(f64::NEG_INFINITY)
            };
            if worst.as_ref().is_none_or(|w| v < w.1) {
                worst = Some((slot.clone(), v));
            }
        }
        worst
    }

    // -- drops and fills -------------------------------------------------------------------

    fn drop_key(&self, p: Pid) -> (f64, f64) {
        if self.d.units == "dynasty" {
            (self.v(p), self.fpg(p))
        } else {
            (self.fpg(p), self.v(p))
        }
    }

    /// `_drop_order`.
    fn drop_order(&self, mut cands: Vec<Pid>) -> Vec<Pid> {
        cands.sort_by(|a, b| {
            let (x, y) = (self.drop_key(*a), self.drop_key(*b));
            cmp_f64(x.0, y.0).then(cmp_f64(x.1, y.1))
        });
        if self.d.units == "dynasty" {
            cands.sort_by_key(|p| self.rows[*p as usize].tier);
        }
        cands
    }

    /// `_choose_drops(players, cands, n, ir)`.
    fn choose_drops(&self, players: &[Pid], cands: &[Pid], n: usize, ir: &[Pid]) -> Vec<Pid> {
        let mut drops: Vec<Pid> = Vec::new();
        let mut cur: Vec<Pid> = players.to_vec();
        let mut pool = self.drop_order(cands.to_vec());
        for _ in 0..n {
            if pool.is_empty() {
                break;
            }
            let tier = self.rows[pool[0] as usize].tier;
            let group: Vec<Pid> = pool.iter().copied().filter(|p| self.rows[*p as usize].tier == tier).collect();
            if !self.masks.is_empty() {
                let pick = self.cheapest_started_drop(&cur, ir, &group);
                drops.push(pick);
                pool.retain(|p| *p != pick);
                cur.retain(|p| *p != pick);
                continue;
            }
            let starters = self.summary(&[&cur, ir]);
            let pick = match group.iter().copied().find(|p| !starters.is_starter(*p)) {
                Some(b) => b,
                None => {
                    let base = starters.value;
                    let all: Vec<Pid> = cur.iter().chain(ir.iter()).copied().collect();
                    let cost = |p: Pid| {
                        let without: Vec<Pid> = all.iter().copied().filter(|q| *q != p).collect();
                        base - self.greedy(&[&without])
                    };
                    let costs: Vec<(Pid, f64)> = group.iter().map(|&p| (p, cost(p))).collect();
                    first_min_by(costs.iter(), |a, b| cmp_f64(a.1, b.1)).expect("group").0
                }
            };
            drops.push(pick);
            pool.retain(|p| *p != pick);
            cur.retain(|p| *p != pick);
        }
        drops
    }

    /// The member of `group` (non-empty, in drop order) whose loss costs the roster `cur` + `ir`
    /// the least started-games value; ties go to the earliest. A player's loss is at most his own
    /// credit (`fpg x started / games`), since the others can only gain the days he frees, so
    /// candidates are tried by ascending credit and the search stops once no credit can beat the
    /// best loss found. Python, and the best-lineup engine, drop the first non-starter instead.
    fn cheapest_started_drop(&self, cur: &[Pid], ir: &[Pid], group: &[Pid]) -> Pid {
        let all = key_of([cur, ir]);
        let starts = started::started_days(&all, &self.spec, &self.fpg, &self.elig, &self.masks);
        let base = started::value_of_starts(&all, &starts, &self.fpg, &self.masks);
        let credit = |p: Pid| match all.binary_search(&p) {
            Ok(i) if starts[i] > 0 => self.fpg(p) * starts[i] as f64 / self.masks[p as usize].count_ones() as f64,
            _ => 0.0,
        };
        let mut order: Vec<(Pid, f64)> = group.iter().map(|&p| (p, credit(p))).collect();
        order.sort_by(|a, b| cmp_f64(a.1, b.1));
        let mut best: Option<(Pid, f64)> = None;
        for (p, c) in order {
            if best.is_some_and(|(_, l)| c >= l - 1e-12) {
                break;
            }
            let without: Vec<Pid> = all.iter().copied().filter(|q| *q != p).collect();
            let loss = base - self.started_value(&without);
            if best.is_none_or(|(_, l)| loss < l - 1e-12) {
                best = Some((p, loss));
            }
        }
        best.expect("group").0
    }

    /// `_pick_fill(players, outgoing, ir)`. With a start window the gain is in started-games value
    /// (which counts a bench fill's starts) rather than Python's best-lineup gain.
    fn pick_fill(&self, players: &[Pid], outgoing: &[Pid], ir: &[Pid]) -> Option<Pid> {
        let has_limits = !self.limit_keys.is_empty();
        let counts = if has_limits { self.counts(players.iter().chain(ir.iter()).copied()) } else { Vec::new() };
        let present: SmallVec<[Pid; 32]> = players.iter().chain(ir.iter()).copied().collect();
        let legal = |fa: Pid| -> bool {
            if present.contains(&fa) {
                return false;
            }
            if has_limits {
                let m = self.rows[fa as usize].limit;
                for (j, (_, lim)) in self.limit_keys.iter().enumerate() {
                    if m & (1 << j) != 0 && counts[j] + 1 > *lim {
                        return false;
                    }
                }
            }
            true
        };
        let mut cands: Vec<Pid> = Vec::new();
        for fas in &self.fas_by_slot {
            let mut n = 0;
            for &fa in fas {
                if n >= FILL_CANDIDATES {
                    break;
                }
                if legal(fa) {
                    if !cands.contains(&fa) {
                        cands.push(fa);
                    }
                    n += 1;
                }
            }
        }
        let lineup: Vec<Pid> = players.iter().chain(ir.iter()).copied().collect();
        let started = !self.masks.is_empty();
        let starters = self.summary(&[&lineup]);
        let base = if started { self.started_value(&lineup) } else { starters.value };
        // a free agent adds at most his own FPG, and in the best lineup only over its weakest starter
        let floor = if !started && starters.starters.len() >= self.spec.n_slots {
            lineup
                .iter()
                .copied()
                .filter(|p| starters.is_starter(*p))
                .map(|p| self.fpg(p))
                .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.min(x))))
                .unwrap_or(0.0)
        } else {
            0.0
        };
        let mut best: Option<Pid> = None;
        let mut best_gain = 1e-9;
        sort_desc_by(&mut cands, |a, b| cmp_f64(self.fpg(*a), self.fpg(*b)));
        for &fa in &cands {
            if self.fpg(fa) - floor <= best_gain + 1e-12 {
                break;
            }
            let gain = if started {
                let with: Vec<Pid> = lineup.iter().copied().chain(std::iter::once(fa)).collect();
                self.started_value(&with) - base
            } else {
                self.greedy(&[&lineup, &[fa]]) - base
            };
            if gain > best_gain + 1e-12 {
                best = Some(fa);
                best_gain = gain;
            }
        }
        if best.is_some() {
            return best;
        }
        let nt = self.spec.types.len();
        let mut types: Vec<usize> = (0..nt).filter(|&t| self.spec.types[t] != "UTIL").collect();
        if types.is_empty() {
            types = (0..nt).collect();
        }
        let mut need: u32 = 0;
        for &o in outgoing {
            for &t in &types {
                if self.eligible_at(o, t) {
                    need |= 1 << t;
                }
            }
        }
        self.fas.iter().copied().find(|&fa| legal(fa) && (self.elig[fa as usize] as u32) & need != 0)
    }

    /// `_free_fill(active, ir, capacity)`: (FA, active roster with him) when the lineup leaves a
    /// starting slot empty and a free agent can fill it today. Cached by (active, ir, capacity).
    fn free_fill(&self, active: &[Pid], ir: &[Pid], capacity: usize) -> Option<(Pid, Arc<Vec<Pid>>)> {
        let key = (key_of([active]), key_of([ir]), capacity);
        if let Some(hit) = self.free_memo.get(&key) {
            return hit.clone();
        }
        let mut hit = None;
        if self.summary(&[active, ir]).starters.len() < self.spec.n_slots {
            if let Some(fa) = self.pick_fill(active, &[], ir) {
                let mut alt: Option<Vec<Pid>> = if active.len() < capacity {
                    Some(active.iter().copied().chain(std::iter::once(fa)).collect())
                } else {
                    let cut = self.choose_drops(active, active, 1, ir);
                    cut.first().map(|c| active.iter().copied().filter(|p| p != c).chain(std::iter::once(fa)).collect())
                };
                if let Some(a) = &alt {
                    if !self.limit_keys.is_empty() && self.breaks_limits(&[active, ir], &[a, ir]) {
                        alt = None;
                    }
                }
                if let Some(a) = alt {
                    hit = Some((fa, Arc::new(a)));
                }
            }
        }
        self.free_memo.insert(key, hit.clone());
        hit
    }

    /// `any(after[k] > max(limit, before[k]) for k in limits)`.
    fn breaks_limits(&self, before: &[&[Pid]], after: &[&[Pid]]) -> bool {
        let b = self.counts(before.iter().flat_map(|x| x.iter().copied()));
        let a = self.counts(after.iter().flat_map(|x| x.iter().copied()));
        self.limit_keys.iter().enumerate().any(|(j, (_, lim))| a[j] > (*lim).max(b[j]))
    }

    /// `_with_free_fill(active, ir, capacity, side)`.
    fn with_free_fill(&self, active: &[Pid], ir: &[Pid], capacity: usize, side: Option<&Side<'a>>) -> Vec<Vec<Pid>> {
        let mut rosters: Vec<Vec<Pid>> = vec![active.iter().chain(ir.iter()).copied().collect()];
        if let Some(s) = side {
            if self.free_fill(&s.active, &s.ir, s.capacity).is_none() {
                return rosters;
            }
        }
        if let Some((_, alt)) = self.free_fill(active, ir, capacity) {
            rosters.push(alt.iter().chain(ir.iter()).copied().collect());
        }
        rosters
    }

    /// `_baselines(side, fill)`.
    fn baselines(&self, si: usize, fill: Option<Pid>) -> Arc<Vec<Vec<Pid>>> {
        let side = &self.sides[si];
        let rosters = self.with_free_fill(&side.active, &side.ir, side.capacity, None);
        let Some(fill) = fill.filter(|f| !side.active.contains(f)) else {
            return Arc::new(rosters);
        };
        if let Some(hit) = self.base_memo.get(&(si, fill)) {
            return hit.clone();
        }
        let mut out = rosters;
        out.extend(self.baselines_uncached(side, fill).into_iter().skip(1));
        let out = Arc::new(out);
        self.base_memo.insert((si, fill), out.clone());
        out
    }

    fn baselines_uncached(&self, side: &Side<'a>, fill: Pid) -> Vec<Vec<Pid>> {
        let cur: Vec<Pid> = side.active.iter().chain(side.ir.iter()).copied().collect();
        let alt: Vec<Pid> = if side.active.len() < side.capacity {
            side.active.iter().copied().chain(std::iter::once(fill)).collect()
        } else {
            let cut = self.choose_drops(&side.active, &side.active, 1, &side.ir);
            let Some(c) = cut.first() else { return vec![cur] };
            side.active.iter().copied().filter(|p| p != c).chain(std::iter::once(fill)).collect()
        };
        if !self.limit_keys.is_empty() && self.breaks_limits(&[&side.active, &side.ir], &[&alt, &side.ir]) {
            return vec![cur];
        }
        let alt_full: Vec<Pid> = alt.iter().chain(side.ir.iter()).copied().collect();
        vec![cur, alt_full]
    }

    /// `_holes(side, before, after)`: starting slots `after` can fill fewer than `before`.
    fn holes(&self, side: &Side<'a>, before: &[Pid], after: &[Pid]) -> (i64, Option<String>) {
        let b = self.summary(&[before]);
        let a = self.summary(&[after]);
        let mut parts: Vec<String> = Vec::new();
        let mut n = 0i64;
        for (ti, s) in self.spec.types.iter().enumerate() {
            let bn = b.counts[ti] as i64;
            if bn == 0 {
                continue; // `for s in b`: only types b fills
            }
            let an = a.counts[ti] as i64;
            let k = bn - an;
            if k > 0 {
                n += k;
                let shape = self.ctx.roster_shape.get(s).map_or("None".to_string(), |v| v.to_string());
                parts.push(format!("{k} {s} ({an} of {shape} filled)"));
            }
        }
        if n <= 0 {
            return (0, None);
        }
        (n, Some(format!("leaves {} short {}", side.team.name, parts.join(", "))))
    }

    /// `_settle_roster(side, players, incoming, outgoing)`.
    fn settle_roster(&self, side: &Side<'a>, mut players: Vec<Pid>, incoming: &[Pid], outgoing: &[Pid]) -> Settled {
        let mut blocked: Option<String> = None;
        let mut cap_note: Option<String> = None;
        let mut drops: Vec<Pid> = Vec::new();
        let mut to_ir: Vec<Pid> = Vec::new();
        let mut fill: Option<Pid> = None;
        let ir_left: Vec<Pid> = side.ir.iter().copied().filter(|p| !outgoing.contains(p)).collect();
        if !self.limit_keys.is_empty() && !incoming.is_empty() {
            let before = self.counts(side.active.iter().chain(side.ir.iter()).copied());
            let after = self.counts(players.iter().chain(ir_left.iter()).copied());
            let mut notes: Vec<String> = Vec::new();
            let mut order: Vec<usize> = (0..self.limit_keys.len()).collect();
            order.sort_by(|a, b| self.limit_keys[*a].0.cmp(&self.limit_keys[*b].0));
            for j in order {
                let (k, lim) = &self.limit_keys[j];
                let need = after[j] - (*lim).max(before[j]);
                if need <= 0 {
                    continue;
                }
                let pool = self.drop_order(
                    players
                        .iter()
                        .copied()
                        .filter(|p| !incoming.contains(p) && !drops.contains(p) && self.rows[*p as usize].limit & (1 << j) != 0)
                        .collect(),
                );
                if (pool.len() as i64) < need {
                    blocked = blocked.or_else(|| Some(format!("would break the {}", cap_text(self.ctx, k, Some(after[j])))));
                }
                drops.extend(pool.iter().take(need as usize).copied());
                notes.push(cap_text(self.ctx, k, Some(after[j])));
            }
            if !notes.is_empty() {
                cap_note = Some(notes.join("; "));
                players.retain(|p| !drops.contains(p));
            }
        }
        let mut over = players.len() as i64 - side.capacity as i64;
        if over > 0 {
            if side.free_ir > 0 {
                let mut movable: Vec<Pid> =
                    players.iter().copied().filter(|p| matches!(self.p(*p).status, Status::Ltir | Status::Ir)).collect();
                let idx = |p: Pid| if self.p(p).status == Status::Ltir { 0 } else { 1 };
                movable.sort_by(|a, b| idx(*a).cmp(&idx(*b)).then(cmp_f64(-self.fpg(*a), -self.fpg(*b))));
                movable.truncate((over as usize).min(side.free_ir));
                to_ir = movable;
                over -= to_ir.len() as i64;
            }
            let kept: Vec<Pid> = players.iter().copied().filter(|p| !to_ir.contains(p)).collect();
            let cands: Vec<Pid> = kept.iter().copied().filter(|p| !incoming.contains(p)).collect();
            let ir_pool: Vec<Pid> = ir_left.iter().chain(to_ir.iter()).copied().collect();
            let size_drops = self.choose_drops(&kept, &cands, over.max(0) as usize, &ir_pool);
            drops.extend(size_drops.iter().copied());
            players.retain(|p| !to_ir.contains(p) && !size_drops.contains(p));
        } else if players.len() < side.capacity && outgoing.len() > incoming.len() {
            fill = self.pick_fill(&players, outgoing, &ir_left);
            if let Some(f) = fill {
                players.push(f);
            }
        }
        let v_in = py_sum(incoming.iter().map(|&p| self.v(p)));
        for &dp in &drops {
            if let Some(why) = self.protected.get(&dp) {
                if v_in < PROTECT_OVERRIDE * self.v(dp) {
                    blocked = blocked.or_else(|| Some(format!("would force dropping {} ({why})", self.p(dp).name)));
                    break;
                }
            }
        }
        Settled { players, drops, fill, to_ir, blocked, cap_note }
    }

    // -- acceptance ---------------------------------------------------------------------------

    /// `p_accept(deal, their_team, ...)` with the engine's market, params and standings.
    pub fn accept(&self, ev: &TradeEval<'a>) -> Acceptance {
        let ctx = self.ctx;
        let tid = ev.team.team_id.as_str();
        let st_them = if !ev.give_picks.is_empty() || !ev.get_picks.is_empty() {
            self.standing.get(tid).map(|s| s.as_str())
        } else {
            None
        };
        let gets = package_value(
            ev.give_ix.iter().map(|&p| self.worth[p as usize]).chain(ev.give_picks.iter().map(|a| pick_worth(&self.market, a, st_them))),
        );
        let gives = package_value(
            ev.get_ix.iter().map(|&p| self.worth[p as usize]).chain(ev.get_picks.iter().map(|a| pick_worth(&self.market, a, st_them))),
        );
        let mut perceived = gets - gives;
        let mut notes: Vec<String> = Vec::new();
        if !ev.their_drops.is_empty() {
            let pen = ROSTER_SPOT_PENALTY * ev.their_drops.len() as f64;
            perceived -= pen;
            notes.push(format!("-{} they must drop {}", g(pen), names(ev.their_drops.iter())));
        }
        if ev.need_text.is_some() {
            perceived += NEED_PTS;
            notes.push(format!("+{} fills their weakest slot", g(NEED_PTS)));
        }
        if ev.hole_slots != 0 {
            let pen = HOLE_PTS * ev.hole_slots as f64;
            perceived -= pen;
            notes.push(format!("-{} {}", g(pen), ev.hole_text.as_deref().unwrap_or("None")));
        }
        if ev.block_text.is_some() {
            perceived += BLOCK_PTS;
            notes.push(format!("+{} trade block", g(BLOCK_PTS)));
        }
        if ev.pressure_text.as_deref().is_some_and(|s| !s.is_empty()) {
            perceived += PRESSURE_PTS;
            notes.push(format!("+{} relieves their roster pressure", g(PRESSURE_PTS)));
        }
        let p_raw = self.params.p(perceived);
        let mut p = p_raw;
        let add: Vec<&Player> = ev.give.iter().copied().chain(ev.their_fill).collect();
        let drop: Vec<&Player> = ev.get.iter().copied().chain(ev.their_drops.iter().copied()).collect();
        let (legal, why) = roster_legal_after(ev.team, ctx, &add, &drop, &ev.their_ir);
        if !legal {
            p = 0.0;
            notes.push(format!("illegal for them ({})", why.as_deref().unwrap_or("None")));
        }
        if ctx.dynasty && p > 0.0 {
            let st = self.standing.get(tid).map(|s| s.as_str());
            if st == Some("contender") && ev.delta_them <= 0.0 {
                p *= CONTENDER_FUTURE_MULT;
                notes.push(format!("x{}: contender, no help this season", g(CONTENDER_FUTURE_MULT)));
            } else if st == Some("rebuilder") {
                let (a_in, a_out) = (self.mean_age(&ev.give), self.mean_age(&ev.get));
                if let (Some(a_in), Some(a_out)) = (a_in, a_out) {
                    if a_in >= a_out + REBUILD_AGE_GAP {
                        p *= REBUILDER_VETERAN_MULT;
                        notes.push(format!("x{}: rebuilding, gets older players", g(REBUILDER_VETERAN_MULT)));
                    }
                }
            }
        }
        let sources = self.market.sources(ev.give.iter().copied().chain(ev.get.iter().copied()));
        Acceptance { gets, gives, perceived, p_raw, p, notes, legal, legal_note: why, sources, params: self.params.clone() }
    }

    fn mean_age(&self, ps: &[&Player]) -> Option<f64> {
        let ages: Vec<f64> = ps.iter().filter_map(|p| player_age(p, self.ctx, self.d.dynasty_values)).collect();
        if ages.is_empty() {
            None
        } else {
            Some(py_sum(ages.iter().copied()) / ages.len() as f64)
        }
    }

    /// `_pkg(players)`: package market worth.
    pub(crate) fn pkg(&self, ps: &[Pid]) -> f64 {
        package_value(ps.iter().map(|&p| self.worth[p as usize]))
    }

    // -- one proposal ---------------------------------------------------------------------------

    /// `evaluate(team_id, give, get, screen, exact, pressure)`.
    pub(crate) fn evaluate(&self, si: usize, give: &[Pid], get: &[Pid], screen: bool, exact: bool, pressure: Option<String>) -> TradeEval<'a> {
        let me = &self.sides[self.me];
        let them = &self.sides[si];
        let vs = |ps: &[Pid]| ps.iter().map(|&p| self.v(p)).collect::<Vec<f64>>();
        let (fair, dev) = fairness(&vs(get), &vs(give), self.d.repl);

        let me_after: Vec<Pid> = me.active.iter().copied().filter(|p| !give.contains(p)).chain(get.iter().copied()).collect();
        let mine = self.settle_roster(me, me_after, get, give);
        let v_in = py_sum(get.iter().map(|&p| self.v(p)));
        let v_out = py_sum(give.iter().map(|&p| self.v(p)));
        let net_added = get.len() as i64 - give.len() as i64 - mine.drops.len() as i64 + mine.fill.is_some() as i64;
        let me_ir_pool: Vec<Pid> = me.ir.iter().copied().filter(|p| !give.contains(p)).chain(mine.to_ir.iter().copied()).collect();
        let spot_cost = self.d.repl * net_added as f64;
        let mut keeper_change = None;
        let d_dyn = if self.keepers.is_some() {
            let before = me.active.iter().chain(me.ir.iter()).map(|&p| self.p(p));
            let after = mine.players.iter().chain(me_ir_pool.iter()).map(|&p| self.p(p));
            let ch = dynasty_change(self.ctx, self.d.dyn_.as_deref(), before, after);
            let d = ch.total - spot_cost;
            keeper_change = Some(ch);
            d
        } else {
            v_in - v_out - py_sum(mine.drops.iter().map(|&p| self.v(p))) + mine.fill.map_or(0.0, |f| self.v(f)) - spot_cost
        };
        let d_dyn_n = if self.mode.is_some() { d_dyn / self.scale } else { 0.0 };
        let me_pool = self.with_free_fill(&mine.players, &me_ir_pool, me.capacity, Some(me));
        let me_base = self.baselines(self.me, mine.fill);
        let d_me = self.delta(&me_pool, &me_base, false);
        let edge_error = self.edge_error(give, get);
        let bar = (MIN_GAIN_ME - SCREEN_MARGIN) * self.unit;
        let mine_ok = match self.mode {
            None => d_me > bar,
            Some(m) => {
                let core = me_weight(m) * d_me + dyn_weight(m) * d_dyn_n;
                core > bar || d_dyn_n > bar
            }
        };
        let mut ev = TradeEval {
            team: them.team,
            give: self.players(give),
            get: self.players(get),
            give_ix: SmallVec::from_slice(give),
            get_ix: SmallVec::from_slice(get),
            v_in,
            v_out,
            fair,
            fair_dev: dev,
            delta_me: d_me,
            delta_them: 0.0,
            need_text: None,
            my_drops: self.players(&mine.drops),
            their_drops: Pl::new(),
            my_fill: mine.fill.map(|f| self.p(f)),
            their_fill: None,
            roto_adj: 0.0,
            my_ir: self.players(&mine.to_ir),
            their_ir: Pl::new(),
            mode: self.mode,
            delta_dyn: d_dyn,
            delta_dyn_n: d_dyn_n,
            keeper_change,
            spot_cost,
            blocked: mine.blocked.clone(),
            my_cap: mine.cap_note.clone(),
            their_cap: None,
            block_text: None,
            my_illegal: None,
            acceptance: None,
            screened: false,
            pressure_text: pressure,
            hole_text: None,
            hole_slots: 0,
            rate: Some(self.rate),
            refined: false,
            me_after: Arc::new(Vec::new()),
            them_after: Arc::new(Vec::new()),
            me_base: Arc::new(Vec::new()),
            them_base: Arc::new(Vec::new()),
            free_fill: None,
            give_picks: Vec::new(),
            get_picks: Vec::new(),
            unit: self.unit,
            edge_error,
        };
        if screen && (!mine_ok || mine.blocked.is_some()) {
            ev.screened = true;
            return ev;
        }

        let them_after: Vec<Pid> = them.active.iter().copied().filter(|p| !get.contains(p)).chain(give.iter().copied()).collect();
        let theirs = self.settle_roster(them, them_after, give, get);
        let blocked = mine.blocked.clone().or_else(|| theirs.blocked.as_ref().map(|b| format!("{} {b}", them.team.name)));
        let mut need_text = None;
        if let Some((slot, v)) = &them.weakest {
            let ti = self.spec.type_index(slot);
            let fits: Vec<Pid> = give
                .iter()
                .copied()
                .filter(|&p| {
                    ti.is_some_and(|t| self.eligible_at(p, t))
                        && !matches!(self.p(p).status, Status::Out | Status::Ir | Status::Ltir | Status::Suspended)
                })
                .collect();
            if let Some(&f) = fits.first() {
                let vtxt = if *v == f64::NEG_INFINITY { "empty".to_string() } else { format!("starter VORP {}", fixed(*v, 2, true)) };
                need_text = Some(format!("{} fills {}'s weakest slot {slot} ({vtxt})", self.p(f).name, them.team.name));
            }
        }
        let their_ir_pool: Vec<Pid> = them.ir.iter().copied().filter(|p| !get.contains(p)).chain(theirs.to_ir.iter().copied()).collect();
        let before_them: Vec<Pid> = them.active.iter().chain(them.ir.iter()).copied().collect();
        let after_them: Vec<Pid> = theirs.players.iter().chain(their_ir_pool.iter()).copied().collect();
        let (hole_slots, hole_text) = self.holes(them, &before_them, &after_them);
        let mut block: Vec<String> = Vec::new();
        let tid = them.team.team_id.as_str();
        let offered: Vec<&Player> = match self.offered.get(tid) {
            Some(off) => get.iter().map(|&p| self.p(p)).filter(|p| off.iter().any(|c| *c == p.cid)).collect(),
            None => Vec::new(),
        };
        if !offered.is_empty() {
            block.push(format!(
                "{} {} on {}'s trade block",
                names(offered.iter()),
                if offered.len() == 1 { "is" } else { "are" },
                them.team.name
            ));
        }
        if let Some(want) = self.wants.get(tid).filter(|w| !w.is_empty()) {
            if let Some(f) = give.iter().map(|&p| self.p(p)).find(|p| p.positions.iter().any(|x| want.contains(x))) {
                let mut w: Vec<&str> = want.iter().map(|s| s.as_str()).collect();
                w.sort();
                w.dedup();
                block.push(format!("{} matches {}'s trade-block wants ({})", f.name, them.team.name, w.join(", ")));
            }
        }
        let add: Vec<&Player> = ev.get.iter().copied().chain(ev.my_fill).collect();
        let drop: Vec<&Player> = ev.give.iter().copied().chain(ev.my_drops.iter().copied()).collect();
        let (ok, why) = roster_legal_after(me.team, self.ctx, &add, &drop, &ev.my_ir);
        let them_pool = self.with_free_fill(&theirs.players, &their_ir_pool, them.capacity, Some(them));
        let them_base = self.baselines(si, theirs.fill);
        let d_them = self.delta(&them_pool, &them_base, false);
        ev.delta_them = d_them;
        ev.need_text = need_text;
        ev.their_drops = self.players(&theirs.drops);
        ev.their_fill = theirs.fill.map(|f| self.p(f));
        ev.their_ir = self.players(&theirs.to_ir);
        ev.blocked = blocked;
        ev.their_cap = theirs.cap_note;
        ev.block_text = if block.is_empty() { None } else { Some(block.join("; ")) };
        ev.my_illegal = if ok { None } else { Some(why.unwrap_or_else(|| "None".to_string())) };
        ev.hole_text = hole_text;
        ev.hole_slots = hole_slots;
        ev.free_fill = self.free_fill(&me.active, &me.ir, me.capacity).map(|(fa, _)| self.p(fa));
        ev.me_after = Arc::new(me_pool);
        ev.them_after = Arc::new(them_pool);
        ev.me_base = me_base;
        ev.them_base = them_base;
        ev.acceptance = Some(self.accept(&ev));
        if exact && (!screen || ev.near_plausible()) {
            self.refine(&mut ev);
        }
        if let Some(roto) = &self.roto {
            let mi = self.me;
            let others: Vec<&Map<String, f64>> = self.totals.iter().enumerate().filter(|(i, _)| *i != mi).map(|(_, t)| t).collect();
            let before = roto.category_balance_penalty(&self.totals[mi], others.into_iter());
            let them_totals = self.totals_of(&theirs.players);
            let others_after: Vec<&Map<String, f64>> = self
                .totals
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != mi)
                .map(|(i, t)| if i == si { &them_totals } else { t })
                .collect();
            let after = roto.category_balance_penalty(&self.totals_of(&mine.players), others_after.into_iter());
            ev.roto_adj = ROTO_PENALTY_WEIGHT * (after - before);
        }
        ev
    }

    /// `refine(ev)`: re-solve both lineups exactly and refresh the acceptance in dynasty leagues.
    pub fn refine(&self, ev: &mut TradeEval<'a>) {
        if ev.refined || ev.screened {
            return;
        }
        ev.refined = true;
        if !self.exact {
            return;
        }
        ev.delta_me = self.delta(&ev.me_after, &ev.me_base, true);
        ev.delta_them = self.delta(&ev.them_after, &ev.them_base, true);
        if self.ctx.dynasty {
            ev.acceptance = Some(self.accept(ev));
        }
    }

    /// `with_picks(ev, give_picks, get_picks)`: a copy of `ev` with draft picks added.
    pub fn with_picks(&self, ev: &TradeEval<'a>, give_picks: &[PickAsset], get_picks: &[PickAsset]) -> TradeEval<'a> {
        let mut var = ev.clone();
        var.give_picks = give_picks.to_vec();
        var.get_picks = get_picks.to_vec();
        var.delta_dyn = ev.delta_dyn + py_sum(get_picks.iter().map(|a| a.net)) - py_sum(give_picks.iter().map(|a| a.net));
        var.delta_dyn_n = if self.mode.is_some() { var.delta_dyn / self.scale } else { 0.0 };
        var.acceptance = Some(self.accept(&var));
        var
    }

    /// `with_pick(ev)`: `ev` with one draft pick added to balance it, or None.
    pub fn with_pick(&self, ev: &TradeEval<'a>) -> Option<TradeEval<'a>> {
        if self.mode.is_none()
            || self.d.picks.is_empty()
            || ev.screened
            || ev.acceptance.is_none()
            || !ev.give_picks.is_empty()
            || !ev.get_picks.is_empty()
        {
            return None;
        }
        let mine = self.ctx.teams[self.me].team_id.as_str();
        let empty: Vec<PickAsset> = Vec::new();
        let (give_side, options): (bool, Vec<&PickAsset>) = if ev.perceived() > PICK_ASK_MIN {
            (false, self.d.picks.get(&ev.team.team_id).unwrap_or(&empty).iter().collect())
        } else if ev.p() < MIN_P_ACCEPT {
            let mut o: Vec<&PickAsset> = self.d.picks.get(mine).unwrap_or(&empty).iter().collect();
            o.sort_by(|a, b| cmp_f64(a.net, b.net).then(cmp_f64(-a.value, -b.value)));
            (true, o)
        } else {
            return None;
        };
        let mut best: Option<TradeEval<'a>> = None;
        for a in options {
            let one = std::slice::from_ref(a);
            let var = if give_side { self.with_picks(ev, one, &[]) } else { self.with_picks(ev, &[], one) };
            if !var.accepted() || (ev.accepted() && var.ev() <= ev.ev()) {
                continue;
            }
            if give_side {
                return Some(var);
            }
            if best.as_ref().is_none_or(|b| var.ev() > b.ev()) {
                best = Some(var);
            }
        }
        best
    }

    /// `candidates(team_id)`: (give, get, market base) for every 1-for-1 / 2-for-1 / 1-for-2 that
    /// could still reach MIN_P_ACCEPT with every bonus, closest to even first, capped.
    pub(crate) fn candidates(&self, si: usize) -> Vec<(SmallVec<[Pid; 2]>, SmallVec<[Pid; 2]>, f64)> {
        let mine = &self.sides[self.me].top;
        let theirs = &self.sides[si].top;
        let floor = self.params.perceived_for(MIN_P_ACCEPT) - NEED_PTS - BLOCK_PTS;
        let mut shapes: Vec<(SmallVec<[Pid; 2]>, SmallVec<[Pid; 2]>)> = Vec::new();
        for &a in mine {
            for &b in theirs {
                shapes.push((smallvec::smallvec![a], smallvec::smallvec![b]));
            }
        }
        for i in 0..mine.len() {
            for j in i + 1..mine.len() {
                for &b in theirs {
                    shapes.push((smallvec::smallvec![mine[i], mine[j]], smallvec::smallvec![b]));
                }
            }
        }
        for &a in mine {
            for i in 0..theirs.len() {
                for j in i + 1..theirs.len() {
                    shapes.push((smallvec::smallvec![a], smallvec::smallvec![theirs[i], theirs[j]]));
                }
            }
        }
        let mut out: Vec<(SmallVec<[Pid; 2]>, SmallVec<[Pid; 2]>, f64)> = shapes
            .into_iter()
            .filter_map(|(give, get)| {
                let base = self.pkg(&give) - self.pkg(&get);
                (floor - 1e-9 <= base && base <= OVERPAY_CAP + 1e-9).then_some((give, get, base))
            })
            .collect();
        out.sort_by(|a, b| (a.0.len() + a.1.len()).cmp(&(b.0.len() + b.1.len())).then(cmp_f64(a.2.abs(), b.2.abs())));
        out.truncate(MAX_EVALS_PER_TEAM);
        out
    }
}

/// `f"{x:g}"`.
pub(crate) fn g(x: f64) -> String {
    fm_pycompat::fmt::general(x, None, false)
}
