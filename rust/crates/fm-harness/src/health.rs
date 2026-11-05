//! Rust port of `fantasy_manager/harness/health.py`: everything the /health page,
//! `/api/health.json` and the digest's "Model health" block read (M4 surfaces), built from the
//! ledger and the params versions. Pure reads: nothing is written.
//! Owner: work package `W6-harness-metrics-refit`.
//!
//! Deviations:
//! * The clock is a parameter: `capture` / `extend_league` / `full_report` take `now` (Python
//!   read `datetime.now()`), `calendar` / `full_report` take `today`.
//! * `capture`'s matchup-SD warning needs `harness::matchup_calib` (W8-outlook-playoffs-calib, not
//!   ported yet): [`matchup_warning`] returns None until it is.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use chrono::{Duration, NaiveDateTime};
use fm_core::prefs::harness_auto_apply;
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::round_to;
use serde_json::{json, Value};

use crate::ledger::{Ledger, Row};
use crate::metrics::{
    f_of, i_of, iso, latest_rows, league_report, need_for, ofv, params_info, parse_day, pool_label, s_of,
    status_report, truthy, wilson, headline, hit_trust, BASELINES, HIT_KINDS, HIT_PROVISIONAL, POOLS,
};
use crate::params_store::ParamsStore;
use crate::refit::{auto_apply_start, is_refit_day, next_refit_day, refit_start, tier_b_apply_start};

pub const MODEL_ORIGINS: [&str; 3] = ["followed", "partial", "ignored"];
pub const SCORECARD_ORIGINS: [&str; 4] = ["followed", "partial", "ignored", "user_only"];
pub const ROLLBACK_FROM: [&str; 3] = ["shadow", "retired", "rolled_back"];
pub const STALE_RUN_HOURS: f64 = 36.0;
/// MAE within 1% of the previous week: flat
pub const TREND_TOLERANCE: f64 = 0.01;

fn decode(mut r: Row) -> Row {
    let raw = r.shift_remove("detail_json");
    let d = raw
        .as_ref()
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({})))
        .unwrap_or_else(|| json!({}));
    r.insert("detail".into(), d);
    r
}

/// Every stored `metric_snapshots` row of `league` (optionally one metric), oldest week first.
pub fn history_rows(ledger: &Ledger, league: &str, metric: Option<&str>) -> Result<Vec<Row>> {
    let lg = league.to_string();
    let rows = match metric {
        Some(m) => {
            let m = m.to_string();
            ledger.query("SELECT * FROM metric_snapshots WHERE league=? AND metric=? ORDER BY week, pool", &[&lg, &m])?
        }
        None => ledger.query("SELECT * FROM metric_snapshots WHERE league=? ORDER BY week, pool", &[&lg])?,
    };
    Ok(rows.into_iter().map(decode).collect())
}

/// `(d or {}).get(k)` on JSON (None for non-objects / missing).
fn jget<'a>(v: Option<&'a Value>, k: &str) -> Option<&'a Value> {
    v.and_then(|x| x.as_object()).and_then(|o| o.get(k))
}

/// `(x or {})`-style truthy object.
fn obj<'a>(v: Option<&'a Value>) -> Option<&'a Value> {
    v.filter(|x| truthy(Some(x)))
}

fn cloned(v: Option<&Value>) -> Value {
    v.cloned().unwrap_or(Value::Null)
}

// --------------------------------------------------------------------------- chart series

