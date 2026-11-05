//! "How right was the model?" Two read-only views over the ledger, shown by `fm harness review`
//! and the dashboard's Health tab:
//!
//! * **Move review**: every graded suggestion (followed or not) and every move of your own, marked
//!   against what the players actually scored so far in its window. Unlike the weekly bar it shows
//!   each move from its first graded day, labelled "so far" until the window is complete, so
//!   there is something to read in the first week instead of after 20 graded moves.
//! * **Free-agent ranking**: on every archived day, did the free agents the model ranked highest
//!   go on to outscore the ones the market (% rostered) or recent scoring (season-to-date points
//!   per game) ranked highest? Every day adds a whole pool of players, so this becomes readable
//!   much sooner than per-move hit rates.
//!
//! Both read `outcomes` (written by `outcomes::grade_outcomes`), `projections`, `lineup_days` and
//! `realized_daily`; nothing is written.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::{Duration, NaiveDate};
use fm_backtest::evaluate::spearman;
use fm_pycompat::num::py_sum;
use serde_json::{json, Value};

use crate::ledger::{Ledger, Row};
use crate::metrics::{f_of, i_of, primary_window, s_of, wilson};
use crate::outcomes::{days_between, fpg_of, points_scorer, scoring_config, Realized, FLAG_KINDS};

/// Free agents per ranker whose realized points are averaged.
pub const TOP_K: usize = 10;
/// Forward windows (days after the projection day) the ranking is checked over.
pub const RANK_WINDOWS: [i64; 3] = [7, 14, 28];
/// Fewer evaluated days than this is labelled an early read (consecutive days overlap).
pub const RANK_EARLY_DAYS: usize = 14;
/// A 28-day window with fewer graded days than this is "too early to tell".
pub const EARLY_DAYS: i64 = 7;

fn parse_day(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn mean(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        None
    } else {
        Some(py_sum(xs.iter().copied()) / xs.len() as f64)
    }
}

fn round(x: f64, nd: i32) -> f64 {
    let m = 10f64.powi(nd);
    (x * m).round() / m
}

fn ofv(x: Option<f64>, nd: i32) -> Value {
    x.filter(|v| v.is_finite()).map(|v| json!(round(v, nd))).unwrap_or(Value::Null)
}

// --------------------------------------------------------------------------- move review

/// Who made the move: the model's suggestion you took, one you didn't, or your own.
pub fn group_of(origin: &str) -> &'static str {
    match origin {
        "followed" | "partial" | "proposed" => "followed",
        "user_only" => "own",
        _ => "not_followed",
    }
}

fn basis_rank(basis: &str) -> i32 {
    match basis {
        "acted_on" => 0,
        "first_seen" => 1,
        _ => 2,
    }
}

fn window_rank(kind: &str, units: Option<&str>, window: &str) -> i32 {
    if window == primary_window(kind, units) {
        0
    } else {
        match window {
            "28d" => 1,
            "7d" => 2,
            _ => 3,
        }
    }
}

/// "right" / "wrong" once the window is complete, "ahead" / "behind" while it runs,
/// "no result" when nobody involved played (or a player is unmatched).
pub fn verdict(realized: Option<f64>, hit: Option<i64>, complete: bool) -> &'static str {
    match (realized, hit, complete) {
        (None, _, _) | (_, None, _) => "no result",
        (_, Some(1), true) => "right",
        (_, Some(_), true) => "wrong",
        (_, Some(1), false) => "ahead",
        _ => "behind",
    }
}

