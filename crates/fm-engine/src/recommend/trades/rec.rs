//! `_to_rec` and the reader-facing texts of a proposal; `diverse`.

use std::collections::HashMap;

use fm_core::models::{GainUnits, Player, Reason, RecKind, Recommendation};
use fm_core::{Cid, Map};
use fm_pycompat::fmt::{fixed, percent};
use fm_pycompat::num::{py_round_i64, round_to};

use super::engine::{dyn_weight, g, me_weight, names, TradeEval};
use super::*;
use crate::recommend::base::{gain_season, gain_week, keepers_text, GameRate};
use crate::recommend::market::{
    deal_drivers, market_check_reasons, market_source_label, pick_check_reason, trade_market_check, MarketPool,
};

/// A TradeEval without a rate: league-average defaults.
pub const DEFAULT_RATE: GameRate =
    GameRate { per_week: crate::recommend::base::DEFAULT_GAMES_PER_WEEK, remaining: crate::recommend::base::SEASON_GAMES as f64, source: "default" };

/// How a deal looks to the counterparty by market value.
pub fn market_label(perceived: f64) -> &'static str {
    if perceived.abs() <= 3.0 {
        "Looks even to them"
    } else if perceived > CLEAR_FAVOR_PTS {
        "Looks clearly in their favor"
    } else if perceived > 0.0 {
        "Looks slightly in their favor"
    } else if perceived >= -10.0 {
        "Looks slightly in your favor"
    } else {
        "Looks clearly in your favor"
    }
}

/// Acceptance rounded to 5%: '~70%'.
pub fn pct_text(p: f64) -> String {
    format!("~{}%", 5 * py_round_i64(p * 20.0))
}

/// The ROSTER_CONSEQUENCE text.
pub fn roster_text(ev: &TradeEval<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !ev.my_ir.is_empty() {
        parts.push(format!("you move {} to IR", names(ev.my_ir.iter())));
    }
    if !ev.my_drops.is_empty() {
        parts.push(format!("you drop {}", names(ev.my_drops.iter())));
    }
    if let Some(f) = ev.my_fill {
        parts.push(format!("you pick up {} (FA)", f.name));
    }
    if !ev.their_ir.is_empty() {
        parts.push(format!("{} moves {} to IR", ev.team.name, names(ev.their_ir.iter())));
    }
    if !ev.their_drops.is_empty() {
        parts.push(format!("{} drops {}", ev.team.name, names(ev.their_drops.iter())));
    }
    if let Some(f) = ev.their_fill {
        parts.push(format!("{} can pick up {} (FA)", ev.team.name, f.name));
    }
    if parts.is_empty() {
        return format!("Roster: straight {}-for-{}, no other moves needed", ev.give.len(), ev.get.len());
    }
    format!("Roster: {}", parts.join("; "))
}

/// DYNASTY_DELTA text in keeper leagues.
pub fn keeper_delta_text(ev: &TradeEval<'_>) -> String {
    let ch = ev.keeper_change.as_ref().expect("keeper change");
    let mode = ev.mode.expect("mode");
    let k = ch.k.map_or("None".to_string(), |k| k.to_string());
    let mut parts = vec![
        format!("Keepers: top-{k} long-term {} ({})", fixed(ch.keepers, 2, true), keepers_text(ch)),
        format!("this season {}", fixed(ch.season, 2, true)),
    ];
    if ev.spot_cost.abs() > 1e-9 {
        parts.push(format!("roster spot {}", fixed(-ev.spot_cost, 2, true)));
    }
    let picks = ev.delta_dyn - (ch.total - ev.spot_cost);
    if picks.abs() > 1e-9 {
        parts.push(format!("draft picks {}", fixed(picks, 2, true)));
    }
    format!(
        "{} = dynasty value {} for me ({} FPG-equivalent); {mode} edge = {} x lineup + {} x dynasty",
        parts.join("; "),
        fixed(ev.delta_dyn, 2, true),
        fixed(ev.delta_dyn_n, 2, true),
        g(me_weight(mode)),
        g(dyn_weight(mode))
    )
}

fn reason(code: &str, text: String, value: Option<f64>, baseline: Option<f64>) -> Reason {
    Reason { code: code.to_string(), text, value, baseline }
}

