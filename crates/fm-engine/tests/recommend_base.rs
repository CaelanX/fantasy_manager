//! W5-recommend-base: ports of tests/test_headline.py, test_strength.py, test_market.py,
//! test_move_budget.py (the recommend.base / streaming parts), test_schedule_grid.py, unit tests
//! for the keeper / roster / position helpers, and strength / headline parity against the stored
//! engine outputs (`fixtures/<league>/engines/*.json`).

use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{Duration, NaiveDate};
use fm_core::models::{GainUnits, LeagueContext, Player, RecKind, Reason, Recommendation};
use fm_core::provider::LeagueProvider;
use fm_core::snapshot::{EspnLeagueMeta, EspnLeagueSettings, LeagueRules, ProviderExtras};
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::Map;
use fm_engine::analysis::schedule_grid as sg;
use fm_engine::recommend::base::*;
use fm_engine::recommend::headline::{apply_headlines, day_swap_headline, headline_for};
use fm_engine::recommend::market::*;
use fm_engine::recommend::strength::*;
use serde_json::{json, Value};

fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, dd).unwrap()
}

fn approx(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

// ------------------------------------------------------------------------------------ builders

fn pl_json(cid: &str, extra: Value) -> Player {
    let mut v = json!({"cid": cid, "name": cid, "name_norm": cid.to_lowercase(), "ids": {}, "team": "EDM", "positions": ["C"]});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::from_value(v).unwrap()
}

fn pl(name: &str) -> Player {
    pl_json(name, json!({}))
}

fn pl_status(name: &str, status: &str) -> Player {
    pl_json(name, json!({"status": status}))
}

fn pl_gp(cid: &str, gp: i64) -> Player {
    if gp == 0 {
        return pl(cid);
    }
    pl_json(cid, json!({"lines": {"season": {"split": "season", "gp": gp, "stats": {"GP": gp}}}}))
}

fn rec(kind: RecKind, reasons: Vec<Reason>) -> Recommendation {
    let mut r = Recommendation::new(kind, 1.0, "t");
    r.reasons = reasons;
    r
}

fn rs(code: &str, text: &str, value: Option<f64>) -> Reason {
    Reason::new(code, text).with_value(value)
}

fn pv(p: &Player, fpg: f64, reasons: Vec<Reason>) -> PlayerValue {
    serde_json::from_value(json!({
        "player": p, "fpg": fpg, "fpg_season": fpg, "fpg_week": fpg, "vorp": fpg - 1.0, "vorp_week": fpg - 1.0,
        "reasons": reasons,
    }))
    .unwrap()
}

fn slot(s: &str, p: &Player, starting: bool) -> Value {
    json!({"slot": s, "player": p, "starting": starting})
}

fn mk_ctx(slots: Vec<Value>, extra: Value) -> LeagueContext {
    let mut v = json!({
        "provider": "test", "league_id": "1", "season": 2027, "name": "T",
        "scoring": {"kind": "points", "weights": {"G": 1.0}}, "roster_shape": {"C": 12},
        "teams": [{"team_id": "me", "name": "Me", "owner_is_me": true, "slots": slots}],
        "free_agents": [], "matchup_period": 1, "as_of": "2026-10-01",
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::from_value(v).unwrap()
}

fn dv(p: &Player, value: f64, season_part: f64, surplus: f64, keeper: bool) -> DynastyValue {
    serde_json::from_value(json!({
        "player": p, "value": value, "age": null, "age_mult": 1.0, "upside": 0.0, "horizon_years": 3,
        "season_part": season_part, "keeper_surplus": surplus, "keeper": keeper,
    }))
    .unwrap()
}

// ------------------------------------------------------------------------------------ headline

#[test]
fn headline_day_swaps() {
    let (n, s) = (pl("Nylander"), pl("Stenberg"));
    assert_eq!(day_swap_headline(Some(&n), Some(&s), 5.4, 0.0, false, Some("F"), Some("@CGY")), "Stenberg has no game today; Nylander plays @CGY");
    assert_eq!(day_swap_headline(Some(&n), Some(&s), 5.4, 0.0, true, None, None), "Stenberg isn't expected to play today; Nylander has a game");
    assert_eq!(day_swap_headline(Some(&n), Some(&s), 5.4, 3.1, true, None, None), "Nylander projects 5.4 pts today vs 3.1 for Stenberg");
    assert_eq!(day_swap_headline(Some(&n), None, 5.4, 0.0, false, Some("F"), Some("LAK")), "Empty F slot today: Nylander plays LAK");
    assert_eq!(day_swap_headline(None, Some(&s), 0.0, 0.0, false, None, None), "Stenberg isn't expected to score today; leave the slot empty");
}

#[test]
fn headline_lineup_from_reasons() {
    let (n, s) = (pl("Nylander"), pl("Stenberg"));
    let mut r = rec(
        RecKind::Lineup,
        vec![rs("DAY_VALUE", "Nylander: 5.40 projected pts today", Some(5.4)), rs("DAY_VALUE", "Stenberg: no game today", Some(0.0))],
    );
    r.add = vec![n.clone()];
    r.drop = vec![s.clone()];
    assert_eq!(headline_for(&r).unwrap(), "Stenberg has no game today; Nylander has a game");
    let mut weekly = rec(
        RecKind::Lineup,
        vec![rs("GAMES_NEXT7", "Nylander: 4 games next 7 days", Some(4.0)), rs("GAMES_NEXT7", "Stenberg: 2 games next 7 days", Some(2.0))],
    );
    weekly.add = vec![n];
    weekly.drop = vec![s];
    assert_eq!(headline_for(&weekly).unwrap(), "Nylander: 4 games this week vs 2 for Stenberg");
}

#[test]
fn headline_ir_swap_and_bench() {
    let (hy, ma) = (pl_status("Hyman", "dtd"), pl_status("Marchand", "ir"));
    let mut swap = rec(RecKind::Lineup, vec![rs("IR_SWAP", "...", None)]);
    swap.add = vec![hy];
    swap.subjects = vec![ma.clone()];
    assert_eq!(headline_for(&swap).unwrap(), "Hyman is back (dtd); Marchand (ir) takes his IR slot");
    let mut bench = rec(RecKind::Lineup, vec![rs("STATUS", "Marchand is ir", None)]);
    bench.drop = vec![ma];
    assert_eq!(headline_for(&bench).unwrap(), "Marchand is ir: he scores nothing from a starting slot");
}

#[test]
fn headline_waivers_lead_with_matchup_points_and_market() {
    let (a, dr) = (pl("Brodin"), pl("Giroux"));
    let mut stream = rec(
        RecKind::Waiver,
        vec![
            rs("VORP_DELTA", "+0.5 FPG (week)", Some(0.5)),
            rs("MATCHUP_GAIN", "+1.5 pts through Sun", Some(1.5)),
            rs("MARKET_CHECK", "Brodin: market 40th vs model 45th, agrees", Some(5.0)),
        ],
    );
    stream.add = vec![a.clone()];
    stream.drop = vec![dr.clone()];
    assert_eq!(headline_for(&stream).unwrap(), "+1.5 pts over the rest of this matchup with your best daily lineup \u{b7} market agrees");
    let mut fill = rec(
        RecKind::Waiver,
        vec![
            rs("VORP_DELTA", "fills empty D slot (+3.76 FPG, season); drop bench F X", Some(3.76)),
            rs("MARKET_CHECK", "Pionk: model disagrees with market, rates him higher", Some(30.0)),
        ],
    );
    fill.add = vec![a.clone()];
    fill.drop = vec![dr.clone()];
    assert_eq!(headline_for(&fill).unwrap(), "Fills your empty D slot \u{b7} model disagrees with market");
    let mut season = rec(RecKind::Waiver, vec![rs("VORP_DELTA", "+0.72 FPG (season)", Some(0.72)), rs("OPEN_SPOT", "1 open roster spot", Some(1.0))]);
    season.add = vec![a];
    assert_eq!(headline_for(&season).unwrap(), "+0.72 FPG for the rest of the season; no drop needed");
    let mut claim = rec(RecKind::Waiver, vec![rs("VORP_DELTA", "+0.72", Some(0.72)), rs("WAIVER_CLAIM", "claim", None)]);
    claim.add = vec![pl_json("X", json!({"waiver_until": "2026-10-28"}))];
    claim.drop = vec![dr];
    assert!(headline_for(&claim).unwrap().ends_with("; claim (on waivers until Wed)"));
}

#[test]
fn headline_trade_flags_overpay() {
    let r = rec(
        RecKind::Trade,
        vec![
            rs("MY_EDGE", "edge", Some(0.81)),
            rs("P_ACCEPT", "p", Some(0.9)),
            rs("MARKET_CHECK", "Model disagrees with market: by market value you overpay", None).with_baseline(Some(21.0)),
        ],
    );
    assert_eq!(headline_for(&r).unwrap(), "Your lineup gains +0.81 FPG; ~90% they accept \u{b7} market says you overpay");
}

#[test]
fn headline_injury_and_flags() {
    let hold = rec(RecKind::Injury, vec![rs("HOLD_VERDICT", "Swapping IR slots makes keeping Marchand free", None)]);
    assert_eq!(headline_for(&hold).unwrap(), "Swapping IR slots makes keeping Marchand free");
    let mut cut = rec(
        RecKind::Injury,
        vec![rs("HOLD_PTS", "h", Some(40.0)), rs("CUT_PTS", "c", Some(90.0)), rs("BREAK_EVEN", "Cut only if he misses more than ~20 games", None)],
    );
    cut.drop = vec![pl_status("Tippett", "ir")];
    cut.add = vec![pl("Pionk")];
    assert_eq!(headline_for(&cut).unwrap(), "Cutting wins by ~50 pts rest of season; cut only if he misses more than ~20 games");
    let hot = rec(RecKind::SellHigh, vec![rs("FORM_RATIO", "f", Some(1.6)), rs("XG_LUCK", "x", Some(0.4))]);
    assert_eq!(headline_for(&hot).unwrap(), "Running hot: last 15 days 1.60x his baseline, scoring above his expected goals");
    let cold = rec(RecKind::BuyLow, vec![rs("FORM_RATIO", "f", Some(0.59)), rs("SOG_RATE", "s", None), rs("XG_LUCK", "x", Some(-0.5))]);
    assert_eq!(headline_for(&cold).unwrap(), "Cold stretch: last 15 days 0.59x his baseline; his shots held up, finishing below expected");
}

#[test]
fn headline_apply_ranks_fills_missing_only_and_cut() {
    let a = rec(RecKind::Alert, vec![rs("X", "Judd Caulfield now on PP1", None)]);
    let mut keep = rec(RecKind::Lineup, vec![]);
    keep.headline = Some("already set".to_string());
    let mut recs = vec![a, keep];
    apply_ranks(&mut recs);
    assert_eq!(recs[0].headline.as_deref(), Some("Judd Caulfield now on PP1"));
    assert_eq!(recs[1].headline.as_deref(), Some("already set"));
    let mut w = vec![rec(RecKind::Waiver, vec![])];
    apply_headlines(&mut w);
    assert!(w[0].headline.is_none());
    // _cut counts code points and appends the ellipsis
    let long = "\u{e9}".repeat(200);
    let r = rec(RecKind::Alert, vec![rs("X", &long, None)]);
    let h = headline_for(&r).unwrap();
    assert_eq!(h.chars().count(), 140);
    assert!(h.ends_with('\u{2026}'));
}

#[test]
fn day_scale_keeps_routine_swaps_below_ten() {
    assert!(approx(interp(5.4, &DAY_SCALE), 5.6, 1e-9));
    let mut r = Recommendation::new(RecKind::Lineup, 5.4, "t");
    r.predicted_gain = Some(5.4);
    r.gain_units = Some(GainUnits::DayPts);
    r.horizon_days = Some(1);
    let s = strength_for(&r);
    assert!(5.0 < s && s < 6.5);
    r.predicted_gain = Some(25.0);
    assert_eq!(strength_for(&r), 10.0);
}

// ------------------------------------------------------------------------------------ strength

fn waiver(gain: f64, gp: i64) -> Recommendation {
    let mut r = Recommendation::new(RecKind::Waiver, gain, "w");
    r.add = vec![pl_gp("a", gp)];
    r.predicted_gain = Some(gain);
    r.gain_units = Some(GainUnits::SeasonFpg);
    r
}

#[test]
fn scales_are_monotone_and_capped() {
    let scales: [&Scale; 5] = [&WAIVER_SCALE, &WEEK_SCALE, &TRADE_SCALE, &TRADE_EV_SCALE, &FLAG_SCALE];
    for scale in scales {
        let xs: Vec<f64> = (0..120).map(|i| i as f64 / 10.0).collect();
        let ys: Vec<f64> = xs.iter().map(|x| interp(*x, scale)).collect();
        assert!(ys.windows(2).all(|w| w[0] <= w[1]) && ys[0] == 0.0 && ys.iter().all(|y| *y <= 10.0));
        for (x, y) in scale {
            assert!(approx(interp(*x, scale), *y, 1e-9));
        }
    }
}

#[test]
fn waiver_anchor_points_with_full_confidence() {
    assert!(approx(strength_for(&waiver(0.3, 5000)), 3.0, 0.1));
    assert!(approx(strength_for(&waiver(1.0, 5000)), 6.0, 0.1));
    assert!(approx(strength_for(&waiver(2.0, 5000)), 9.0, 0.1));
    assert_eq!(strength_for(&waiver(5.0, 5000)), 10.0);
    let gs = [0.2, 0.5, 0.9, 1.5];
    let ys: Vec<f64> = gs.iter().map(|g| strength_for(&waiver(*g, 5000))).collect();
    assert!(ys.windows(2).all(|w| w[0] <= w[1]));
}

#[test]
fn confidence_scales_preseason_moves_down() {
    let (pre, late) = (waiver(0.95, 0), waiver(0.95, 1000));
    assert!(strength_for(&pre) < strength_for(&late));
    assert!(approx(strength_for(&pre), interp(0.95 * 0.5, &WAIVER_SCALE), 0.01));
}

#[test]
fn legacy_trade_keeps_the_dynasty_bump() {
    let mk = |reasons: Vec<Reason>| {
        let mut r = Recommendation::new(RecKind::Trade, 1.0, "t");
        r.add = vec![pl_gp("a", 1000)];
        r.drop = vec![pl_gp("b", 1000)];
        r.predicted_gain = Some(1.0);
        r.gain_units = Some(GainUnits::LineupFpg);
        r.reasons = reasons;
        r
    };
    let plain = strength_for(&mk(vec![]));
    let up = strength_for(&mk(vec![rs("DYNASTY_DELTA", "", Some(0.5))]));
    let down = strength_for(&mk(vec![rs("DYNASTY_DELTA", "", Some(-0.5))]));
    assert!(approx(up, plain + 1.0, 1e-9) && approx(down, plain - 1.0, 1e-9));
}

#[test]
fn flags_injury_and_lineup_rules() {
    let flag = rec(RecKind::SellHigh, vec![rs("FORM_RATIO", "", Some(1.6))]);
    assert!(approx(strength_for(&flag), 7.0, 1e-9));
    let mut ir = Recommendation::new(RecKind::Injury, 1.0, "Move x to IR");
    ir.predicted_gain = Some(0.1);
    ir.gain_units = Some(GainUnits::SeasonFpg);
    ir.reasons = vec![rs("IR_SLOT", "", None)];
    assert_eq!(strength_for(&ir), 5.0);
    let bench = rec(RecKind::Lineup, vec![rs("STATUS", "", None)]);
    assert_eq!(strength_for(&bench), 5.0);
    let mut wk = Recommendation::new(RecKind::Lineup, 1.0, "s");
    wk.predicted_gain = Some(3.0);
    wk.gain_units = Some(GainUnits::WeekPts);
    wk.horizon_days = Some(7);
    assert!(approx(strength_for(&wk), 6.0, 1e-9));
    assert_eq!(gain_text(&wk), "+3.0 pts/wk");
    // a bare status alert maps severity
    let mut st = Recommendation::new(RecKind::Injury, 1.0, "x");
    st.subjects = vec![pl_status("a", "ltir")];
    assert_eq!(strength_for(&st), 10.0);
}

#[test]
fn apply_ranks_groups_flags() {
    let mut recs: Vec<Recommendation> =
        [RecKind::Waiver, RecKind::SellHigh, RecKind::Waiver, RecKind::BuyLow].iter().map(|k| Recommendation::new(*k, 1.0, k.as_str())).collect();
    apply_ranks(&mut recs);
    let got: Vec<(i64, i64)> = recs.iter().map(|r| (r.rank_in_kind.unwrap(), r.kind_total.unwrap())).collect();
    assert_eq!(got, vec![(1, 2), (1, 2), (2, 2), (2, 2)]);
    assert_eq!(rank_text(&recs[2]), "2/2");
}

fn trade(edge: f64, p: f64, gp: i64, units: GainUnits) -> Recommendation {
    let mut r = Recommendation::new(RecKind::Trade, edge * p, "t");
    r.add = vec![pl_gp("a", gp)];
    r.drop = vec![pl_gp("b", gp)];
    r.predicted_gain = Some(edge);
    r.gain_units = Some(units);
    r.reasons = vec![rs("MY_EDGE", "", Some(edge)), rs("P_ACCEPT", "", Some(p))];
    r
}

#[test]
fn trade_strength_anchors_on_expected_value() {
    let t = |e, p| strength_for(&trade(e, p, 5000, GainUnits::LineupFpg));
    assert!(approx(t(0.5, 0.7), 6.0, 0.1));
    assert!(approx(t(1.0, 0.6), 8.0, 0.1));
    assert!(approx(t(2.0, 0.5), 10.0, 0.1));
    assert!(approx(t(0.3, 0.5), 3.0, 0.1));
    assert_eq!(t(3.0, 0.9), 10.0);
    assert!(t(0.5, 0.7) > t(1.5, 0.2));
}

#[test]
fn trade_strength_keeps_confidence_scaling() {
    let pre = strength_for(&trade(0.5, 0.7, 0, GainUnits::LineupFpg));
    let late = strength_for(&trade(0.5, 0.7, 1000, GainUnits::LineupFpg));
    assert!(pre < late);
    assert!(approx(pre, interp(0.35 * 0.5, &TRADE_EV_SCALE), 0.01));
}

#[test]
fn future_only_trade_uses_the_dynasty_gain() {
    let mut r = trade(0.4, 0.5, 5000, GainUnits::Dynasty);
    r.reasons[0] = rs("MY_EDGE", "", Some(-0.9));
    assert!(approx(strength_for(&r), interp(0.2 * 0.995, &TRADE_EV_SCALE), 0.05));
}

#[test]
fn matchup_points_label_says_days_left() {
    let mut r = Recommendation::new(RecKind::Waiver, 1.0, "x");
    r.predicted_gain = Some(3.1);
    r.gain_units = Some(GainUnits::MatchupPts);
    r.horizon_days = Some(6);
    assert_eq!(gain_text(&r), "+3.1 pts (6d left)");
    r.horizon_days = None;
    assert_eq!(gain_text(&r), "+3.1 pts (this matchup)");
    r.predicted_gain = None;
    assert_eq!(gain_text(&r), "-");
}

// ------------------------------------------------------------------------------------ market

fn pool_ctx(dynasty: bool) -> (LeagueContext, Map<String, PlayerValue>, Vec<Player>) {
    let mut rows = Vec::new();
    let mut values = Map::new();
    for i in 0..12 {
        let p = pl_json(&format!("p{i}"), json!({"adp": (i + 1) as f64, "pct_owned": 100.0 - 5.0 * i as f64}));
        values.insert(p.cid.clone(), pv(&p, 3.0 - 0.1 * i as f64, vec![]));
        rows.push(p);
    }
    let slots = rows.iter().map(|p| slot("C", p, true)).collect();
    (mk_ctx(slots, json!({"dynasty": dynasty})), values, rows)
}

#[test]
fn market_consensus_uses_only_independent_signals() {
    let (ctx, values, rows) = pool_ctx(true);
    let dyn_: Map<String, f64> = rows.iter().enumerate().map(|(i, p)| (p.cid.clone(), 20.0 - i as f64)).collect();
    let pool = MarketPool::new(&ctx, Some(&values), Some(&dyn_));
    assert!(pool.value_and_sources(&rows[0]).1.contains(&"dyn"));
    assert_eq!(pool.consensus(&rows[0]).unwrap().1.as_slice(), &["adp", "owned"]);
    let bare = pl("bare");
    assert!(pool.consensus(&bare).is_none());
    assert!(market_check_reason(&bare, &pool, "add").is_none());
    // a non-dynasty league ignores the dynasty map
    let (ctx2, values2, rows2) = pool_ctx(false);
    let pool2 = MarketPool::new(&ctx2, Some(&values2), Some(&dyn_));
    assert!(!pool2.value_and_sources(&rows2[0]).1.contains(&"dyn"));
}

#[test]
fn market_check_agrees_and_disagrees_with_drivers() {
    let (ctx, mut values, rows) = pool_ctx(false);
    let pool = MarketPool::new(&ctx, Some(&values), None);
    let agree = market_check_reason(&rows[0], &pool, "add").unwrap();
    assert_eq!(agree.code, "MARKET_CHECK");
    assert!(agree.text.contains("market agrees") && agree.value.unwrap().abs() < DISAGREE_GAP);
    // the market's last player, whom our model ranks first because of a PP1 role signal
    values.insert("p11".into(), pv(&rows[11], 3.5, vec![rs("ROLE_DFO", "PP1", None), rs("FPG_BLEND", "", None)]));
    let pool = MarketPool::new(&ctx, Some(&values), None);
    let r = market_check_reason(&rows[11], &pool, "add").unwrap();
    assert!(r.value.unwrap() >= DISAGREE_GAP && r.baseline.unwrap() < 10.0);
    assert!(r.text.contains("model disagrees with market, rates him higher (line / PP role)"), "{}", r.text);
}

#[test]
fn market_value_and_percentile_numbers() {
    let (ctx, values, rows) = pool_ctx(false);
    let pool = MarketPool::new(&ctx, Some(&values), None);
    // adp percentile of the best player: 100 * (0 + 0.5)/12 on -adp; here ref sorted ascending of -adp
    let comps = pool.components(&rows[0]);
    let by: Map<&str, f64> = comps.iter().copied().collect();
    assert!(approx(by["adp"], 100.0 * 11.5 / 12.0, 1e-9));
    assert!(approx(by["owned"], 100.0 * 11.5 / 12.0, 1e-9));
    assert!(approx(by["fpg"], 100.0 * 11.5 / 12.0, 1e-9));
    assert!(approx(pool.value(&rows[0]), 100.0 * 11.5 / 12.0, 1e-9));
    assert_eq!(pool.sources(rows.iter().take(2)), vec!["adp", "owned"]);
    assert_eq!(percentile(1.0, &[]), 50.0);
    assert!(approx(percentile(2.0, &[1.0, 2.0, 2.0, 3.0]), 50.0, 1e-9));
    assert_eq!(ordinal(42.5), "42nd");
    assert_eq!(ordinal(43.5), "44th");
    assert_eq!(ordinal(11.0), "11th");
    assert_eq!(ordinal(103.0), "103rd");
}

#[test]
fn market_disagrees_flags() {
    let mut r = Recommendation::new(RecKind::Waiver, 1.0, "x");
    assert_eq!(market_disagrees(&r), None);
    r.reasons = vec![rs("MARKET_CHECK", "", Some(5.0))];
    assert_eq!(market_disagrees(&r), Some(false));
    r.reasons.push(rs("MARKET_CHECK", "", Some(-30.0)));
    assert_eq!(market_disagrees(&r), Some(true));
    let mut t = Recommendation::new(RecKind::Trade, 1.0, "t");
    t.reasons = vec![rs("MARKET_VIEW", "", Some(17.5)), rs("MARKET_CHECK", "", Some(2.0))];
    assert_eq!(market_disagrees(&t), Some(false));
    let why = vec!["x_d: line / PP role".to_string()];
    let deal = trade_market_check(17.5, 0.99, Some(&why)).unwrap();
    assert!(deal.text.starts_with("Model disagrees with market: you overpay by market value (+18 worth pts"), "{}", deal.text);
    assert!(deal.text.contains("worth it only if you trust the model's edge of +0.99 (drivers: x_d: line / PP role)"));
    t.reasons.push(deal);
    assert_eq!(market_disagrees(&t), Some(true));
    assert!(trade_market_check(17.5, 0.99, None).unwrap().text.contains("projection only"));
    assert!(trade_market_check(2.0, 0.99, None).is_none() && trade_market_check(17.5, -0.1, None).is_none());
}

#[test]
fn pick_checks() {
    let (ctx, values, _rows) = pool_ctx(true);
    let dyn_: Map<String, f64> = (0..12).map(|i| (format!("p{i}"), 10.0 + i as f64)).collect();
    let pool = MarketPool::new(&ctx, Some(&values), Some(&dyn_));
    assert!(approx(pick_percentile(&pool, 15.5), 50.0, 1e-9));
    let (ctx2, values2, _) = pool_ctx(false);
    let pool2 = MarketPool::new(&ctx2, Some(&values2), None);
    assert_eq!(pick_percentile(&pool2, 5.0), 50.0);
    let pick: fm_core::PickAsset = serde_json::from_value(json!({
        "year": 2027, "round": 1, "holder": "me", "original": "t2", "original_name": "Them", "years_ahead": 1,
        "expected_slot": 4.4, "overall": 4.4, "value": 12.34, "net": 3.21,
    }))
    .unwrap();
    let r = pick_check_reason(&pick, "receive", Some("me"));
    assert_eq!(
        r.text,
        "Market check (receive) 2027 R1 (from Them) pick: model-only value 12.3 (3.2 net of its roster spot; expected slot 4); no consensus source for draft picks"
    );
    assert!(r.value.is_none() && r.baseline.is_none());
}

// ------------------------------------------------------------------------------------ base: moves

fn budget(ctx: &mut LeagueContext, limit: Option<i64>, used: Option<i64>) {
    ctx.moves_limit_per_period = limit;
    ctx.moves_used_this_period = used;
    ctx.period_start = Some(d(2026, 9, 28));
    ctx.period_end = Some(d(2026, 10, 4));
    ctx.moves_period_label = Some("matchup period".to_string());
}

#[test]
fn move_scarcity_threshold_table() {
    let cases: [(Option<i64>, Option<i64>, Option<f64>, Option<f64>); 8] = [
        (None, None, Some(0.3), None),
        (Some(7), Some(2), Some(0.3), None),
        (Some(4), Some(0), Some(0.3), None),
        (Some(4), Some(1), Some(0.6), None),
        (Some(4), Some(2), Some(0.6), None),
        (Some(4), Some(3), Some(1.0), Some(0.3)),
        (Some(4), Some(4), None, None),
        (Some(2), Some(5), None, None),
    ];
    for (limit, used, expected, injury) in cases {
        let mut ctx = mk_ctx(vec![], json!({}));
        budget(&mut ctx, limit, used);
        assert_eq!(move_scarcity_threshold(&ctx, false), expected, "{limit:?} {used:?}");
        assert_eq!(move_scarcity_threshold(&ctx, true), injury.or(expected), "{limit:?} {used:?}");
    }
}

/// On the last day of the move period this period's unused adds expire, so the scarcity bars no
/// longer apply: 1 or 2 moves left ask the base 0.3 FPG instead of 1.0 or 0.6. A season limit that
/// is nearly used up still binds, because a move saved today is still there tomorrow.
#[test]
fn move_bar_lapses_on_the_period_last_day() {
    let mut ctx = mk_ctx(vec![], json!({}));
    budget(&mut ctx, Some(4), Some(3));
    assert_eq!(move_scarcity_threshold(&ctx, false), Some(1.0));
    ctx.as_of = d(2026, 10, 4);
    assert_eq!(moves_left(&ctx), Some(1));
    assert_eq!(moves_left_to_save(&ctx), None);
    assert_eq!(move_scarcity_threshold(&ctx, false), Some(0.3));
    assert_eq!(moves_budget_reason(&ctx).unwrap().text, "1 of 4 moves left this week: unused moves expire after today");
    // a free agent takes effect today; a waiver claim is processed tomorrow and keeps the full bar
    let fa = pl("FA");
    let claim = pl_json("WV", json!({"waiver_until": "2026-10-04"}));
    assert_eq!(add_scarcity_threshold_at(&ctx, &fa, false, 1.0), Some(0.3));
    assert_eq!(add_scarcity_threshold_at(&ctx, &claim, false, 1.0), Some(1.0));
    ctx.moves_used_this_period = Some(2);
    assert_eq!(move_scarcity_threshold(&ctx, false), Some(0.3));
    ctx.moves_used_this_period = Some(4);
    assert_eq!(move_scarcity_threshold(&ctx, false), None);
    // a season limit with 1 left (9 of 10 used) still makes the last move scarce
    ctx.moves_used_this_period = Some(0);
    ctx.moves_limit_season = Some(10);
    ctx.moves_used_season = Some(9);
    assert_eq!(moves_left_to_save(&ctx), Some(1));
    assert_eq!(move_scarcity_threshold(&ctx, false), Some(1.0));
    // the day before, nothing changes
    ctx.moves_limit_season = None;
    ctx.moves_used_this_period = Some(3);
    ctx.as_of = d(2026, 10, 3);
    assert_eq!(move_scarcity_threshold(&ctx, false), Some(1.0));
}

#[test]
fn moves_left_texts_and_season_cap() {
    let mut ctx = mk_ctx(vec![], json!({}));
    budget(&mut ctx, Some(4), Some(2));
    assert_eq!(moves_left(&ctx), Some(2));
    assert_eq!(moves_text(&ctx), "4 per matchup period (2 used, 2 left, resets Mon Oct 5)");
    assert_eq!(moves_budget_reason(&ctx).unwrap().text, "2 of 4 moves left this week: adds must gain >= 0.6 FPG");
    ctx.moves_used_this_period = Some(4);
    assert_eq!(no_moves_text(&ctx), "No moves left this week (4/4 used); waiver suggestions resume Mon Oct 5");
    ctx.moves_used_this_period = Some(0);
    ctx.moves_limit_season = Some(10);
    ctx.moves_used_season = Some(9);
    assert_eq!(moves_left(&ctx), Some(1));
    assert!(moves_budget_reason(&ctx).unwrap().text.starts_with("1 of 10 moves left this season"));
    assert_eq!(moves_text(&ctx), "4 per matchup period (0 used, 4 left, resets Mon Oct 5); season limit 10 (9 used, 1 left)");
    let mut free = mk_ctx(vec![], json!({}));
    assert_eq!(moves_left(&free), None);
    assert_eq!(moves_text(&free), "unlimited");
    assert!(moves_budget_reason(&free).is_none());
    free.moves_used_this_period = Some(3);
    assert_eq!(moves_text(&free), "unlimited (3 adds this week)");
}

#[test]
fn churn_guard() {
    let mut ctx = mk_ctx(vec![], json!({}));
    let sk = pl("Skinner");
    ctx.recent_adds.insert(sk.cid.clone(), d(2026, 9, 30));
    assert!(recently_added(&ctx, &sk, RECENT_ADD_DAYS));
    assert_eq!(added_days_ago(&ctx, &sk), Some(1));
    assert_eq!(churn_text(&ctx, &sk), "added 1 day ago");
    assert!(churn_blocked(&ctx, &sk, 0.5));
    assert!(!churn_blocked(&ctx, &sk, 1.5));
    assert!(!churn_blocked(&ctx, &pl_status("Skinner", "out"), 0.5));
    ctx.recent_adds.insert("Skinner".into(), d(2026, 10, 1));
    assert_eq!(churn_text(&ctx, &sk), "added today");
    ctx.recent_adds.insert("Skinner".into(), d(2026, 9, 1));
    assert!(!recently_added(&ctx, &sk, RECENT_ADD_DAYS));
    assert_eq!(churn_text(&ctx, &pl("Other")), "added recently");
    let hurt = pl_status("Skinner", "ir");
    ctx.recent_adds.insert("Skinner".into(), d(2026, 9, 30));
    assert!(!churn_blocked(&ctx, &hurt, 0.5));
}

// ------------------------------------------------------------------------------------ base: dynasty

fn keeper_fixture() -> (LeagueContext, Vec<Player>, Map<String, DynastyValue>) {
    let ps: Vec<Player> = ["a", "b", "c", "d"].iter().map(|c| pl(c)).collect();
    let mut dyn_ = Map::new();
    for (p, (sp, sur)) in ps.iter().zip([(2.0, 5.0), (2.0, 3.0), (2.0, 1.0), (1.0, 4.0)]) {
        dyn_.insert(p.cid.clone(), dv(p, sp + sur, sp, sur, true));
    }
    (mk_ctx(vec![], json!({"dynasty": true, "keepers_per_team": 2})), ps, dyn_)
}

#[test]
fn keeper_leagues_change_math() {
    let (ctx, ps, dyn_) = keeper_fixture();
    assert_eq!(keeper_k(&ctx, Some(&dyn_)), Some(2));
    assert_eq!(keeper_k(&ctx, None), Some(2));
    assert_eq!(keeper_k(&ctx, Some(&Map::new())), None);
    let (a, b, c, dd) = (&ps[0], &ps[1], &ps[2], &ps[3]);
    let ks = keeper_set([a, b, c, dd], &dyn_, 2);
    assert_eq!(ks.iter().map(|p| p.cid.as_str()).collect::<Vec<_>>(), vec!["a", "d"]);
    let ch = dynasty_change(&ctx, Some(&dyn_), [a, b, c], [a, b, c, dd]);
    assert!(approx(ch.season, 1.0, 1e-12) && approx(ch.keepers, 1.0, 1e-12) && approx(ch.total, 2.0, 1e-12));
    assert_eq!(ch.k, Some(2));
    assert_eq!(keepers_text(&ch), "d replaces b as a keeper");
    assert!(approx(dynasty_delta(&ctx, Some(&dyn_), [a, b, c], [a, b, c, dd]), 2.0, 1e-12));
    // dropping a non-keeper only costs his season part
    let ch2 = dynasty_change(&ctx, Some(&dyn_), [a, b, c], [a, b]);
    assert!(approx(ch2.total, -2.0, 1e-12) && ch2.kept_in.is_empty() && ch2.kept_out.is_empty());
    assert_eq!(keepers_text(&ch2), "keepers unchanged");
    // losing a keeper
    let ch3 = dynasty_change(&ctx, Some(&dyn_), [a, b], [b]);
    assert_eq!(keepers_text(&ch3), "a is no longer a keeper");
    // swap_values: add - drop is the swap's exact delta
    let (av, dvv) = swap_values(&ctx, Some(&dyn_), [a, b, c], Some(dd), Some(c));
    assert!(approx(av, 2.0, 1e-12) && approx(dvv, 2.0, 1e-12));
    let exact = dynasty_delta(&ctx, Some(&dyn_), [a, b, c], [a, b, dd]);
    assert!(approx(av - dvv, exact, 1e-12));
}

#[test]
fn keep_everyone_changes_are_additive() {
    let (mut ctx, ps, dyn_) = keeper_fixture();
    ctx.keepers_per_team = None;
    assert_eq!(keeper_k(&ctx, Some(&dyn_)), None);
    let (a, b, c, dd) = (&ps[0], &ps[1], &ps[2], &ps[3]);
    let ch = dynasty_change(&ctx, Some(&dyn_), [a, b], [b, c, dd]);
    assert!(approx(ch.total, (3.0 + 5.0) - 7.0, 1e-12) && ch.k.is_none());
    assert_eq!(swap_values(&ctx, Some(&dyn_), [a, b], Some(dd), Some(a)), (5.0, 7.0));
    assert_eq!(dynasty_change(&ctx, None, [a], [b]).total, 0.0);
}

#[test]
fn dynasty_maps_and_ages() {
    let (ctx, ps, dyn_) = keeper_fixture();
    let f = dynasty_floats(Some(&dyn_)).unwrap();
    assert_eq!(f["a"], 7.0);
    assert!(dynasty_floats(Some(&Map::new())).is_none() && dynasty_floats(None).is_none());
    let values: Map<String, PlayerValue> = Map::new();
    let non_dyn = mk_ctx(vec![], json!({}));
    assert!(dynasty_objects(&non_dyn, &values, Some(&dyn_)).is_none());
    assert_eq!(dynasty_objects(&ctx, &values, Some(&dyn_)).unwrap().len(), 4);
    let young = pl_json("y", json!({"birth_date": "2005-10-01"}));
    let age = player_age(&young, &ctx, None).unwrap();
    assert!(approx(age, 21.0, 0.01));
    assert!(player_age(&ps[0], &ctx, None).is_none());
}

#[test]
fn protection_rules() {
    let (ctx, _ps, mut dyn_) = keeper_fixture();
    let pick = pl_json("pk", json!({"draft_overall": 10, "pct_owned": 5.0}));
    assert_eq!(protection_reason(&pick, Some(21.4)).unwrap(), "pk is 21 and a #10 overall pick");
    assert!(protection_reason(&pick, Some(24.0)).is_none() && protection_reason(&pick, None).is_none());
    let owned = pl_json("ow", json!({"pct_owned": 85.0}));
    assert_eq!(protection_reason(&owned, Some(22.0)).unwrap(), "ow is 22 and rostered in 85% of leagues");
    let unproven = pl_json("un", json!({"pct_owned": 65.0, "career_gp": 40}));
    assert_eq!(protection_reason(&unproven, Some(20.0)).unwrap(), "un is 20, 40 NHL GP and rostered in 65% of leagues");
    let vet = pl_json("vt", json!({"pct_owned": 65.0, "career_gp": 400}));
    assert!(protection_reason(&vet, Some(20.0)).is_none());
    // keeper leagues: only for a keeper on his team
    let mut pk = pick.clone();
    pk.birth_date = Some(d(2005, 10, 1));
    dyn_.insert("pk".into(), dv(&pk, 5.0, 1.0, 4.0, true));
    assert!(protected_reason(&pk, &ctx, Some(&dyn_), None).unwrap().ends_with(", a top-2 keeper"));
    dyn_.insert("pk".into(), dv(&pk, 5.0, 1.0, 0.0, false));
    assert!(protected_reason(&pk, &ctx, Some(&dyn_), None).is_none());
    let mut one = ctx.clone();
    one.keepers_per_team = Some(1);
    dyn_.insert("pk".into(), dv(&pk, 5.0, 1.0, 4.0, true));
    assert!(protected_reason(&pk, &one, Some(&dyn_), None).unwrap().ends_with(", his team's keeper"));
    // a player without a dynasty value keeps the plain rule
    let mut nb = pl_json("nb", json!({"draft_overall": 10}));
    nb.birth_date = Some(d(2005, 10, 1));
    assert_eq!(protected_reason(&nb, &ctx, Some(&dyn_), None).unwrap(), "nb is 21 and a #10 overall pick, a top-2 keeper");
}

// ------------------------------------------------------------------------------------ base: roster

#[test]
fn positions_and_roster_legality() {
    let gs: Vec<Player> = (0..2).map(|i| pl_json(&format!("g{i}"), json!({"positions": ["G"]}))).collect();
    let c = pl_json("c0", json!({"positions": ["C"]}));
    let w = pl_json("w0", json!({"positions": ["LW"]}));
    let mut slots: Vec<Value> = gs.iter().map(|g| slot("G", g, true)).collect();
    slots.push(slot("C", &c, true));
    slots.push(slot("IR", &w, false));
    let ctx = mk_ctx(slots, json!({"position_limits": {"G": 2, "F": 3}, "max_roster_size": 3}));
    let team = ctx.my_team().unwrap();
    let pc = position_counts(team, &ctx);
    assert_eq!((pc["G"], pc["F"]), (2, 2));
    let g_new = pl_json("g9", json!({"positions": ["G"]}));
    let f_new = pl_json("f9", json!({"positions": ["RW"]}));
    assert_eq!(position_room(team, &ctx, &g_new), 0);
    assert_eq!(position_room(team, &ctx, &f_new), 1);
    assert_eq!(position_room(team, &ctx, &pl_json("d9", json!({"positions": ["D"]}))), NO_LIMIT);
    assert_eq!(position_room_for(team, &ctx, &["C", "G"]), 0);
    assert_eq!(capped_positions(team, &ctx, &g_new), vec!["G"]);
    assert_eq!(cap_text(&ctx, "G", None), "G limit 2");
    assert_eq!(cap_text(&ctx, "G", Some(3)), "G limit 2: would roster 3 goalies");
    assert_eq!(roster_legal_after(team, &ctx, &[&g_new], &[], &[]), (false, Some("G limit 2: would roster 3 goalies".to_string())));
    // dropping a goalie makes room
    assert_eq!(roster_legal_after(team, &ctx, &[&g_new], &[&gs[0]], &[]), (true, None));
    // roster size 3 excludes IR: 3 counted now; adding a skater fails, unless a player leaves
    assert_eq!(
        roster_legal_after(team, &ctx, &[&f_new], &[], &[]),
        (false, Some("roster limit 3: would roster 4 players".to_string()))
    );
    assert_eq!(roster_legal_after(team, &ctx, &[&f_new], &[&c], &[]), (true, None));
    assert_eq!(roster_legal_after(team, &ctx, &[&f_new], &[], &[&c]), (true, None));
    assert_eq!(position_limits_text(&ctx).unwrap(), "max F 3, G 2 (incl. IR); roster max 3 (excl. IR)");
    assert!(position_limits_text(&mk_ctx(vec![], json!({}))).is_none());
    // F key matters for a "F" position player
    assert_eq!(limit_positions(&c, &ctx.position_limits), vec!["F"]);
    let prim = pl_json("pp", json!({"positions": ["C", "LW"], "primary_position": "LW"}));
    assert_eq!(limit_positions(&prim, &ctx.position_limits), vec!["F"]);
}

#[test]
fn ir_and_roster_spots() {
    let hurt = pl_status("h", "ltir");
    let hurt2 = pl_status("h2", "ir");
    let ok = pl("ok");
    let ctx = mk_ctx(
        vec![slot("C", &hurt2, true), slot("BN", &ok, false), slot("BN", &hurt, false), slot("IR", &pl_status("z", "ir"), false)],
        json!({"roster_shape": {"C": 2, "BN": 3, "IR": 2}}),
    );
    let team = ctx.my_team().unwrap();
    assert_eq!(free_ir_slots(team, &ctx.roster_shape), 1);
    assert_eq!(open_roster_spots(team, &ctx.roster_shape), 2);
    // The shape's slots (C 2 + BN 3 = 5, 3 used) leave 2 open, but a league maximum of 4 leaves 1.
    let mut capped = ctx.clone();
    capped.max_roster_size = Some(4);
    assert_eq!(open_spots_under_cap(team, &ctx), 2);
    assert_eq!(open_spots_under_cap(capped.my_team().unwrap(), &capped), 1);
    // At the maximum the roster reads as full (the IR-activation checks rely on this) though the shape has room.
    capped.max_roster_size = Some(3);
    assert_eq!(open_spots_under_cap(capped.my_team().unwrap(), &capped), 0);
    let mut values = Map::new();
    values.insert("h".to_string(), pv(&hurt, 2.0, vec![]));
    values.insert("h2".to_string(), pv(&hurt2, 3.0, vec![]));
    let mv = ir_movable(team, Some(&values));
    assert_eq!(mv.iter().map(|p| p.cid.as_str()).collect::<Vec<_>>(), vec!["h", "h2"]); // ltir first
    assert_eq!(droppable_players(team).len(), 3);
    assert!(has_data(Some(&values["h"])) && !has_data(None));
    assert!(!has_data(Some(&pv(&ok, 1.0, vec![rs("NO_DATA", "", None)]))));
    assert!(shares_default_slot(&pl("a"), &pl("b")));
    assert!(!shares_default_slot(&pl("a"), &pl_json("g", json!({"positions": ["G"]}))));
}

#[test]
fn confidence_formula() {
    let p = pl_json("v", json!({"lines": {
        "season": {"split": "season", "gp": 40, "stats": {}}, "prior": {"split": "prior", "gp": 82, "stats": {}},
        "prior2": {"split": "prior2", "gp": 82, "stats": {}}, "prior3": {"split": "prior3", "gp": 50, "stats": {}}}}));
    let e = 40.0 + 82.0 + 0.8 * 82.0 + 0.6 * 50.0;
    assert!(approx(evidence_games(&p), e, 1e-9));
    assert!(approx(confidence(&p), 0.5 + 0.5 * e / (e + 40.0), 1e-12));
    assert_eq!(confidence(&pl("none")), 0.5);
}

// ------------------------------------------------------------------------------------ base: game rate

#[test]
fn game_rate_schedule_and_default() {
    let sched: Vec<NaiveDate> = [5, 6, 8, 10, 12].iter().map(|x| d(2026, 10, *x)).collect();
    let me = pl_json("m", json!({"team": "AAA"}));
    let nosched = pl_json("n", json!({"team": "ZZZ"}));
    let mut ctx = mk_ctx(vec![slot("C", &me, true), slot("C", &nosched, true)], json!({"as_of": "2026-10-05"}));
    ctx.schedule.insert("AAA".into(), sched);
    let r = game_rate(&ctx, None).unwrap();
    assert_eq!(r.source, "schedule");
    assert!(approx(r.remaining, 5.0, 1e-12) && approx(r.per_week, 5.0 / (8.0 / 7.0), 1e-12));
    assert_eq!(gain_units_text(0.66, &r), "+0.66/g \u{b7} +2.9/wk \u{b7} +3/season");
    assert!(approx(gain_week(1.0, &r), r.per_week, 1e-12) && approx(gain_season(1.0, &r), 5.0, 1e-12));
    ctx.as_of = d(2026, 10, 20);
    assert_eq!(game_rate(&ctx, None).unwrap(), GameRate { per_week: 0.0, remaining: 0.0, source: "season over" });
    // no schedule: defaults
    let ctx2 = mk_ctx(vec![slot("C", &me, true)], json!({"as_of": "2026-10-01"}));
    let r2 = game_rate(&ctx2, None).unwrap();
    assert_eq!((r2.per_week, r2.remaining, r2.source), (3.4, 82.0, "default"));
    assert_eq!(season_bounds(&ctx2), (d(2026, 10, 7), d(2027, 4, 16)));
    // a schedule without my starters' teams: its games per team (5) x the share of its days left
    // (Oct 9..12 of Oct 5..12 = 4 of 8)
    ctx.as_of = d(2026, 10, 9);
    let r3 = game_rate(&ctx, Some(&[&nosched])).unwrap();
    assert_eq!((r3.per_week, r3.remaining, r3.source), (3.4, 2.5, "default"));
    // no team of mine
    let mut none = ctx2.clone();
    none.teams[0].owner_is_me = false;
    assert!(game_rate(&none, None).is_err());
    assert!(game_rate(&none, Some(&[&me])).is_ok());
}

// ------------------------------------------------------------------------------------ schedule grid

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn schedule() -> Vec<(&'static str, Vec<NaiveDate>)> {
    let ds = |xs: &[u32]| xs.iter().map(|x| d(2026, 10, *x)).collect::<Vec<_>>();
    vec![("AAA", ds(&[5, 6, 8, 10, 12])), ("BBB", ds(&[6, 10, 11, 12])), ("CCC", ds(&[7, 8, 9]))]
}

fn gpd() -> Vec<(NaiveDate, i64)> {
    [(5, 3), (6, 12), (7, 9), (8, 4), (9, 10), (10, 13), (11, 5), (12, 5)].iter().map(|(x, n)| (d(2026, 10, *x), *n)).collect()
}

fn sg_player(cid: &str, team: &str, pos: &[&str], status: &str) -> Player {
    pl_json(cid, json!({"team": team, "positions": pos, "status": status, "name": cid.to_uppercase()}))
}

fn make_ctx(provider: &str, fas: Vec<Player>, as_of: NaiveDate) -> LeagueContext {
    let mine = sg_player("mine", "AAA", &["C"], "healthy");
    let mut ctx = mk_ctx(
        vec![slot("C", &mine, true)],
        json!({"provider": provider, "roster_shape": {"C": 1}, "free_agents": fas, "as_of": as_of.to_string(), "season_start": "2026-10-05"}),
    );
    for (t, ds) in schedule() {
        let opps: Map<NaiveDate, String> =
            ds.iter().enumerate().map(|(i, dt)| (*dt, format!("{}XXX", if i % 2 == 1 { "@" } else { "" }))).collect();
        ctx.schedule.insert(t.to_string(), ds);
        ctx.opponents.insert(t.to_string(), opps);
    }
    ctx.games_per_day = gpd().into_iter().collect();
    ctx
}

fn sg_pv(p: &Player, fpg: f64, share: Option<f64>) -> PlayerValue {
    let mut v = pv(p, fpg, vec![]);
    v.start_share = share;
    v
}

#[test]
fn sg_window_counts() {
    let g: Map<NaiveDate, i64> = gpd().into_iter().collect();
    let s = schedule();
    assert_eq!(sg::window_counts(&s[0].1, &g, d(2026, 10, 5), d(2026, 10, 11), 8), (4, 2, 1));
    assert_eq!(sg::window_counts(&s[2].1, &g, d(2026, 10, 5), d(2026, 10, 11), 8), (3, 1, 2));
    assert_eq!(sg::window_counts(&s[1].1, &g, d(2026, 10, 12), d(2026, 10, 18), 8), (1, 1, 1));
}

#[test]
fn sg_season_grid_weeks_and_totals() {
    let ctx = make_ctx("test", vec![], d(2026, 10, 5));
    let grid = sg::season_grid(&ctx, None, None, 8);
    assert_eq!(grid.weeks, vec![d(2026, 10, 5), d(2026, 10, 12)]);
    assert_eq!(grid.week_ends, vec![d(2026, 10, 11), d(2026, 10, 12)]);
    let (a, b, c) = (grid.team("AAA").unwrap(), grid.team("BBB").unwrap(), grid.team("CCC").unwrap());
    assert_eq!((&a.games, &a.offnights, &a.b2b), (&vec![4, 1], &vec![2, 1], &vec![1, 0]));
    assert_eq!((&b.games, &b.offnights, &b.b2b), (&vec![3, 1], &vec![1, 1], &vec![1, 1]));
    assert_eq!((&c.games, &c.offnights, &c.b2b), (&vec![3, 0], &vec![1, 0], &vec![2, 0]));
    assert!(a.total_games == 5 && c.total_b2b == 2);
    assert_eq!(grid.league_games, vec![56, 5]);
    assert_eq!(grid.offnight_days, vec![3, 1]);
    assert_eq!(grid.week_index(d(2026, 10, 9)), Some(0));
    assert_eq!(grid.week_index(d(2027, 1, 1)), None);
    assert_eq!(sg::team_week_counts(&ctx, 8), grid);
    let part = sg::season_grid(&ctx, Some(d(2026, 10, 8)), Some(d(2026, 10, 9)), 8);
    assert_eq!(part.team("CCC").unwrap().games, vec![2]);
    assert_eq!(part.team("AAA").unwrap().games, vec![1]);
    let mut empty = ctx.clone();
    empty.schedule.clear();
    empty.games_per_day.clear();
    assert!(sg::season_grid(&empty, None, None, 8).teams.is_empty());
}

#[test]
fn sg_week_rows_matrix() {
    let ctx = make_ctx("test", vec![], d(2026, 10, 5));
    let view = sg::week_rows(&ctx, d(2026, 10, 8), 8, 7);
    assert_eq!((view.start, view.end, view.days.len()), (d(2026, 10, 5), d(2026, 10, 11), 7));
    assert_eq!(view.off_days, vec![true, false, false, true, false, false, true]);
    assert_eq!(view.league_games[0], 3);
    assert_eq!(view.rows.iter().map(|r| r.team.as_str()).collect::<Vec<_>>(), vec!["AAA", "BBB", "CCC"]);
    let aaa = &view.rows[0];
    assert_eq!((aaa.games, aaa.offnights, aaa.b2b), (4, 2, 1));
    let c0 = aaa.cells[0].as_ref().unwrap();
    assert!(c0.opp.as_deref() == Some("XXX") && c0.off && !c0.b2b);
    assert!(aaa.cells[1].as_ref().unwrap().b2b && aaa.cells[2].is_none());
    assert_eq!(view.label(), "Oct 05 - Oct 11");
    assert_eq!(sg::default_week(&make_ctx("test", vec![], d(2026, 10, 9))), d(2026, 10, 5));
    assert_eq!(sg::default_week(&make_ctx("test", vec![], d(2026, 9, 20))), d(2026, 10, 5));
}

fn streaming_fixture() -> (LeagueContext, Map<String, PlayerValue>) {
    let fas = vec![
        sg_player("fa1", "AAA", &["C"], "healthy"),
        sg_player("fa2", "BBB", &["LW"], "healthy"),
        sg_player("fa3", "CCC", &["D"], "healthy"),
        sg_player("fa4", "AAA", &["G"], "healthy"),
        sg_player("hurt", "AAA", &["C"], "out"),
        sg_player("nogames", "ZZZ", &["C"], "healthy"),
    ];
    let mut values = Map::new();
    for (p, f, sh) in [(&fas[0], 2.0, None), (&fas[1], 2.5, None), (&fas[2], 1.0, None), (&fas[3], 4.0, Some(0.5)), (&fas[4], 9.0, None), (&fas[5], 9.0, None)] {
        values.insert(p.cid.clone(), sg_pv(p, f, sh));
    }
    (make_ctx("test", fas, d(2026, 10, 5)), values)
}

#[test]
fn sg_streaming_targets_rank_by_games_and_offnights() {
    let (ctx, values) = streaming_fixture();
    let bonus = sg::offnight_bonus();
    let opts = sg::StreamingOpts { today: Some(d(2026, 10, 5)), ..Default::default() };
    let plan = sg::streaming_targets(&ctx, &values, d(2026, 10, 5), &opts);
    let f = &plan.by_slot["F"];
    assert_eq!(f.iter().map(|t| t.cid.as_str()).collect::<Vec<_>>(), vec!["fa1", "fa2"]);
    assert!(f[0].games == 4 && f[0].offnights == 2);
    assert!(approx(f[0].proj, 2.0 * 4.0 * (1.0 + bonus * 2.0), 1e-9));
    assert!(approx(f[1].proj, 2.5 * 3.0 * (1.0 + bonus), 1e-9));
    assert_eq!(plan.by_slot["D"][0].cid, "fa3");
    let g = &plan.by_slot["G"][0];
    assert!(approx(g.per_game, 2.0, 1e-9) && approx(g.proj, 2.0 * 4.0 * (1.0 + bonus * 2.0), 1e-9));
    assert_eq!(plan.teams.iter().map(|r| r.team.as_str()).collect::<Vec<_>>(), vec!["AAA", "BBB", "CCC"]);
    assert_eq!(plan.teams[0].free_agents, Some(2));
    assert_eq!(f[0].opps, vec!["XXX", "@XXX", "XXX", "@XXX"]);
    let late_opts = sg::StreamingOpts { today: Some(d(2026, 10, 9)), ..Default::default() };
    let late = sg::streaming_targets(&ctx, &values, d(2026, 10, 5), &late_opts);
    assert_eq!(late.from_day, d(2026, 10, 9));
    assert_eq!(late.by_slot["F"].iter().map(|t| t.cid.as_str()).collect::<Vec<_>>(), vec!["fa2", "fa1"]);
    assert_eq!(late.by_slot["F"][0].games, 2);
    assert_eq!(late.teams[0].team, "BBB");
    let one = sg::StreamingOpts { today: Some(d(2026, 10, 5)), slots: &["F"], limit: 1, ..Default::default() };
    let p1 = sg::streaming_targets(&ctx, &values, d(2026, 10, 5), &one);
    assert_eq!(p1.by_slot.len(), 1);
    assert_eq!(p1.by_slot["F"], vec![plan.by_slot["F"][0].clone()]);
}

#[test]
fn sg_streaming_marks_position_capped_pickups() {
    let (mut ctx, values) = streaming_fixture();
    ctx.free_agents.retain(|p| p.cid == "fa1" || p.cid == "fa4");
    for i in 0..3 {
        let g = sg_player(&format!("g{i}"), "BBB", &["G"], "healthy");
        ctx.teams[0].slots.push(serde_json::from_value(slot("BN", &g, false)).unwrap());
    }
    let opts = sg::StreamingOpts { today: Some(d(2026, 10, 5)), ..Default::default() };
    let plan = sg::streaming_targets(&ctx, &values, d(2026, 10, 5), &opts);
    assert!(plan.by_slot["G"][0].needs_drop.is_none());
    ctx.position_limits.insert("G".into(), 3);
    let plan = sg::streaming_targets(&ctx, &values, d(2026, 10, 5), &opts);
    assert_eq!(plan.by_slot["G"][0].needs_drop.as_deref(), Some("G limit 3 reached: drop a G"));
    assert!(plan.by_slot["F"][0].needs_drop.is_none());
    ctx.position_limits.insert("G".into(), 4);
    let plan = sg::streaming_targets(&ctx, &values, d(2026, 10, 5), &opts);
    assert!(plan.by_slot["G"][0].needs_drop.is_none());
}

#[test]
fn sg_streaming_targets_carry_the_moves_note() {
    let (mut ctx, values) = streaming_fixture();
    ctx.free_agents.retain(|p| p.cid == "fa1" || p.cid == "fa4");
    let opts = sg::StreamingOpts { today: Some(d(2026, 10, 5)), ..Default::default() };
    let plan = sg::streaming_targets(&ctx, &values, d(2026, 10, 5), &opts);
    assert!(plan.moves_note.is_none() && plan.by_slot["F"][0].note.is_none());
    ctx.moves_limit_per_period = Some(2);
    ctx.moves_used_this_period = Some(1);
    ctx.period_start = Some(d(2026, 10, 5));
    ctx.period_end = Some(d(2026, 10, 11));
    let plan = sg::streaming_targets(&ctx, &values, d(2026, 10, 5), &opts);
    let t = &plan.by_slot["F"][0];
    assert!(t.note.as_deref() == Some("1 of 2 moves left this week") && t.needs_drop.is_none());
    for i in 0..3 {
        let g = sg_player(&format!("g{i}"), "BBB", &["G"], "healthy");
        ctx.teams[0].slots.push(serde_json::from_value(slot("BN", &g, false)).unwrap());
    }
    ctx.position_limits.insert("G".into(), 3);
    let plan = sg::streaming_targets(&ctx, &values, d(2026, 10, 5), &opts);
    assert_eq!(plan.by_slot["G"][0].needs_drop.as_deref(), Some("G limit 3 reached: drop a G; 1 of 2 moves left this week"));
    ctx.moves_used_this_period = Some(2);
    let plan = sg::streaming_targets(&ctx, &values, d(2026, 10, 5), &opts);
    assert_eq!(plan.by_slot["F"][0].needs_drop.as_deref(), Some("no moves left this week (2/2 used)"));
}

#[test]
fn sg_per_game_value_from_week_projection() {
    let p = pl("x");
    let mut v = pv(&p, 3.0, vec![]);
    v.proj_week = Some(12.0 * (1.0 + sg::offnight_bonus()));
    v.games_next7 = Some(4);
    v.offnight_next7 = Some(1);
    assert!(approx(sg::per_game_value(Some(&v)).unwrap(), 3.0, 1e-9));
    assert!(sg::per_game_value(None).is_none());
    assert!(approx(sg::per_game_value(Some(&sg_pv(&p, 4.0, Some(0.5)))).unwrap(), 2.0, 1e-12));
}

#[test]
fn sg_parse_fantrax_periods() {
    assert_eq!(sg::parse_fantrax_period("(Mar 22/27 - Mar 28/27)"), Some((d(2027, 3, 22), d(2027, 3, 28))));
    assert_eq!(sg::parse_fantrax_period("Full Season"), None);
    let ps = sg::fantrax_periods(&[
        json!({"name": "Full Season", "value": 9999}),
        json!({"name": "(Oct 12/26 - Oct 12/26)", "value": 2}),
        json!({"name": "(Oct 5/26 - Oct 11/26)", "value": "1"}),
        json!({"name": "(Oct 19/26 - Oct 25/26)", "value": "x"}),
    ]);
    let got: Vec<(i64, NaiveDate, NaiveDate)> = ps.iter().map(|p| (p.number, p.start, p.end)).collect();
    assert_eq!(got, vec![(1, d(2026, 10, 5), d(2026, 10, 11)), (2, d(2026, 10, 12), d(2026, 10, 12))]);
}

struct FakeFantrax {
    extras: ProviderExtras,
}

impl FakeFantrax {
    fn new(start: Option<i64>) -> Self {
        let mut rules = LeagueRules::default();
        if let Some(s) = start {
            let mut sect = Map::new();
            sect.insert(sg::FANTRAX_PLAYOFF_LABEL.to_string(), s.to_string());
            rules.settings.insert("Playoffs".to_string(), sect);
        }
        FakeFantrax { extras: ProviderExtras { rules: Some(rules), ..Default::default() } }
    }
}

impl LeagueProvider for FakeFantrax {
    fn name(&self) -> &str {
        "fantrax"
    }
    fn load(&mut self) -> anyhow::Result<LeagueContext> {
        anyhow::bail!("not used")
    }
    fn extras(&self) -> &ProviderExtras {
        &self.extras
    }
    fn fantrax_call(&self, calls: &[(String, Map<String, Value>)], _fresh: bool) -> anyhow::Result<Vec<Value>> {
        assert_eq!(calls[0].0, "getTeamRosterInfo");
        assert_eq!(calls[0].1["view"], "GAMES_PER_POS");
        Ok(vec![json!({"displayedLists": {"scoringPeriodList": [
            {"name": "Full Season", "value": 9999}, {"name": "(Oct 5/26 - Oct 11/26)", "value": 1},
            {"name": "(Oct 12/26 - Oct 12/26)", "value": 2}]}})])
    }
}

struct FakeEspn {
    extras: ProviderExtras,
}

impl LeagueProvider for FakeEspn {
    fn name(&self) -> &str {
        "espn"
    }
    fn load(&mut self) -> anyhow::Result<LeagueContext> {
        anyhow::bail!("not used")
    }
    fn extras(&self) -> &ProviderExtras {
        &self.extras
    }
}

#[test]
fn sg_playoff_weeks_fantrax_rules() {
    let _g = lock();
    std::env::remove_var(sg::PLAYOFF_ENV);
    let ctx = make_ctx("fantrax", vec![], d(2026, 10, 5));
    let po = sg::playoff_weeks(&ctx, Some(&FakeFantrax::new(Some(2))), None, 8);
    assert_eq!(po.playoff_start, Some(2));
    assert_eq!(po.periods.iter().map(|p| p.number).collect::<Vec<_>>(), vec![2]);
    assert_eq!(po.source, "Fantrax scoring periods");
    assert_eq!(po.teams.iter().map(|t| (t.team.as_str(), t.total)).collect::<Vec<_>>(), vec![("AAA", 1), ("BBB", 1), ("CCC", 0)]);
    assert_eq!(po.teams[1].total_b2b, 1);
    assert!(approx(po.avg_total, 2.0 / 3.0, 1e-12));
    assert_eq!(po.notes, vec!["Playoffs start period 2 (Fantrax Rules page)"]);
    let cal = sg::league_calendar(&ctx, Some(&FakeFantrax::new(Some(1))));
    assert_eq!(cal.current(d(2026, 10, 12)).unwrap().number, 2);
    assert_eq!(cal.current(d(2026, 9, 1)).unwrap().number, 1);
    assert!(cal.periods.iter().all(|p| p.playoffs));
    assert_eq!(cal.get(2).unwrap().days(), 1);
    assert_eq!(cal.periods[0].label(), "Oct 05 - Oct 11");
}

#[test]
fn sg_playoff_start_default_and_env() {
    let _g = lock();
    std::env::remove_var(sg::PLAYOFF_ENV);
    let prov = FakeFantrax::new(None);
    assert_eq!(sg::fantrax_playoff_start(&prov), (Some(25), "default for this league (period 25)".to_string()));
    std::env::set_var(sg::PLAYOFF_ENV, "2");
    assert_eq!(sg::fantrax_playoff_start(&prov).0, Some(2));
    std::env::remove_var(sg::PLAYOFF_ENV);
}

#[test]
fn sg_playoff_weeks_espn_settings() {
    let _g = lock();
    std::env::remove_var(sg::PLAYOFF_ENV);
    let mut mp = Map::new();
    mp.insert("1".to_string(), vec![1]);
    mp.insert("2".to_string(), vec![2]);
    let meta = EspnLeagueMeta {
        current_week: Some(1),
        final_scoring_period: Some(8),
        settings: EspnLeagueSettings { matchup_periods: mp, reg_season_count: Some(1), ..Default::default() },
        ..Default::default()
    };
    let prov = FakeEspn { extras: ProviderExtras { espn_league: Some(meta), ..Default::default() } };
    let ctx = make_ctx("espn", vec![], d(2026, 10, 5));
    let cal = sg::league_calendar(&ctx, Some(&prov));
    assert_eq!((cal.source.as_str(), cal.playoff_start), ("ESPN matchup periods", Some(2)));
    let got: Vec<(i64, NaiveDate, NaiveDate, bool)> = cal.periods.iter().map(|p| (p.number, p.start, p.end, p.playoffs)).collect();
    assert_eq!(got, vec![(1, d(2026, 10, 5), d(2026, 10, 11), false), (2, d(2026, 10, 12), d(2026, 10, 12), true)]);
    assert_eq!(cal.notes, vec!["ESPN: 2 matchup periods, playoffs start period 2"]);
    let po = sg::playoff_weeks(&ctx, Some(&prov), None, 8);
    assert_eq!(po.periods.iter().map(|p| p.number).collect::<Vec<_>>(), vec![2]);
    assert_eq!(po.teams[0].total, 1);
}

#[test]
fn sg_calendar_failure_falls_back_to_weeks() {
    let _g = lock();
    std::env::remove_var(sg::PLAYOFF_ENV);
    let ctx = make_ctx("espn", vec![], d(2026, 10, 5));
    let prov = FakeEspn { extras: ProviderExtras::default() };
    let cal = sg::league_calendar(&ctx, Some(&prov));
    assert!(cal.source.starts_with("NHL calendar weeks"));
    assert!(cal.notes[0].starts_with("League periods unavailable (LookupError: "), "{:?}", cal.notes);
    assert_eq!(cal.periods.len(), 2);
    assert_eq!(cal.playoff_start, Some(1)); // last 3 weeks -> everything here
    assert_eq!(cal.notes[1], "Playoffs start period 1 (assumed: last 3 weeks)");
    let none = sg::league_calendar(&ctx, None);
    assert_eq!(none.notes, vec!["Playoffs start period 1 (assumed: last 3 weeks)"]);
}

#[test]
fn sg_calendar_weeks_merges_break_and_opening_week() {
    let start = d(2026, 9, 29);
    let days: Vec<NaiveDate> = (0..34).map(|i| start + Duration::days(i)).collect();
    let mut gpd: Map<NaiveDate, i64> = days.iter().map(|x| (*x, 10)).collect();
    for i in 0..7 {
        gpd.insert(d(2026, 10, 12) + Duration::days(i), 1);
    }
    let mut ctx = make_ctx("test", vec![], d(2026, 10, 5));
    ctx.schedule.clear();
    ctx.schedule.insert("AAA".into(), days);
    ctx.games_per_day = gpd;
    assert_eq!(
        sg::calendar_weeks(&ctx, None, None, None),
        vec![(d(2026, 9, 29), d(2026, 10, 4)), (d(2026, 10, 5), d(2026, 10, 11)), (d(2026, 10, 12), d(2026, 10, 25)), (d(2026, 10, 26), d(2026, 11, 1))]
    );
    assert_eq!(
        sg::calendar_weeks(&ctx, Some(3), None, None),
        vec![(d(2026, 9, 29), d(2026, 10, 11)), (d(2026, 10, 12), d(2026, 10, 25)), (d(2026, 10, 26), d(2026, 11, 1))]
    );
}

// ------------------------------------------------------------------------------------ parity

fn fixtures_root() -> PathBuf {
    match std::env::var("FM_PARITY_FIXTURES") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures"),
    }
}

/// Recompute `strength_for` / `headline_for` for every recommendation stored in the engine
/// fixtures and compare. Alerts and "Hold ..." injury recs carry a strength their engine sets
/// directly (alerts.py / flags.py / injuries.py), and role/line alerts are never run through
/// `apply_ranks`, so those are exempt.
#[test]
fn parity_strength_and_headline_for_engine_recs() {
    let (mut checked_s, mut checked_h) = (0usize, 0usize);
    let mut fails: Vec<String> = Vec::new();
    for league in ["espn", "fantrax"] {
        let dir = fixtures_root().join(league).join("engines");
        let Ok(rd) = std::fs::read_dir(&dir) else {
            eprintln!("SKIP {league}: no engines fixtures at {}", dir.display());
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let text = std::fs::read_to_string(e.path()).unwrap();
            let v: Value = serde_json::from_str(&text).unwrap();
            let Some(arr) = v.as_array() else { continue };
            for r in arr {
                let Ok(rec) = serde_json::from_value::<Recommendation>(r.clone()) else { continue };
                if rec.kind == RecKind::Alert {
                    continue;
                }
                if let Some(s) = rec.strength {
                    if !rec.title.starts_with("Hold ") {
                        checked_s += 1;
                        let mine = strength_for(&rec);
                        if (mine - s).abs() > 1e-9 {
                            fails.push(format!("{league}/{name} strength {:?} {}: stored {s} mine {mine}", rec.kind, rec.title));
                        }
                    }
                }
                if rec.headline.is_some() {
                    let mut r2 = rec.clone();
                    r2.headline = None;
                    let mine = headline_for(&r2);
                    // engines may set a sharper headline themselves (daily lineup swaps, injuries)
                    let engine_set = matches!(rec.kind, RecKind::Injury) || (rec.kind == RecKind::Lineup && rec.reasons.iter().any(|x| x.code == "DAY_VALUE"));
                    if !engine_set {
                        checked_h += 1;
                        if mine != rec.headline {
                            fails.push(format!("{league}/{name} headline {:?}: stored {:?} mine {:?}", rec.kind, rec.headline, mine));
                        }
                    }
                }
            }
        }
    }
    eprintln!("strength checked {checked_s}, headline checked {checked_h}");
    assert!(fails.is_empty(), "{} mismatches:\n{}", fails.len(), fails.iter().take(15).cloned().collect::<Vec<_>>().join("\n"));
}

/// A Fantrax provider whose official API answers; its private calls and Rules page must not be used.
struct OfficialFantrax {
    extras: ProviderExtras,
}

impl LeagueProvider for OfficialFantrax {
    fn name(&self) -> &str {
        "fantrax"
    }
    fn load(&mut self) -> anyhow::Result<LeagueContext> {
        anyhow::bail!("not used")
    }
    fn extras(&self) -> &ProviderExtras {
        &self.extras
    }
    fn official_calendar(&self) -> anyhow::Result<fm_core::provider::OfficialCalendar> {
        Ok(fm_core::provider::OfficialCalendar {
            periods: vec![(18, d(2027, 1, 18), d(2027, 1, 31)), (19, d(2027, 2, 1), d(2027, 2, 14)), (20, d(2027, 2, 15), d(2027, 2, 21))],
            playoff_start: Some(20),
        })
    }
}

#[test]
fn sg_fantrax_calendar_prefers_official_periods_and_playoff_start_but_env_wins() {
    let _g = lock();
    std::env::remove_var(sg::PLAYOFF_ENV);
    let ctx = make_ctx("fantrax", vec![], d(2027, 2, 1));
    let prov = OfficialFantrax { extras: ProviderExtras::default() };
    let cal = sg::fantrax_calendar(&ctx, &prov).unwrap();
    assert_eq!(cal.source, "Fantrax official scoring periods");
    assert_eq!(cal.get(19).unwrap().days(), 14);
    assert_eq!(cal.get(18).unwrap().days(), 14);
    assert_eq!(cal.playoff_start, Some(20));
    assert_eq!(cal.periods.iter().map(|p| p.playoffs).collect::<Vec<_>>(), vec![false, false, true]);
    assert_eq!(cal.notes, vec!["Playoffs start period 20 (Fantrax official API)"]);
    // The environment override stays ahead of the official value.
    std::env::set_var(sg::PLAYOFF_ENV, "19");
    assert_eq!(sg::fantrax_playoff_start(&prov), (Some(19), sg::PLAYOFF_ENV.to_string()));
    std::env::remove_var(sg::PLAYOFF_ENV);
}