/// One line per graded move: its main window (7 days for lineups and this-week moves, else 28),
/// on the day you acted when you did, else the day it was first suggested.
pub fn move_rows(ledger: &Ledger, league: &str) -> Result<Vec<Value>> {
    let l = league.to_string();
    let rows = ledger.query("SELECT * FROM outcomes WHERE league=?", &[&l])?;
    let mut best: HashMap<String, (i32, i32, Row)> = HashMap::new();
    for r in rows {
        let key = match s_of(&r, "episode_id") {
            Some(e) if !e.is_empty() => format!("ep:{e}"),
            _ => format!("dec:{}", s_of(&r, "decision_id").unwrap_or_default()),
        };
        let kind = s_of(&r, "kind").unwrap_or_default();
        let units = s_of(&r, "gain_units");
        let window = s_of(&r, "window").unwrap_or_default();
        if window == "ros" {
            continue; // the rest of the season is listed for trades in the bar, not here
        }
        let rank = (basis_rank(&s_of(&r, "basis").unwrap_or_default()), window_rank(&kind, units.as_deref(), &window));
        match best.get(&key) {
            Some((b, w, _)) if (*b, *w) <= rank => {}
            _ => {
                best.insert(key, (rank.0, rank.1, r));
            }
        }
    }
    let names = player_names(ledger, league)?;
    let mut moves: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    for d in ledger.query("SELECT decision_id, adds_json, drops_json FROM decisions WHERE league=?", &[&l])? {
        let list = |k: &str| -> Vec<String> { s_of(&d, k).and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default() };
        if let Some(id) = s_of(&d, "decision_id") {
            moves.insert(id.to_string(), (list("adds_json"), list("drops_json")));
        }
    }
    let mut out: Vec<(String, String, Value)> = Vec::new();
    for (key, (_, _, r)) in best {
        let detail: Value = s_of(&r, "detail_json").and_then(|d| serde_json::from_str(&d).ok()).unwrap_or(json!({}));
        let kind = s_of(&r, "kind").unwrap_or_default();
        let origin = s_of(&r, "origin").unwrap_or_default();
        let complete = i_of(&r, "complete").unwrap_or(0) == 1;
        let realized = f_of(&r, "realized_gain");
        let hit = i_of(&r, "hit");
        let start = s_of(&r, "window_start").as_deref().and_then(parse_day);
        let end = s_of(&r, "window_end").as_deref().and_then(parse_day);
        let n_days = match (start, end) {
            (Some(s), Some(e)) => (e - s).num_days() + 1,
            _ => 0,
        };
        let graded = detail.get("days").and_then(|v| v.as_i64()).unwrap_or(0);
        let predicted = f_of(&r, "predicted_pts");
        let so_far = predicted.map(|p| if complete || n_days == 0 { p } else { p * graded as f64 / n_days as f64 });
        let flag = FLAG_KINDS.contains(&kind);
        let day = detail.get("day").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let alt = detail.get("alt").filter(|a| a.is_object());
        out.push((
            day.clone(),
            key,
            json!({
                "day": day,
                "kind": kind,
                "title": detail.get("title").filter(|t| t.is_string()).cloned().unwrap_or_else(|| {
                    let ids = s_of(&r, "decision_id").and_then(|id| moves.get(id));
                    ids.map(|(a, d)| json!(move_title(a, d, &names))).unwrap_or(Value::Null)
                }),
                "origin": origin,
                "group": group_of(&origin),
                "window": s_of(&r, "window"),
                "days_graded": graded,
                "window_days": n_days,
                "graded_through": detail.get("graded_through").cloned().unwrap_or(Value::Null),
                "complete": complete,
                "early": !complete && n_days > EARLY_DAYS && graded < EARLY_DAYS,
                // flags compare points per game with his last 15 days; everything else is points
                "units": if flag { "fpg_vs_l15" } else { "pts" },
                "predicted_pts": ofv(predicted, 2),
                "predicted_so_far": ofv(so_far, 2),
                "realized": ofv(realized, 2),
                "verdict": verdict(realized, hit, complete),
                "model_pick": alt.and_then(|a| a.get("title")).cloned().unwrap_or(Value::Null),
                "model_pick_realized": ofv(alt.and_then(|a| a.get("gain")).and_then(|g| g.as_f64()), 2),
            }),
        ));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(out.into_iter().map(|(_, _, v)| v).collect())
}

/// cid -> player name, from what the ledger saw (projections, then transactions).
fn player_names(ledger: &Ledger, league: &str) -> Result<HashMap<String, String>> {
    let l = league.to_string();
    let mut out = HashMap::new();
    for sql in [
        "SELECT cid, name FROM projections WHERE league=? AND name IS NOT NULL",
        "SELECT cid, player_name AS name FROM transactions WHERE league=? AND player_name IS NOT NULL",
    ] {
        for r in ledger.query(sql, &[&l])? {
            if let (Some(c), Some(n)) = (s_of(&r, "cid"), s_of(&r, "name")) {
                out.insert(c.to_string(), n.to_string());
            }
        }
    }
    Ok(out)
}

/// "Add A, B; drop C" for a move of your own.
pub fn move_title(adds: &[String], drops: &[String], names: &HashMap<String, String>) -> String {
    let named = |cs: &[String]| cs.iter().map(|c| names.get(c).cloned().unwrap_or_else(|| c.clone())).collect::<Vec<_>>().join(", ");
    match (adds.is_empty(), drops.is_empty()) {
        (false, false) => format!("Add {}; drop {}", named(adds), named(drops)),
        (false, true) => format!("Add {}", named(adds)),
        (true, false) => format!("Drop {}", named(drops)),
        (true, true) => "Move".to_string(),
    }
}

/// Counts and rates of one group of move rows.
pub fn summarize(label: &str, rows: &[&Value]) -> Value {
    let count = |v: &str| rows.iter().filter(|r| r["verdict"] == v).count() as i64;
    let (right, wrong, ahead, behind) = (count("right"), count("wrong"), count("ahead"), count("behind"));
    let final_n = right + wrong;
    let (lo, hi) = wilson(right, final_n);
    let read_n = final_n + ahead + behind;
    let pts: Vec<(f64, f64)> = rows
        .iter()
        .filter(|r| r["units"] == "pts")
        .filter_map(|r| Some((r["predicted_so_far"].as_f64()?, r["realized"].as_f64()?)))
        .collect();
    json!({
        "label": label,
        "n": rows.len(),
        "right": right, "wrong": wrong, "ahead": ahead, "behind": behind,
        "no_result": count("no result"),
        "final_rate": if final_n > 0 { json!(round(right as f64 / final_n as f64, 4)) } else { Value::Null },
        "final_ci": [ofv(lo, 4), ofv(hi, 4)],
        "so_far_rate": if read_n > 0 { json!(round((right + ahead) as f64 / read_n as f64, 4)) } else { Value::Null },
        "predicted_pts": ofv(Some(py_sum(pts.iter().map(|p| p.0))).filter(|_| !pts.is_empty()), 1),
        "realized_pts": ofv(Some(py_sum(pts.iter().map(|p| p.1))).filter(|_| !pts.is_empty()), 1),
    })
}

pub const KIND_ORDER: [&str; 8] = ["waiver", "trade", "injury", "lineup", "sell_high", "buy_low", "alert", "user_only"];
pub const GROUP_ORDER: [(&str, &str); 3] =
    [("followed", "Suggestions you took"), ("not_followed", "Suggestions you didn't take"), ("own", "Your own moves")];

pub fn move_review(ledger: &Ledger, league: &str, limit: usize) -> Result<Value> {
    let rows = move_rows(ledger, league)?;
    let mut by_group = Vec::new();
    for (g, label) in GROUP_ORDER {
        let rs: Vec<&Value> = rows.iter().filter(|r| r["group"] == g).collect();
        if !rs.is_empty() {
            let mut s = summarize(label, &rs);
            s["group"] = json!(g);
            by_group.push(s);
        }
    }
    let mut kinds: Vec<String> = rows.iter().filter_map(|r| r["kind"].as_str().map(String::from)).collect::<HashSet<_>>().into_iter().collect();
    kinds.sort_by_key(|k| (KIND_ORDER.iter().position(|o| o == k).unwrap_or(KIND_ORDER.len()), k.clone()));
    let by_kind: Vec<Value> = kinds
        .iter()
        .map(|k| {
            let rs: Vec<&Value> = rows.iter().filter(|r| r["kind"] == k.as_str()).collect();
            let mut s = summarize(k, &rs);
            s["kind"] = json!(k);
            s
        })
        .collect();
    let all: Vec<&Value> = rows.iter().collect();
    Ok(json!({
        "total": summarize("All graded moves", &all),
        "by_group": by_group,
        "by_kind": by_kind,
        "moves": rows.iter().take(limit).cloned().collect::<Vec<_>>(),
        "n_moves": rows.len(),
    }))
}

// --------------------------------------------------------------------------- free-agent ranking

struct ProjRow {
    cid: String,
    nhl_id: i64,
    fpg: f64,
    pct_owned: f64,
    hot: f64,
}

/// Mean realized points of the first `k` of `order` (indices into `vals`).
fn top_mean(order: &[usize], vals: &[f64], k: usize) -> Option<f64> {
    let top: Vec<f64> = order.iter().take(k).map(|&i| vals[i]).collect();
    if top.len() < k {
        None
    } else {
        mean(&top)
    }
}

fn order_by(pool: &[ProjRow], key: impl Fn(&ProjRow) -> f64) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..pool.len()).collect();
    idx.sort_by(|&a, &b| {
        key(&pool[b]).partial_cmp(&key(&pool[a])).unwrap_or(std::cmp::Ordering::Equal).then_with(|| pool[a].cid.cmp(&pool[b].cid))
    });
    idx
}