fn proj_series(latest: &[Row]) -> (Value, Value) {
    let mut mae = serde_json::Map::new();
    let mut skill = serde_json::Map::new();
    let mut by: HashMap<(String, String), Value> = HashMap::new();
    for r in latest {
        by.insert(
            (s_of(r, "metric").unwrap_or("").to_string(), s_of(r, "pool").unwrap_or("").to_string()),
            Value::Object(r.clone()),
        );
    }
    for pool in POOLS {
        let m = by.get(&("proj_fpg_mae".to_string(), pool.to_string()));
        let s = by.get(&("proj_fpg_skill".to_string(), pool.to_string()));
        let empty = json!({});
        let d = obj(jget(m, "detail")).unwrap_or(&empty);
        let per_week: Vec<Value> = obj(d.get("per_week")).and_then(|x| x.as_array()).cloned().unwrap_or_default();
        let weeks: Vec<Value> = per_week.iter().map(|w| cloned(w.get("week"))).collect();
        let mut base: HashMap<String, Value> = HashMap::new();
        for w in obj(d.get("per_week_base")).and_then(|x| x.as_array()).cloned().unwrap_or_default() {
            base.insert(w.get("week").map(|x| x.to_string()).unwrap_or_default(), w);
        }
        let bget = |wk: &Value| base.get(&wk.to_string()).filter(|b| truthy(Some(b)));
        let mut lines = serde_json::Map::new();
        lines.insert("fm".into(), Value::Array(per_week.iter().map(|w| cloned(w.get("mae"))).collect()));
        for b in BASELINES {
            lines.insert(
                b.into(),
                Value::Array(weeks.iter().map(|wk| cloned(jget(obj(jget(bget(wk), "base_mae")), b))).collect()),
            );
        }
        let get_or = |row: Option<&Value>, k: &str, default: Value| match row.and_then(|r| r.as_object()) {
            Some(o) => o.get(k).cloned().unwrap_or(default),
            None => default,
        };
        let need_default = |metric: &str| need_for(metric, 0).map(Value::from).unwrap_or(Value::Null);
        mae.insert(
            pool.into(),
            json!({
                "trust": get_or(m, "trust", json!("hidden")), "n": get_or(m, "n", json!(0)),
                "need": d.get("need").cloned().unwrap_or_else(|| need_default("proj_fpg_mae")),
                "value": get_or(m, "value", Value::Null),
                "x": weeks, "lines": lines, "n_by_week": per_week.iter().map(|w| cloned(w.get("n"))).collect::<Vec<_>>(),
            }),
        );
        let mut sk_lines = serde_json::Map::new();
        sk_lines.insert(
            "fm".into(),
            Value::Array(weeks.iter().map(|wk| cloned(jget(obj(jget(bget(wk), "skill")), "fm"))).collect()),
        );
        sk_lines.insert(
            "to_date".into(),
            Value::Array(
                weeks.iter().map(|wk| if truthy(jget(bget(wk), "n_to_date")) { json!(0.0) } else { Value::Null }).collect(),
            ),
        );
        for b in BASELINES {
            if b != "to_date" {
                sk_lines.insert(
                    b.into(),
                    Value::Array(weeks.iter().map(|wk| cloned(jget(obj(jget(bget(wk), "skill")), b))).collect()),
                );
            }
        }
        let sd = obj(jget(s, "detail")).unwrap_or(&empty);
        let has_base = !base.is_empty();
        skill.insert(
            pool.into(),
            json!({
                "trust": get_or(s, "trust", json!("hidden")), "n": get_or(s, "n", json!(0)),
                "need": sd.get("need").cloned().unwrap_or_else(|| need_default("proj_fpg_skill")),
                "value": get_or(s, "value", Value::Null),
                "ci_lo": get_or(s, "ci_lo", Value::Null), "ci_hi": get_or(s, "ci_hi", Value::Null),
                "x": if has_base { Value::Array(weeks.clone()) } else { json!([]) },
                "lines": if has_base { Value::Object(sk_lines) } else { json!({}) },
            }),
        );
    }
    (Value::Object(mae), Value::Object(skill))
}

fn vi64(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)).unwrap_or(0),
        Some(Value::Bool(b)) => *b as i64,
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn hits_of(r: &Row) -> i64 {
    vi64(r.get("detail").and_then(|d| d.get("hits")))
}

fn n_of(r: &Row) -> i64 {
    i_of(r, "n").unwrap_or(0)
}