/// `_to_rec(ev, V, units, source, market)`.
pub fn to_rec(ev: &TradeEval<'_>, v: &Map<Cid, f64>, units: &str, source: &str, market: Option<&MarketPool<'_>>) -> Recommendation {
    let vl = |ps: &[&Player]| {
        ps.iter().map(|p| format!("{} ({})", p.name, fixed(v.get(&p.cid).copied().unwrap_or(0.0), 2, false))).collect::<Vec<_>>().join(", ")
    };
    let acc = ev.acceptance.as_ref();
    let band = if ev.give.len() == ev.get.len() { FAIR_BAND } else { UNEVEN_BAND.1 - 1.0 };
    let rate = ev.rate.unwrap_or(DEFAULT_RATE);
    let (wk, ssn) = (gain_week(ev.delta_me, &rate), gain_season(ev.delta_me, &rate));
    let mut edge_txt = format!(
        "You gain {} pts/game this season ({} pts/week, {} rest of season)",
        fixed(ev.delta_me, 2, true),
        fixed(wk, 1, true),
        fixed(ssn, 0, true)
    );
    if let Some(mode) = ev.mode {
        edge_txt += &format!(" (dynasty {}; {mode} edge {})", fixed(ev.delta_dyn_n, 2, true), fixed(ev.core(), 2, true));
    }
    if ev.roto_adj != 0.0 {
        edge_txt += &format!("; roto balance {}", fixed(-ev.roto_adj, 2, true));
    }
    if ev.edge_error > 0.0 {
        let e = ev.my_edge();
        edge_txt += &format!(
            "; projection error puts the edge between {} and {}",
            fixed(e - ev.edge_error, 2, true),
            fixed(e + ev.edge_error, 2, true)
        );
        if e - ev.edge_error <= 0.0 {
            // the same rule as the offer verdict (offer::verdict): a range that includes zero is a coin flip
            edge_txt += ", so it is too close to call";
        }
    }
    let mut reasons = vec![reason("MY_EDGE", edge_txt, Some(ev.my_edge()), Some(MIN_GAIN_ME * ev.unit))];
    if let Some(acc) = acc {
        let src = acc.sources.iter().map(|s| market_source_label(s)).collect::<Vec<_>>().join("/");
        let src = if src.is_empty() { "no market data".to_string() } else { src };
        reasons.push(reason(
            "MARKET_VIEW",
            format!(
                "{} by market value ({src}, {} worth pts to them); acceptance {}",
                market_label(ev.market_overpay()),
                fixed(ev.market_overpay(), 0, true),
                pct_text(acc.p)
            ),
            Some(acc.perceived),
            Some(acc.base()),
        ));
        if ev.overpay_cost() > 0.0 {
            reasons.push(reason(
                "MARKET_COST",
                format!(
                    "You hand over {} market worth pts ({} free): ranked {} lower, since that value could buy something in another trade",
                    fixed(ev.market_overpay(), 0, false),
                    g(OVERPAY_FREE_PTS),
                    fixed(ev.overpay_cost(), 2, false)
                ),
                Some(-ev.overpay_cost()),
                Some(ev.market_overpay()),
            ));
        }
    }
    if let Some(t) = &ev.need_text {
        reasons.push(reason("THEIR_NEED", t.clone(), Some(NEED_PTS), None));
    }
    if let Some(t) = &ev.hole_text {
        reasons.push(reason("THEIR_HOLE", format!("Trade {t}: a hard sell"), Some(-HOLE_PTS * ev.hole_slots as f64), None));
    }
    if let Some(t) = &ev.block_text {
        reasons.push(reason("TRADE_BLOCK", t.clone(), Some(BLOCK_PTS), None));
    }
    reasons.push(reason("ROSTER_CONSEQUENCE", roster_text(ev), Some((ev.my_drops.len() + ev.their_drops.len()) as f64), None));
    if ev.sweet_spot() {
        reasons.push(reason(
            "SWEET_SPOT",
            format!(
                "Sweet spot: the market calls it fair ({} pts) while our model has you gaining {}",
                fixed(ev.perceived(), 1, true),
                fixed(ev.my_edge(), 2, true)
            ),
            Some(ev.sweet_spot_score()),
            Some(ev.perceived()),
        ));
    }
    if let Some(acc) = acc {
        let how = acc.notes.join("; ");
        let mut text = format!(
            "Acceptance {} ({} logistic: they get {}, give {} market pts",
            percent(acc.p, 0, false),
            acc.params.source,
            fixed(acc.gets, 0, false),
            fixed(acc.gives, 0, false)
        );
        if !how.is_empty() {
            text += &format!("; {how}");
        }
        text += &format!(
            " -> {} pts). EV = {} x {} = {}",
            fixed(acc.perceived, 1, true),
            fixed(ev.my_edge(), 2, true),
            fixed(acc.p, 2, false),
            fixed(ev.ev(), 2, true)
        );
        if ev.overpay_cost() > 0.0 {
            text += &format!("; score {} after the market cost", fixed(ev.score(), 2, true));
        }
        reasons.push(reason("P_ACCEPT", text, Some(acc.p), Some(acc.p_raw)));
    }
    let src_txt = match rate.source {
        "schedule" => "your starters' NHL schedule",
        "season over" => "season over",
        _ => "no schedule loaded: league-average default",
    };
    reasons.push(reason(
        "GAIN_WEEK",
        format!(
            "{} pts/week ({}/game x {} games/week, {src_txt})",
            fixed(wk, 1, true),
            fixed(ev.delta_me, 2, true),
            fixed(rate.per_week, 1, false)
        ),
        Some(wk),
        Some(rate.per_week),
    ));
    reasons.push(reason(
        "GAIN_SEASON",
        format!(
            "{} pts rest of season ({}/game x {} games left)",
            fixed(ssn, 0, true),
            fixed(ev.delta_me, 2, true),
            fixed(rate.remaining, 0, false)
        ),
        Some(ssn),
        Some(rate.remaining),
    ));
    let mut dm = format!("My lineup {} FPG ({source}", fixed(ev.delta_me, 2, true));
    if let Some(f) = ev.my_fill.filter(|_| ev.me_base_len() > 1) {
        dm += &format!("; before = my roster with the same pickup of {}", f.name);
    }
    if let Some(f) = ev.free_fill {
        dm += &format!("; before and after both fill your empty slot from waivers ({}) when that is better", f.name);
    }
    dm += ")";
    reasons.push(reason("DELTA_ME", dm, Some(ev.delta_me), Some(MIN_GAIN_ME * ev.unit)));
    reasons.push(reason(
        "DELTA_THEM",
        format!("{} lineup {} FPG (our model)", ev.team.name, fixed(ev.delta_them, 2, true)),
        Some(ev.delta_them),
        None,
    ));
    reasons.push(reason("VALUE_IN", format!("Receive {}: {} {units}", vl(&ev.get), fixed(ev.v_in, 2, false)), Some(ev.v_in), None));
    reasons.push(reason("VALUE_OUT", format!("Send {}: {} {units}", vl(&ev.give), fixed(ev.v_out, 2, false)), Some(ev.v_out), None));
    reasons.push(reason(
        "FAIR_PCT",
        format!("Model-value gap {} (shown only; old band {})", pct0(ev.fair_dev), pct0(band)),
        Some(ev.fair_dev),
        Some(band),
    ));
    if let (Some(_), Some(_)) = (ev.mode, ev.keeper_change.as_ref()) {
        reasons.push(reason("DYNASTY_DELTA", keeper_delta_text(ev), Some(ev.delta_dyn_n), Some(ev.delta_dyn)));
    } else if let Some(mode) = ev.mode {
        reasons.push(reason(
            "DYNASTY_DELTA",
            format!(
                "Dynasty value {} for me ({} FPG-equivalent); {mode} edge = {} x lineup + {} x dynasty",
                fixed(ev.delta_dyn, 2, true),
                fixed(ev.delta_dyn_n, 2, true),
                g(me_weight(mode)),
                g(dyn_weight(mode))
            ),
            Some(ev.delta_dyn_n),
            Some(ev.delta_dyn),
        ));
    }
    if ev.win_now_cost() {
        reasons.push(reason(
            "WIN_NOW_COST",
            format!(
                "Costs {} FPG of this season's lineup (contend mode allows {}): future-only",
                fixed(-ev.delta_me, 2, false),
                g(round_to(WIN_NOW_TOLERANCE * ev.unit, 2))
            ),
            Some(ev.delta_me),
            Some(-WIN_NOW_TOLERANCE * ev.unit),
        ));
    }
    if !ev.my_ir.is_empty() {
        reasons.push(reason("IR_MOVE", format!("You would move {} to IR (no drop)", names(ev.my_ir.iter())), None, None));
    }
    if !ev.their_ir.is_empty() {
        reasons.push(reason(
            "IR_MOVE",
            format!("{} would move {} to IR (no drop)", ev.team.name, names(ev.their_ir.iter())),
            None,
            None,
        ));
    }
    if let Some(c) = &ev.my_cap {
        reasons.push(reason(
            "POSITION_CAP",
            format!("League roster maximum ({c}): the drop must be a same-position player"),
            None,
            None,
        ));
    }
    if let Some(c) = &ev.their_cap {
        reasons.push(reason(
            "POSITION_CAP",
            format!("{} is at a league roster maximum ({c}): they must drop a same-position player", ev.team.name),
            None,
            None,
        ));
    }
    if !ev.my_drops.is_empty() {
        reasons.push(reason("ROSTER_DROP", format!("You would need to drop {}", names(ev.my_drops.iter())), None, None));
    }
    if !ev.their_drops.is_empty() {
        reasons.push(reason(
            "ROSTER_DROP",
            format!("{} would need to drop {}", ev.team.name, names(ev.their_drops.iter())),
            None,
            None,
        ));
    }
    if let Some(f) = ev.my_fill {
        reasons.push(reason("FA_FILL", format!("Open roster spot: pick up {}", f.name), None, None));
    }
    if ev.roto_adj != 0.0 {
        reasons.push(reason("ROTO_BALANCE", format!("Roto category balance {}", fixed(-ev.roto_adj, 2, true)), Some(-ev.roto_adj), None));
    }
    let tid = ev.team.team_id.as_str();
    for a in &ev.get_picks {
        reasons.push(reason(
            "PICK_IN",
            format!(
                "Ask for {}'s {} pick (dynasty value {}, {} net of its roster spot; expected slot {}): the deal is far in their favour by market value, so a pick back evens it",
                ev.team.name,
                a.short(Some(tid)),
                fixed(a.value, 1, false),
                fixed(a.net, 1, false),
                fixed(a.expected_slot, 0, false)
            ),
            Some(a.net),
            Some(a.value),
        ));
    }
    for a in &ev.give_picks {
        reasons.push(reason(
            "PICK_OUT",
            format!(
                "Add your {} pick (dynasty value {}, costs you {} net of its roster spot; expected slot {}): without it they would likely refuse",
                a.short(Some(&a.holder)),
                fixed(a.value, 1, false),
                fixed(a.net, 1, false),
                fixed(a.expected_slot, 0, false)
            ),
            Some(-a.net),
            Some(a.value),
        ));
    }
    if let Some(market) = market {
        if acc.is_some() {
            let why = deal_drivers(market, ev.get.iter().copied(), ev.give.iter().copied());
            if let Some(deal) = trade_market_check(ev.market_overpay(), ev.my_edge(), Some(&why)) {
                reasons.push(deal);
            }
        }
        let roles: Vec<(&Player, &str)> =
            ev.get.iter().map(|p| (*p, "receive")).chain(ev.give.iter().map(|p| (*p, "send"))).collect();
        reasons.extend(market_check_reasons(market, roles));
        reasons.extend(ev.get_picks.iter().map(|a| pick_check_reason(a, "receive", Some(tid))));
        reasons.extend(ev.give_picks.iter().map(|a| pick_check_reason(a, "send", Some(&a.holder))));
    }
    let give_txt: Vec<String> = ev
        .give
        .iter()
        .map(|p| p.name.clone())
        .chain(ev.give_picks.iter().map(|a| format!("your {} pick", a.short(Some(&a.holder)))))
        .collect();
    let get_txt: Vec<String> = ev
        .get
        .iter()
        .map(|p| p.name.clone())
        .chain(ev.get_picks.iter().map(|a| format!("their {} pick", a.short(Some(tid)))))
        .collect();
    let mut r = Recommendation::new(
        RecKind::Trade,
        ev.score(),
        format!("Trade {} to {} for {}", give_txt.join(" + "), ev.team.name, get_txt.join(" + ")),
    );
    r.add = ev.get.iter().map(|p| (*p).clone()).collect();
    r.drop = ev.give.iter().map(|p| (*p).clone()).collect();
    r.counterparty = Some(ev.team.name.clone());
    r.reasons = reasons;
    r.predicted_gain = Some(ev.delta_me);
    r.gain_units = Some(GainUnits::LineupFpg);
    r.horizon_days = None;
    r
}

