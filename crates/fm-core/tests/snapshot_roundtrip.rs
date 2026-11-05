//! The shared types are the contract: prove they hold every field of the Python fixtures by
//! deserializing and re-serializing them and comparing with the original JSON.

use std::path::PathBuf;

use fm_core::snapshot::Snapshot;
use fm_core::{DynastyValue, LeagueContext, PlayerValue};
use serde_json::Value;

fn fixtures() -> PathBuf {
    std::env::var_os("FM_PARITY_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures"))
}

fn read(path: PathBuf) -> Value {
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// Structural equality; reports the first differing path.
fn same(a: &Value, b: &Value, path: &str) -> Result<(), String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                let w = y.get(k).ok_or_else(|| format!("{path}.{k}: missing after roundtrip"))?;
                same(v, w, &format!("{path}.{k}"))?;
            }
            for k in y.keys() {
                if !x.contains_key(k) {
                    return Err(format!("{path}.{k}: extra after roundtrip"));
                }
            }
            let (kx, ky): (Vec<_>, Vec<_>) = (x.keys().collect(), y.keys().collect());
            if kx != ky {
                return Err(format!("{path}: key order differs"));
            }
            Ok(())
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Err(format!("{path}: len {} != {}", x.len(), y.len()));
            }
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                same(v, w, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        (Value::Number(x), Value::Number(y)) => {
            if x.as_f64() == y.as_f64() && x.is_f64() == y.is_f64() {
                Ok(())
            } else {
                Err(format!("{path}: {x} != {y}"))
            }
        }
        _ if a == b => Ok(()),
        _ => Err(format!("{path}: {a} != {b}")),
    }
}

fn leagues() -> Vec<&'static str> {
    vec!["espn", "fantrax"]
}

#[test]
fn league_context_roundtrips() {
    for lg in leagues() {
        let raw = read(fixtures().join(lg).join("snapshot.json"));
        let mut ctx_json = raw["ctx"].clone();
        let ctx: LeagueContext = serde_json::from_value(ctx_json.clone()).expect(lg);
        let back = serde_json::to_value(&ctx).unwrap();
        ctx_json.as_object_mut().unwrap().shift_remove("deployment_details"); // excluded like Python
        same(&ctx_json, &back, &format!("{lg}.ctx")).unwrap();
        assert!(!ctx.all_players().is_empty());
    }
}

#[test]
fn raw_context_roundtrips() {
    for lg in leagues() {
        let raw = read(fixtures().join(lg).join("raw_ctx.json"));
        let ctx: LeagueContext = serde_json::from_value(raw.clone()).expect(lg);
        same(&raw, &serde_json::to_value(&ctx).unwrap(), &format!("{lg}.raw_ctx")).unwrap();
    }
}

#[test]
fn snapshot_loads_with_registries() {
    for lg in leagues() {
        let snap = Snapshot::load(fixtures().join(lg).join("snapshot.json")).expect(lg);
        assert_eq!(snap.league, lg);
        assert!(!snap.ctx.enrich.is_empty(), "{lg}: registries empty");
        assert!(snap.ctx.my_team().is_ok());
    }
}

/// values.json / dynasty.json store `cid` instead of the player: re-attach it, roundtrip, and
/// compare everything but the dump-excluded fields.
#[test]
fn values_roundtrip() {
    for lg in leagues() {
        let snap = Snapshot::load(fixtures().join(lg).join("snapshot.json")).unwrap();
        let players: std::collections::HashMap<String, Value> = snap
            .ctx
            .all_players()
            .into_iter()
            .map(|p| (p.cid.clone(), serde_json::to_value(p).unwrap()))
            .collect();
        let values = read(fixtures().join(lg).join("values.json"));
        for (cid, v) in values.as_object().unwrap() {
            let mut o = v.as_object().unwrap().clone();
            o.shift_remove("cid");
            o.insert("player".into(), players[cid].clone());
            let pv: PlayerValue = serde_json::from_value(Value::Object(o.clone()))
                .unwrap_or_else(|e| panic!("{lg} {cid}: {e}"));
            let back = serde_json::to_value(&pv).unwrap();
            for k in ["share_source", "share_starts", "share_team_games", "rookie"] {
                o.shift_remove(k);
            }
            // PlayerValue serializes `player` first, like pydantic
            let mut expected = serde_json::Map::new();
            expected.insert("player".into(), o["player"].clone());
            for (k, x) in o.iter().filter(|(k, _)| *k != "player") {
                expected.insert(k.clone(), x.clone());
            }
            same(&Value::Object(expected), &back, &format!("{lg}.values.{cid}")).unwrap();
        }
        let dyn_json = read(fixtures().join(lg).join("dynasty.json"));
        if let Some(map) = dyn_json.as_object() {
            for (cid, v) in map {
                let mut v = v.clone();
                let o = v.as_object_mut().unwrap();
                o.shift_remove("cid");
                o.insert("player".into(), players[cid].clone());
                let dv: DynastyValue = serde_json::from_value(v).unwrap_or_else(|e| panic!("{lg} dyn {cid}: {e}"));
                assert_eq!(&dv.player.cid, cid);
            }
        }
    }
}