/// Per kind, per graded week: the model's recs (followed + partial + ignored) pooled.
pub fn hit_series(history: &[Row]) -> serde_json::Map<String, Value> {
    let weeks: Vec<String> =
        history.iter().filter_map(|r| s_of(r, "week").map(String::from)).collect::<BTreeSet<_>>().into_iter().collect();
    let mut out = serde_json::Map::new();
    for kind in HIT_KINDS {
        let pools: Vec<String> = MODEL_ORIGINS.iter().map(|o| format!("{kind}:{o}")).collect();
        let (mut rate, mut lo, mut hi, mut ns) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for wk in &weeks {
            let rows: Vec<&Row> = history
                .iter()
                .filter(|r| s_of(r, "week") == Some(wk.as_str()) && pools.iter().any(|p| Some(p.as_str()) == s_of(r, "pool")))
                .collect();
            let n: i64 = rows.iter().map(|r| n_of(r)).sum();
            let k: i64 = rows.iter().map(|r| hits_of(r)).sum();
            let (a, b) = wilson(k, n);
            rate.push(ofv((n != 0).then(|| round_to(k as f64 / n as f64, 4))));
            lo.push(ofv(a.map(|x| round_to(x, 4))));
            hi.push(ofv(b.map(|x| round_to(x, 4))));
            ns.push(n);
        }
        if !ns.iter().any(|n| *n != 0) {
            continue;
        }
        let last_n = *ns.last().unwrap_or(&0);
        out.insert(
            kind.into(),
            json!({
                "trust": hit_trust(last_n.max(0) as usize), "n": last_n,
                "need": 0i64.max(HIT_PROVISIONAL as i64 - last_n),
                "value": rate.last().cloned().unwrap_or(Value::Null), "x": weeks,
                "lines": {"rate": rate}, "band": {"lo": lo, "hi": hi}, "n_by_week": ns,
            }),
        );
    }
    out
}

pub fn series(ledger: &Ledger, league: &str) -> Result<Value> {
    let (_, latest) = latest_rows(ledger, league)?;
    let (mae, skill) = proj_series(&latest);
    let hit = hit_series(&history_rows(ledger, league, Some("hit_rate"))?);
    Ok(json!({"mae": mae, "skill": skill, "hit": hit}))
}

// --------------------------------------------------------------------------- scorecard

/// Followed / partial / ignored / user-only: counts and hit rates over every kind (trades
/// excluded: they are a case list), latest bar.
pub fn scorecard(latest: &[Row]) -> Vec<Value> {
    let mut out = Vec::new();
    for origin in SCORECARD_ORIGINS {
        let suffix = format!(":{origin}");
        let rows: Vec<&Row> = latest
            .iter()
            .filter(|r| s_of(r, "metric") == Some("hit_rate") && s_of(r, "pool").is_some_and(|p| p.ends_with(&suffix)))
            .collect();
        let n: i64 = rows.iter().map(|r| n_of(r)).sum();
        let k: i64 = rows.iter().map(|r| hits_of(r)).sum();
        let (lo, hi) = wilson(k, n);
        let mut kinds = serde_json::Map::new();
        for r in &rows {
            if truthy(r.get("n")) {
                kinds.insert(s_of(r, "pool").unwrap_or("").split(':').next().unwrap_or("").to_string(), json!(n_of(r)));
            }
        }
        out.push(json!({
            "origin": origin, "n": n, "hits": k, "rate": ofv((n != 0).then(|| k as f64 / n as f64)),
            "ci_lo": ofv(lo), "ci_hi": ofv(hi), "trust": hit_trust(n.max(0) as usize),
            "need": 0i64.max(HIT_PROVISIONAL as i64 - n), "kinds": kinds,
        }));
    }
    out
}

// --------------------------------------------------------------------------- data capture

fn last_daily(ledger: &Ledger) -> Result<Option<Row>> {
    let rows = ledger.query(
        "SELECT run_id, league, started_at, finished_at, status, summary_json FROM runs WHERE command='daily' \
         ORDER BY run_id DESC LIMIT 1",
        &[],
    )?;
    let Some(mut r) = rows.into_iter().next() else { return Ok(None) };
    let raw = r.shift_remove("summary_json");
    let summary = raw
        .as_ref()
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({})))
        .unwrap_or_else(|| json!({}));
    r.insert("summary".into(), summary);
    Ok(Some(r))
}

/// Python `str(value)` for warning texts.
fn pystr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => {
            n.as_i64().map(|i| i.to_string()).unwrap_or_else(|| fm_pycompat::fmt::repr_f64(n.as_f64().unwrap_or(f64::NAN)))
        }
        other => other.to_string(),
    }
}

