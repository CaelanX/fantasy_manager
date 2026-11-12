//! Ports of tests/test_scoring.py, test_categories.py, test_blend.py, test_replacement.py,
//! test_availability.py (parsing part), test_assist_shrink.py (math), test_goalie_gsax.py
//! (regression parts), test_params_layering.py + the snapshot params parity checks.

use std::sync::Mutex;

use chrono::{Duration, NaiveDate};
use fm_core::models::{Player, ScoringConfig, StatLine};
use fm_core::snapshot::{GoalieGsax, Snapshot};
use fm_core::values::Horizon;
use fm_core::Map;
use fm_engine::scoring::*;
use fm_engine::valuation::adjust::{self, *};
use fm_engine::valuation::blend::*;
use fm_engine::valuation::params as vp;
use fm_engine::valuation::regression::*;
use fm_engine::valuation::replacement::*;
use serde_json::{json, Value};

/// Tests touching the process-global params / env serialize on this.
static GLOBAL: Mutex<()> = Mutex::new(());

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}
macro_rules! assert_close {
    ($a:expr, $b:expr) => {{
        let (a, b): (f64, f64) = ($a, $b);
        assert!(close(a, b), "{} != {} ({}:{})", a, b, file!(), line!());
    }};
}

fn st(pairs: &[(&str, f64)]) -> Map<String, f64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}
fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, dd).unwrap()
}
fn line(split: &str, gp: i64, pairs: &[(&str, f64)]) -> StatLine {
    serde_json::from_value(json!({"split": split, "gp": gp, "stats": st(pairs)})).unwrap()
}
fn player(cid: &str, positions: &[&str]) -> Player {
    serde_json::from_value(json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": null,
                                  "positions": positions}))
    .unwrap()
}

// ----------------------------------------------------------------------------- scoring

#[test]
fn points_weighted_sum() {
    let s = PointsScoring::new(st(&[("G", 3.0), ("A", 2.0), ("SOG", 0.4), ("HIT", 0.3)]), None);
    assert_close!(s.value(&st(&[("G", 0.5), ("A", 1.0), ("SOG", 4.0), ("HIT", 2.0)])), 1.5 + 2.0 + 1.6 + 0.6);
    let s = PointsScoring::new(st(&[("G", 3.0), ("BLK", 0.5)]), None);
    assert_close!(s.value(&st(&[("G", 1.0), ("PIM", 10.0)])), 3.0);
    assert_eq!(s.value(&Map::new()), 0.0);
    let s = PointsScoring::new(st(&[("W", 4.0), ("GA", -2.0), ("SV", 0.2), ("SO", 3.0)]), None);
    assert_close!(s.value(&st(&[("W", 0.5), ("GA", 2.5), ("SV", 27.0), ("SO", 0.1)])), 2.0 - 5.0 + 5.4 + 0.3);
}

#[test]
fn goalie_weights_apply_to_goalie_lines_only() {
    let s = PointsScoring::new(st(&[("G", 3.0), ("SV", 0.1)]), Some(st(&[("SV", 0.2), ("W", 4.0)])));
    assert_close!(s.value(&st(&[("G", 1.0)])), 3.0);
    assert_close!(s.value(&st(&[("SV", 10.0), ("W", 1.0)])), 2.0 + 4.0);
    let b = s.breakdown(&st(&[("SV", 10.0), ("W", 1.0)]));
    assert_eq!(b.keys().collect::<Vec<_>>(), vec!["SV", "W"]);
}

#[test]
fn from_config_dispatch() {
    let cfg = |v: Value| -> ScoringConfig { serde_json::from_value(v).unwrap() };
    assert!(matches!(from_config(&cfg(json!({"kind": "points", "weights": {"G": 1.0}}))), Scoring::Points(_)));
    let cat = from_config(&cfg(json!({"kind": "categories", "categories": ["G", "A"]})));
    assert!(matches!(cat, Scoring::Categories(_)));
    assert!(matches!(from_config(&cfg(json!({"kind": "roto", "categories": ["G"]}))), Scoring::Roto(_)));
    assert_eq!(cat.value(&st(&[("G", 1.0)])), 0.0); // unfitted
    let s = from_config(&cfg(json!({"kind": "categories", "categories": ["G", "PIM"], "weights": {"PIM": -1.0}})));
    let c = s.as_categories().unwrap();
    assert_eq!((c.weight("PIM"), c.weight("G")), (-1.0, 1.0));
    let r = from_config(&cfg(json!({"kind": "roto", "categories": ["GAA"]})));
    assert_eq!(r.as_categories().unwrap().weight("GAA"), -1.0);
}

fn goalie(sa: f64, pct: f64) -> Map<String, f64> {
    st(&[("SA", sa), ("SV", sa * pct), ("SVPCT", pct), ("GS", 1.0)])
}

#[test]
fn counting_z_scores() {
    let pool = vec![st(&[("G", 0.0)]), st(&[("G", 1.0)]), st(&[("G", 2.0)]), st(&[("G", 3.0)])];
    let mut s = CategoriesScoring::new(vec!["G".into()], None);
    s.fit(pool.iter());
    let (mu, sd) = s.params()["G"];
    assert_close!(mu, 1.5);
    assert_close!(sd, 1.25f64.sqrt());
    assert_close!(s.value(&st(&[("G", 3.0)])), 1.5 / 1.25f64.sqrt());
    assert_close!(s.value(&st(&[("G", 1.5)])), 0.0);
}

#[test]
fn multiple_categories_and_weights() {
    let pool = vec![st(&[("G", 0.0), ("A", 0.0)]), st(&[("G", 2.0), ("A", 4.0)])];
    let mut s = CategoriesScoring::new(vec!["G".into(), "A".into()], Some(st(&[("A", 0.5)])));
    s.fit(pool.iter());
    let b = s.breakdown(&st(&[("G", 2.0), ("A", 4.0)]));
    assert_close!(b["G"], 1.0);
    assert_close!(b["A"], 0.5);
    assert_close!(s.value(&st(&[("G", 2.0), ("A", 4.0)])), 1.5);
    assert_close!(s.value(&st(&[("G", 2.0)])), 1.0);
}

#[test]
fn skaters_standardized_among_skaters_only() {
    let pool = vec![st(&[("G", 0.0)]), st(&[("G", 2.0)]), st(&[("SV", 25.0), ("SA", 27.0)])];
    let mut s = CategoriesScoring::new(vec!["G".into()], None);
    s.fit(pool.iter());
    let (mu, sd) = s.params()["G"];
    assert_close!(mu, 1.0);
    assert_close!(sd, 1.0);
    assert_eq!(s.value(&st(&[("SV", 25.0), ("SA", 27.0)])), 0.0);
}

