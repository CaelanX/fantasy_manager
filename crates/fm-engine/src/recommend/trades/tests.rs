//! Port of tests/test_trades.py, tests/test_trade_eval.py and the trade part of
//! tests/test_keeper_consumers.py (intent). Tests that monkeypatch the market pool in Python are
//! covered by the fixture parity tests (fm-parity/tests/stage_trades.rs) instead.

use std::collections::HashMap;

use fm_core::models::{FantasyTeam, LeagueContext, Player, Reason, RosterSlot, Status};
use fm_core::values::{DynastyValue, PickAsset, PlayerValue};
use fm_core::{Cid, Map};
use serde_json::json;

use super::*;

const SHAPE: [(&str, i64); 6] = [("C", 1), ("LW", 1), ("D", 2), ("G", 1), ("BN", 2), ("IR", 1)];

fn pl(cid: &str, pos: &[&str]) -> Player {
    serde_json::from_value(json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": "EDM",
                                  "positions": pos, "status": "healthy"}))
    .unwrap()
}

fn pv(p: &Player, fpg: f64) -> PlayerValue {
    serde_json::from_value(json!({"player": p, "fpg": fpg, "fpg_season": fpg, "fpg_week": fpg, "vorp": fpg - 1.0,
        "vorp_week": fpg - 1.0, "horizon_values": {"season": fpg, "week": fpg, "vorp_season": fpg - 1.0, "vorp_week": fpg - 1.0}}))
    .unwrap()
}

fn shape(xs: &[(&str, i64)]) -> Map<String, i64> {
    xs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

fn team(tid: &str, name: &str, mine: bool, entries: Vec<(&str, Player)>) -> FantasyTeam {
    FantasyTeam {
        team_id: tid.into(),
        name: name.into(),
        owner_is_me: mine,
        slots: entries
            .into_iter()
            .map(|(s, p)| RosterSlot { slot: s.into(), player: Some(p), starting: !["BN", "IR"].contains(&s) })
            .collect(),
        record: None,
    }
}

fn ctx_of(teams: Vec<FantasyTeam>, fas: Vec<Player>, sh: &[(&str, i64)]) -> LeagueContext {
    serde_json::from_value(json!({"provider": "test", "league_id": "1", "season": 2027, "name": "T",
        "scoring": {"kind": "points", "weights": {"G": 1.0}}, "roster_shape": shape(sh), "teams": teams,
        "free_agents": fas, "matchup_period": 1, "as_of": "2026-10-01"}))
    .unwrap()
}

type Spec<'s> = &'s [(&'s str, &'s str, &'s [&'s str], f64)];

/// Me: C-rich, D-poor. Opponent A: D-rich, C-poor. Opponent B: balanced/thin.
fn build() -> (LeagueContext, Map<Cid, PlayerValue>) {
    let spec: [(&str, Spec); 3] = [
        ("me", &[("C", "m_c1", &["C"], 3.0), ("BN", "m_c2", &["C"], 2.8), ("LW", "m_lw", &["LW"], 2.0), ("D", "m_d1", &["D"], 1.0),
                 ("D", "m_d2", &["D"], 0.4), ("G", "m_g", &["G"], 2.5), ("BN", "m_lw2", &["LW"], 0.3)]),
        ("A", &[("C", "a_c", &["C"], 1.2), ("LW", "a_lw", &["LW"], 1.9), ("D", "a_d1", &["D"], 2.9), ("D", "a_d2", &["D"], 2.7),
                ("BN", "a_d3", &["D"], 1.0), ("G", "a_g", &["G"], 2.4)]),
        ("B", &[("C", "b_c", &["C"], 1.8), ("LW", "b_lw", &["LW"], 1.7), ("D", "b_d1", &["D"], 1.6), ("D", "b_d2", &["D"], 1.3),
                ("G", "b_g", &["G"], 2.0), ("BN", "b_star", &["C", "LW"], 4.5)]),
    ];
    let mut values = Map::new();
    let mut teams = Vec::new();
    for (tid, rows) in spec {
        let mut entries = Vec::new();
        for (slot, cid, pos, f) in rows {
            let p = pl(cid, pos);
            values.insert(cid.to_string(), pv(&p, *f));
            entries.push((*slot, p));
        }
        teams.push(team(tid, &format!("Team {tid}"), tid == "me", entries));
    }
    let mut fas = Vec::new();
    for (cid, pos, f) in [("fa_c", ["C"], 0.9), ("fa_d", ["D"], 0.8), ("fa_g", ["G"], 1.0)] {
        let p = pl(cid, &pos);
        values.insert(cid.to_string(), pv(&p, f));
        fas.push(p);
    }
    (ctx_of(teams, fas, &SHAPE), values)
}

/// Plain dynasty values (Python's float maps): value only, keeper fields at their defaults.
fn dynv(ctx: &LeagueContext, values: &Map<Cid, PlayerValue>, base: f64, over: &[(&str, f64)]) -> Map<Cid, DynastyValue> {
    let players: HashMap<String, Player> = ctx.all_players().into_iter().map(|p| (p.cid.clone(), p.clone())).collect();
    values
        .keys()
        .map(|c| {
            let v = over.iter().find(|(k, _)| k == c).map_or(base, |x| x.1);
            let dv: DynastyValue = serde_json::from_value(
                json!({"player": players[c], "value": v, "age": null, "age_mult": 1.0, "upside": 0.0, "horizon_years": 3}),
            )
            .unwrap();
            (c.clone(), dv)
        })
        .collect()
}

fn greedy() -> EngineOpts<'static> {
    EngineOpts { greedy_only: true, ..EngineOpts::default() }
}

fn eval<'a>(
    ctx: &'a LeagueContext,
    values: &'a Map<Cid, PlayerValue>,
    give: &[&str],
    get: &[&str],
    dyn_: Option<&'a Map<Cid, DynastyValue>>,
    opts: &EngineOpts<'_>,
) -> TradeEval<'a> {
    evaluate_trade(ctx, values, give, get, None, dyn_, opts, &[], &[]).unwrap().0
}

fn cids(ps: &[&Player]) -> Vec<String> {
    ps.iter().map(|p| p.cid.clone()).collect()
}

fn opts_g(limit: usize, per_team: usize) -> TradeOpts<'static> {
    TradeOpts { limit, max_per_team: per_team, engine: greedy(), ..TradeOpts::default() }
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

fn set_status(ctx: &mut LeagueContext, cid: &str, st: Status) {
    for t in &mut ctx.teams {
        for s in &mut t.slots {
            if let Some(p) = s.player.as_mut().filter(|p| p.cid == cid) {
                p.status = st;
            }
        }
    }
}