fn row1(ledger: &Ledger, sql: &str, league: &str) -> Result<Row> {
    let lg = league.to_string();
    Ok(ledger.query(sql, &[&lg])?.into_iter().next().unwrap_or_default())
}

fn int0(r: &Row, k: &str) -> i64 {
    i_of(r, k).unwrap_or(0)
}

pub fn capture(ledger: &Ledger, league: &str, now: NaiveDateTime) -> Result<Value> {
    let lg = league.to_string();
    let eps = ledger.query(
        "SELECT kind, status, COUNT(*) n FROM rec_episodes WHERE league=? GROUP BY 1, 2 ORDER BY 1, 2",
        &[&lg],
    )?;
    let statuses: BTreeSet<String> = eps.iter().filter_map(|e| s_of(e, "status").map(String::from)).collect();
    let mut kinds = serde_json::Map::new();
    for e in &eps {
        let k = s_of(e, "kind").unwrap_or("").to_string();
        let entry = kinds.entry(k).or_insert_with(|| json!({}));
        if let Value::Object(m) = entry {
            m.insert(s_of(e, "status").unwrap_or("").to_string(), cloned(e.get("n")));
        }
    }
    let tx = row1(
        ledger,
        "SELECT COUNT(*) n, SUM(is_me) mine, MAX(day) last, SUM(nhl_id IS NULL) unmatched FROM transactions WHERE league=?",
        league,
    )?;
    let mut dec = serde_json::Map::new();
    for r in ledger.query("SELECT origin, COUNT(*) n FROM decisions WHERE league=? GROUP BY 1", &[&lg])? {
        dec.insert(r.get("origin").map(pystr).unwrap_or_default(), cloned(r.get("n")));
    }
    let proj = row1(ledger, "SELECT COUNT(DISTINCT as_of) days, MAX(as_of) last FROM projections WHERE league=?", league)?;
    let (mut unmatched_proj, mut total_proj) = (0i64, 0i64);
    if let Some(last) = s_of(&proj, "last").filter(|s| !s.is_empty()) {
        let last = last.to_string();
        let u = ledger
            .query(
                "SELECT COUNT(*) n, SUM(nhl_id IS NULL) un FROM projections WHERE league=? AND as_of=?",
                &[&lg, &last],
            )?
            .into_iter()
            .next()
            .unwrap_or_default();
        total_proj = int0(&u, "n");
        unmatched_proj = int0(&u, "un");
    }
    let rp = row1(ledger, "SELECT COUNT(*) n, SUM(nhl_id IS NULL) un FROM rec_players WHERE league=?", league)?;
    let pulls = ledger
        .query(
            "SELECT COUNT(*) pulled, SUM(n_players > 0) with_games, MIN(game_date) first, MAX(game_date) last \
             FROM realized_pulls",
            &[],
        )?
        .into_iter()
        .next()
        .unwrap_or_default();
    let lineups = row1(ledger, "SELECT COUNT(DISTINCT day) days FROM lineup_days WHERE league=?", league)?;
    let last = last_daily(ledger)?;
    let mut warnings: Vec<String> = Vec::new();
    match &last {
        None => warnings.push("No daily capture has run yet: schedule `fm harness daily` (docs/scheduling.md).".into()),
        Some(last) => {
            let started = last.get("started_at").map(pystr).unwrap_or_else(|| "None".into());
            if let Some((started, _)) = fm_pycompat::time::fromisoformat(&started) {
                let age_h = (now - started).num_milliseconds() as f64 / 1000.0 / 3600.0;
                if age_h > STALE_RUN_HOURS {
                    warnings.push(format!(
                        "The last daily capture ran {} days ago: transaction feeds only keep recent moves, so run it \
                         every day.",
                        fixed(age_h / 24.0, 1, false)
                    ));
                }
            }
            let empty = json!({});
            let summ = obj(last.get("summary")).unwrap_or(&empty);
            for l in obj(summ.get("leagues")).and_then(|x| x.as_array()).cloned().unwrap_or_default() {
                if l.get("league").and_then(|x| x.as_str()) != Some(league) {
                    continue;
                }
                if let Some(sk) = obj(l.get("skipped")) {
                    warnings.push(format!("Last daily run skipped this league: {}", pystr(sk)));
                }
                if let Some(ws) = obj(l.get("warnings")).and_then(|x| x.as_array()) {
                    warnings.extend(ws.iter().map(pystr));
                }
            }
            if let Some(e) = obj(jget(obj(summ.get("realized")), "error")) {
                warnings.push(format!("NHL results pull failed: {}", pystr(e)));
            }
            if let Some(e) = obj(jget(obj(summ.get("refit")), "error")) {
                warnings.push(format!("Refit failed: {}", pystr(e)));
            }
            match last.get("status") {
                None | Some(Value::Null) => {}
                Some(Value::String(s)) if s == "ok" => {}
                Some(st) => warnings.push(format!("Last daily run finished with status {}.", pystr(st))),
            }
        }
    }
    if let Some(first) = s_of(&pulls, "first").filter(|s| !s.is_empty()).and_then(parse_day) {
        let yday = now.date() - Duration::days(1);
        let expected = (yday - first).num_days() + 1;
        let missing = expected - int0(&pulls, "pulled");
        if missing > 0 {
            warnings.push(format!(
                "{missing} game date(s) between {} and {} were never pulled: those windows cannot mature.",
                iso(first),
                iso(yday)
            ));
        }
    }
    if let Some(mc) = matchup_warning(ledger, league) {
        warnings.push(mc);
    }
    if total_proj != 0 && unmatched_proj as f64 / total_proj as f64 > 0.15 {
        warnings.push(format!(
            "{unmatched_proj} of {total_proj} players in the latest snapshot have no NHL id (`fm sync --review`)."
        ));
    }
    let total: i64 = eps.iter().map(|e| int0(e, "n")).sum();
    let last_daily = match &last {
        None => Value::Null,
        Some(l) => json!({"started_at": cloned(l.get("started_at")), "finished_at": cloned(l.get("finished_at")),
                          "status": cloned(l.get("status")), "league": cloned(l.get("league"))}),
    };
    Ok(json!({
        "episodes": {"statuses": statuses.into_iter().collect::<Vec<_>>(), "kinds": kinds, "total": total},
        "transactions": {"n": int0(&tx, "n"), "mine": int0(&tx, "mine"),
                         "others": int0(&tx, "n") - int0(&tx, "mine"), "last": cloned(tx.get("last"))},
        "decisions": dec,
        "projections": {"days": int0(&proj, "days"), "last": cloned(proj.get("last"))},
        "lineup_days": int0(&lineups, "days"),
        "realized": {"pulled": int0(&pulls, "pulled"), "with_games": int0(&pulls, "with_games"),
                     "first": cloned(pulls.get("first")), "last": cloned(pulls.get("last"))},
        "unmatched": {"projections": unmatched_proj, "projections_total": total_proj,
                      "transactions": int0(&tx, "unmatched"), "rec_players": int0(&rp, "un"),
                      "rec_players_total": int0(&rp, "n")},
        "last_daily": last_daily,
        "warnings": warnings,
    }))
}