#[test]
fn unfitted_and_lazy_fit_with_pool() {
    let mut s = CategoriesScoring::new(vec!["G".into()], None);
    assert!(!s.fitted());
    assert_eq!(s.value(&st(&[("G", 2.0)])), 0.0);
    let pool = vec![st(&[("G", 0.0)]), st(&[("G", 2.0)])];
    assert_close!(s.value_with_pool(&st(&[("G", 2.0)]), Some(&pool)), 1.0);
    assert!(s.fitted());
    assert_close!(s.value(&st(&[("G", 0.0)])), -1.0);
}

#[test]
fn negative_categories_flip_sign() {
    let pool: Vec<_> = [2.0, 3.0, 4.0].iter().map(|g| st(&[("GAA", *g), ("GS", 1.0)])).collect();
    for dir in [None, Some(st(&[("GAA", 1.0)])), Some(st(&[("GAA", -1.0)]))] {
        let mut s = CategoriesScoring::new(vec!["GAA".into()], dir);
        s.fit(pool.iter());
        assert_close!(s.value(&st(&[("GAA", 2.0), ("GS", 1.0)])), 1.0 / (2.0f64 / 3.0).sqrt());
    }
    let pim = vec![st(&[("PIM", 0.0)]), st(&[("PIM", 2.0)])];
    let mut a = CategoriesScoring::new(vec!["PIM".into()], None);
    a.fit(pim.iter());
    assert_close!(a.value(&st(&[("PIM", 2.0)])), 1.0);
    let mut b = CategoriesScoring::new(vec!["PIM".into()], Some(st(&[("PIM", -1.0)])));
    b.fit(pim.iter());
    assert_close!(b.value(&st(&[("PIM", 2.0)])), -1.0);
}

#[test]
fn svpct_volume_weighting() {
    let pool = vec![goalie(30.0, 0.90), goalie(25.0, 0.89), goalie(28.0, 0.91), goalie(22.0, 0.90)];
    let mut s = CategoriesScoring::new(vec!["SVPCT".into()], None);
    s.fit(pool.iter());
    let (rf, sd) = s.params()["SVPCT"];
    let sv: f64 = pool.iter().map(|p| p["SV"]).sum();
    let sa: f64 = pool.iter().map(|p| p["SA"]).sum();
    assert_close!(rf, sv / sa);
    let (busy, quiet) = (s.value(&goalie(32.0, 0.915)), s.value(&goalie(20.0, 0.915)));
    assert!(busy > quiet && quiet > 0.0);
    assert_close!(busy, (32.0 * 0.915 - rf * 32.0) / sd);
    assert!(s.value(&goalie(32.0, 0.88)) < s.value(&goalie(20.0, 0.88)));
    assert!(s.value(&goalie(20.0, 0.88)) < 0.0);
}

#[test]
fn gaa_volume_weighted_by_starts() {
    let pool = vec![st(&[("GAA", 2.5), ("GS", 0.8)]), st(&[("GAA", 3.0), ("GS", 0.6)]), st(&[("GAA", 3.2), ("GS", 0.4)])];
    let mut s = CategoriesScoring::new(vec!["GAA".into()], None);
    s.fit(pool.iter());
    assert_close!(s.params()["GAA"].0, (2.5 * 0.8 + 3.0 * 0.6 + 3.2 * 0.4) / 1.8);
    assert!(s.value(&st(&[("GAA", 2.4), ("GS", 0.9)])) > s.value(&st(&[("GAA", 2.4), ("GS", 0.3)])));
    assert!(s.value(&st(&[("GAA", 2.4), ("GS", 0.3)])) > 0.0);
}

#[test]
fn team_totals_and_roto_balance() {
    let r = RotoScoring::new(vec!["G".into(), "SVPCT".into(), "GAA".into()], None);
    let tot = r.team_totals(vec![st(&[("G", 0.5)]), st(&[("G", 0.25)]), goalie(30.0, 0.92), goalie(10.0, 0.88)]);
    assert_close!(tot["G"], 0.75 + 0.0 * 1.0 + 0.0); // goalie rows add 0 for G
    assert_close!(tot["SVPCT"], (27.6 + 8.8) / 40.0);
    assert!(tot.contains_key("GAA") == false);
    let me = st(&[("G", 3.0), ("GAA", 2.5)]);
    let others = vec![st(&[("G", 2.0), ("GAA", 3.0)]), st(&[("G", 1.0), ("GAA", 2.0)]), st(&[("G", 3.0), ("GAA", 2.8)])];
    let pts = r.roto_points(&me, &others);
    assert_close!(pts["G"], 3.5);
    assert_close!(pts["GAA"], 3.0);
    assert_eq!(r.category_balance_penalty(&me, &others), 0.0);
    let punted = st(&[("G", 0.0), ("GAA", 2.5)]);
    assert_close!(r.category_balance_penalty(&punted, &others), (1.5 / 4.0) / 2.0);
}

#[test]
fn pool_and_fit_to_context() {
    let pl = |cid: &str, g: f64| -> Player {
        let mut p = player(cid, &["C"]);
        p.lines.insert("season".into(), line("season", 10, &[("G", g * 10.0), ("GP", 10.0)]));
        p
    };
    let rostered = vec![pl("a", 0.0), pl("b", 2.0)];
    let fa = pl("fa", 10.0);
    let ctx: fm_core::models::LeagueContext = serde_json::from_value(json!({
        "provider": "t", "league_id": "1", "season": 2027, "name": "T",
        "scoring": {"kind": "categories", "categories": ["G"]}, "roster_shape": {"C": 2},
        "teams": [{"team_id": "1", "name": "me", "owner_is_me": true,
                   "slots": rostered.iter().map(|p| json!({"slot": "C", "player": p, "starting": true})).collect::<Vec<_>>()}],
        "free_agents": [fa], "matchup_period": 1, "as_of": "2026-10-01"
    }))
    .unwrap();
    assert_eq!(pool_from_players(rostered.iter(), &DEFAULT_SPLITS, 1), vec![st(&[("G", 0.0)]), st(&[("G", 2.0)])]);
    let mut s = from_config(&ctx.scoring);
    fit_to_context(&mut s, &ctx);
    assert_close!(s.value(&st(&[("G", 2.0)])), 1.0); // FA excluded from the reference pool
}

// ----------------------------------------------------------------------------- blend

fn cur(gp: i64, g_pg: f64) -> StatLine {
    line("season", gp, &[("G", g_pg * gp as f64), ("A", 0.5 * gp as f64), ("GP", gp as f64)])
}
fn proj() -> StatLine {
    line("projected", 82, &[("G", 41.0), ("A", 41.0), ("GP", 82.0)])
}