/// For every archived projection day whose window has fully played out: the free agents (players
/// on no fantasy roster that day) ranked by the model's FPG, by % rostered (market) and by
/// season-to-date points per game (hot), and the realized points of each ranker's top `TOP_K`.
pub fn fa_ranking(ledger: &Ledger, league: &str, today: NaiveDate) -> Result<Value> {
    let (cfg, _) = scoring_config(ledger, league)?;
    let Some(sc) = points_scorer(cfg.as_ref()) else {
        return Ok(json!({"note": "not a points league (or no scoring known): ranking not checked", "windows": []}));
    };
    let l = league.to_string();
    let mut proj: HashMap<NaiveDate, Vec<ProjRow>> = HashMap::new();
    for r in ledger.query(
        "SELECT as_of, cid, nhl_id, fpg, pct_owned, inputs_json FROM projections WHERE league=? AND nhl_id IS NOT NULL",
        &[&l],
    )? {
        let (Some(day), Some(cid), Some(nhl_id), Some(fpg)) =
            (s_of(&r, "as_of").as_deref().and_then(parse_day), s_of(&r, "cid"), i_of(&r, "nhl_id"), f_of(&r, "fpg"))
        else {
            continue;
        };
        let inputs: Value = s_of(&r, "inputs_json").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
        let hot = fpg_of(inputs.get("season"), &sc).unwrap_or(f64::NEG_INFINITY);
        proj.entry(day).or_default().push(ProjRow { cid: cid.to_string(), nhl_id, fpg, pct_owned: f_of(&r, "pct_owned").unwrap_or(0.0), hot });
    }
    let mut rostered: HashMap<NaiveDate, HashSet<String>> = HashMap::new();
    for r in ledger.query("SELECT day, cid FROM lineup_days WHERE league=?", &[&l])? {
        if let (Some(day), Some(cid)) = (s_of(&r, "day").as_deref().and_then(parse_day), s_of(&r, "cid")) {
            rostered.entry(day).or_default().insert(cid.to_string());
        }
    }
    let first = proj.keys().min().copied().unwrap_or(today);
    let realized = Realized::new(ledger, &sc, Some(first), Some(today))?;
    let mut days: Vec<NaiveDate> = proj.keys().copied().collect();
    days.sort();
    let mut no_roster = 0usize;
    // per window: per evaluated day (model, market, hot, best, median, rho model, rho market, rho hot)
    let mut per: Vec<Vec<[Option<f64>; 8]>> = vec![Vec::new(); RANK_WINDOWS.len()];
    let mut pending = vec![0usize; RANK_WINDOWS.len()];
    for day in &days {
        // ESPN rosters come from the previous day's box scores, Fantrax from the same day
        let Some(roster) = rostered.get(day).or_else(|| rostered.get(&(*day - Duration::days(1)))) else {
            no_roster += 1;
            continue;
        };
        let pool: Vec<ProjRow> = proj[day].iter().filter(|p| !roster.contains(&p.cid)).map(|p| ProjRow { cid: p.cid.clone(), ..*p }).collect();
        if pool.len() < TOP_K {
            continue;
        }
        for (wi, n) in RANK_WINDOWS.iter().enumerate() {
            let window = days_between(*day + Duration::days(1), *day + Duration::days(*n));
            if *window.last().expect("non-empty window") >= today || !realized.covered(&window) {
                pending[wi] += 1;
                continue;
            }
            let vals: Vec<f64> = pool.iter().map(|p| realized.window(p.nhl_id, &window).1).collect();
            let model = order_by(&pool, |p| p.fpg);
            let market = order_by(&pool, |p| p.pct_owned);
            let hot = order_by(&pool, |p| p.hot);
            let mut sorted = vals.clone();
            sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
            let best = mean(&sorted[..TOP_K]);
            let median = crate::metrics::percentile(&vals, 50.0);
            let rho = |key: &dyn Fn(&ProjRow) -> f64| -> Option<f64> {
                let xs: Vec<f64> = pool.iter().map(|p| key(p).max(-1e9)).collect();
                Some(spearman(&xs, &vals)).filter(|r| r.is_finite())
            };
            per[wi].push([
                top_mean(&model, &vals, TOP_K),
                top_mean(&market, &vals, TOP_K),
                top_mean(&hot, &vals, TOP_K),
                best,
                median,
                rho(&|p| p.fpg),
                rho(&|p| p.pct_owned),
                rho(&|p| p.hot),
            ]);
        }
    }
    let windows: Vec<Value> = RANK_WINDOWS
        .iter()
        .enumerate()
        .map(|(wi, n)| {
            let rows = &per[wi];
            let col = |i: usize| -> Vec<f64> { rows.iter().filter_map(|r| r[i]).collect() };
            let beats = |other: usize| rows.iter().filter(|r| matches!((r[0], r[other]), (Some(m), Some(o)) if m > o)).count();
            json!({
                "days": n,
                "evaluated": rows.len(),
                "pending": pending[wi],
                "early": rows.len() < RANK_EARLY_DAYS,
                "model": ofv(mean(&col(0)), 2),
                "market": ofv(mean(&col(1)), 2),
                "hot": ofv(mean(&col(2)), 2),
                "best": ofv(mean(&col(3)), 2),
                "median": ofv(mean(&col(4)), 2),
                "beat_market": beats(1),
                "beat_hot": beats(2),
                "rho_model": ofv(mean(&col(5)), 3),
                "rho_market": ofv(mean(&col(6)), 3),
                "rho_hot": ofv(mean(&col(7)), 3),
            })
        })
        .collect();
    Ok(json!({
        "top_k": TOP_K,
        "projection_days": days.len(),
        "days_without_rosters": no_roster,
        "first_day": days.first().map(|d| iso(*d)),
        "windows": windows,
        "note": Value::Null,
    }))
}

/// Both views for one league.
pub fn review(ledger: &Ledger, league: &str, today: NaiveDate, limit: usize) -> Result<Value> {
    Ok(json!({"league": league, "moves": move_review(ledger, league, limit)?, "ranking": fa_ranking(ledger, league, today)?}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts() {
        assert_eq!(verdict(Some(3.0), Some(1), true), "right");
        assert_eq!(verdict(Some(-3.0), Some(0), true), "wrong");
        assert_eq!(verdict(Some(1.0), Some(1), false), "ahead");
        assert_eq!(verdict(Some(-1.0), Some(0), false), "behind");
        assert_eq!(verdict(None, None, true), "no result");
    }

    #[test]
    fn groups() {
        assert_eq!(group_of("followed"), "followed");
        assert_eq!(group_of("proposed"), "followed");
        assert_eq!(group_of("ignored"), "not_followed");
        assert_eq!(group_of("open"), "not_followed");
        assert_eq!(group_of("user_only"), "own");
    }
}