#[test]
fn greedy_lineup_value_respects_slots() {
    let ps: Vec<Player> = [("c", "C"), ("lw", "LW"), ("rw", "RW"), ("d", "D"), ("g", "G"), ("g2", "G")]
        .iter()
        .map(|(c, p)| pl(c, &[p]))
        .collect();
    let f: HashMap<&str, f64> = [("c", 3.0), ("lw", 2.0), ("rw", 1.5), ("d", 1.0), ("g", 2.0), ("g2", 5.0)].into_iter().collect();
    let refs: Vec<&Player> = ps.iter().collect();
    let v = greedy_lineup_value(&refs, |p| f.get(p.cid.as_str()).copied(), &shape(&[("C", 1), ("F", 1), ("UTIL", 1), ("G", 1), ("BN", 3), ("IR", 1)]));
    assert!(approx(v, 3.0 + 2.0 + 1.5 + 5.0));
    let v = greedy_lineup_value(&refs[5..], |p| f.get(p.cid.as_str()).copied(), &shape(&[("UTIL", 2), ("BN", 1)]));
    assert_eq!(v, 0.0);
}

#[test]
fn fairness_bands() {
    let (ok, dev) = fairness(&[1.0], &[1.12], 0.0);
    assert!(ok && approx(dev, 0.12 / 1.12));
    assert!(!fairness(&[1.0], &[1.2], 0.0).0);
    assert!(fairness(&[2.0], &[1.0, 1.0], 0.0).0);
    assert!(!fairness(&[2.0], &[1.0, 1.0], 0.5).0);
    assert!(fairness(&[1.7], &[1.0, 1.0], 0.5).0);
}

#[test]
fn positional_need_swap_is_proposed() {
    for greedy_only in [false, true] {
        let (ctx, values) = build();
        let opts = TradeOpts { engine: EngineOpts { greedy_only, ..EngineOpts::default() }, ..TradeOpts::default() };
        let recs = recommend_trades(&ctx, &values, &opts, None, None).unwrap();
        assert!(!recs.is_empty());
        let top = recs
            .iter()
            .find(|r| r.counterparty.as_deref() == Some("Team A") && r.drop.iter().map(|p| p.cid.as_str()).collect::<Vec<_>>() == ["m_c2"])
            .expect("a C-for-D swap with Team A");
        assert!(["a_d1", "a_d2"].contains(&top.add[0].cid.as_str()));
        let codes: Vec<&str> = top.reasons.iter().map(|r| r.code.as_str()).collect();
        assert_eq!(&codes[..3], &["MY_EDGE", "MARKET_VIEW", "THEIR_NEED"]);
        for c in ["P_ACCEPT", "ROSTER_CONSEQUENCE", "DELTA_ME", "DELTA_THEM", "FAIR_PCT"] {
            assert!(codes.contains(&c), "{c}");
        }
        let edge = top.reason("MY_EDGE").unwrap();
        let p = top.reason("P_ACCEPT").unwrap().value.unwrap();
        assert!(edge.value.unwrap() > 0.3 && p >= 0.25 && edge.text.starts_with("You gain +"));
        let mv = &top.reason("MARKET_VIEW").unwrap().text;
        assert!(mv.contains("by market value") && mv.contains("acceptance ~"));
        assert!(approx(top.score, edge.value.unwrap() * p));
        let scores: Vec<f64> = recs.iter().map(|r| r.score).collect();
        assert!(scores.windows(2).all(|w| w[0] >= w[1]));
    }
}

#[test]
fn unfair_three_for_one_rejected_and_shapes_bounded() {
    let (ctx, values) = build();
    let ev = eval(&ctx, &values, &["m_d1", "m_d2", "m_lw2"], &["b_star"], None, &greedy());
    assert_eq!(ev.team.team_id, "B");
    assert!(!ev.fair && !ev.accepted());
    let recs = recommend_trades(&ctx, &values, &opts_g(50, 50), None, None).unwrap();
    assert!(recs.iter().all(|r| r.drop.len() + r.add.len() <= 3 && r.drop.len().min(r.add.len()) == 1));
}

#[test]
fn proposals_are_deduped_and_not_padded() {
    let (ctx, values) = build();
    let recs = recommend_trades(&ctx, &values, &opts_g(100, 100), None, None).unwrap();
    let key = |r: &fm_core::Recommendation| {
        let mut a: Vec<String> = r.drop.iter().map(|p| p.cid.clone()).collect();
        let mut b: Vec<String> = r.add.iter().map(|p| p.cid.clone()).collect();
        a.sort();
        b.sort();
        (a, b)
    };
    let keys: Vec<_> = recs.iter().map(key).collect();
    for (i, k) in keys.iter().enumerate() {
        assert!(!keys[i + 1..].contains(k));
        assert!(!keys.contains(&(k.1.clone(), k.0.clone())));
    }
    let singles: Vec<(_, f64)> = recs.iter().filter(|r| r.drop.len() == 1 && r.add.len() == 1).map(|r| (key(r), r.score)).collect();
    for r in recs.iter().filter(|r| r.drop.len() + r.add.len() == 3) {
        for g in &r.drop {
            for a in &r.add {
                let core = (vec![g.cid.clone()], vec![a.cid.clone()]);
                assert!(singles.iter().all(|(k, s)| *k != core || *s < r.score));
            }
        }
    }
}

#[test]
fn max_per_team_and_limit() {
    let (ctx, values) = build();
    let recs = recommend_trades(&ctx, &values, &opts_g(10, 1), None, None).unwrap();
    let mut teams: Vec<_> = recs.iter().map(|r| r.counterparty.clone()).collect();
    let n = teams.len();
    teams.dedup();
    assert_eq!(teams.len(), n);
    assert!(recommend_trades(&ctx, &values, &opts_g(1, 3), None, None).unwrap().len() <= 1);
}

#[test]
fn receiving_two_needs_a_drop() {
    let (ctx, values) = build();
    let ev = eval(&ctx, &values, &["m_c2"], &["a_d2", "a_d3"], None, &greedy());
    assert_eq!(cids(&ev.my_drops), ["m_lw2"]);
    assert!(ev.their_drops.is_empty());
}

#[test]
fn dynasty_values_drive_fairness() {
    let (ctx, values) = build();
    let dyn_ = dynv(&ctx, &values, 1.0, &[("m_c2", 10.0), ("a_d1", 1.0)]);
    assert!(!eval(&ctx, &values, &["m_c2"], &["a_d1"], Some(&dyn_), &greedy()).fair);
}