/// A capture warning when the fitted matchup SD scale says the win probabilities are mis-sized.
/// `harness::matchup_calib` (W8-outlook-playoffs-calib) is not ported yet: None.
pub fn matchup_warning(_ledger: &Ledger, _league: &str) -> Option<String> {
    None
}

// --------------------------------------------------------------------------- params

fn metric(m: &Value, key: &str) -> Value {
    match m.get(key) {
        Some(Value::Number(n)) => ofv(n.as_f64()),
        Some(Value::Bool(b)) => json!(if *b { 1.0 } else { 0.0 }),
        _ => Value::Null,
    }
}

/// Params versions newest first, the active one and the rollback targets.
pub fn changelog(store: &ParamsStore<'_>) -> Value {
    let versions = store.versions();
    let active = store.active_name();
    let mut rows = Vec::new();
    let empty = json!({});
    for v in versions.iter().rev() {
        let vv = Value::Object(v.clone());
        let m = obj(vv.get("metrics")).unwrap_or(&empty);
        let events: Vec<Value> = obj(vv.get("changelog")).and_then(|x| x.as_array()).cloned().unwrap_or_default();
        let changed_keys = obj(vv.get("changed_keys")).cloned().unwrap_or_else(|| json!([]));
        rows.push(json!({
            "version": cloned(vv.get("version")), "parent": cloned(vv.get("parent")),
            "created": cloned(vv.get("created")), "status": cloned(vv.get("status")), "changed_keys": changed_keys,
            "holdout_before": metric(m, "holdout_before"), "holdout_after": metric(m, "holdout_after"),
            "hist_before": metric(m, "hist_before"), "hist_after": metric(m, "hist_after"),
            "n_live": cloned(m.get("n_live")), "hash": cloned(vv.get("hash")), "applied_by": cloned(vv.get("applied_by")),
            "applied_at": cloned(vv.get("applied_at")),
            "last_event": events.last().cloned().unwrap_or(Value::Null),
            "events": events.iter().map(|e| json!({"at": cloned(e.get("at")), "event": cloned(e.get("event")),
                                                    "by": cloned(e.get("by")), "note": cloned(e.get("note"))}))
                .collect::<Vec<_>>(),
        }));
    }
    let mut targets: Vec<String> = Vec::new();
    let mut default = Value::Null;
    if active != "packaged" {
        let cur = versions.iter().find(|v| v.get("version").and_then(|x| x.as_str()) == Some(active.as_str()));
        let d = cur
            .and_then(|c| c.get("parent"))
            .filter(|p| truthy(Some(p)))
            .map(pystr)
            .unwrap_or_else(|| "packaged".into());
        targets.push(d.clone());
        for v in versions.iter().rev() {
            let st = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
            let ver = v.get("version").map(pystr).unwrap_or_default();
            if ROLLBACK_FROM.contains(&st) && ver != active && ver != d {
                targets.push(ver);
            }
        }
        if !targets.iter().any(|t| t == "packaged") {
            targets.push("packaged".into());
        }
        default = json!(d);
    }
    json!({"active": active, "rows": rows, "targets": targets, "default_target": default})
}

