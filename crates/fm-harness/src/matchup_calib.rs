//! Rust port of `fantasy_manager/harness/matchup_calib.py`: matchup win-probability calibration.
//! Records what `analysis.matchup` predicted every day, the final score once the period ends,
//! and grades the predictions (Brier, log loss, reliability bins, fitted SD scale).
//! Owner: work package `W8-outlook-playoffs-calib`.
//!
//! Deviations (behaviour-preserving):
//! * `capture_matchup` takes the clock (`now`) and the schedule memo explicitly, and optional
//!   closures for `matchup_fn` / `finals_fn`; its warnings use `Error` where Python printed the
//!   exception class name.
//! * `record_snapshot` / `snapshot_row` take an [`fm_engine::analysis::matchup::Matchup`].
//! * Reports are `serde_json::Value`s in the Python dict's key order.

use anyhow::Result;
use chrono::{Duration, NaiveDate};
use fm_core::models::LeagueContext;
use fm_core::provider::LeagueProvider;
use fm_core::values::PlayerValue;
use fm_core::{Cid, Map};
use fm_engine::analysis::matchup::{
    current_matchup, parse_fantrax_schedule, Matchup, MatchupOpts, SdFit, CHASE_BELOW, GOALIE_SD_K, PROTECT_ABOVE, SKATER_SD_K,
};
use fm_engine::valuation::schedule::ScheduleMemo;
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::{py_sum, round_to};
use fm_core::PyDateTime;
use indexmap::IndexMap;
use serde_json::{json, Map as JsonMap, Value};

use crate::ledger::{json_to_sql, now_iso, Ledger, Row};

/// completed matchups before the SD scale is fitted
pub const MIN_MATCHUPS: usize = 8;
pub const RELIABLE_MATCHUPS: usize = 20;
/// re-read a final score this many days after the period ends
pub const RECHECK_DAYS: i64 = 3;
/// snapshots with a smaller margin SD (nothing left to play) are not graded
pub const SD_MIN: f64 = 0.5;
pub const LOGLOSS_EPS: f64 = 1e-3;
pub const BINS: usize = 5;
/// fitted scale within this band: "about right"
pub const SCALE_OK: (f64, f64) = (0.9, 1.1);

/// One provider final score: `{mine, theirs, opponent_id, source}`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Final {
    pub mine: Option<f64>,
    pub theirs: Option<f64>,
    pub opponent_id: Option<String>,
    pub source: Option<String>,
}

// --------------------------------------------------------------------------- recording

/// `_num(v)`: a finite float out of a number / numeric string, else None.
pub fn num(v: Option<&Value>) -> Option<f64> {
    let f = match v? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.replace(',', "").trim().parse::<f64>().ok()?,
        _ => return None,
    };
    f.is_finite().then_some(f)
}

/// A `matchup_snapshots` row from a [`Matchup`]; None when there is no opponent or win
/// probability to record.
pub fn snapshot_row(league: &str, day: NaiveDate, m: &Matchup) -> Option<Row> {
    let p = m.win_probability?;
    m.opponent_team.as_ref()?;
    let (my_sd, their_sd) = (m.my_sd, m.their_sd);
    let mine = m.my_points_so_far + m.my_projected_remaining;
    let theirs = m.their_points_so_far + m.their_projected_remaining;
    let mut r = JsonMap::new();
    r.insert("league".into(), json!(league));
    r.insert("day".into(), json!(day.format("%Y-%m-%d").to_string()));
    r.insert("period".into(), json!(m.period));
    r.insert("period_start".into(), json!(m.start.map(|d| d.format("%Y-%m-%d").to_string())));
    r.insert("period_end".into(), json!(m.end.map(|d| d.format("%Y-%m-%d").to_string())));
    r.insert("opponent_id".into(), json!(m.opponent_id));
    r.insert("opponent".into(), json!(m.opponent_team));
    r.insert("days_left".into(), json!(m.days_left));
    r.insert("playoffs".into(), json!(m.playoffs as i64));
    r.insert("my_so_far".into(), json!(m.my_points_so_far));
    r.insert("their_so_far".into(), json!(m.their_points_so_far));
    r.insert("my_proj_total".into(), json!(mine));
    r.insert("their_proj_total".into(), json!(theirs));
    r.insert("margin".into(), json!(mine - theirs));
    r.insert("my_sd".into(), json!(my_sd));
    r.insert("their_sd".into(), json!(their_sd));
    r.insert("margin_sd".into(), json!((my_sd * my_sd + their_sd * their_sd).sqrt()));
    r.insert("win_prob".into(), json!(p));
    r.insert("skater_sd_k".into(), json!(m.sd_model.skater_k));
    r.insert("goalie_sd_k".into(), json!(m.sd_model.goalie_k));
    r.insert("source".into(), json!(m.source));
    r.insert("recorded_at".into(), json!(now_iso()));
    Some(r)
}

/// Store today's snapshot once: "recorded", "already recorded today" or "nothing to record".
pub fn record_snapshot(ledger: &Ledger, league: &str, day: NaiveDate, m: &Matchup) -> Result<String> {
    let Some(row) = snapshot_row(league, day, m) else {
        return Ok("nothing to record (no opponent or win probability)".to_string());
    };
    let cols: Vec<&String> = row.keys().collect();
    let sql = format!(
        "INSERT OR IGNORE INTO matchup_snapshots ({}) VALUES ({})",
        cols.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", "),
        vec!["?"; cols.len()].join(", ")
    );
    let vals: Vec<rusqlite::types::Value> = row.values().map(json_to_sql).collect();
    let refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    let n = ledger.execute(&sql, &refs)?;
    ledger.commit()?;
    Ok(if n > 0 { "recorded" } else { "already recorded today" }.to_string())
}

/// Ended periods (`period_end` before `today`) without a result, or still inside the re-check
/// window.
pub fn pending_periods(ledger: &Ledger, league: &str, today: NaiveDate) -> Result<Vec<Row>> {
    let recheck = (today - Duration::days(RECHECK_DAYS)).format("%Y-%m-%d").to_string();
    let today_s = today.format("%Y-%m-%d").to_string();
    ledger.query(
        "SELECT s.period, s.period_start, MAX(s.period_end) period_end FROM matchup_snapshots s \
         LEFT JOIN matchup_results r ON r.league=s.league AND r.period=s.period AND r.period_end=s.period_end \
         WHERE s.league=? AND s.period IS NOT NULL AND s.period_end IS NOT NULL AND s.period_end < ? \
         AND (r.result IS NULL OR s.period_end >= ?) GROUP BY s.period, s.period_end ORDER BY s.period_end",
        &[&league, &today_s, &recheck],
    )
}

