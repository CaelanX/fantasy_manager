//! Rust port of `fantasy_manager/recommend/trades.py`: trade proposals ranked by expected value,
//! my gain x the chance the other manager says yes (see the Python module docstring for the
//! model: ΔMe from optimal lineups, the market-perception acceptance model with ROSTER_SPOT,
//! NEED, HOLE, BLOCK and PRESSURE points, dynasty / keeper-league ΔDyn, draft-pick add-ins,
//! sweet spot, exploits and the need finder). Owner: work package `W7-trades`.
//!
//! Submodules: `greedy` (slot structure, greedy lineup, memo keys), `accept` (market worth and
//! the acceptance curve), `engine` (`_TradeEngine`, `TradeEval`), `rec` (`_to_rec`, `diverse`),
//! `search` (`recommend_trades`), `offer` (`evaluate_trade` / `evaluate_offer` /
//! `offer_summary`, trade blocks, name resolution), `exploit`, `need`.
//!
//! Performance (the hot path of the app): the engine is built once per league; counterparties
//! and their candidates are evaluated in parallel with rayon and merged back in Python's order
//! before the same stable sorts. Roster caches are keyed by sorted player-index lists instead of
//! frozensets of cids and shared in concurrent maps; each cached value is computed from its key
//! alone, so results never depend on thread scheduling.
//!
//! Deliberate deviations (all invisible unless noted):
//! * The exact lineup solver cannot fail, so Python's permanent switch to greedy after a solver
//!   exception does not exist; `EngineOpts::greedy_only` is `lineup_fn=False`.
//! * Cached roster results are computed from the players in index order (Python: the list order of
//!   the first call). They differ only when two players tie exactly on season FPG at the lineup
//!   margin; totals can then differ in the last bit (Python sums a frozenset there).
//! * `_free_fill` is cached by (active, IR, capacity) instead of the union of active + IR.
//! * `find_needs` seeds keeper-league position groups in (F, D, G) order (a Python set).
//! * Engine construction warnings (draft pick values unavailable) cannot occur: pick valuation is
//!   infallible here.
//! * `calibrate_acceptance` reads the ledger through the `LedgerQuery` trait (fm-harness is above
//!   this crate).

mod accept;
mod engine;
mod exploit;
mod greedy;
mod need;
mod offer;
mod rec;
mod search;

pub use crate::recommend::market::CLEAR_FAVOR_PTS;
pub use accept::{
    acceptance_observations, calibrate_acceptance, calibrate_from, market_worth, package_value, pick_worth, team_standing,
    AcceptParams, Acceptance, LedgerQuery,
};
pub use engine::{dyn_weight, dynasty_scale, fairness, me_weight, season_weight, trade_values, EngineOpts, TradeData, TradeEngine, TradeEval};
pub use exploit::{exploit_opportunities, exploit_text, relieves, team_pressures, Pressure};
pub use greedy::{eligible, greedy_assignment, starting_slots, Held, Pid, SlotSpec};
pub use need::{find_needs, pitch_of, pitch_text, recommend_need_trades, Need, OutlookView};
pub use offer::{
    evaluate_offer, evaluate_trade, find_pick, find_players, find_team, offer_summary, pick_assets, trade_block_offered,
    trade_block_wants, verdict, OfferEval, PickRef,
};
pub use rec::{diverse, keeper_delta_text, market_label, pct_text, roster_text, to_rec, DEFAULT_RATE};
pub use search::{recommend_trades, TradeOpts};

/// `_greedy_lineup_value(players, values, roster_shape)` on the season horizon (also used by
/// `analysis.outlook` with its own per-player values): total per-game value of the best starting
/// lineup by greedy assignment.
pub fn greedy_lineup_value(players: &[&fm_core::Player], fpg: impl Fn(&fm_core::Player) -> Option<f64>, roster_shape: &fm_core::Map<String, i64>) -> f64 {
    let spec = greedy::SlotSpec::new(roster_shape);
    let vals: Vec<Option<f64>> = players.iter().map(|p| fpg(p)).collect();
    let f: Vec<f64> = vals.iter().map(|v| v.unwrap_or(0.0)).collect();
    let elig: Vec<u16> = players.iter().map(|p| spec.elig_mask(p)).collect();
    let ids: Vec<greedy::Pid> = (0..players.len() as greedy::Pid).collect();
    let held = greedy::greedy_assignment(&ids, &spec, &f, &elig);
    fm_pycompat::num::py_sum(held.iter().flat_map(|h| h.iter()).filter_map(|&i| vals[i as usize]))
}