#[test]
fn trade_block_bonus_applies() {
    let (ctx, values) = build();
    let base = eval(&ctx, &values, &["m_d1"], &["b_d2"], None, &greedy());
    let wants: Map<String, Vec<String>> = [("B".to_string(), vec!["D".to_string()])].into_iter().collect();
    let o = EngineOpts { wants: Some(&wants), ..greedy() };
    let wanted = eval(&ctx, &values, &["m_d1"], &["b_d2"], None, &o);
    assert!(base.block_text.is_none() && wanted.block_text.as_deref().unwrap().contains("trade-block wants (D)"));
    assert!(approx(wanted.perceived(), base.perceived() + BLOCK_PTS) && wanted.p() > base.p());
    let offered: Map<String, Vec<String>> = [("B".to_string(), vec!["b_d2".to_string()])].into_iter().collect();
    let o = EngineOpts { offered: Some(&offered), ..greedy() };
    let ev = eval(&ctx, &values, &["m_d1"], &["b_d2"], None, &o);
    assert!(ev.block_text.as_deref().unwrap().contains("b_d2 is on Team B's trade block"));
    let r = to_rec(&ev, &Map::new(), "VORP", "greedy", None);
    assert!(r.reason("TRADE_BLOCK").is_some());
}

#[test]
fn ir_status_player_moves_to_ir_before_any_drop() {
    let (mut ctx, values) = build();
    set_status(&mut ctx, "m_lw", Status::Ir);
    let ev = eval(&ctx, &values, &["m_c2"], &["a_d2", "a_d3"], None, &greedy());
    assert_eq!(cids(&ev.my_ir), ["m_lw"]);
    assert!(ev.my_drops.is_empty());
    let r = to_rec(&ev, &Map::new(), "VORP", "greedy", None);
    assert!(r.reasons.iter().any(|x| x.code == "IR_MOVE" && x.text.contains("move m_lw to IR")));
    assert!(r.reason("ROSTER_DROP").is_none());
}

#[test]
fn no_ir_slot_means_drop() {
    let (mut ctx, values) = build();
    ctx.roster_shape.shift_remove("IR");
    set_status(&mut ctx, "m_lw", Status::Ir);
    let ev = eval(&ctx, &values, &["m_c2"], &["a_d2", "a_d3"], None, &greedy());
    assert!(ev.my_ir.is_empty() && ev.my_drops.len() == 1);
}

fn with_ir_d(status: &str) -> (LeagueContext, Map<Cid, PlayerValue>) {
    let (mut ctx, mut values) = build();
    let mut p = pl("m_dir", &["D"]);
    p.status = Status::parse(status).unwrap();
    values.insert("m_dir".into(), pv(&p, 2.6));
    ctx.teams[0].slots.push(RosterSlot { slot: "IR".into(), player: Some(p), starting: false });
    (ctx, values)
}

#[test]
fn dtd_player_in_ir_slot_counts_in_lineup() {
    let (ctx, values) = with_ir_d("dtd");
    let ev = eval(&ctx, &values, &["m_c2"], &["a_d1"], None, &greedy());
    assert!(approx(ev.delta_me, 1.9), "{}", ev.delta_me);
}

#[test]
fn trading_away_ir_slot_player_is_not_free() {
    let (ctx, values) = with_ir_d("ir");
    let ev = eval(&ctx, &values, &["m_dir"], &["a_d3"], None, &greedy());
    assert!(approx(ev.delta_me, -1.6), "{}", ev.delta_me);
    assert_eq!(cids(&ev.my_drops), ["m_lw2"]);
}

#[test]
fn dynasty_drop_uses_dynasty_value_and_skips_players_without_stats() {
    let (ctx, values) = build();
    let dyn_ = dynv(&ctx, &values, 5.0, &[("m_lw2", 9.0), ("m_d2", 0.5), ("m_c2", 8.0), ("a_d2", 4.0), ("a_d3", 4.0)]);
    assert_eq!(cids(&eval(&ctx, &values, &["m_c2"], &["a_d2", "a_d3"], Some(&dyn_), &greedy()).my_drops), ["m_d2"]);
    let (ctx, mut values) = build();
    values.get_mut("m_lw2").unwrap().reasons.push(Reason::new("NO_DATA", "no stats"));
    let dyn_ = dynv(&ctx, &values, 5.0, &[("m_lw2", 0.0), ("m_d2", 1.0), ("m_c2", 8.0), ("a_d2", 4.0), ("a_d3", 4.0)]);
    assert_eq!(cids(&eval(&ctx, &values, &["m_c2"], &["a_d2", "a_d3"], Some(&dyn_), &greedy()).my_drops), ["m_d2"]);
}

fn dynasty_ctx(mode: &str) -> (LeagueContext, Map<Cid, PlayerValue>, Map<Cid, DynastyValue>) {
    let (mut ctx, values) = build();
    ctx.dynasty = true;
    ctx.dynasty_mode = fm_core::models::DynastyMode::parse(mode).unwrap();
    let dyn_ = dynv(&ctx, &values, 1.0, &[("m_lw", 10.0), ("a_d3", 11.3)]);
    (ctx, values, dyn_)
}

#[test]
fn contend_mode_rule_and_weights() {
    let (ctx, values, dyn_) = dynasty_ctx("contend");
    let ev = eval(&ctx, &values, &["m_lw"], &["a_d3"], Some(&dyn_), &greedy());
    assert_eq!(ev.mode, Some("contend"));
    assert!(ev.fair && approx(ev.delta_me, -1.1) && approx(ev.delta_dyn, 1.3));
    assert!(approx(dynasty_scale(&ctx), 1.0 + 0.65 + 0.45 + 0.8f64.powi(3) * 0.8));
    assert!(approx(ev.delta_dyn_n, 1.3 / dynasty_scale(&ctx)));
    assert!(ev.win_now_cost() && !ev.accepted());
    let r = to_rec(&ev, &Map::new(), "dynasty", "greedy", None);
    assert!(r.reason("WIN_NOW_COST").is_some() && r.reason("DYNASTY_DELTA").is_some());
    let mut fut = Vec::new();
    let opts = TradeOpts { dynasty_values: Some(&dyn_), limit: 50, engine: greedy(), ..TradeOpts::default() };
    let recs = recommend_trades(&ctx, &values, &opts, Some(&mut fut), None).unwrap();
    assert!(recs.iter().all(|r| r.reason("DELTA_ME").unwrap().value.unwrap() >= -WIN_NOW_TOLERANCE));
    assert!(fut.iter().all(|r| r.reason("WIN_NOW_COST").is_some()));
    for (mode, me_w, dyn_w) in [("contend", 1.0, 0.5), ("balanced", 1.0, 1.0), ("rebuild", 0.5, 1.0)] {
        let (ctx, values, dyn_) = dynasty_ctx(mode);
        let ev = eval(&ctx, &values, &["m_lw"], &["a_d3"], Some(&dyn_), &greedy());
        assert!(approx(ev.core(), me_w * ev.delta_me + dyn_w * ev.delta_dyn_n), "{mode}");
        assert!(!ev.win_now_cost() || mode == "contend");
    }
    let (mut ctx, values, dyn_) = dynasty_ctx("contend");
    ctx.dynasty = false;
    let ev = eval(&ctx, &values, &["m_lw"], &["a_d3"], Some(&dyn_), &greedy());
    // dynasty values passed in a redraft league still set the units, but no mode blend applies
    assert!(ev.mode.is_none() && approx(ev.core(), ev.delta_me));
}

