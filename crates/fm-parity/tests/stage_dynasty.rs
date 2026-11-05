//! Dynasty / keeper and draft-pick parity (work package W4-valuate): `apply_dynasty(values, ctx,
//! ages = snapshot.provider.ages)` on the fixture values must reproduce `dynasty.json` (null outside
//! dynasty leagues), and `picks_by_holder(value_picks(...))` must reproduce `picks.json` (the
//! CLI's `_pick_json` rounding). The Fantrax fixture runs with FANTRAX_KEEPERS=3 (keeper mode).

use std::sync::Mutex;

use fm_core::{DynastyValue, Map, PickAsset};
use fm_engine::valuation::dynasty::apply_dynasty;
use fm_engine::valuation::params;
use fm_engine::valuation::picks::{picks_by_holder, value_picks};
use fm_parity::{assert_parity, skip, Fixtures, Tolerance};
use fm_pycompat::num::round_to;
use serde_json::{json, Value};

static LOCK: Mutex<()> = Mutex::new(());

fn dyn_json(values: &Map<String, DynastyValue>) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(c, dv)| {
                let mut v = serde_json::to_value(dv).unwrap();
                let o = v.as_object_mut().unwrap();
                o.shift_remove("player");
                o.insert("cid".into(), Value::String(dv.player.cid.clone()));
                (c.clone(), v)
            })
            .collect(),
    )
}

/// `cli._pick_json`.
fn pick_json(a: &PickAsset) -> Value {
    json!({"year": a.year, "round": a.round, "holder": a.holder, "original": a.original,
           "original_name": a.original_name, "expected_slot": round_to(a.expected_slot, 2),
           "overall": round_to(a.overall, 1), "value": round_to(a.value, 2), "net": round_to(a.net, 2),
           "years_ahead": a.years_ahead, "inferred": a.inferred})
}

fn run_league(league: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let test = format!("stage_dynasty_{league}");
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let snap = fx.snapshot();
    params::install_with_active(snap.params.merged.clone(), Some(snap.params.active.clone()));
    let ctx = &snap.ctx;
    let values = fx.values(&snap);
    let expected = fx.json("dynasty.json");
    if !ctx.dynasty {
        assert!(expected.is_null(), "{league}: not a dynasty league, dynasty.json must be null");
        assert_eq!(fx.json("picks.json"), json!({}));
        return;
    }
    let ages = Some(&snap.provider.ages).filter(|a| !a.is_empty());
    let dynv = apply_dynasty(&values, ctx, ages, None);
    assert_eq!(expected.as_object().map(|o| o.len()), Some(dynv.len()));
    assert_parity!(&test, expected, dyn_json(&dynv), Tolerance::default());

    let by_holder = if ctx.draft_picks.is_empty() {
        Map::new()
    } else {
        picks_by_holder(value_picks(ctx, &values, Some(&dynv)))
    };
    let actual: Value = Value::Object(
        by_holder.iter().map(|(k, v)| (k.clone(), Value::Array(v.iter().map(pick_json).collect()))).collect(),
    );
    assert_parity!(&format!("stage_picks_{league}"), fx.json("picks.json"), actual, Tolerance::default());
}

#[test]
fn stage_dynasty_espn() {
    run_league("espn");
}

#[test]
fn stage_dynasty_fantrax() {
    run_league("fantrax");
}
