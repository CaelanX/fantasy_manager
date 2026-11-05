//! Enrichment parity (work package W4-enrich): `enrich_context(raw_ctx)` replayed offline from
//! the HTTP tape (`HttpCache::from_tape`, LLM off, clock pinned to `snapshot.now`, data dir = a
//! temp copy of `fixtures/_datadir`) must reproduce `snapshot.ctx` (source notes / sources
//! compared after normalizing ages and cache timestamps), `deployment_details` and the
//! enrichment registries (`snapshot.extras`).
//!
//! Needs the gitignored parts (`tape.json.gz`, `_datadir/harness.db`): skips when absent
//! (point `FM_PARITY_FIXTURES` at the full set).

use std::sync::Arc;

use fm_core::config::Settings;
use fm_core::models::LeagueContext;
use fm_core::snapshot::EnrichState;
use fm_http::{HttpCache, Tape};
use fm_parity::{assert_parity, skip, Fixtures, Tolerance};
use fm_providers::enrich::{enrich_context, Clock, EnrichOptions};
use serde_json::{json, Value};

const AGE_RE: &str = r"(?:just now|\b\d+(?:s|m|h|d) ago)";

fn state_json(st: &EnrichState) -> Value {
    let mut pre: Vec<Value> = st.preseason.values().map(|l| serde_json::to_value(l).unwrap()).collect();
    pre.sort_by_key(|v| (v["season"].as_i64(), v["nhl_id"].as_i64()));
    let mut hist: Vec<(&i64, &Vec<fm_core::snapshot::LeagueSeason>)> = st.rookie_history.iter().collect();
    hist.sort_by_key(|(k, _)| **k);
    let mut gsax: Vec<Value> = st
        .gsax
        .iter()
        .map(|((y, _), g)| {
            let mut v = serde_json::to_value(g).unwrap();
            v.as_object_mut().unwrap().shift_insert(0, "season_start_year".into(), json!(y));
            v
        })
        .collect();
    gsax.sort_by_key(|v| (v["season_start_year"].as_i64(), v["nhl_id"].as_i64()));
    let norms: serde_json::Map<String, Value> =
        st.xg_norms.iter().map(|(k, v)| (k.to_string(), serde_json::to_value(v).unwrap())).collect();
    let meta: serde_json::Map<String, Value> =
        st.line_team_meta.iter().map(|(k, v)| (k.format("%Y-%m-%d").to_string(), serde_json::to_value(v).unwrap())).collect();
    json!({
        "preseason_lines": pre,
        "rookie_history": hist.into_iter().map(|(k, v)| (k.to_string(), serde_json::to_value(v).unwrap())).collect::<serde_json::Map<_, _>>(),
        "rookie_signals": serde_json::to_value(&st.rookie_signals).unwrap(),
        "gsax": gsax,
        "xg_norms": norms,
        "line_team_meta": meta,
    })
}

fn sorted_extras(mut v: Value) -> Value {
    if let Some(a) = v["preseason_lines"].as_array_mut() {
        a.sort_by_key(|v| (v["season"].as_i64(), v["nhl_id"].as_i64()));
    }
    if let Some(a) = v["gsax"].as_array_mut() {
        a.sort_by_key(|v| (v["season_start_year"].as_i64(), v["nhl_id"].as_i64()));
    }
    if let Some(m) = v["rookie_history"].as_object() {
        let mut items: Vec<(String, Value)> = m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        items.sort_by_key(|(k, _)| k.parse::<i64>().unwrap_or(0));
        v["rookie_history"] = Value::Object(items.into_iter().collect());
    }
    v
}

fn run_league(league: &str) {
    let test = format!("stage_enrich_{league}");
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let Some(tape) = fx.tape_path() else { return skip(&test, "tape.json.gz missing (set FM_PARITY_FIXTURES)") };
    if Fixtures::harness_db().is_none() {
        return skip(&test, "_datadir/harness.db missing (set FM_PARITY_FIXTURES)");
    }
    let snap = fx.snapshot();
    let expected: Value = fx.json("snapshot.json");
    let mut ctx: LeagueContext = serde_json::from_value(fx.raw_ctx()).expect("raw_ctx parses");
    let (dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let settings = Settings { fm_data_dir: dir.clone(), ..Settings::default() };
    let cache = Arc::new(HttpCache::from_tape(Tape::load(&tape).expect("tape loads")));
    let now = snap.now.to_utc().expect("aware snapshot clock");
    let clock = Clock { now: now.timestamp() as f64 + now.timestamp_subsec_micros() as f64 / 1e6, today: snap.today };

    enrich_context(&mut ctx, &settings, Some(cache), false, EnrichOptions { clock, ..Default::default() });

    let mut exp_ctx = expected["ctx"].clone();
    let exp_details = exp_ctx.as_object_mut().unwrap().shift_remove("deployment_details").unwrap_or(Value::Null);
    let actual = serde_json::to_value(&ctx).unwrap();
    let tol = Tolerance::default()
        .with_normalize("$.source_notes[*]", AGE_RE, "<age>")
        .with_ignore("$.sources[*].fetched_at")
        .with_ignore("$.sources[*].age_seconds");
    assert_parity!(&format!("{test}_ctx"), exp_ctx, actual, tol);
    // the ledger's pull time is not a cache timestamp: it must match exactly
    let dep = |v: &Value| v["sources"].as_array().unwrap().iter().find(|s| s["name"] == "Deployment ledger").cloned();
    assert_eq!(dep(&expected["ctx"]), dep(&serde_json::to_value(&ctx).unwrap()), "Deployment ledger source");
    let details = serde_json::to_value(&ctx.deployment_details).unwrap();
    assert_parity!(&format!("{test}_deployment_details"), exp_details, details, Tolerance::default());
    let extras = sorted_extras(expected["extras"].clone());
    assert_parity!(&format!("{test}_extras"), extras, state_json(&ctx.enrich), Tolerance::default());
}

#[test]
fn stage_enrich_espn() {
    run_league("espn");
}

#[test]
fn stage_enrich_fantrax() {
    run_league("fantrax");
}