#[test]
fn blend_shrunk_rates() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    vp::install(vp::load_packaged(None));
    let (ks, kg) = (k_inseason_snapshot_now("skater"), k_inseason_snapshot_now("goalie"));
    assert_eq!((ks, kg), (25.0, 40.0));
    let p = proj();
    let r = shrunk_rates(Some(&cur(0, 1.0)), Some(&p), false, None);
    assert_eq!(r, p.per_game());
    assert_eq!(shrunk_rates(None, Some(&p), false, None), st(&[("G", 0.5), ("A", 0.5)]));
    let r = shrunk_rates(Some(&cur(10, 1.0)), None, false, None);
    assert_close!(r["G"], 1.0);
    assert_close!(r["A"], 0.5);
    let r5 = shrunk_rates(Some(&cur(5, 1.0)), Some(&p), false, None);
    let r40 = shrunk_rates(Some(&cur(40, 1.0)), Some(&p), false, None);
    assert_close!(r5["G"], (5.0 * 1.0 + ks * 0.5) / (5.0 + ks));
    assert_close!(r40["G"], (40.0 * 1.0 + ks * 0.5) / (40.0 + ks));
    assert!(r5["G"] < r40["G"] && r40["G"] < 1.0);
    assert_close!(r5["A"], 0.5);
    let prior = line("prior", 50, &[("W", 25.0), ("GP", 50.0)]);
    let now = line("season", 6, &[("W", 6.0), ("GP", 6.0)]);
    assert_close!(shrunk_rates(Some(&now), Some(&prior), true, None)["W"], (6.0 * 1.0 + kg * 0.5) / (6.0 + kg));
    let now = line("season", 10, &[("G", 5.0), ("HIT", 30.0), ("GP", 10.0)]);
    // HIT has its own packaged in-season k (8), so it is shrunk less than the scoring stats (25)
    assert_eq!(vp::k_inseason_volume(None), st(&[("SOG", 5.0), ("HIT", 8.0), ("BLK", 30.0), ("PIM", 100.0)]));
    assert_close!(shrunk_rates(Some(&now), Some(&p), false, None)["HIT"], 3.0 * 10.0 / (10.0 + 8.0));
    assert_eq!(params_k_baseline(), st(&[("F", 8.0), ("D", 14.0), ("G", 120.0)]));
}

#[test]
fn shrink_inseason_uses_the_volume_stats_own_k() {
    let params = json!({"inseason": {"k_skater": 25.0, "k_goalie": 40.0,
        "k_skater_volume": {"SOG": 5.0, "BLK": 30.0, "G": 1.0, "HIT": -3.0}}});
    // only volume stats with a valid k are kept: G is not a volume stat, a negative k is dropped
    assert_eq!(vp::k_inseason_volume(Some(&params)), st(&[("SOG", 5.0), ("BLK", 30.0)]));
    let cur = st(&[("G", 1.0), ("SOG", 4.0), ("HIT", 3.0), ("BLK", 2.0)]);
    let prior = st(&[("G", 0.5), ("SOG", 2.0), ("HIT", 1.0), ("BLK", 1.0)]);
    let r = shrink_inseason(&cur, 10.0, &prior, false, Some(&params));
    assert_close!(r["G"], (10.0 * 1.0 + 25.0 * 0.5) / 35.0); // 0.642857
    assert_close!(r["SOG"], (10.0 * 4.0 + 5.0 * 2.0) / 15.0); // 3.333333, was 2.571429 with k 25
    assert_close!(r["HIT"], (10.0 * 3.0 + 25.0 * 1.0) / 35.0); // 1.571429, no own k
    assert_close!(r["BLK"], (10.0 * 2.0 + 30.0 * 1.0) / 40.0); // 1.25
    // goalies keep one k for every stat
    let g = shrink_inseason(&cur, 10.0, &prior, true, Some(&params));
    assert_close!(g["SOG"], (10.0 * 4.0 + 40.0 * 2.0) / 50.0);
    // without the key every skater stat uses k_skater (the Python behaviour)
    let plain = json!({"inseason": {"k_skater": 25.0, "k_goalie": 40.0}});
    assert_eq!(shrink_inseason(&cur, 10.0, &prior, false, Some(&plain)), shrink_toward(&cur, 10.0, &prior, 25.0));
}

fn k_inseason_snapshot_now(g: &str) -> f64 {
    vp::k_inseason_for(g, None)
}
fn params_k_baseline() -> Map<String, f64> {
    vp::k_baseline(None)
}

#[test]
fn recency() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    vp::install(vp::load_packaged(None));
    for (a, b, c) in [(0, 0, 0), (13, 7, 4), (5, 3, 1), (30, 15, 7), (2, 2, 2)] {
        let w = recency_weights(a, b, c, None);
        assert_close!(w.values().sum::<f64>(), 1.0);
        assert!(w.values().all(|v| *v >= 0.0));
    }
    let w = recency_weights(0, 0, 0, None);
    assert_eq!((w["season"], w["last30"], w["last15"], w["last7"]), (1.0, 0.0, 0.0, 0.0));
    let w = recency_weights(13, 7, 4, None);
    assert_close!(w["season"], 0.85);
    assert_close!(w["last30"], 0.15);
    let half = recency_weights(6, 6, 3, None); // python passes floats; ints floor the ratio
    assert!(half["last30"] > 0.0);
    let l30 = line("last30", 13, &[("G", 13.0), ("GP", 13.0)]);
    let out = blend_recency(&st(&[("G", 0.5)]), Some(&l30), None, None, None);
    assert_close!(out["G"], 0.85 * 0.5 + 0.15 * 1.0);
    assert_eq!(blend_recency(&st(&[("G", 0.5)]), None, None, None, None), st(&[("G", 0.5)]));
}

fn lne(split: &str, gp: i64, g: f64) -> StatLine {
    line(split, gp, &[("G", g * gp as f64), ("GP", gp as f64)])
}

#[test]
fn multi_season() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    vp::install(vp::load_packaged(None));
    let (a, b, c) = (lne("prior", 60, 0.9), lne("prior2", 80, 0.6), lne("prior3", 40, 0.3));
    let (rates, n, gp) = multi_season_sample(&[Some(&a), Some(&b), Some(&c)], &MULTI_SEASON_WEIGHTS).unwrap();
    let wg = 5.0 * 60.0 + 4.0 * 80.0 + 3.0 * 40.0;
    assert_close!(rates["G"], (5.0 * 60.0 * 0.9 + 4.0 * 80.0 * 0.6 + 3.0 * 40.0 * 0.3) / wg);
    assert_close!(n, wg / 5.0);
    assert_eq!(gp, 180);
    assert!(multi_season_sample(&[None, None, None], &MULTI_SEASON_WEIGHTS).is_none());
    let (r2, n2, _) = multi_season_sample(&[None, Some(&b), None], &MULTI_SEASON_WEIGHTS).unwrap();
    assert_close!(r2["G"], 0.6);
    assert_close!(n2, 80.0 * 4.0 / 5.0);
    // a stat only some seasons carry is not diluted
    let x = line("prior", 50, &[("G", 10.0), ("HAT", 1.0)]);
    let y = line("prior2", 80, &[("G", 40.0)]);
    let (r, _, _) = multi_season_sample(&[Some(&x), Some(&y)], &MULTI_SEASON_WEIGHTS).unwrap();
    assert_close!(r["HAT"], 1.0 / 50.0);

    let l = lne("prior", 30, 0.8);
    let mut means: Map<String, Map<String, f64>> = Map::new();
    means.insert("F".into(), st(&[("G", 0.2)]));
    means.insert("D".into(), st(&[("G", 0.2)]));
    means.insert("G".into(), st(&[("G", 0.0)]));
    assert_close!(multi_season_baseline(&[Some(&l)], "F", &means, None)["G"], (30.0 * 0.8 + 8.0 * 0.2) / 38.0);
    assert_close!(multi_season_baseline(&[Some(&l)], "D", &means, None)["G"], (30.0 * 0.8 + 14.0 * 0.2) / 44.0);
    assert_close!(multi_season_baseline(&[Some(&l)], "F", &means, Some(0.0))["G"], 0.8);
    assert_close!(multi_season_baseline(&[Some(&l)], "F", &Map::new(), None)["G"], 0.8);
    assert!(multi_season_baseline(&[None], "F", &means, None).is_empty());
}