pub fn calendar(today: chrono::NaiveDate, data_dir: &Path) -> Value {
    let pref = harness_auto_apply(data_dir);
    json!({
        "today": iso(today), "locked": today < refit_start(), "refit_start": iso(refit_start()),
        "next_refit": iso(if is_refit_day(today) { today } else { next_refit_day(today) }),
        "is_refit_day": is_refit_day(today), "auto_apply_start": iso(auto_apply_start()),
        "tier_b_apply_start": iso(tier_b_apply_start()), "auto_apply_pref": pref,
        "auto_apply_active": pref && today >= auto_apply_start(),
    })
}

// --------------------------------------------------------------------------- the whole report

/// `metrics.status_report` (unchanged keys) plus, per league, `series` / `scorecard` / `capture`
/// and, top level, `changelog` and `calendar`.
pub fn full_report(
    ledger: &Ledger,
    leagues: Option<&[String]>,
    today: chrono::NaiveDate,
    data_dir: Option<&Path>,
    now: NaiveDateTime,
) -> Result<Value> {
    let mut rep = status_report(ledger, leagues)?;
    if let Some(Value::Object(ls)) = rep.get_mut("leagues") {
        let keys: Vec<String> = ls.keys().cloned().collect();
        for lg in keys {
            let l = ls.get_mut(&lg).expect("league");
            let taken = std::mem::take(l);
            *l = extend_league(ledger, &lg, Some(taken), now)?;
        }
    }
    let store = ParamsStore::new(None, Some(ledger), None);
    if let Value::Object(m) = &mut rep {
        m.insert("changelog".into(), changelog(&store));
        m.insert("calendar".into(), calendar(today, data_dir.unwrap_or(&ledger.data_dir)));
    }
    Ok(rep)
}