#[test]
fn trade_never_forces_dropping_a_protected_prospect() {
    let (mut ctx, values) = build();
    ctx.dynasty = true;
    for s in &mut ctx.teams[0].slots {
        if let Some(p) = s.player.as_mut().filter(|p| p.cid == "m_lw2") {
            p.birth_date = chrono::NaiveDate::from_ymd_opt(2007, 9, 30);
            p.draft_overall = Some(2);
        }
    }
    let dyn_ = dynv(&ctx, &values, 7.0, &[("m_lw2", 6.0), ("m_c2", 8.0), ("a_d2", 4.0), ("a_d3", 4.0)]);
    let ev = eval(&ctx, &values, &["m_c2"], &["a_d2", "a_d3"], Some(&dyn_), &greedy());
    assert!(!cids(&ev.my_drops).contains(&"m_lw2".to_string()));
    assert!(ev.blocked.is_none());
    drop(ev);
    for s in &mut ctx.teams[0].slots {
        if let Some(p) = s.player.as_mut().filter(|p| p.cid != "m_lw2" && p.cid != "m_c2") {
            p.birth_date = chrono::NaiveDate::from_ymd_opt(2006, 1, 1);
            p.draft_overall = Some(10);
        }
    }
    let dyn_ = dynv(&ctx, &values, 7.0, &[("m_lw2", 6.0), ("m_c2", 8.0), ("a_d2", 4.0), ("a_d3", 4.0)]);
    let ev = eval(&ctx, &values, &["m_c2"], &["a_d2", "a_d3"], Some(&dyn_), &greedy());
    assert!(ev.blocked.is_some() && !ev.accepted());
}

#[test]
fn trade_predicted_gain_is_lineup_delta_me() {
    let (ctx, values) = build();
    let recs = recommend_trades(&ctx, &values, &opts_g(10, 3), None, None).unwrap();
    assert!(!recs.is_empty());
    for r in &recs {
        assert!(approx(r.predicted_gain.unwrap(), r.reason("DELTA_ME").unwrap().value.unwrap()));
        assert_eq!(r.gain_units, Some(fm_core::models::GainUnits::LineupFpg));
        assert!(r.strength.is_some_and(|s| (0.0..=10.0).contains(&s)));
    }
    let ranks: Vec<i64> = recs.iter().map(|r| r.rank_in_kind.unwrap()).collect();
    assert_eq!(ranks, (1..=recs.len() as i64).collect::<Vec<_>>());
}

#[test]
fn position_caps() {
    let (mut ctx, values) = build();
    ctx.position_limits = shape(&[("D", 3)]);
    let ev = eval(&ctx, &values, &["m_c2"], &["a_d2", "a_d3"], None, &greedy());
    assert_eq!(cids(&ev.my_drops), ["m_d2"]);
    assert_eq!(ev.my_cap.as_deref(), Some("D limit 3: would roster 4 defensemen"));
    assert!(ev.blocked.is_none());
    let r = to_rec(&ev, &Map::new(), "VORP", "greedy", None);
    let cap = r.reason("POSITION_CAP").unwrap();
    assert!(cap.text.contains("D limit 3") && cap.text.contains("drop"));
    assert!(r.reasons.iter().any(|x| x.code == "ROSTER_DROP" && x.text.contains("m_d2")));
    drop(ev);

    ctx.position_limits = shape(&[("C", 1)]);
    let ev = eval(&ctx, &values, &["m_c1", "m_c2"], &["a_d1"], None, &greedy());
    assert!(ev.blocked.as_deref().is_some_and(|b| b.contains("C limit 1")) && !ev.accepted());
    assert!(ev.their_cap.is_some() && cids(&ev.their_drops) == ["a_c"]);
    drop(ev);

    ctx.position_limits = shape(&[("D", 4), ("G", 2)]);
    let ev = eval(&ctx, &values, &["m_c2"], &["a_d2", "a_d3"], None, &greedy());
    assert!(cids(&ev.my_drops) == ["m_lw2"] && ev.my_cap.is_none());
}

#[test]
fn league_roster_max_below_the_slots_forces_drops_on_both_sides() {
    let (mut ctx, values) = build();
    ctx.roster_shape.insert("BN".into(), 3);
    ctx.max_roster_size = Some(7);
    let ev = eval(&ctx, &values, &["m_c2"], &["a_d2", "a_d3"], None, &greedy());
    assert!(ev.my_drops.len() == 1 && ev.my_illegal.is_none());
    drop(ev);
    ctx.max_roster_size = Some(6);
    let ev = eval(&ctx, &values, &["m_c1", "m_c2"], &["a_d1"], None, &greedy());
    assert!(ev.their_drops.len() == 1 && ev.acceptance.as_ref().unwrap().legal);
}

#[test]
fn market_worth_package_and_logistic() {
    assert_eq!(market_worth(100.0), 100.0);
    assert!(approx(market_worth(50.0), 12.5));
    assert_eq!(market_worth(-5.0), 0.0);
    assert_eq!(market_worth(120.0), 100.0);
    let two = package_value([market_worth(86.0), market_worth(62.0)]);
    assert!(market_worth(99.5) - two > 20.0);
    assert_eq!(package_value([80.0]), 80.0);
    assert!(approx(package_value([60.0, 80.0]), 80.0 + DEPTH_WEIGHT * 60.0));
    assert_eq!(package_value(std::iter::empty()), 0.0);
    let a = AcceptParams::default();
    assert!(approx(a.p(0.0), 0.5) && approx(a.p(10.0), 0.75) && approx(a.p(-10.0), 0.25));
    assert!(approx(a.p(60.0), a.ceiling) && a.p(-1e6) >= 0.0);
    assert!(approx(a.perceived_for(0.25), -10.0) && approx(a.perceived_for(0.75), 10.0));
}