pub fn result_of(mine: f64, theirs: f64) -> &'static str {
    if (mine - theirs).abs() < 0.005 {
        "tie"
    } else if mine > theirs {
        "win"
    } else {
        "loss"
    }
}

/// Upsert `matchup_results` for every pending period with a final score; returns rows.
pub fn attach_results(ledger: &Ledger, league: &str, finals: &Map<i64, Final>, pending: &[Row]) -> Result<usize> {
    let mut rows: Vec<Row> = Vec::new();
    for p in pending {
        let Some(period) = p.get("period").and_then(|v| v.as_i64()) else { continue };
        let Some(f) = finals.get(&period) else { continue };
        let (Some(mine), Some(theirs)) = (f.mine, f.theirs) else { continue };
        let mut r = JsonMap::new();
        r.insert("league".into(), json!(league));
        r.insert("period".into(), json!(period));
        r.insert("period_end".into(), p.get("period_end").cloned().unwrap_or(Value::Null));
        r.insert("period_start".into(), p.get("period_start").cloned().unwrap_or(Value::Null));
        r.insert("opponent_id".into(), json!(f.opponent_id));
        r.insert("my_final".into(), json!(mine));
        r.insert("their_final".into(), json!(theirs));
        r.insert("result".into(), json!(result_of(mine, theirs)));
        r.insert("source".into(), json!(f.source));
        r.insert("recorded_at".into(), json!(now_iso()));
        rows.push(r);
    }
    ledger.upsert("matchup_results", &rows, &["league", "period", "period_end"])
}

// --------------------------------------------------------------------------- provider final scores