#[test]
fn projection_weights_and_shrink_toward() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    vp::install(vp::load_packaged(None));
    assert_eq!(projection_blend_weight(0, None), 1.0);
    assert_close!(projection_blend_weight(40, None), 0.6);
    assert_close!(projection_blend_weight(300, None), 0.6);
    assert_close!(projection_blend_weight(20, None), 0.8);
    let out = blend_rates(&st(&[("G", 1.0), ("HIT", 2.0)]), &st(&[("G", 0.5)]), 0.6);
    assert_close!(out["G"], 0.8);
    assert_close!(out["HIT"], 2.0);
    let out = shrink_toward(&st(&[("G", 0.5), ("FT", 1.0)]), 1.0, &st(&[("G", 0.3)]), 8.0);
    assert_close!(out["FT"], 1.0 / 9.0);
    assert_close!(out["G"], (0.5 + 8.0 * 0.3) / 9.0);
    let est = shrink_toward(&st(&[("FT", 0.2)]), 60.0, &st(&[("G", 0.3)]), 8.0);
    assert_close!(est["FT"], 0.2 * 60.0 / 68.0);
    assert_close!(preseason_weight(3, 0), 0.25 * 3.0 / 12.0);
    assert_close!(preseason_weight(3, 10), 0.25 * 3.0 / 12.0 * 0.5);
    assert_eq!(preseason_weight(0, 0), 0.0);
    let b = blend_preseason(&st(&[("G", 0.2), ("HIT", 1.0)]), &st(&[("G", 0.6), ("HIT", 9.0), ("A", 1.0)]), 0.1, &PRESEASON_STATS);
    assert_close!(b["G"], 0.9 * 0.2 + 0.1 * 0.6);
    assert_eq!(b["HIT"], 1.0);
    assert!(!b.contains_key("A"));
}

// ----------------------------------------------------------------------------- replacement

#[test]
fn vorp_and_eligibility() {
    let repl = st(&[("C", 2.0), ("LW", 1.0), ("RW", 1.5), ("D", 1.2), ("G", 3.0)]);
    assert_close!(vorp(3.0, &["C", "LW", "F"], &repl), 2.0);
    assert_close!(vorp(3.0, &["C"], &repl), 1.0);
    assert_close!(vorp(3.0, &["G"], &repl), 0.0);
    assert_eq!(best_slot(&["C", "LW", "F"], &repl).as_deref(), Some("LW"));
    assert_eq!(eligible_slots(&["F"], &DEFAULT_SLOTS), vec!["C", "LW", "RW"]);
    assert_eq!(eligible_slots(&["C", "F"], &["C", "F", "UTIL", "G"]), vec!["C", "F", "UTIL"]);
    assert_eq!(eligible_slots(&["G"], &["C", "UTIL", "G"]), vec!["G"]);
    assert_eq!(eligible_slots(&["D"], &["C", "D", "UTIL"]), vec!["D", "UTIL"]);
}

#[test]
fn replacement_top3_zero_padded() {
    let players: Map<String, Player> = "abcd".chars().map(|c| (c.to_string(), player(&c.to_string(), &["C"]))).collect();
    let repl = replacement_levels(&st(&[("a", 3.0), ("b", 2.0), ("c", 1.0), ("d", 0.5)]), &players, &["C", "D"]);
    assert_close!(repl["C"], 2.0);
    assert_eq!(repl["D"], 0.0);
    assert_close!(replacement_levels(&st(&[("a", 3.0)]), &players, &["C"])["C"], 1.0);
    // fallback: slot D has no free agent -> percentile of rostered D
    let mut pl2 = players.clone();
    pl2.insert("r1".into(), player("r1", &["D"]));
    pl2.insert("r2".into(), player("r2", &["D"]));
    let (lv, fb) = replacement_with_fallback(
        &st(&[("a", 3.0)]),
        &st(&[("r1", 1.0), ("r2", 3.0)]),
        &pl2,
        &["C", "D"],
        ROSTERED_FALLBACK_PCT,
    );
    assert_close!(lv["D"], percentile([1.0, 3.0], 0.10));
    assert_close!(lv["D"], 1.2);
    assert!(fb.contains("D") && !fb.contains("C"));
}

#[test]
fn adding_free_agent_never_lowers_replacement() {
    // deterministic LCG instead of random.Random(7)
    let mut x: u64 = 7;
    let mut rnd = || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((x >> 33) as f64) / ((1u64 << 31) as f64)
    };
    let positions: [&[&str]; 8] = [&["C"], &["LW"], &["RW"], &["D"], &["G"], &["C", "LW"], &["F"], &["LW", "RW"]];
    for _ in 0..200 {
        let mut players: Map<String, Player> = Map::new();
        let mut vals = Map::new();
        for i in 0..((rnd() * 9.0) as usize) {
            let cid = format!("p{i}");
            players.insert(cid.clone(), player(&cid, positions[(rnd() * 8.0) as usize % 8]));
            vals.insert(cid, rnd() * 5.0 - 1.0);
        }
        let before = replacement_levels(&vals, &players, &DEFAULT_SLOTS);
        players.insert("new".into(), player("new", positions[(rnd() * 8.0) as usize % 8]));
        vals.insert("new".into(), rnd() * 5.0 - 1.0);
        let after = replacement_levels(&vals, &players, &DEFAULT_SLOTS);
        for (s, b) in &before {
            assert!(after[s] >= b - 1e-12);
        }
    }
}

// ----------------------------------------------------------------------------- availability