#[test]
fn team_standing_contenders_and_rebuilders() {
    let (mut ctx, _) = build();
    let rec = [("me", (5, 5, 0)), ("A", (10, 0, 0)), ("B", (0, 10, 0))];
    for t in &mut ctx.teams {
        t.record = rec.iter().find(|(k, _)| *k == t.team_id).map(|x| x.1);
    }
    let st = team_standing(&ctx);
    assert_eq!(st["A"], "contender");
    assert_eq!(st["me"], "contender");
    assert_eq!(st["B"], "rebuilder");
    for t in &mut ctx.teams {
        t.record = Some((0, 0, 0));
    }
    assert!(team_standing(&ctx).is_empty());
}

struct FakeLedger(Vec<(f64, i64)>);

impl LedgerQuery for FakeLedger {
    fn query(&self, sql: &str, args: &[serde_json::Value]) -> anyhow::Result<Vec<Map<String, serde_json::Value>>> {
        if sql.contains("rec_episodes") {
            return Ok(self
                .0
                .iter()
                .enumerate()
                .map(|(i, (_, y))| {
                    json!({"league": "L", "rec_key": format!("k{i}"), "status": if *y == 1 { "followed" } else { "proposed" }})
                        .as_object()
                        .unwrap()
                        .clone()
                        .into_iter()
                        .collect()
                })
                .collect());
        }
        let i: usize = args[1].as_str().unwrap()[1..].parse()?;
        let reasons = json!([{"code": "MARKET_VIEW", "text": "", "value": self.0[i].0}]).to_string();
        Ok(vec![json!({"reasons_json": reasons}).as_object().unwrap().clone().into_iter().collect()])
    }
}

struct BrokenLedger;

impl LedgerQuery for BrokenLedger {
    fn query(&self, _: &str, _: &[serde_json::Value]) -> anyhow::Result<Vec<Map<String, serde_json::Value>>> {
        anyhow::bail!("no ledger")
    }
}

#[test]
fn calibrate_acceptance_keeps_the_prior_until_twenty_proposals() {
    let d = AcceptParams::default();
    let few = calibrate_acceptance(&FakeLedger(vec![(0.0, 1); 5]), None, None, None);
    assert_eq!((few.x0, few.k, few.n, few.source.as_str()), (d.x0, d.k, 5, "prior"));
    assert_eq!(calibrate_acceptance(&BrokenLedger, None, None, None).n, 0);
    let truth = AcceptParams { x0: 8.0, ceiling: 1.0, ..AcceptParams::default() };
    let obs: Vec<(f64, i64)> = (0..241)
        .map(|i| {
            let x = -30.0 + 0.25 * i as f64;
            let u = (i as f64 * 0.6180339887) % 1.0;
            (x, if u < truth.p(x) { 1 } else { 0 })
        })
        .collect();
    let fit = calibrate_acceptance(&FakeLedger(obs), None, None, None);
    assert!(fit.n == 241 && fit.source.starts_with("fit on") && fit.x0 > 4.0 && fit.k > 0.0);
}

#[test]
fn diversity_caps_and_reserved_sweet_spots() {
    let (ctx, values) = build();
    let capped = recommend_trades(&ctx, &values, &TradeOpts { limit: 50, engine: greedy(), ..TradeOpts::default() }, None, Some(&mut Vec::new())).unwrap();
    let mut by_team: HashMap<String, usize> = HashMap::new();
    let mut given: HashMap<String, usize> = HashMap::new();
    for r in &capped {
        *by_team.entry(r.counterparty.clone().unwrap()).or_default() += 1;
        for p in &r.drop {
            *given.entry(p.cid.clone()).or_default() += 1;
        }
    }
    assert!(by_team.values().all(|n| *n <= 3) && given.values().all(|n| *n <= MAX_PER_GIVEN));
    let mut sweet = Vec::new();
    let plain = recommend_trades(&ctx, &values, &opts_g(4, 3), None, Some(&mut sweet)).unwrap();
    let reserved = recommend_trades(&ctx, &values, &opts_g(4, 3), None, None).unwrap();
    assert!(plain.len() <= 4 && reserved.len() <= 4);
    if let Some(s) = sweet.first() {
        assert!(reserved.iter().any(|r| r.title == s.title));
    }
    assert!(reserved.windows(2).all(|w| w[0].score >= w[1].score));
}

// -- offer evaluation (tests/test_trade_eval.py) ---------------------------------------------

#[test]
fn good_offer_is_accept_and_uses_the_proposal_scoring() {
    let (ctx, values) = build();
    let o = evaluate_offer(&ctx, &values, &["m_c2"], &["a_d1"], None, None, &[], &[], &greedy()).unwrap();
    let plain = eval(&ctx, &values, &["m_c2"], &["a_d1"], None, &greedy());
    assert_eq!(o.ev.team.team_id, "A");
    assert!(approx(o.ev.delta_me, plain.delta_me) && o.ev.delta_me > EVAL_ACCEPT_EDGE);
    assert!(o.verdict == "accept" && o.why[0].starts_with("You gain"));
    let codes: Vec<&str> = o.rec.reasons.iter().map(|r| r.code.as_str()).collect();
    assert_eq!(&codes[..2], &["MY_EDGE", "MARKET_VIEW"]);
    let s = offer_summary(&o);
    assert_eq!(s["verdict"], "accept");
    assert_eq!(s["team"], "Team A");
    assert!(s["dynasty_delta"].is_null() && s["units"] == "VORP");
    assert!(s["fairness"]["market_label"].as_str().unwrap().starts_with("Looks"));
}

#[test]
fn losing_near_even_and_bad_offers() {
    let (ctx, values) = build();
    let o = evaluate_offer(&ctx, &values, &["m_c1", "m_c2"], &["a_d3"], None, None, &[], &[], &greedy()).unwrap();
    assert!(o.verdict == "decline" && o.ev.my_edge() < 0.0 && o.why[0].contains("You lose"));
    let o = evaluate_offer(&ctx, &values, &["m_lw"], &["a_lw"], None, None, &[], &[], &greedy()).unwrap();
    assert!(o.verdict == "counter" && (-0.3..EVAL_ACCEPT_EDGE).contains(&o.ev.my_edge()));
    let err = |r: anyhow::Result<OfferEval<'_>>| r.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(err(evaluate_offer(&ctx, &values, &["m_c2"], &["a_d1"], Some("B"), None, &[], &[], &greedy())).contains("Team B's roster"));
    assert!(err(evaluate_offer(&ctx, &values, &["m_c2"], &["a_d1", "b_c"], None, None, &[], &[], &greedy())).contains("single team"));
    assert!(err(evaluate_offer(&ctx, &values, &["a_c"], &["b_c"], None, None, &[], &[], &greedy())).contains("your roster"));
    assert!(evaluate_offer(&ctx, &values, &[], &[], None, None, &[], &[], &greedy()).is_err());
    let e = evaluate_offer(&ctx, &values, &["m_c2"], &["a_d1"], None, None, &[], &[PickRef::Key("pick:2027:1:A".into())], &greedy());
    assert!(err(e).contains("dynasty"));
}