fn id_str(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn espn_finals(ctx: &LeagueContext, provider: &dyn LeagueProvider, periods: &[i64]) -> Result<Map<i64, Final>> {
    let data = provider.espn_league_get(&[("view".to_string(), "mMatchupScore".to_string())])?;
    let my_id: i64 = ctx.my_team()?.team_id.trim().parse()?;
    let mut out: Map<i64, Final> = Map::new();
    let empty: Vec<Value> = vec![];
    for m in data.get("schedule").and_then(|s| s.as_array()).unwrap_or(&empty) {
        let Some(mp) = m.get("matchupPeriodId").and_then(|v| v.as_i64()) else { continue };
        if !periods.contains(&mp) {
            continue;
        }
        let null = Value::Null;
        let home = m.get("home").filter(|v| v.is_object()).unwrap_or(&null);
        let away = m.get("away").filter(|v| v.is_object()).unwrap_or(&null);
        let (hid, aid) = (home.get("teamId").and_then(|v| v.as_i64()), away.get("teamId").and_then(|v| v.as_i64()));
        if Some(my_id) != hid && Some(my_id) != aid {
            continue;
        }
        let winner = match m.get("winner") {
            Some(Value::String(s)) => s.to_uppercase(),
            _ => String::new(),
        };
        if winner == "UNDECIDED" {
            continue; // ESPN has not finalized the period yet
        }
        let (me, them) = if hid == Some(my_id) { (home, away) } else { (away, home) };
        let pts = |side: &Value| num(side.get("totalPoints")).or_else(|| num(side.get("totalPointsLive")));
        let them_present = them.as_object().is_some_and(|o| !o.is_empty());
        out.insert(
            mp,
            Final {
                mine: pts(me),
                theirs: if them_present { pts(them) } else { None },
                opponent_id: them.get("teamId").and_then(id_str),
                source: Some("ESPN scoreboard".to_string()),
            },
        );
    }
    Ok(out)
}

/// Finals from the Fantrax official API, one `getMatchupScores` call per period. Verified on 2026-11-10
/// for a finished period: period 6 gave 191.6 to 197.55, the final the private schedule view had stored.
/// Periods it cannot answer (no official API, a failed read, playoff placeholders) are left out, and each
/// failure adds its warning once.
fn official_fantrax_finals(ctx: &LeagueContext, provider: &dyn LeagueProvider, periods: &[i64], warnings: &mut Vec<String>) -> Map<i64, Final> {
    let mut out: Map<i64, Final> = Map::new();
    let Ok(my) = ctx.my_team() else { return out };
    for &period in periods {
        match provider.official_matchup(&my.team_id, period) {
            Ok(Some(m)) => {
                out.insert(
                    period,
                    Final {
                        mine: Some(m.my_points),
                        theirs: Some(m.their_points),
                        opponent_id: Some(m.opponent_id),
                        source: Some("Fantrax official API".to_string()),
                    },
                );
            }
            Ok(None) => {}
            Err(w) => {
                if !warnings.contains(&w) {
                    warnings.push(w);
                }
            }
        }
    }
    out
}

/// Official finals first, then the private schedule view for the periods still missing; an error of that
/// private call only fails the load when the official API gave nothing.
fn fantrax_finals(ctx: &LeagueContext, provider: &dyn LeagueProvider, periods: &[i64], warnings: &mut Vec<String>) -> Result<Map<i64, Final>> {
    let mut out = official_fantrax_finals(ctx, provider, periods, warnings);
    let missing: Vec<i64> = periods.iter().copied().filter(|p| !out.contains_key(p)).collect();
    if missing.is_empty() {
        return Ok(out);
    }
    match private_fantrax_finals(ctx, provider, &missing) {
        Ok(private) => out.extend(private),
        Err(e) if out.is_empty() => return Err(e),
        Err(_) => {}
    }
    Ok(out)
}

fn private_fantrax_finals(ctx: &LeagueContext, provider: &dyn LeagueProvider, periods: &[i64]) -> Result<Map<i64, Final>> {
    let mut params: Map<String, Value> = Map::new();
    params.insert("view".to_string(), Value::String("SCHEDULE".into()));
    let data = provider.fantrax_call(&[("getStandings".to_string(), params)], false)?;
    let data = data.into_iter().next().ok_or_else(|| anyhow::anyhow!("empty getStandings response"))?;
    let my_id = Value::String(ctx.my_team()?.team_id.clone());
    let mut out: Map<i64, Final> = Map::new();
    for t in parse_fantrax_schedule(&data) {
        let Some(number) = t.number.filter(|n| periods.contains(n)) else { continue };
        for (away, apts, home, hpts) in &t.games {
            if *away != my_id && *home != my_id {
                continue;
            }
            let mine_away = *away == my_id;
            let opp = if mine_away { home } else { away };
            out.insert(
                number,
                Final {
                    mine: Some(if mine_away { *apts } else { *hpts }),
                    theirs: Some(if mine_away { *hpts } else { *apts }),
                    opponent_id: id_str(opp),
                    source: Some("Fantrax schedule".to_string()),
                },
            );
        }
    }
    Ok(out)
}

/// `{period: Final}` for the given ended periods (errors on provider failures; the caller
/// treats them as warnings). `warnings` collects the Fantrax official API read failures that the
/// private fallback covered.
pub fn fetch_final_scores(
    ctx: &LeagueContext,
    provider: Option<&dyn LeagueProvider>,
    periods: &[i64],
    warnings: &mut Vec<String>,
) -> Result<Map<i64, Final>> {
    let Some(provider) = provider else { return Ok(Map::new()) };
    if periods.is_empty() {
        return Ok(Map::new());
    }
    match ctx.provider.as_str() {
        "espn" => espn_finals(ctx, provider, periods),
        "fantrax" => fantrax_finals(ctx, provider, periods, warnings),
        _ => Ok(Map::new()),
    }
}

/// `matchup_fn` / `finals_fn` overrides for [`capture_matchup`] (tests).
pub type MatchupFn<'a> = &'a dyn Fn() -> Result<Matchup>;
pub type FinalsFn<'a> = &'a dyn Fn(&[i64]) -> Result<Map<i64, Final>>;

/// The daily step: snapshot today's matchup, then attach final scores of ended periods. Never
/// fails; problems land in `warnings`.
#[allow(clippy::too_many_arguments)]
pub fn capture_matchup(
    ledger: &Ledger,
    league: &str,
    ctx: &LeagueContext,
    provider: Option<&dyn LeagueProvider>,
    values: &Map<Cid, PlayerValue>,
    today: NaiveDate,
    now: &PyDateTime,
    memo: &ScheduleMemo,
    matchup_fn: Option<MatchupFn<'_>>,
    finals_fn: Option<FinalsFn<'_>>,
) -> Value {
    let mut warnings: Vec<Value> = Vec::new();
    let mut out: JsonMap<String, Value> = JsonMap::new();
    let snap: Result<(String, Option<(Option<i64>, f64)>)> = (|| {
        let m = match matchup_fn {
            Some(f) => f()?,
            None => {
                let seed: i64 = today.format("%Y%m%d").to_string().parse()?;
                let opts = MatchupOpts { today: Some(today), streaming: false, seed: Some(seed), ..MatchupOpts::default() };
                current_matchup(ctx, provider, Some(values), opts, now, memo)?
            }
        };
        let status = record_snapshot(ledger, league, today, &m)?;
        Ok((status, m.win_probability.map(|p| (m.period, p))))
    })();
    let mut snapshot_ok = false;
    match snap {
        Ok((status, extra)) => {
            snapshot_ok = true;
            out.insert("snapshot".into(), json!(status));
            if let Some((period, p)) = extra {
                out.insert("period".into(), json!(period));
                out.insert("win_prob".into(), json!(round_to(p, 3)));
            }
        }
        Err(e) => {
            out.insert("snapshot".into(), json!("failed"));
            warnings.push(json!(format!("matchup snapshot failed: Error: {e}")));
        }
    }
    let _ = snapshot_ok;
    let mut results = 0usize;
    match pending_periods(ledger, league, today) {
        Ok(pend) if !pend.is_empty() => {
            let periods: Vec<i64> = pend.iter().filter_map(|p| p.get("period").and_then(|v| v.as_i64())).collect();
            let mut official_warnings: Vec<String> = Vec::new();
            let finals = match finals_fn {
                Some(f) => f(&periods),
                None => fetch_final_scores(ctx, provider, &periods, &mut official_warnings),
            };
            warnings.extend(official_warnings.into_iter().map(Value::from));
            match finals.and_then(|f| attach_results(ledger, league, &f, &pend)) {
                Ok(n) => results = n,
                Err(e) => warnings.push(json!(format!("matchup final scores unavailable: Error: {e}"))),
            }
        }
        Ok(_) => {}
        Err(e) => warnings.push(json!(format!("matchup final scores unavailable: Error: {e}"))),
    }
    out.insert("results".into(), json!(results));
    let mut res = JsonMap::new();
    res.insert("warnings".into(), Value::Array(warnings));
    for (k, v) in out {
        res.insert(k, v);
    }
    Value::Object(res)
}

// --------------------------------------------------------------------------- grading

pub fn outcome_value(result: &str) -> f64 {
    match result {
        "win" => 1.0,
        "loss" => 0.0,
        _ => 0.5,
    }
}

pub fn brier(ps: &[f64], ys: &[f64]) -> Option<f64> {
    if ps.is_empty() {
        return None;
    }
    Some(py_sum(ps.iter().zip(ys).map(|(p, y)| (p - y) * (p - y))) / ps.len() as f64)
}

/// Mean binary cross-entropy; probabilities are clipped to [eps, 1 - eps], a tie is y = 0.5.
pub fn log_loss(ps: &[f64], ys: &[f64], eps: f64) -> Option<f64> {
    if ps.is_empty() {
        return None;
    }
    let mut tot = 0.0;
    for (p, y) in ps.iter().zip(ys) {
        let p = (1.0 - eps).min(eps.max(*p));
        tot -= y * p.ln() + (1.0 - y) * (1.0 - p).ln();
    }
    Some(tot / ps.len() as f64)
}

/// Equal-width bins of predicted win probability: n, mean predicted, observed win rate.
pub fn reliability(ps: &[f64], ys: &[f64], bins: usize) -> Vec<Value> {
    let mut out = Vec::new();
    for b in 0..bins {
        let (lo, hi) = (b as f64 / bins as f64, (b + 1) as f64 / bins as f64);
        let idx: Vec<usize> = (0..ps.len()).filter(|&i| (lo <= ps[i] && ps[i] < hi) || (b == bins - 1 && ps[i] == 1.0)).collect();
        let mean = |xs: &[f64]| -> Value {
            if idx.is_empty() {
                Value::Null
            } else {
                json!(py_sum(idx.iter().map(|&i| xs[i])) / idx.len() as f64)
            }
        };
        out.push(json!({"lo": round_to(lo, 3), "hi": round_to(hi, 3), "n": idx.len(), "mean_pred": mean(ps), "observed": mean(ys)}));
    }
    out
}

/// ML scale of a zero-mean normal error from standardized errors `z` grouped by matchup:
/// `sqrt(mean over matchups of mean z**2)` (equal weight per matchup).
pub fn fit_sd_scale<K>(groups: &IndexMap<K, Vec<f64>>) -> Option<f64> {
    let per: Vec<f64> = groups.values().filter(|zs| !zs.is_empty()).map(|zs| py_sum(zs.iter().map(|z| z * z)) / zs.len() as f64).collect();
    if per.is_empty() {
        return None;
    }
    Some((py_sum(per.iter().copied()) / per.len() as f64).sqrt())
}

fn phi(x: f64) -> f64 {
    0.5 * (1.0 + libm::erf(x / 2.0_f64.sqrt()))
}

pub fn trust_for(n_matchups: usize, min_matchups: usize) -> &'static str {
    if n_matchups < min_matchups {
        "hidden"
    } else if n_matchups >= RELIABLE_MATCHUPS {
        "reliable"
    } else {
        "provisional"
    }
}