/// Add the M4 keys to one league's report (built when not given).
pub fn extend_league(ledger: &Ledger, league: &str, l: Option<Value>, now: NaiveDateTime) -> Result<Value> {
    let mut l = match l {
        Some(v) => v,
        None => league_report(ledger, league)?,
    };
    let (_, latest) = latest_rows(ledger, league)?;
    if let Value::Object(m) = &mut l {
        m.insert("series".into(), series(ledger, league)?);
        m.insert("scorecard".into(), Value::Array(scorecard(&latest)));
        m.insert("capture".into(), capture(ledger, league, now)?);
    }
    Ok(l)
}

// --------------------------------------------------------------------------- digest block

fn arrow(cur: Option<f64>, prev: Option<f64>) -> &'static str {
    let (Some(cur), Some(prev)) = (cur, prev) else { return "→" };
    if prev == 0.0 {
        return "→";
    }
    let change = (cur - prev) / prev.abs();
    if change.abs() < TREND_TOLERANCE {
        "→"
    } else if change < 0.0 {
        "↓" // MAE: down is better
    } else {
        "↑"
    }
}

/// Three lines (projection MAE trend, hit rate, params version) for the digest, or None while
/// nothing is trustworthy (the same rule as `metrics.headline`).
pub fn health_block(ledger: &Ledger, league: &str) -> Result<Option<Vec<String>>> {
    if headline(ledger, league)?.is_none() {
        return Ok(None);
    }
    let (week, latest) = latest_rows(ledger, league)?;
    let lg = league.to_string();
    let week = week.unwrap_or_default();
    let prev_week = ledger
        .query("SELECT MAX(week) w FROM metric_snapshots WHERE league=? AND week < ?", &[&lg, &week])?
        .first()
        .and_then(|r| s_of(r, "w").map(String::from))
        .filter(|w| !w.is_empty());
    let mut prev: HashMap<String, Option<f64>> = HashMap::new();
    if let Some(pw) = &prev_week {
        for r in ledger.query(
            "SELECT pool, value FROM metric_snapshots WHERE league=? AND week=? AND metric='proj_fpg_mae'",
            &[&lg, pw],
        )? {
            prev.insert(s_of(&r, "pool").unwrap_or("").to_string(), f_of(&r, "value"));
        }
    }
    let mut mae_bits = Vec::new();
    for r in &latest {
        let Some(value) = f_of(r, "value") else { continue };
        if s_of(r, "metric") != Some("proj_fpg_mae") || s_of(r, "trust") == Some("hidden") {
            continue;
        }
        let pool = s_of(r, "pool").unwrap_or("");
        let p = prev.get(pool).copied().flatten();
        let was = p.map(|p| format!(" (was {})", fixed(p, 2, false))).unwrap_or_default();
        mae_bits.push(format!("{} {} {}{was}", pool_label(pool), fixed(value, 2, false), arrow(Some(value), p)));
    }
    let line1 = format!(
        "Projection MAE (FPG, next 28 days): {}",
        if mae_bits.is_empty() { "not judgeable yet".to_string() } else { mae_bits.join("; ") }
    );
    let mut hit_bits = Vec::new();
    for (kind, s) in hit_series(&history_rows(ledger, league, Some("hit_rate"))?) {
        let trust = s.get("trust").and_then(|t| t.as_str()).unwrap_or("hidden");
        let Some(value) = s.get("value").and_then(|v| v.as_f64()) else { continue };
        if trust != "hidden" {
            hit_bits.push(format!("{kind} {}% (n={}, {trust})", fixed(value * 100.0, 0, false), cloned(s.get("n"))));
        }
    }
    let line2 = format!(
        "Hit rate of the model's recs: {}",
        if hit_bits.is_empty() { "not judgeable yet".to_string() } else { hit_bits.join("; ") }
    );
    let pi = params_info(ledger);
    let h: String = pi.get("hash").and_then(|x| x.as_str()).unwrap_or("").chars().take(10).collect();
    let line3 = format!(
        "Params: {}{}, {}",
        pi.get("version").map(pystr).unwrap_or_default(),
        if h.is_empty() { String::new() } else { format!(" (hash {h})") },
        pi.get("source").map(pystr).unwrap_or_default()
    );
    Ok(Some(vec![line1, line2, line3]))
}