const AS_OF: fn() -> NaiveDate = || d(2026, 9, 29);
fn start() -> NaiveDate {
    d(2026, 10, 6)
}
fn dates() -> Vec<NaiveDate> {
    let mut v: Vec<NaiveDate> = [6, 7, 9, 11, 13, 14, 16].iter().map(|x| d(2026, 10, *x)).collect();
    v.extend((0..75).map(|i| d(2026, 10, 17) + Duration::days(2 * i)));
    v
}
fn est(status: &str, note: Option<&str>, as_of: NaiveDate) -> Option<ReturnEstimate> {
    return_estimate(status, note, as_of, Some(&dates()), Some(start()), SeasonLength::DEFAULT)
}

#[test]
fn availability_flat_table() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    vp::install(vp::load_packaged(None));
    for (s, w, se) in [("healthy", 1.0, 1.0), ("dtd", 0.75, 0.75), ("out", 0.0, 0.6), ("ir", 0.0, 0.4),
                       ("ltir", 0.0, 0.1), ("suspended", 0.0, 0.5), ("unknown", 1.0, 1.0)] {
        assert_eq!(availability_multiplier(s, Horizon::Week, None), w, "{s}");
        assert_eq!(availability_multiplier(s, Horizon::Season, None), se, "{s}");
    }
}

#[test]
fn suspension_is_about_seven_games() {
    let e = est("suspended", Some("suspension (est. return 2026-10-17)"), AS_OF()).unwrap();
    assert_eq!(e.return_date, Some(d(2026, 10, 17)));
    assert_eq!((e.games_missed, e.games_remaining), (7.0, 82.0));
    assert_close!(e.multiplier, 1.0 - 7.0 / 82.0);
    assert!(e.text().contains("est. return 2026-10-17") && e.text().contains("~7 of 82"));
    assert_eq!(
        expected_return(
            "suspended",
            Some("suspension (est. return 2026-10-17)"),
            AS_OF(),
            Some(&dates()),
            Some(start()),
            SeasonLength::DEFAULT
        ),
        (Some(d(2026, 10, 17)), Some(7.0))
    );
}

#[test]
fn game_counts() {
    for (status, note, games) in [
        ("ir", "Lower Body: Marchand will miss Florida's first 14 regular-season games if he's able to return Nov. 2.", 14usize),
        ("suspended", "McAvoy is set to serve a six-game suspension to begin the year.", 6),
        ("suspended", "He was suspended three games for boarding.", 3),
        ("dtd", "It's possible Marchenko is forced to miss the first two games of the year.", 2),
        ("suspended", "Suspended", 5),
    ] {
        let e = est(status, Some(note), AS_OF()).unwrap();
        assert_eq!(e.games_missed, games as f64, "{note}");
        assert_close!(e.multiplier, 1.0 - games as f64 / 82.0);
        assert_eq!(e.return_date, Some(dates()[games]), "{note}");
    }
}

#[test]
fn return_dates_from_notes() {
    let ds = dates();
    for (note, back) in [
        ("Gustavsson is expected to be out until at least early November.", d(2026, 11, 5)),
        ("The expectation is for him to rejoin the team sometime in November.", d(2026, 11, 15)),
        ("Gourde won't be available to play until at least December.", d(2026, 12, 1)),
        ("He is expected to return Nov. 2 versus Detroit.", d(2026, 11, 2)),
        ("Expected to be out for another 2-3 months.", start() + Duration::days((2.5f64 * 30.4).round() as i64)),
        ("Tucker is week-to-week and will miss the start of the campaign.", start() + Duration::days(21)),
        ("Dickinson is expected to miss at least the first week of the 2026-27 campaign.", start() + Duration::days(7)),
        ("Jarvis was initially pegged to miss about 4-6 months after having shoulder surgery in June.",
         d(2026, 6, 15) + Duration::days((5.0f64 * 30.4).round() as i64)),
    ] {
        let e = est("ir", Some(note), AS_OF()).unwrap();
        assert_eq!(e.return_date, Some(back), "{note}");
        assert_eq!(e.games_missed, ds.iter().filter(|x| **x < back).count() as f64, "{note}");
    }
}

#[test]
fn defaults_and_flat_fallback() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    vp::install(vp::load_packaged(None));
    let e = est("ir", Some("Demko missed the second half of last season after hip surgery in January."), AS_OF()).unwrap();
    assert_eq!(e.return_date, Some(start() + Duration::days(30)));
    assert!(e.source.contains("30 days assumed"));
    let ir = est("ir", None, d(2026, 10, 20)).unwrap();
    assert_eq!(ir.return_date, Some(d(2026, 11, 19)));
    let lt = est("ltir", Some("Injured Reserve List - LTIR"), AS_OF()).unwrap();
    assert_eq!(lt.multiplier, FLOOR);
    assert_eq!(lt.games_missed, lt.games_remaining);
    let dtd = est("dtd", Some("Undisclosed"), AS_OF()).unwrap();
    assert_eq!(dtd.return_date, Some(start() + Duration::days(3)));
    assert_close!(dtd.multiplier, 1.0 - 2.0 / 82.0);
    let back = est("dtd", Some("Lower Body: day-to-day (est. return 2026-10-02)"), AS_OF()).unwrap();
    assert_eq!((back.games_missed, back.multiplier), (0.0, 1.0));
    assert!(est("out", Some("OUT"), AS_OF()).is_none());
    assert_eq!(season_availability("out", Some("OUT"), AS_OF(), None, None, SeasonLength::DEFAULT, None).0, 0.6);
    assert!(est("healthy", Some("est. return 2026-12-01"), AS_OF()).is_none());
    // not injury related
    let today = d(2026, 10, 26);
    let note = "Not Injury Related: While Hellebuyck appears to be close to making his team debut, Keefe said \
                that there are \"several boxes they'll need to tick\" before the Devils give the green light";
    let e = est("out", Some(note), today).unwrap();
    assert_eq!(e.return_date, Some(today + Duration::days(3)));
    assert!(e.multiplier > 0.95 && e.source.contains("not injury related"));
    assert!(est("out", Some("Lower Body: Hellebuyck (lower body) is out indefinitely"), today).is_none());
    // no schedule: games from days
    let note = Some("suspension (est. return 2026-10-17)");
    let e = return_estimate("suspended", note, AS_OF(), None, Some(start()), SeasonLength::DEFAULT).unwrap();
    assert_eq!(e.games_remaining, 82.0);
    assert_close!(e.games_missed, 11.0 * 82.0 / 186.0);
    let _ = adjust::availability_snapshot();
}