/// Snapshots of completed matchups (period still being played on the snapshot day) joined with
/// their result.
pub fn graded_rows(ledger: &Ledger, league: &str) -> Result<Vec<Row>> {
    ledger.query(
        "SELECT s.*, r.my_final, r.their_final, r.result FROM matchup_snapshots s \
         JOIN matchup_results r ON r.league=s.league AND r.period=s.period AND r.period_end=s.period_end \
         WHERE s.league=? AND s.days_left > 0 AND s.win_prob IS NOT NULL ORDER BY s.day",
        &[&league],
    )
}

fn fv(r: &Row, k: &str) -> f64 {
    r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

fn opt_f(r: &Row, k: &str) -> Option<f64> {
    r.get(k).and_then(|v| v.as_f64())
}

fn matchup_key(r: &Row) -> (i64, String) {
    (
        r.get("period").and_then(|v| v.as_i64()).unwrap_or(0),
        r.get("period_end").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    )
}

fn opt(x: Option<f64>) -> Value {
    x.map_or(Value::Null, |v| json!(v))
}

/// A snapshot's margin SD restated with the reference constants (SKATER_SD_K, GOALIE_SD_K), so
/// the fitted scale is always a multiplier on those constants, whatever the snapshot used (the
/// league point scale or an earlier fitted scale). The ratio is taken on the skater constant; it
/// is exact when the snapshot scaled skaters and goalies alike (a fitted scale, or the reference)
/// and approximate for the goalie share when the league's skater and goalie point scales differ.
fn reference_margin_sd(r: &Row) -> f64 {
    let used = opt_f(r, "skater_sd_k").filter(|k| *k > 0.0).unwrap_or(SKATER_SD_K);
    fv(r, "margin_sd") * SKATER_SD_K / used
}

/// Weight of a scale fitted on `n` matchups against the league's default: n / (n + MIN_MATCHUPS).
/// The default counts as a prior worth MIN_MATCHUPS matchups, the fewest the harness fits from, so
/// a first fit gets half the weight (its SD estimate has about 1/sqrt(2n) = 25% sampling error at
/// n = 8) and the default fades as matchups accrue (0.71 at RELIABLE_MATCHUPS).
pub fn sd_fit_weight(n: usize) -> f64 {
    n as f64 / (n + MIN_MATCHUPS) as f64
}

/// The fitted SD scale the matchup simulation should use for `league`: the report's scale and its
/// weight once `MIN_MATCHUPS` matchups are graded, else None (the simulation keeps its default).
/// Err when the ledger cannot be read, so the caller can keep what it had installed.
pub fn applied_sd_scale(ledger: &Ledger, league: &str) -> Result<Option<SdFit>> {
    let rep = calibration_report(ledger, league, MIN_MATCHUPS)?;
    Ok(rep.get("scale").and_then(|sc| {
        let s = sc.get("s")?.as_f64()?;
        let n = sc.get("n_matchups")?.as_u64()? as usize;
        Some(SdFit { scale: s, weight: sd_fit_weight(n) })
    }))
}

/// Brier, log loss, reliability bins and the fitted SD scale for one league. The scale is a
/// multiplier on the reference constants and is applied by the simulation (`applied_sd_scale`),
/// blended with the league's default at `weight`; Python only suggested it for a manual change.
pub fn calibration_report(ledger: &Ledger, league: &str, min_matchups: usize) -> Result<Value> {
    let rows = graded_rows(ledger, league)?;
    let n_snap = ledger.count("matchup_snapshots", "WHERE league=?", &[&league])?;
    let mut last: IndexMap<(i64, String), &Row> = IndexMap::new();
    for r in &rows {
        last.insert(matchup_key(r), r); // one row per matchup (the last wins)
    }
    let n_keys = last.len();
    let result_of_row = |r: &Row| r.get("result").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let ps: Vec<f64> = rows.iter().map(|r| fv(r, "win_prob")).collect();
    let ys: Vec<f64> = rows.iter().map(|r| outcome_value(&result_of_row(r))).collect();
    let mut rep: JsonMap<String, Value> = JsonMap::new();
    rep.insert("league".into(), json!(league));
    rep.insert("snapshots".into(), json!(n_snap));
    rep.insert("graded_snapshots".into(), json!(rows.len()));
    rep.insert("matchups".into(), json!(n_keys));
    rep.insert("min_matchups".into(), json!(min_matchups));
    rep.insert("trust".into(), json!(trust_for(n_keys, min_matchups)));
    rep.insert("brier".into(), opt(brier(&ps, &ys)));
    rep.insert("log_loss".into(), opt(log_loss(&ps, &ys, LOGLOSS_EPS)));
    rep.insert("brier_coin_flip".into(), if rows.is_empty() { Value::Null } else { json!(0.25) });
    rep.insert("reliability".into(), Value::Array(if rows.is_empty() { vec![] } else { reliability(&ps, &ys, BINS) }));
    let mut record = JsonMap::new();
    for k in ["win", "loss", "tie"] {
        record.insert(k.into(), json!(last.values().filter(|r| result_of_row(r) == k).count()));
    }
    rep.insert("record".into(), Value::Object(record));
    rep.insert("scale".into(), Value::Null);
    rep.insert("status".into(), if rows.is_empty() { json!("no completed matchups yet") } else { Value::Null });
    // stance check: how often a "chase" (< 0.40) / "protect" (> 0.60) snapshot ended in a win
    let (lo, hi) = (CHASE_BELOW, PROTECT_ABOVE);
    for (name, sel) in [
        ("chase", ps.iter().zip(&ys).filter(|(p, _)| **p < lo).map(|(_, y)| *y).collect::<Vec<_>>()),
        ("protect", ps.iter().zip(&ys).filter(|(p, _)| **p > hi).map(|(_, y)| *y).collect::<Vec<_>>()),
    ] {
        rep.insert(
            format!("{name}_win_rate"),
            json!({"n": sel.len(),
                   "rate": if sel.is_empty() { Value::Null } else { json!(py_sum(sel.iter().copied()) / sel.len() as f64) },
                   "threshold": if name == "chase" { lo } else { hi }}),
        );
    }
    let mut groups: IndexMap<(i64, String), Vec<f64>> = IndexMap::new();
    let fit_rows: Vec<&Row> = rows.iter().filter(|r| opt_f(r, "margin_sd").unwrap_or(0.0) >= SD_MIN).collect();
    for r in &fit_rows {
        let actual = fv(r, "my_final") - fv(r, "their_final");
        groups.entry(matchup_key(r)).or_default().push((actual - fv(r, "margin")) / reference_margin_sd(r));
    }
    if !rows.is_empty() && n_keys < min_matchups {
        rep.insert("status".into(), json!(format!("insufficient data ({n_keys} of {min_matchups} completed matchups)")));
    } else if !rows.is_empty() {
        match fit_sd_scale(&groups) {
            None => {
                rep.insert("status".into(), json!("insufficient data (no snapshot with a margin SD)"));
            }
            Some(s) => {
                let sk = rows.iter().rev().find_map(|r| opt_f(r, "skater_sd_k"));
                let gk = rows.iter().rev().find_map(|r| opt_f(r, "goalie_sd_k"));
                let ps2: Vec<f64> = fit_rows.iter().map(|r| phi(fv(r, "margin") / (s * reference_margin_sd(r)))).collect();
                let ys2: Vec<f64> = fit_rows.iter().map(|r| outcome_value(&result_of_row(r))).collect();
                let verdict = if s > SCALE_OK.1 {
                    "overconfident"
                } else if s < SCALE_OK.0 {
                    "underconfident"
                } else {
                    "about right"
                };
                rep.insert(
                    "scale".into(),
                    json!({"s": s, "n_matchups": groups.len(), "n_snapshots": fit_rows.len(), "verdict": verdict,
                           "skater_sd_k": opt(sk), "goalie_sd_k": opt(gk),
                           "recommended_skater_sd_k": SKATER_SD_K * s,
                           "recommended_goalie_sd_k": GOALIE_SD_K * s,
                           "brier_if_scaled": opt(brier(&ps2, &ys2)), "applied": true,
                           "weight": sd_fit_weight(groups.len())}),
                );
                rep.insert("status".into(), json!(format!("SD scale {} ({verdict})", fixed(s, 2, false))));
            }
        }
    }
    Ok(Value::Object(rep))
}

/// One line for the CLI / Health warnings.
pub fn summary_line(rep: &Value) -> String {
    let lg = rep.get("league").and_then(|v| v.as_str()).unwrap_or("None");
    let graded = rep.get("graded_snapshots").and_then(|v| v.as_i64()).unwrap_or(0);
    if graded == 0 {
        return format!(
            "{lg}: matchup win probability: {} snapshot(s), no completed matchup yet",
            rep.get("snapshots").and_then(|v| v.as_i64()).unwrap_or(0)
        );
    }
    let b = rep.get("brier").and_then(|v| v.as_f64()).unwrap_or(f64::NAN);
    let ll = rep.get("log_loss").and_then(|v| v.as_f64()).unwrap_or(f64::NAN);
    let base = format!(
        "{lg}: matchup win probability over {} matchup(s) / {graded} snapshot(s): Brier {} (coin flip 0.250), log loss {}",
        rep.get("matchups").and_then(|v| v.as_i64()).unwrap_or(0),
        fixed(b, 3, false),
        fixed(ll, 3, false)
    );
    if let Some(sc) = rep.get("scale").filter(|s| s.is_object()) {
        let mut rec = String::new();
        if let (Some(rs), Some(rg)) =
            (sc.get("recommended_skater_sd_k").and_then(|v| v.as_f64()), sc.get("recommended_goalie_sd_k").and_then(|v| v.as_f64()))
        {
            rec = format!(
                "; SD constants {} skaters / {} goalies (applied at weight {} against the league default)",
                fixed(rs, 2, false),
                fixed(rg, 2, false),
                fixed(sc.get("weight").and_then(|v| v.as_f64()).unwrap_or(f64::NAN), 2, false)
            );
        }
        return format!(
            "{base}; fitted SD scale {} ({}){rec}",
            fixed(sc.get("s").and_then(|v| v.as_f64()).unwrap_or(f64::NAN), 2, false),
            sc.get("verdict").and_then(|v| v.as_str()).unwrap_or("")
        );
    }
    format!("{base}; {}", rep.get("status").and_then(|v| v.as_str()).unwrap_or("None"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fm_core::provider::{LeagueProvider, OfficialMatchup};
    use fm_engine::analysis::matchup::SdModel;
    use fm_core::snapshot::ProviderExtras;
    use fm_core::models::{FantasyTeam, RosterSlot};

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }
    const START: (i32, u32, u32) = (2026, 11, 2);
    const END: (i32, u32, u32) = (2026, 11, 8);

    #[allow(clippy::too_many_arguments)]
    fn mu(period: i64, p: Option<f64>, mine: (f64, f64), theirs: (f64, f64), sds: (f64, f64), days_left: i64, opp: Option<&str>) -> Matchup {
        Matchup {
            league: "espn".into(),
            my_team: "Me".into(),
            opponent_team: opp.map(str::to_string),
            opponent_id: Some("t2".into()),
            period: Some(period),
            start: Some(d(START.0, START.1, START.2)),
            end: Some(d(END.0, END.1, END.2)),
            from_day: None,
            days_left,
            playoffs: false,
            my_points_so_far: mine.0,
            their_points_so_far: theirs.0,
            my_projected_remaining: mine.1,
            their_projected_remaining: theirs.1,
            my_games_left: 0,
            their_games_left: 0,
            my_sd: sds.0,
            their_sd: sds.1,
            win_probability: p,
            stance: None,
            key_players_theirs: vec![],
            gap_by_position: Map::new(),
            advice: vec![],
            my_players: vec![],
            their_players: vec![],
            source: "test".into(),
            lineup_basis: String::new(),
            warnings: vec![],
            draws: 0,
            sd_model: fm_engine::analysis::matchup::SdModel::REFERENCE,
        }
    }

    fn default_mu() -> Matchup {
        mu(1, Some(0.6), (50.0, 30.0), (45.0, 30.0), (3.0, 4.0), 5, Some("Opp"))
    }

    fn led() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::tempdir().unwrap();
        let l = Ledger::open(dir.path()).unwrap();
        (dir, l)
    }

    #[test]
    fn snapshot_recorded_once_per_league_and_day() {
        let (_d, l) = led();
        let day = d(2026, 11, 4);
        assert_eq!(record_snapshot(&l, "espn", day, &default_mu()).unwrap(), "recorded");
        assert_eq!(record_snapshot(&l, "espn", day, &default_mu()).unwrap(), "already recorded today");
        assert_eq!(record_snapshot(&l, "fantrax", day, &default_mu()).unwrap(), "recorded");
        assert_eq!(l.count("matchup_snapshots", "", &[]).unwrap(), 2);
        let row = &l.query("SELECT * FROM matchup_snapshots WHERE league='espn'", &[]).unwrap()[0];
        assert_eq!(row["margin"].as_f64().unwrap(), 5.0);
        assert!((row["margin_sd"].as_f64().unwrap() - 5.0).abs() < 1e-12);
        assert_eq!(row["skater_sd_k"].as_f64().unwrap(), SKATER_SD_K);
    }

    #[test]
    fn nothing_recorded_without_opponent_or_probability() {
        let (_d, l) = led();
        let m = mu(1, Some(0.6), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0), 5, None);
        assert!(record_snapshot(&l, "espn", d(2026, 11, 4), &m).unwrap().starts_with("nothing to record"));
        let m = mu(1, None, (0.0, 0.0), (0.0, 0.0), (0.0, 0.0), 5, Some("Opp"));
        assert!(record_snapshot(&l, "espn", d(2026, 11, 4), &m).unwrap().starts_with("nothing to record"));
        assert_eq!(l.count("matchup_snapshots", "", &[]).unwrap(), 0);
    }

    #[test]
    fn results_attach_after_period_and_are_rechecked_briefly() {
        let (_d, l) = led();
        record_snapshot(&l, "espn", d(2026, 11, 4), &default_mu()).unwrap();
        assert!(pending_periods(&l, "espn", d(2026, 11, 8)).unwrap().is_empty()); // period not over
        let pend = pending_periods(&l, "espn", d(2026, 11, 9)).unwrap();
        assert_eq!(pend.len(), 1);
        let mut finals = Map::new();
        finals.insert(1, Final { mine: Some(80.0), theirs: Some(70.0), opponent_id: Some("t2".into()), source: Some("x".into()) });
        assert_eq!(attach_results(&l, "espn", &finals, &pend).unwrap(), 1);
        // still rechecked inside the window, left alone after it
        assert_eq!(pending_periods(&l, "espn", d(2026, 11, 10)).unwrap().len(), 1);
        assert!(pending_periods(&l, "espn", d(2026, 11, 12)).unwrap().is_empty());
        let r = &l.query("SELECT * FROM matchup_results", &[]).unwrap()[0];
        assert_eq!(r["result"], "win");
        assert_eq!(result_of(70.0, 70.001), "tie");
        assert_eq!(result_of(1.0, 2.0), "loss");
    }

    #[test]
    fn brier_logloss_reliability() {
        assert_eq!(brier(&[0.5, 1.0], &[1.0, 1.0]).unwrap(), 0.125);
        assert!(brier(&[], &[]).is_none());
        let ll = log_loss(&[1.0, 0.0], &[1.0, 0.0], LOGLOSS_EPS).unwrap();
        assert!((ll - (-(1.0f64 - 1e-3).ln())).abs() < 1e-12);
        let rel = reliability(&[0.1, 0.15, 0.95, 1.0], &[0.0, 1.0, 1.0, 1.0], 5);
        assert_eq!(rel[0]["n"], 2);
        assert_eq!(rel[0]["observed"], 0.5);
        assert_eq!(rel[4]["n"], 2);
        assert_eq!(rel[2]["n"], 0);
        assert!(rel[2]["mean_pred"].is_null());
    }

    #[test]
    fn sd_scale_weights_each_matchup_equally() {
        let mut g: IndexMap<i32, Vec<f64>> = IndexMap::new();
        g.insert(1, vec![2.0, 2.0, 2.0, 2.0]);
        g.insert(2, vec![0.0]);
        // matchup means of z^2 are 4 and 0 -> sqrt(2)
        assert!((fit_sd_scale(&g).unwrap() - 2.0_f64.sqrt()).abs() < 1e-12);
        let e: IndexMap<i32, Vec<f64>> = IndexMap::new();
        assert!(fit_sd_scale(&e).is_none());
    }

    fn seed_matchups(l: &Ledger, n: i64, my_final: f64, their_final: f64) {
        seed_matchups_with(l, n, my_final, their_final, SdModel::REFERENCE);
    }

    /// `n` graded matchups, each projected 80 - 75 with SDs (3, 4) (margin SD 5) computed with
    /// `model`'s constants.
    fn seed_matchups_with(l: &Ledger, n: i64, my_final: f64, their_final: f64, model: SdModel) {
        for i in 0..n {
            let start = d(2026, 10, 1) + Duration::days(7 * i);
            let end = start + Duration::days(6);
            let mut m = mu(i + 1, Some(0.6), (50.0, 30.0), (45.0, 30.0), (3.0, 4.0), 5, Some("Opp"));
            m.sd_model = model;
            m.start = Some(start);
            m.end = Some(end);
            record_snapshot(l, "espn", start + Duration::days(2), &m).unwrap();
            let pend = pending_periods(l, "espn", end + Duration::days(1)).unwrap();
            let mut finals = Map::new();
            finals.insert(i + 1, Final { mine: Some(my_final), theirs: Some(their_final), opponent_id: None, source: None });
            attach_results(l, "espn", &finals, &pend).unwrap();
        }
    }

    #[test]
    fn report_fits_scale_with_enough_matchups_and_guards_small_samples() {
        let (_d, l) = led();
        seed_matchups(&l, 3, 80.0, 70.0);
        let rep = calibration_report(&l, "espn", MIN_MATCHUPS).unwrap();
        assert_eq!(rep["trust"], "hidden");
        assert!(rep["scale"].is_null());
        assert_eq!(rep["status"], "insufficient data (3 of 8 completed matchups)");
        assert!(summary_line(&rep).contains("insufficient data"));

        let (_d2, l2) = led();
        seed_matchups(&l2, 9, 80.0, 75.0); // actual margin 5 = projected 5 -> z = 0 -> s = 0
        let rep = calibration_report(&l2, "espn", MIN_MATCHUPS).unwrap();
        assert_eq!(rep["matchups"], 9);
        assert_eq!(rep["trust"], "provisional");
        assert_eq!(rep["scale"]["verdict"], "underconfident");
        assert_eq!(rep["record"]["win"], 9);
        assert_eq!(rep["scale"]["applied"], true);
        assert!(summary_line(&rep).contains("fitted SD scale"));

        let (_d3, l3) = led();
        seed_matchups(&l3, 9, 95.0, 70.0); // actual margin 25 vs projected 5 (sd 5): z = 4
        let rep = calibration_report(&l3, "espn", MIN_MATCHUPS).unwrap();
        assert_eq!(rep["scale"]["verdict"], "overconfident");
        assert!((rep["scale"]["s"].as_f64().unwrap() - 4.0).abs() < 1e-9);

        let (_d4, l4) = led();
        seed_matchups(&l4, 9, 80.0, 70.0); // z = 1
        let rep = calibration_report(&l4, "espn", MIN_MATCHUPS).unwrap();
        assert_eq!(rep["scale"]["verdict"], "about right");
        assert_eq!(rep["status"], "SD scale 1.00 (about right)");
    }

    #[test]
    fn fitted_scale_is_relative_to_the_reference_constants_and_applied_from_eight_matchups() {
        // snapshots taken with twice the reference constants (a league whose weights are 4x ESPN's):
        // margin SD 5 there is 5 x 1.3 / 2.6 = 2.5 with the reference constants; the actual margin
        // 15 misses the projected 5 by 10 = 4 reference SDs, so s = 4 (constants 5.2 / 10.0)
        let doubled = SdModel { skater_k: 2.0 * SKATER_SD_K, goalie_k: 2.0 * GOALIE_SD_K };
        let (_d, l) = led();
        seed_matchups_with(&l, MIN_MATCHUPS as i64 - 1, 85.0, 70.0, doubled);
        assert_eq!(applied_sd_scale(&l, "espn").unwrap(), None);
        let (_d2, l2) = led();
        seed_matchups_with(&l2, MIN_MATCHUPS as i64, 85.0, 70.0, doubled);
        let rows = graded_rows(&l2, "espn").unwrap();
        assert_eq!(rows[0]["skater_sd_k"].as_f64().unwrap(), 2.6);
        // 8 matchups against a prior worth 8: half weight
        let fit = applied_sd_scale(&l2, "espn").unwrap().unwrap();
        assert!((fit.scale - 4.0).abs() < 1e-9 && (fit.weight - 0.5).abs() < 1e-12, "{fit:?}");
        assert!((sd_fit_weight(RELIABLE_MATCHUPS) - 20.0 / 28.0).abs() < 1e-12);
        // the league loader reads it through a read-only connection
        let ro = Ledger::open_read_only(&l2.data_dir).expect("read-only ledger");
        assert_eq!(applied_sd_scale(&ro, "espn").unwrap(), Some(fit));
        assert!(ro.execute("DELETE FROM matchup_snapshots", &[]).is_err());
        let rep = calibration_report(&l2, "espn", MIN_MATCHUPS).unwrap();
        assert!((rep["scale"]["recommended_skater_sd_k"].as_f64().unwrap() - 5.2).abs() < 1e-9);
        assert!((rep["scale"]["recommended_goalie_sd_k"].as_f64().unwrap() - 10.0).abs() < 1e-9);
        assert!(
            summary_line(&rep).ends_with("SD constants 5.20 skaters / 10.00 goalies (applied at weight 0.50 against the league default)"),
            "{}",
            summary_line(&rep)
        );
    }

    #[test]
    fn ended_period_snapshots_are_not_graded() {
        let (_d, l) = led();
        let m = mu(1, Some(0.6), (50.0, 0.0), (45.0, 0.0), (0.0, 0.0), 0, Some("Opp"));
        record_snapshot(&l, "espn", d(2026, 11, 8), &m).unwrap();
        let pend = pending_periods(&l, "espn", d(2026, 11, 9)).unwrap();
        let mut finals = Map::new();
        finals.insert(1, Final { mine: Some(80.0), theirs: Some(70.0), opponent_id: None, source: None });
        attach_results(&l, "espn", &finals, &pend).unwrap();
        assert!(graded_rows(&l, "espn").unwrap().is_empty());
    }

    #[test]
    fn empty_report_and_summary() {
        let (_d, l) = led();
        let rep = calibration_report(&l, "espn", MIN_MATCHUPS).unwrap();
        assert_eq!(rep["status"], "no completed matchups yet");
        assert!(rep["brier"].is_null());
        assert_eq!(summary_line(&rep), "espn: matchup win probability: 0 snapshot(s), no completed matchup yet");
    }

    struct Stub {
        extras: ProviderExtras,
        espn: Value,
        fantrax: Value,
    }
    impl LeagueProvider for Stub {
        fn name(&self) -> &str {
            "stub"
        }
        fn load(&mut self) -> Result<LeagueContext> {
            anyhow::bail!("no")
        }
        fn extras(&self) -> &ProviderExtras {
            &self.extras
        }
        fn espn_league_get(&self, _p: &[(String, String)]) -> Result<Value> {
            Ok(self.espn.clone())
        }
        fn fantrax_call(&self, _c: &[(String, Map<String, Value>)], _f: bool) -> Result<Vec<Value>> {
            Ok(vec![self.fantrax.clone()])
        }
    }

    fn ctx_for(provider: &str, my_id: &str) -> LeagueContext {
        let v = json!({"provider": provider, "league_id": "1", "season": 2027, "name": "L",
                       "scoring": {"kind": "points", "weights": {}}, "roster_shape": {}, "teams": [],
                       "free_agents": [], "matchup_period": null, "as_of": "2026-11-04"});
        let mut ctx: LeagueContext = serde_json::from_value(v).unwrap();
        ctx.teams.push(FantasyTeam { team_id: my_id.into(), name: "Me".into(), owner_is_me: true, slots: Vec::<RosterSlot>::new(), record: None });
        ctx
    }

    #[test]
    fn espn_final_scores_skip_undecided_periods() {
        let ctx = ctx_for("espn", "3");
        let espn = json!({"schedule": [
            {"matchupPeriodId": 1, "winner": "HOME", "home": {"teamId": 3, "totalPoints": 80.5}, "away": {"teamId": 5, "totalPoints": 70.0}},
            {"matchupPeriodId": 2, "winner": "UNDECIDED", "home": {"teamId": 3, "totalPointsLive": 10.0}, "away": {"teamId": 5, "totalPointsLive": 9.0}},
            {"matchupPeriodId": 1, "winner": "HOME", "home": {"teamId": 8, "totalPoints": 1.0}, "away": {"teamId": 9, "totalPoints": 2.0}}]});
        let prov = Stub { extras: ProviderExtras::default(), espn, fantrax: Value::Null };
        let out = fetch_final_scores(&ctx, Some(&prov), &[1, 2], &mut Vec::new()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[&1], Final { mine: Some(80.5), theirs: Some(70.0), opponent_id: Some("5".into()), source: Some("ESPN scoreboard".into()) });
    }

    #[test]
    fn fantrax_final_scores_from_schedule_tables() {
        let ctx = ctx_for("fantrax", "me");
        let data = json!({"tableList": [{
            "caption": "Scoring Period 4", "subCaption": "(Mon Nov 2, 2026 - Sun Nov 8, 2026)",
            "header": {"cells": [{"key": "away"}, {"key": "awayScore"}, {"key": "home"}, {"key": "homeScore"}]},
            "rows": [{"cells": [{"teamId": "me", "content": "Me"}, {"content": "81.5"}, {"teamId": "x", "content": "X"}, {"content": "70"}]}]}]});
        let prov = Stub { extras: ProviderExtras::default(), espn: Value::Null, fantrax: data };
        let out = fetch_final_scores(&ctx, Some(&prov), &[4], &mut Vec::new()).unwrap();
        if let Some(f) = out.get(&4) {
            assert_eq!(f.mine, Some(81.5));
            assert_eq!(f.theirs, Some(70.0));
            assert_eq!(f.opponent_id.as_deref(), Some("x"));
        }
        assert!(fetch_final_scores(&ctx, None, &[4], &mut Vec::new()).unwrap().is_empty());
    }

    /// Official answers by period, and the private schedule view (None: it fails).
    struct OfficialStub {
        extras: ProviderExtras,
        official: Map<i64, std::result::Result<Option<OfficialMatchup>, String>>,
        private: Option<Value>,
    }
    impl LeagueProvider for OfficialStub {
        fn name(&self) -> &str {
            "stub"
        }
        fn load(&mut self) -> Result<LeagueContext> {
            anyhow::bail!("no")
        }
        fn extras(&self) -> &ProviderExtras {
            &self.extras
        }
        fn fantrax_call(&self, _c: &[(String, Map<String, Value>)], _f: bool) -> Result<Vec<Value>> {
            Ok(vec![self.private.clone().ok_or_else(|| anyhow::anyhow!("private view down"))?])
        }
        fn official_matchup(&self, my_team_id: &str, period: i64) -> std::result::Result<Option<OfficialMatchup>, String> {
            assert_eq!(my_team_id, "me");
            self.official.get(&period).cloned().unwrap_or(Ok(None))
        }
    }

    fn official_final(mine: f64, theirs: f64) -> std::result::Result<Option<OfficialMatchup>, String> {
        Ok(Some(OfficialMatchup { opponent_id: "x".into(), my_points: mine, their_points: theirs, playoffs: false }))
    }

    fn private_period(number: i64, mine: f64, theirs: f64) -> Value {
        json!({"caption": format!("Scoring Period {number}"), "subCaption": "(Mon Nov 2, 2026 - Sun Nov 8, 2026)",
            "header": {"cells": [{"key": "away"}, {"key": "awayScore"}, {"key": "home"}, {"key": "homeScore"}]},
            "rows": [{"cells": [{"teamId": "me", "content": "Me"}, {"content": mine.to_string()}, {"teamId": "y", "content": "Y"}, {"content": theirs.to_string()}]}]})
    }

    #[test]
    fn fantrax_finals_come_from_the_official_api_with_the_private_view_for_the_rest() {
        let ctx = ctx_for("fantrax", "me");
        let warn = "Fantrax official API matchup scores unavailable (x); using the private data instead".to_string();
        let mut prov = OfficialStub {
            extras: ProviderExtras::default(),
            official: [(4, official_final(191.6, 197.55)), (5, Ok(None)), (6, Err(warn.clone())), (7, Err(warn.clone()))].into_iter().collect(),
            private: Some(json!({"tableList": [private_period(5, 60.0, 50.0), private_period(6, 70.0, 80.0), private_period(7, 1.0, 2.0)]})),
        };
        let mut warnings = Vec::new();
        let out = fetch_final_scores(&ctx, Some(&prov), &[4, 5, 6, 7], &mut warnings).unwrap();
        assert_eq!(out[&4], Final { mine: Some(191.6), theirs: Some(197.55), opponent_id: Some("x".into()), source: Some("Fantrax official API".into()) });
        // periods the official API gave nothing for (a playoff placeholder, a failed read) use the private view
        assert_eq!((out[&5].mine, out[&5].source.as_deref()), (Some(60.0), Some("Fantrax schedule")));
        assert_eq!((out[&6].mine, out[&6].theirs, out[&7].mine), (Some(70.0), Some(80.0), Some(1.0)));
        // both failed reads give the same text, so the warning appears once
        assert_eq!(warnings, vec![warn]);
        // all periods official: the private view is not asked at all
        prov.private = None;
        let only: Map<i64, _> = [(4, official_final(1.0, 2.0))].into_iter().collect();
        prov.official = only;
        let out = fetch_final_scores(&ctx, Some(&prov), &[4], &mut Vec::new()).unwrap();
        assert_eq!(out[&4].mine, Some(1.0));
    }

    #[test]
    fn fantrax_finals_survive_one_source_failing_but_not_both() {
        let ctx = ctx_for("fantrax", "me");
        let prov = OfficialStub {
            extras: ProviderExtras::default(),
            official: [(4, official_final(5.0, 3.0)), (5, Err("Fantrax official API matchup unavailable (z); using the private data instead".into()))]
                .into_iter()
                .collect(),
            private: None,
        };
        // the private view is down: the official period is kept, the other stays pending
        let mut warnings = Vec::new();
        let out = fetch_final_scores(&ctx, Some(&prov), &[4, 5], &mut warnings).unwrap();
        assert_eq!(out.keys().copied().collect::<Vec<_>>(), vec![4]);
        assert_eq!(warnings.len(), 1);
        // nothing official and the private view down: the old error, for the caller's warning
        let err = fetch_final_scores(&ctx, Some(&prov), &[5], &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("private view down"), "{err}");
    }

    #[test]
    fn capture_step_records_attaches_and_never_raises() {
        let (_d, l) = led();
        let ctx = ctx_for("espn", "3");
        let values = Map::new();
        let now = PyDateTime::naive(d(2026, 11, 9).and_hms_opt(8, 0, 0).unwrap());
        let memo = ScheduleMemo::new(None);
        let ok = || Ok(default_mu());
        let finals = |_p: &[i64]| -> Result<Map<i64, Final>> {
            let mut m = Map::new();
            m.insert(1, Final { mine: Some(80.0), theirs: Some(70.0), opponent_id: None, source: None });
            Ok(m)
        };
        let out = capture_matchup(&l, "espn", &ctx, None, &values, d(2026, 11, 4), &now, &memo, Some(&ok), Some(&finals));
        assert_eq!(out["snapshot"], "recorded");
        assert_eq!(out["win_prob"], 0.6);
        assert_eq!(out["results"], 0); // period still running on the 4th
        let out = capture_matchup(&l, "espn", &ctx, None, &values, d(2026, 11, 9), &now, &memo, Some(&ok), Some(&finals));
        assert_eq!(out["results"], 1);
        let boom = || -> Result<Matchup> { anyhow::bail!("boom") };
        let bad = |_p: &[i64]| -> Result<Map<i64, Final>> { anyhow::bail!("down") };
        let out = capture_matchup(&l, "espn", &ctx, None, &values, d(2026, 11, 10), &now, &memo, Some(&boom), Some(&bad));
        assert_eq!(out["snapshot"], "failed");
        assert_eq!(out["warnings"].as_array().unwrap().len(), 2);
    }
}