// -- filters and ranking ---------------------------------------------------------------------
/// my edge (lineup FPG, or the mode's blend in dynasty) must beat this
pub const MIN_GAIN_ME: f64 = 0.3;
/// plausibility: the counterparty must plausibly say yes
pub const MIN_P_ACCEPT: f64 = 0.25;
/// diversity: proposals per player I give away
pub const MAX_PER_GIVEN: usize = 2;
/// ... and per player I would receive
pub const MAX_PER_RECEIVED: usize = 2;
pub const SWEET_SPOT_N: usize = 5;
/// market calls it fair: |perceived| <= 5 worth points
pub const SWEET_MAX_PERCEIVED: f64 = 5.0;
/// ... while our model says I gain at least this (FPG)
pub const SWEET_MIN_EDGE: f64 = 0.4;
/// 10 market worth points ~ 0.5 FPG (sweet-spot penalty)
pub const MARKET_PTS_TO_FPG: f64 = 0.05;
/// market worth I may hand over before it costs ranking score
pub const OVERPAY_FREE_PTS: f64 = SWEET_MAX_PERCEIVED;
/// ... each further worth point costs this much score (FPG)
pub const OVERPAY_COST_FPG: f64 = MARKET_PTS_TO_FPG;
/// a clear overpay (> CLEAR_FAVOR_PTS worth pts to them) is listed only with this edge
pub const OVERPAY_MIN_EDGE: f64 = 1.0;

// -- market view and acceptance ----------------------------------------------------------------
pub const MARKET_CONVEXITY: f64 = 3.0;
pub const DEPTH_WEIGHT: f64 = 0.5;
pub const ROSTER_SPOT_PENALTY: f64 = 3.0;
pub const NEED_PTS: f64 = 5.0;
pub const HOLE_PTS: f64 = 10.0;
pub const BLOCK_PTS: f64 = 5.0;
pub const ACCEPT_X0: f64 = 0.0;
/// logistic slope: +10 points -> 0.75, -10 -> 0.25 (`math.log(3.0) / 10.0`)
pub const ACCEPT_K: f64 = 0.10986122886681098;
pub const P_CEILING: f64 = 0.9;
pub const CONTENDER_FUTURE_MULT: f64 = 0.85;
pub const REBUILDER_VETERAN_MULT: f64 = 0.85;
pub const REBUILD_AGE_GAP: f64 = 3.0;
pub const CALIBRATION_MIN_N: usize = 20;
pub const PRESSURE_PTS: f64 = 8.0;

// -- draft picks -----------------------------------------------------------------------------------
pub const PICK_REBUILDER_MULT: f64 = 1.2;
pub const PICK_CONTENDER_MULT: f64 = 0.8;
pub const PICK_ASK_MIN: f64 = CLEAR_FAVOR_PTS;
pub const PICK_TRIES_PER_TEAM: usize = 6;

// -- enumeration, legacy display band, dynasty ----------------------------------------------------
pub const FAIR_BAND: f64 = 0.12;
pub const UNEVEN_BAND: (f64, f64) = (0.85, 1.15);
pub const TOP_N: usize = 12;
pub const MAX_EVALS_PER_TEAM: usize = 900;
pub const REFINE_PER_TEAM: usize = 12;
pub const OVERPAY_CAP: f64 = 30.0;
pub const FILL_CANDIDATES: usize = 3;
pub const SCREEN_MARGIN: f64 = 0.1;
pub const ROTO_PENALTY_WEIGHT: f64 = 2.0;
pub const WIN_NOW_TOLERANCE: f64 = 0.3;
pub const BALANCED_TOLERANCE: f64 = 1.0;
pub const DYNASTY_DISCOUNT: f64 = 0.8;
pub const IR_SLOTS: [&str; 2] = ["IR", "IR+"];

// -- exploits --------------------------------------------------------------------------------------
pub const EXPLOIT_N: usize = 8;
pub const EXPLOIT_PER_TEAM: usize = 2;
pub const PRESSURE_WEAK_VORP: f64 = -1.0;
pub const MIN_HEALTHY_GOALIES: usize = 2;

// -- offer evaluation ------------------------------------------------------------------------------
pub const EVAL_ACCEPT_EDGE: f64 = 0.1;
pub const EVAL_DECLINE_EDGE: f64 = -0.3;

// -- need finder -----------------------------------------------------------------------------------
pub const NEED_SLOTS: usize = 2;
pub const NEED_GET_N: usize = 6;
pub const NEED_PER_TEAM: usize = 2;
pub const COMPLEMENT_MULT: f64 = 1.15;
pub const STAR_MULT: f64 = 1.10;
pub const LONG_TERM_BOTTOM: f64 = 1.0 / 3.0;

#[cfg(test)]
mod tests;