#[test]
fn season_length_comes_from_the_schedule() {
    assert_eq!(SeasonLength::of(&Map::new()), SeasonLength { games: 82.0, days: 186.0 });
    // AAA plays 4 dates and BBB 3, between Oct 1 and Oct 10: 4 games per team in 10 days
    let mut sched: Map<String, Vec<NaiveDate>> = Map::new();
    sched.insert("AAA".into(), [1, 3, 5, 10].iter().map(|x| d(2026, 10, *x)).collect());
    sched.insert("BBB".into(), [2, 4, 6].iter().map(|x| d(2026, 10, *x)).collect());
    assert_eq!(SeasonLength::of(&sched), SeasonLength { games: 4.0, days: 10.0 });
    // a player whose team has no dates, in an 84-game season of 194 days (2026-27)
    let season = SeasonLength { games: 84.0, days: 194.0 };
    let note = Some("suspension (est. return 2026-10-17)");
    let e = return_estimate("suspended", note, AS_OF(), None, Some(start()), season).unwrap();
    assert_eq!(e.games_remaining, 84.0);
    assert_close!(e.games_missed, 11.0 * 84.0 / 194.0);
    // 97 days in: half the season is left
    assert_close!(games_remaining(start() + Duration::days(97), None, Some(start()), season), 42.0);
}

// ----------------------------------------------------------------------------- regression

fn skater(onice: Option<f64>, gp: i64, a: f64) -> Player {
    let mut p = player("x", &["C"]);
    p.lines.insert("season".into(), line("season", gp, &[("GP", gp as f64), ("G", 0.3 * gp as f64), ("A", a), ("SOG", 2.5 * gp as f64)]));
    p.lines.insert("prior".into(), line("prior", 82, &[("GP", 82.0), ("G", 25.0), ("A", 30.0)]));
    p.ixg_per_game = Some(0.3);
    p.goals_minus_ixg = Some(0.0);
    p.onice_sh_pct = onice;
    p
}

#[test]
fn assist_shrink_math() {
    let p = skater(Some(0.12), 20, 16.0);
    let sig = luck_signals(&p, Some(&st(&[("F", 0.10)])), None, None, None).unwrap();
    let luck = 0.8 * (1.0 - 0.10 / 0.12);
    assert_close!(sig.assist_luck_fpg, luck);
    assert_close!(sig.confidence, 20.0 / 40.0);
    let (new, delta) = assist_shrink_rates(&st(&[("A", 0.7), ("PTS", 1.0)]), Some(&sig), ASSIST_SHRINK_SHARE);
    assert_close!(delta, -ASSIST_SHRINK_SHARE * luck * 0.5);
    assert_close!(new["A"], 0.7 + delta);
    assert_close!(new["PTS"], 1.0 + delta);
    assert_eq!(assist_shrink_rates(&st(&[("A", 0.7)]), None, 0.35), (st(&[("A", 0.7)]), 0.0));
    // league weights: points league weights goals / assists, categories ignore
    let w = st(&[("G", 2.0), ("A", 1.0)]);
    assert_eq!(league_weights(Some(&w)), (2.0, 1.0));
    assert_eq!(league_weights(None), (1.0, 1.0));
    let s = from_config(&serde_json::from_value(json!({"kind": "points", "weights": {"G": 2.0, "A": 1.0, "PTS": 0.5}})).unwrap());
    assert_eq!(league_weights(Some(&s)), (2.5, 1.5));
    // goalies and missing data
    assert!(luck_signals(&player("g", &["G"]), None, None, None, None).is_none());
    let mut q = skater(None, 20, 16.0);
    q.ixg_per_game = None;
    assert!(luck_signals(&q, None, None, None, None).is_none());
    // prior season used under 5 GP, confidence halved
    let early = skater(Some(0.12), 2, 1.0);
    let s = luck_signals(&early, None, None, None, None).unwrap();
    assert!(s.from_prior && s.gp == 82);
    assert_close!(s.confidence, 82.0 / 102.0 * 0.5);
}

#[test]
fn shrink_goals_and_regressed_rates() {
    let r = st(&[("G", 0.4), ("PTS", 1.0)]);
    let out = shrink_goals(&r, Some(0.3), 15.0, 15.0);
    assert_close!(out["G"], 0.35);
    assert_close!(out["PTS"], 1.0 - 0.05);
    assert_eq!(shrink_goals(&r, None, 15.0, K_GOALS), r);
    assert_close!(shrink_goals(&r, Some(0.3), 15.0, f64::INFINITY)["G"], 0.3);
}

fn gsax(per_100: f64, shots: f64, xsv: f64, league: f64) -> GoalieGsax {
    GoalieGsax {
        nhl_id: 1,
        seasons: vec![2025],
        gsax: per_100 / 100.0 * (shots + GSAX_K_SHOTS),
        shots,
        raw_gsax: 0.0,
        raw_shots: shots,
        xsv_pct: xsv,
        per_shot: per_100 / 100.0,
        shots_per_game: Some(28.0),
        league_sv_pct: Some(league),
    }
}

#[test]
fn regressed_gsax_math() {
    let seasons = [
        GsaxSeason::new(2026, 30.0, 25.0, 300.0, 10.0, Some(0.90)),
        GsaxSeason::new(2025, 150.0, 130.0, 1500.0, 50.0, Some(0.90)),
        GsaxSeason::new(2024, 140.0, 140.0, 1400.0, 48.0, Some(0.91)),
    ];
    let e = regressed_gsax(1, &seasons, GSAX_K_SHOTS, &GSAX_SEASON_WEIGHTS).unwrap();
    let wg = 1.0 * 5.0 + 0.8 * 20.0 + 0.6 * 0.0;
    let ws = 300.0 + 0.8 * 1500.0 + 0.6 * 1400.0;
    assert_close!(e.gsax, wg);
    assert_close!(e.shots, ws);
    assert_close!(e.per_shot, wg / (ws + 3000.0));
    assert_close!(e.raw_gsax, 25.0);
    assert_close!(e.raw_shots, 3200.0);
    assert_close!(e.xsv_pct, 1.0 - (30.0 + 0.8 * 150.0 + 0.6 * 140.0) / ws);
    assert_close!(e.league_sv_pct.unwrap(), (300.0 * 0.90 + 0.8 * 1500.0 * 0.90 + 0.6 * 1400.0 * 0.91) / ws);
    assert_eq!(e.seasons, vec![2026, 2025, 2024]);
    assert_close!(e.shots_per_game.unwrap(), 30.0);
    assert!(e.usable());
    let e2 = regressed_gsax(
        1,
        &[GsaxSeason::new(2026, 0.0, 0.0, 0.0, 0.0, None), GsaxSeason::new(2025, 150.0, 130.0, 1500.0, 50.0, None)],
        GSAX_K_SHOTS,
        &GSAX_SEASON_WEIGHTS,
    )
    .unwrap();
    assert_close!(e2.per_shot, 0.8 * 20.0 / (0.8 * 1500.0 + 3000.0));
    assert!(e2.league_sv_pct.is_none());
    assert!(regressed_gsax(1, &[GsaxSeason::new(2026, 0.0, 0.0, 0.0, 0.0, None)], GSAX_K_SHOTS, &GSAX_SEASON_WEIGHTS).is_none());
    let small = regressed_gsax(1, &[GsaxSeason::new(2026, 5.0, 3.0, 200.0, 7.0, None)], GSAX_K_SHOTS, &GSAX_SEASON_WEIGHTS).unwrap();
    assert!(small.shots < GSAX_MIN_SHOTS && !small.usable());
}