/// `f"{x:.0%}"` (inf -> "inf%").
fn pct0(x: f64) -> String {
    if x.is_infinite() {
        return if x > 0.0 { "inf%".to_string() } else { "-inf%".to_string() };
    }
    percent(x, 0, false)
}

/// `diverse(evals, n, per_team, per_given, per_received)`: indices of the first `n` of `evals`
/// (already in preference order) within the per-counterparty / per-player caps.
pub fn diverse(evals: &[&TradeEval<'_>], n: usize, per_team: usize, per_given: usize, per_received: usize) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut by_team: HashMap<&str, usize> = HashMap::new();
    let mut by_given: HashMap<&str, usize> = HashMap::new();
    let mut by_got: HashMap<&str, usize> = HashMap::new();
    for (i, e) in evals.iter().enumerate() {
        if out.len() >= n {
            break;
        }
        let tid = e.team.team_id.as_str();
        if by_team.get(tid).copied().unwrap_or(0) >= per_team
            || e.give.iter().any(|p| by_given.get(p.cid.as_str()).copied().unwrap_or(0) >= per_given)
            || e.get.iter().any(|p| by_got.get(p.cid.as_str()).copied().unwrap_or(0) >= per_received)
        {
            continue;
        }
        out.push(i);
        *by_team.entry(tid).or_insert(0) += 1;
        for p in &e.give {
            *by_given.entry(p.cid.as_str()).or_insert(0) += 1;
        }
        for p in &e.get {
            *by_got.entry(p.cid.as_str()).or_insert(0) += 1;
        }
    }
    out
}
