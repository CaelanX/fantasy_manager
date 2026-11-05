//! Valuation parity (work package W4-valuate): `valuate_league` on the snapshot context must
//! reproduce `fixtures/<league>/values.json` (every PlayerValue incl. the dump-excluded share_* and
//! rookie fields; rates compared as maps since Python's key order is hash-random). Covers the
//! TEAMMATE_OUT and ROOKIE_BLEND paths of the schedule / rookie modules (asserted present).
//! Also checks that the parallel per-player loop is deterministic (two runs, byte-equal JSON).

use std::sync::Mutex;

use fm_core::snapshot::Snapshot;
use fm_core::{Map, PlayerValue};
use fm_engine::scoring::from_config;
use fm_engine::valuation::params;
use fm_engine::valuation::schedule::ScheduleMemo;
use fm_engine::valuation::valuate::valuate_league_with;
use fm_parity::{assert_parity, normalize, skip, Fixtures, Tolerance};
use serde_json::Value;

/// The valuation params are a process-global: one league at a time.
static LOCK: Mutex<()> = Mutex::new(());

/// `dump_fixtures.pv_json`: PlayerValue without `player`, plus cid, share_* and rookie.
fn pv_json(pv: &PlayerValue) -> Value {
    let mut v = serde_json::to_value(pv).expect("serialize PlayerValue");
    let o = v.as_object_mut().unwrap();
    o.shift_remove("player");
    o.insert("cid".into(), Value::String(pv.player.cid.clone()));
    o.insert("share_source".into(), serde_json::to_value(&pv.share_source).unwrap());
    o.insert("share_starts".into(), serde_json::to_value(pv.share_starts).unwrap());
    o.insert("share_team_games".into(), serde_json::to_value(pv.share_team_games).unwrap());
    o.insert("rookie".into(), serde_json::to_value(&pv.rookie).unwrap());
    v
}

pub fn values_json(values: &Map<String, PlayerValue>) -> Value {
    Value::Object(values.iter().map(|(c, pv)| (c.clone(), pv_json(pv))).collect())
}

fn run(snap: &mut Snapshot) -> Map<String, PlayerValue> {
    params::install_with_active(snap.params.merged.clone(), Some(snap.params.active.clone()));
    let scoring = from_config(&snap.ctx.scoring);
    let memo = ScheduleMemo::new(Some(Fixtures::datadir_path()));
    valuate_league_with(&mut snap.ctx, &scoring, &memo)
}

fn run_league(league: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let test = format!("stage_values_{league}");
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let mut snap = fx.snapshot();
    let warnings_before = snap.ctx.warnings.clone();
    let values = run(&mut snap);
    let actual = values_json(&values);
    let expected = fx.json("values.json");
    assert_eq!(expected.as_object().map(|o| o.len()), Some(values.len()), "{league}: player count");
    let tol = normalize::set_order_fields(Tolerance::default());
    assert_parity!(&test, expected, actual, tol);
    // the snapshot ctx was dumped after valuation: no new warnings
    assert_eq!(snap.ctx.warnings, warnings_before);

    // the paths the schedule package could not check without valuate
    let codes = |code: &str| values.values().filter(|pv| pv.reason(code).is_some()).count();
    assert!(codes("TEAMMATE_OUT") > 0, "{league}: TEAMMATE_OUT exercised");
    assert!(codes("ROOKIE_BLEND") > 0, "{league}: ROOKIE_BLEND exercised");

    // rayon over players: deterministic
    let mut snap2 = fx.snapshot();
    let again = values_json(&run(&mut snap2));
    assert_eq!(serde_json::to_string(&actual).unwrap(), serde_json::to_string(&again).unwrap());
}

#[test]
fn stage_values_espn() {
    run_league("espn");
}

#[test]
fn stage_values_fantrax() {
    run_league("fantrax");
}