#[test]
fn sv_anchoring_and_rates() {
    let e = gsax(0.5, 4000.0, 0.902, 0.896);
    assert_close!(e.sv_pct(), 0.907);
    assert_close!(e.sv_pct_on(Some(0.900)), 0.900 + 0.006 + 0.005);
    assert_eq!(e.sv_pct_on(None), e.sv_pct());
    let rates = st(&[("SA", 30.0), ("SV", 27.6), ("GA", 2.4), ("SVPCT", 0.92), ("GAA", 2.4), ("W", 0.5)]);
    let (new, before, after) = gsax_rates(&rates, &e, Some(0.900));
    assert_close!(before.unwrap(), 0.92);
    assert_close!(after.unwrap(), 0.911);
    assert_close!(new["SV"], 30.0 * 0.911);
    assert_close!(new["GA"], 30.0 * 0.089);
    assert_close!(new["SVPCT"], 0.911);
    assert_close!(new["GAA"], 2.4 * 0.089 / 0.08);
    assert_eq!(new["W"], 0.5);
    assert!(!new.contains_key("SO"));
    assert_close!(gsax_rates(&st(&[("SA", 30.0), ("SVPCT", 92.0)]), &e, Some(0.9)).0["SVPCT"], 91.1);
    assert!(gsax_rates(&st(&[("W", 0.5)]), &e, None).1.is_none());
}

// ----------------------------------------------------------------------------- params

fn write(p: &std::path::Path, text: &str) {
    std::fs::write(p, text).unwrap();
}

/// Layering fixture: temp versions dir + env; restores env at the end.
struct Env {
    dir: tempfile::TempDir,
    pdir: std::path::PathBuf,
    old: Vec<(String, Option<String>)>,
}
impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join("harness").join("params");
        std::fs::create_dir_all(&pdir).unwrap();
        let old = [vp::OVERRIDE_ENV, vp::PARAMS_DIR_ENV].iter().map(|k| (k.to_string(), std::env::var(k).ok())).collect();
        std::env::set_var(vp::OVERRIDE_ENV, "1");
        std::env::set_var(vp::PARAMS_DIR_ENV, &pdir);
        vp::reload();
        Env { dir, pdir, old }
    }
    fn version(&self, name: &str, params: Value) {
        let rec = json!({"version": name, "created": "2026-11-17T09:00:00", "params": params, "status": "active"});
        write(&self.pdir.join(format!("{name}.json")), &rec.to_string());
        write(&self.pdir.join("active.json"), &json!({"version": name}).to_string());
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        for (k, v) in &self.old {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        vp::reload();
        let _ = &self.dir;
    }
}

#[test]
fn params_no_override_is_packaged() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    let _e = Env::new();
    let packaged = vp::load_packaged(None);
    assert_eq!(vp::load_params(None), packaged);
    assert_eq!(vp::params_hash(None), vp::params_hash(Some(&packaged)));
    assert_eq!(vp::source(None), "fit 2026-09-28 (espn)");
    assert!(vp::active_version().is_none());
}

#[test]
fn params_override_deep_merges() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = Env::new();
    e.version("v0003", json!({"inseason": {"k_skater": 30.0}, "projection": {"weight": 0.5},
                              "availability": {"dtd": {"week": 0.6}}, "schedule": {"offnight_bonus": 0.07}}));
    let merged = vp::reload().merged.clone();
    let packaged = vp::load_packaged(None);
    assert_eq!(merged["inseason"]["k_skater"], json!(30.0));
    assert_eq!(merged["inseason"]["k_goalie"], packaged["inseason"]["k_goalie"]);
    assert_eq!(merged["inseason"]["recency_weights"], packaged["inseason"]["recency_weights"]);
    assert_eq!(merged["preseason"], packaged["preseason"]);
    assert_eq!(vp::k_inseason_for("skater", None), 30.0);
    assert_eq!(vp::k_inseason_for("G", None), 40.0);
    assert_eq!(vp::k_inseason_goalie(true, None), 40.0);
    assert_eq!(vp::projection_weight(None), 0.5);
    assert_eq!(vp::offnight_bonus(None), 0.07);
    assert_eq!(vp::availability("dtd", Horizon::Week, None), 0.6);
    assert_eq!(vp::availability("dtd", Horizon::Season, None), 0.75);
    let act = vp::active_version().unwrap();
    assert_eq!(vp::params_hash(None), vp::params_hash(Some(&vp::deep_merge(&packaged, &act["params"]))));
    assert_ne!(vp::params_hash(None), vp::params_hash(Some(&packaged)));
    assert_eq!(vp::source(None), "fit 2026-09-28 (espn) + harness v0003 (2026-11-17)");
    // live code reads the accessor at call time
    e.version("v0001", json!({"inseason": {"k_skater": 22.0, "recency_weights": {"season": 0.8, "last30": 0.1, "last15": 0.1, "last7": 0.0}},
                              "projection": {"k_goalie": 10.0}, "availability": {"out": {"week": 0.1, "season": 0.5}}}));
    vp::reload();
    assert_eq!(shrink_k(false, None), 22.0);
    assert_eq!(projection_k(true, None), 10.0);
    let w = recency_weights(13, 7, 4, None);
    assert_close!(w["season"], 0.8);
    assert_close!(w["last15"], 0.1);
    assert_close!(projection_blend_weight(40, None), 0.6);
    assert_eq!(availability_multiplier("out", Horizon::Week, None), 0.1);
    assert_eq!(availability_multiplier("out", Horizon::Season, None), 0.5);
}

#[test]
fn params_disable_flag_reload_and_rollback() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = Env::new();
    e.version("v0001", json!({"inseason": {"k_skater": 30.0}}));
    vp::reload();
    assert_eq!(vp::k_inseason_for("skater", None), 30.0);
    std::env::set_var(vp::OVERRIDE_ENV, "0");
    vp::reload();
    assert_eq!(vp::k_inseason_for("skater", None), 25.0);
    assert_eq!(vp::load_params(None), vp::load_packaged(None));
    assert!(!vp::override_enabled());
    std::env::set_var(vp::OVERRIDE_ENV, "1");
    vp::reload();
    e.version("v0002", json!({"inseason": {"k_skater": 21.0}}));
    assert_eq!(vp::k_inseason_for("skater", None), 30.0); // cached until reload
    vp::reload();
    assert_eq!(vp::k_inseason_for("skater", None), 21.0);
    write(&e.pdir.join("active.json"), &json!({"version": null}).to_string());
    vp::reload();
    assert_eq!(vp::k_inseason_for("skater", None), 25.0);
    assert!(vp::active_version().is_none());
}