fn named(cid: &str, name: &str, team: &str) -> Player {
    serde_json::from_value(json!({"cid": cid, "name": name, "name_norm": name.to_lowercase(), "ids": {}, "team": team,
                                  "positions": ["C"]}))
    .unwrap()
}

#[test]
fn find_players_by_name() {
    let pool = [
        named("1", "Connor McDavid", "EDM"),
        named("2", "Leon Draisaitl", "EDM"),
        named("3", "Jack Hughes", "NJ"),
        named("4", "Quinn Hughes", "VAN"),
        named("5", "Tim Stützle", "OTT"),
    ];
    let refs: Vec<&Player> = pool.iter().collect();
    for (q, cid) in [("Connor McDavid", "1"), ("mcdavid", "1"), ("c mcdavid", "1"), ("Conor McDavid", "1"), ("J Hughes", "3"), ("tim stutzle", "5"), ("2", "2")] {
        assert_eq!(find_players(&[q], &refs).unwrap()[0].cid, cid, "{q}");
    }
    assert!(find_players(&["hughes"], &refs).unwrap_err().to_string().contains("ambiguous"));
    assert!(find_players(&["Wayne Gretzky"], &refs).unwrap_err().to_string().contains("no player"));
}

#[test]
fn find_team_by_id_name_or_substring() {
    let (ctx, _) = build();
    assert_eq!(find_team(&ctx, "A").unwrap().team_id, "A");
    assert_eq!(find_team(&ctx, "team b").unwrap().team_id, "B");
    assert!(find_team(&ctx, "Team").is_err());
}

fn pick(holder: &str, original: &str, year: i64, round: i64) -> PickAsset {
    PickAsset {
        year,
        round,
        holder: holder.into(),
        original: original.into(),
        original_name: format!("Team {original}"),
        years_ahead: 0,
        expected_slot: 5.0,
        overall: 5.0,
        value: 3.0,
        inferred: false,
        net: 1.0,
    }
}

#[test]
fn find_pick_specs() {
    let picks = [pick("mid", "mid", 2027, 1), pick("mid", "weak", 2027, 1), pick("strong", "strong", 2027, 1)];
    let all: Vec<&PickAsset> = picks.iter().collect();
    let own = find_pick(&all, "2027 R1", "mid").unwrap();
    assert!(own.original == "mid" && own.holder == "mid");
    assert_eq!(find_pick(&all, "2027 1 (from Team weak)", "mid").unwrap().original, "weak");
    assert_eq!(find_pick(&all, "2027 round 1 weak", "mid").unwrap().original, "weak");
    assert!(std::ptr::eq(find_pick(&all, &own.key(), "mid").unwrap(), own));
    assert!(find_pick(&all, "2030 R1", "mid").unwrap_err().to_string().contains("held"));
    assert!(find_pick(&all, "first rounder", "mid").unwrap_err().to_string().contains("bad pick"));
}

// -- keeper leagues (tests/test_keeper_consumers.py) -----------------------------------------

/// Hand-built DynastyValue: value = season + surplus for a keeper, else season.
fn dv(p: &Player, season: f64, surplus: f64, keeper: bool, keep_all: bool, age: f64) -> DynastyValue {
    let keeper = keeper || keep_all;
    serde_json::from_value(json!({"player": p, "value": season + if keeper { surplus } else { 0.0 }, "age": age,
        "age_mult": 1.0, "upside": 0.0, "horizon_years": 3, "season_part": season, "future": surplus,
        "keeper_surplus": surplus, "keeper": keeper}))
    .unwrap()
}