#[test]
fn params_corrupt_override_is_tolerated() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = Env::new();
    let cases: Vec<(String, Option<String>)> = vec![
        ("{not json".into(), None),
        (json!({"version": "v0009"}).to_string(), None),
        (json!({"version": "../../etc"}).to_string(), None),
        (json!({"version": "v0001"}).to_string(), Some("{broken".into())),
        (json!({"version": "v0001"}).to_string(), Some(json!({"params": ["not", "a", "dict"]}).to_string())),
        (json!(["v0001"]).to_string(), Some(json!({"params": {"inseason": {"k_skater": 30}}}).to_string())),
    ];
    for (ptr, body) in cases {
        let _ = std::fs::remove_file(e.pdir.join("v0001.json"));
        write(&e.pdir.join("active.json"), &ptr);
        if let Some(b) = body {
            write(&e.pdir.join("v0001.json"), &b);
        }
        assert_eq!(vp::reload().merged, vp::load_packaged(None), "{ptr}");
        assert_eq!(vp::k_inseason_for("skater", None), 25.0);
        assert_eq!(vp::source(None), "fit 2026-09-28 (espn)");
    }
}

#[test]
fn params_bad_values_and_empty_fallbacks() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = Env::new();
    e.version("v0001", json!({"inseason": {"k_skater": -3, "k_goalie": "x", "recency_weights": {"season": 0.9, "last30": 0.2}},
                              "projection": {"weight": 1.7, "k_skater": null},
                              "availability": {"dtd": [2.0, 0.5], "weird": "x"},
                              "schedule": {"offnight_bonus": "nan", "start_share_prior": 3}}));
    vp::reload();
    assert_eq!(vp::k_inseason(None), vp::fallback_k_inseason());
    assert_eq!(vp::recency_weights(None), vp::fallback_recency());
    assert_eq!(vp::projection_weight(None), vp::FALLBACK_PROJECTION_WEIGHT);
    assert_eq!(vp::k_projection(None), vp::fallback_k_projection());
    assert_eq!(vp::availability("dtd", Horizon::Week, None), 0.75);
    assert_eq!(vp::availability("dtd", Horizon::Season, None), 0.5);
    assert_eq!(vp::offnight_bonus(None), vp::FALLBACK_OFFNIGHT_BONUS);
    assert_eq!(vp::start_share_prior(None), 0.5);
    drop(e);
    let empty = json!({});
    let p = Some(&empty);
    assert_eq!(vp::k_inseason(p), vp::fallback_k_inseason());
    assert_eq!(vp::k_inseason_for("goalie", p), 40.0);
    assert_eq!(vp::k_projection_for("skater", p), 20.0);
    assert_eq!(vp::recency_weights(p), vp::fallback_recency());
    assert_eq!(vp::projection_weight(p), 0.6);
    assert_eq!(vp::availability_table(p), vp::fallback_availability());
    assert_eq!(vp::availability("ir", Horizon::Week, p), 0.0);
    assert_eq!(vp::availability("ir", Horizon::Season, p), 0.4);
    assert_eq!(vp::availability("no-such-status", Horizon::Week, p), 1.0);
    assert_eq!((vp::start_share_prior(p), vp::start_share_k(p), vp::offnight_bonus(p)), (0.5, 10.0, 0.05));
    assert_eq!(vp::source(p), "built-in fallback");
    assert_eq!(vp::params_hash(p), "bf21a9e8fbc5a3846fb05b4fa0859e0917b2202f"); // sha1("{}")
    assert_eq!(vp::age_yoy(p)["F"][&18], 1.111);
    assert_eq!(vp::dynasty_age_curves(p)["D"][0], (19.0, 0.848));
    assert_eq!(vp::age_groups(p), vec!["F".to_string(), "D".to_string()]);
}

#[test]
fn params_dir_default_and_env() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    let old = std::env::var(vp::PARAMS_DIR_ENV).ok();
    std::env::remove_var(vp::PARAMS_DIR_ENV);
    let t = std::path::Path::new("/tmp/x");
    assert_eq!(vp::params_dir(Some(t)), t.join("harness").join("params"));
    std::env::set_var(vp::PARAMS_DIR_ENV, "/tmp/y");
    assert_eq!(vp::params_dir(None), std::path::PathBuf::from("/tmp/y"));
    match old {
        Some(v) => std::env::set_var(vp::PARAMS_DIR_ENV, v),
        None => std::env::remove_var(vp::PARAMS_DIR_ENV),
    }
}

#[test]
fn age_factor_and_curves() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    vp::install(vp::load_packaged(None));
    let yoy = vp::age_yoy(None);
    assert_eq!(vp::age_factor("F", None, None, None), 1.0);
    assert_eq!(vp::age_factor("G", Some(25.0), None, None), 1.0);
    let f25 = yoy["F"][&25];
    assert_eq!(vp::age_factor("F", Some(25.9), None, None), f25);
    assert_eq!(vp::age_factor("F", Some(10.0), None, None), yoy["F"][&18]);
    assert_eq!(vp::age_factor("F", Some(99.0), None, None), yoy["F"][&40]);
    let c = vp::dynasty_age_curves(None);
    assert!(c["F"].len() >= 2 && c["F"].windows(2).all(|w| w[0].0 <= w[1].0));
}

// ----------------------------------------------------------------------------- snapshot parity

fn fixture_roots() -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("FM_PARITY_FIXTURES") {
        v.push(std::path::PathBuf::from(p));
    }
    v.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures"));
    v
}

#[test]
fn snapshot_params_merged_and_hash() {
    let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut checked = 0;
    for root in fixture_roots() {
        for league in ["espn", "fantrax"] {
            let path = root.join(league).join("snapshot.json");
            if !path.exists() {
                continue;
            }
            let snap = Snapshot::load(&path).unwrap();
            let sp = &snap.params;
            vp::install_with_active(sp.merged.clone(), Some(sp.active.clone()));
            // the hash and source of the installed merged params match Python's
            if let Some(h) = &sp.hash {
                assert_eq!(&vp::params_hash(None), h, "{league} hash");
            }
            if let Some(s) = &sp.source {
                assert_eq!(&vp::source(None), s, "{league} source");
            }
            // load_params() on disk (packaged + active override) reproduces the merged dict when the
            // fixtures were produced without a harness version
            let mut packaged = vp::load_packaged(None);
            // the per-stat in-season k of the volume stats was added after the Python port
            packaged["inseason"].as_object_mut().unwrap().shift_remove("k_skater_volume");
            let expect = match sp.active.get("params") {
                Some(p) if sp.active.is_object() => vp::deep_merge(&packaged, p),
                _ => packaged,
            };
            assert_eq!(expect, sp.merged, "{league} merged");
            checked += 1;
        }
        if checked > 0 {
            break;
        }
    }
    assert!(checked > 0, "no snapshot fixtures found");
    vp::reload();
}