fn trade_league(k: Option<i64>, keep_all: bool, kid_keeper: bool) -> (LeagueContext, Map<Cid, PlayerValue>, Map<Cid, DynastyValue>) {
    type Row<'r> = (&'r str, &'r str, &'r [&'r str], f64, f64);
    let kid_s = if kid_keeper { 6.0 } else { 0.8 };
    let me: Vec<Row> = vec![
        ("C", "m_c", &["C"], 3.0, 5.0), ("LW", "m_lw", &["LW"], 2.0, 3.0), ("D", "m_d1", &["D"], 1.5, 1.0), ("D", "m_d2", &["D"], 1.0, 0.0),
        ("G", "m_g", &["G"], 2.5, 0.5), ("BN", "m_bn1", &["C"], 0.6, 0.0), ("BN", "m_bn2", &["LW"], 0.5, 0.0), ("BN", "m_kid", &["LW"], 0.3, kid_s),
    ];
    let a: Vec<Row> = vec![
        ("C", "a_c", &["C"], 1.2, 0.0), ("LW", "a_lw", &["LW"], 2.2, 0.0), ("D", "a_young", &["D"], 1.4, 2.0),
        ("D", "a_kid", &["D"], 1.2, 4.0), ("BN", "a_d", &["D"], 1.0, 0.0), ("G", "a_g", &["G"], 2.4, 0.0),
    ];
    let (mut values, mut dyn_, mut teams) = (Map::new(), Map::new(), Vec::new());
    for (tid, rows) in [("me", me), ("A", a)] {
        let mut ranked: Vec<&Row> = rows.iter().filter(|r| r.4 > 0.0).collect();
        ranked.sort_by(|x, y| y.4.partial_cmp(&x.4).unwrap());
        let keepers: Vec<&str> = ranked.iter().take(k.unwrap_or(0) as usize).map(|r| r.1).collect();
        let mut entries = Vec::new();
        for (slot, cid, pos, f, s) in &rows {
            let kid = *cid == "m_kid";
            let mut p = pl(cid, pos);
            if kid {
                p.birth_date = chrono::NaiveDate::from_ymd_opt(2026 - 19, 1, 1);
                p.draft_overall = Some(2);
            }
            values.insert(cid.to_string(), pv(&p, *f));
            dyn_.insert(cid.to_string(), dv(&p, *f, *s, keepers.contains(cid), keep_all, if kid { 19.0 } else { 27.0 }));
            entries.push((*slot, p));
        }
        teams.push(team(tid, &format!("Team {tid}"), tid == "me", entries));
    }
    let mut fas = Vec::new();
    for (cid, pos, f) in [("fa_c", "C", 0.9), ("fa_d", "D", 0.8), ("fa_g", "G", 1.0)] {
        let p = pl(cid, &[pos]);
        values.insert(cid.to_string(), pv(&p, f));
        dyn_.insert(cid.to_string(), dv(&p, f, 0.0, false, keep_all, 27.0));
        fas.push(p);
    }
    let mut ctx = ctx_of(teams, fas, &[("C", 1), ("LW", 1), ("D", 2), ("G", 1), ("BN", 3), ("IR", 1)]);
    ctx.dynasty = true;
    ctx.dynasty_mode = fm_core::models::DynastyMode::parse("balanced").unwrap();
    ctx.keepers_per_team = k;
    (ctx, values, dyn_)
}

#[test]
fn keeper_trade_values() {
    // a would-be (K+1)-th keeper adds only his season part
    let (ctx, values, dyn_) = trade_league(Some(2), false, false);
    let ev = eval(&ctx, &values, &["m_d2"], &["a_young"], Some(&dyn_), &greedy());
    assert!(ev.my_drops.is_empty() && ev.my_fill.is_none());
    assert!(approx(ev.keeper_change.as_ref().unwrap().keepers, 0.0) && approx(ev.delta_dyn, 1.4 - 1.0));
    assert!(approx(ev.delta_dyn_n, ev.delta_dyn / dynasty_scale(&ctx)));
    // keep-everyone: his whole value
    let (ctx2, values2, dyn2) = trade_league(None, true, false);
    let ev2 = eval(&ctx2, &values2, &["m_d2"], &["a_young"], Some(&dyn2), &greedy());
    assert!(ev2.keeper_change.is_none() && approx(ev2.delta_dyn, 3.4 - 1.0));
    // displacing my K-th keeper
    let ev = eval(&ctx, &values, &["m_d2"], &["a_kid"], Some(&dyn_), &greedy());
    let ch = ev.keeper_change.as_ref().unwrap();
    assert!(approx(ch.keepers, 1.0) && approx(ev.delta_dyn, 0.2 + 1.0));
    assert_eq!(cids(&ch.kept_in), ["a_kid"]);
    assert_eq!(cids(&ch.kept_out), ["m_lw"]);
    let (v, _, _) = trade_values(&ctx, &values, Some(&dyn_));
    let r = to_rec(&ev, &v, "dynasty", "greedy", None);
    let text = &r.reason("DYNASTY_DELTA").unwrap().text;
    assert!(text.starts_with("Keepers: top-2 long-term +1.00 (a_kid replaces m_lw as a keeper); this season +0.20"), "{text}");
    assert!(text.contains("balanced edge = 1 x lineup + 1 x dynasty"));
    // trading a keeper away
    let ev = eval(&ctx, &values, &["m_lw"], &["a_lw"], Some(&dyn_), &greedy());
    let ch = ev.keeper_change.as_ref().unwrap();
    assert!(approx(ch.keepers, -2.0) && approx(ev.delta_dyn, 0.2 - 2.0));
    assert_eq!(cids(&ch.kept_out), ["m_lw"]);
    assert_eq!(cids(&ch.kept_in), ["m_d1"]);
}

#[test]
fn keeper_pick_values_use_float_v_and_fresh_keeper_flags() {
    // Python's engine passes plain floats V into value_picks: slots come from V, the keeper pool
    // (flags, ages, rookie confidence) from a fresh `apply_dynasty(values, ctx)`, not from the
    // supplied DynastyValue objects.
    let (mut ctx, values, dyn_) = trade_league(Some(2), false, false);
    for (tid, orig) in [("me", "A"), ("A", "me")] {
        let rounds: Vec<serde_json::Value> = (1..=3).map(|r| json!({"round": r, "original_owner": orig})).collect();
        let by_year: Map<i64, Vec<fm_core::models::DraftPickRef>> =
            [(2027, serde_json::from_value(json!(rounds)).unwrap())].into_iter().collect();
        ctx.draft_picks.insert(tid.to_string(), by_year);
    }
    let got = pick_assets(&ctx, &values, Some(&dyn_));
    let (v, units, _) = trade_values(&ctx, &values, Some(&dyn_));
    assert_eq!(units, "dynasty");
    let mut fresh = crate::valuation::dynasty::apply_dynasty(&values, &ctx, None, None);
    for (c, d) in fresh.iter_mut() {
        d.value = v[c];
    }
    let want = crate::valuation::picks::picks_by_holder(crate::valuation::picks::value_picks(&ctx, &values, Some(&fresh)));
    assert!(!got.is_empty());
    assert_eq!(got.len(), want.len());
    for (h, ps) in &want {
        for (a, b) in ps.iter().zip(&got[h]) {
            assert_eq!((a.year, a.round, &a.original), (b.year, b.round, &b.original));
            assert!((a.value - b.value).abs() < 1e-9 && (a.net - b.net).abs() < 1e-9, "{} vs {}", a.value, b.value);
        }
    }
}

#[test]
fn keeper_roster_spot_costs_this_season_only() {
    let (mut ctx, values, dyn_) = trade_league(Some(2), false, false);
    ctx.roster_shape.insert("BN".into(), 4);
    let (_, _, repl) = trade_values(&ctx, &values, Some(&dyn_));
    let mut everyone = ctx.clone();
    everyone.keepers_per_team = None;
    let (_, _, repl_all) = trade_values(&everyone, &values, Some(&dyn_));
    assert!(repl > 0.0 && approx(repl, repl_all / dynasty_scale(&ctx) * season_weight(&ctx)));
    let ev = eval(&ctx, &values, &["m_d2"], &["a_young", "a_d"], Some(&dyn_), &greedy());
    assert!(ev.my_drops.is_empty() && ev.my_fill.is_none());
    assert!(approx(ev.spot_cost, repl) && approx(ev.delta_dyn, (1.4 + 1.0 - 1.0) - repl));
}

#[test]
fn non_keeper_prospect_can_be_dropped_but_a_keeper_prospect_stays_protected() {
    let (ctx, values, dyn_) = trade_league(None, true, false);
    let ev = eval(&ctx, &values, &["m_d2"], &["a_young", "a_d"], Some(&dyn_), &greedy());
    assert!(!cids(&ev.my_drops).contains(&"m_kid".to_string()));
    let (ctx, values, dyn_) = trade_league(Some(2), false, false);
    let ev = eval(&ctx, &values, &["m_d2"], &["a_young", "a_d"], Some(&dyn_), &greedy());
    assert_eq!(cids(&ev.my_drops), ["m_kid"]);
    assert!(ev.blocked.is_none() && approx(ev.delta_dyn, 1.4 + 1.0 - 1.0 - 0.3));
    let (ctx, values, dyn_) = trade_league(Some(2), false, true);
    let ev = eval(&ctx, &values, &["m_d2"], &["a_young", "a_d"], Some(&dyn_), &greedy());
    assert!(!cids(&ev.my_drops).contains(&"m_kid".to_string()));
}

#[test]
fn accept_k_is_log3_over_10() {
    assert_eq!(ACCEPT_K, 3.0f64.ln() / 10.0);
}

/// Release timing on the parity snapshots (`FM_PARITY_FIXTURES`); the same loop as
/// benches/trades.rs. Run: cargo test --release -p fm-engine --lib bench_trades_fixtures -- --ignored --nocapture
#[test]
#[ignore]
fn bench_trades_fixtures() {
    use crate::valuation::params;
    use fm_core::snapshot::Snapshot;
    use std::time::Instant;
    let Some(root) = std::env::var_os("FM_PARITY_FIXTURES").map(std::path::PathBuf::from) else {
        eprintln!("SKIP: FM_PARITY_FIXTURES not set");
        return;
    };
    fn attach<T: serde::de::DeserializeOwned>(path: &std::path::Path, snap: &Snapshot) -> Option<Map<String, T>> {
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        let players: HashMap<&str, &Player> = snap.ctx.all_players().into_iter().map(|p| (p.cid.as_str(), p)).collect();
        let mut out = Map::new();
        for (cid, v) in raw.as_object()? {
            let mut o = v.as_object()?.clone();
            o.remove("cid");
            o.insert("player".into(), serde_json::to_value(players.get(cid.as_str())?).ok()?);
            out.insert(cid.clone(), serde_json::from_value(serde_json::Value::Object(o)).ok()?);
        }
        Some(out)
    }
    for league in ["espn", "fantrax"] {
        let dir = root.join(league);
        let snap = Snapshot::load(dir.join("snapshot.json")).expect("snapshot");
        params::install_with_active(snap.params.merged.clone(), Some(snap.params.active.clone()));
        let values: Map<String, PlayerValue> = attach(&dir.join("values.json"), &snap).expect("values");
        let dynasty: Option<Map<String, DynastyValue>> = attach(&dir.join("dynasty.json"), &snap);
        let opts = TradeOpts { dynasty_values: dynasty.as_ref(), ..TradeOpts::default() };
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for _ in 0..7 {
            let t = Instant::now();
            let n = recommend_trades(&snap.ctx, &values, &opts, None, None).unwrap().len();
            a.push(t.elapsed());
            let t = Instant::now();
            let m = recommend_need_trades(&snap.ctx, &values, &opts, None, 5, NEED_PER_TEAM, None).unwrap().len();
            b.push(t.elapsed());
            assert!(n > 0 && m > 0);
        }
        a.sort();
        b.sort();
        eprintln!(
            "{league}: recommend_trades min {:?} median {:?}; recommend_need_trades min {:?} median {:?}; {} threads",
            a[0], a[3], b[0], b[3], rayon::current_num_threads()
        );
    }
}

// -- shop a player / get a player ------------------------------------------------------------

#[test]
fn shop_player_lists_what_teams_give_for_him() {
    for greedy_only in [false, true] {
        let (ctx, values) = build();
        let opts = EngineOpts { greedy_only, ..EngineOpts::default() };
        let offers = shop_player(&ctx, &values, "m_c2", None, &opts, 10).unwrap();
        assert!(!offers.is_empty());
        assert!(offers.iter().any(|o| o.ev.team.team_id == "A" && o.ev.get.iter().any(|p| ["a_d1", "a_d2"].contains(&p.cid.as_str()))));
        for o in &offers {
            assert_eq!(cids(&o.ev.give)[0], "m_c2");
            assert!(o.ev.give.len() <= 2 && (1..=2).contains(&o.ev.get.len()));
            assert!(o.ev.accepted() && o.ev.listable() && o.verdict != "decline");
            assert!(o.rec.rank_in_kind.is_some());
        }
        for t in ["A", "B"] {
            assert!(offers.iter().filter(|o| o.ev.team.team_id == t).count() <= TARGET_PER_TEAM);
        }
        let scores: Vec<f64> = offers.iter().map(|o| o.ev.score()).collect();
        assert!(scores.windows(2).all(|w| w[0] >= w[1]));
        // no padded deal: a bigger package never sits beside a sub-deal of it that does as well
        for a in &offers {
            for b in &offers {
                let inside = a.ev.give.len() + a.ev.get.len() < b.ev.give.len() + b.ev.get.len()
                    && a.ev.team.team_id == b.ev.team.team_id
                    && cids(&a.ev.give).iter().all(|c| cids(&b.ev.give).contains(c))
                    && cids(&a.ev.get).iter().all(|c| cids(&b.ev.get).contains(c));
                assert!(!(inside && a.ev.score() >= b.ev.score()));
            }
        }
    }
}

#[test]
fn acquire_player_ranks_likely_cheap_packages_first() {
    let (ctx, values) = build();
    let offers = acquire_player(&ctx, &values, "a_d1", None, &EngineOpts::default(), 10).unwrap();
    assert!(!offers.is_empty());
    for o in &offers {
        assert_eq!(o.ev.team.team_id, "A");
        assert_eq!(cids(&o.ev.get), ["a_d1"]);
        assert!((1..=2).contains(&o.ev.give.len()));
        assert!(o.ev.p() >= MIN_P_ACCEPT - 1e-9 && o.ev.my_edge() > 0.0 && o.verdict != "decline");
    }
    let key: Vec<(bool, f64)> = offers.iter().map(|o| (o.ev.p() >= TARGET_LIKELY_P - 1e-9, o.ev.my_edge())).collect();
    assert!(key.windows(2).all(|w| w[0].0 > w[1].0 || (w[0].0 == w[1].0 && w[0].1 >= w[1].1 - 1e-9)));
}

#[test]
fn shop_and_acquire_reject_players_on_the_wrong_side() {
    let (ctx, values) = build();
    let err = |r: anyhow::Result<Vec<OfferEval<'_>>>| r.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(err(shop_player(&ctx, &values, "a_d1", None, &greedy(), 10)).contains("not on your roster"));
    assert!(err(acquire_player(&ctx, &values, "m_c1", None, &greedy(), 10)).contains("another team"));
    assert!(err(acquire_player(&ctx, &values, "fa_c", None, &greedy(), 10)).contains("another team"));
}
