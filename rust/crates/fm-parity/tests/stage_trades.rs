//! Trade engine parity (work package W7-trades): each engine called on the snapshot with the
//! fixture values must reproduce `fixtures/<league>/engines/{trades,trades_with_blocks,
//! need_trades}.json` and the engine part of `cli/{trades,trades_need,trade_eval}.json`. The
//! parallel search must be deterministic (run twice, byte-identical) and its time is printed
//! (`--nocapture`).

use std::sync::Mutex;
use std::time::Instant;

use fm_core::Map;
use fm_engine::recommend::trades::{
    evaluate_offer, exploit_opportunities, find_needs, find_players, offer_summary, pick_assets, recommend_need_trades,
    recommend_trades, EngineOpts, OutlookView, TradeOpts, EXPLOIT_N, EXPLOIT_PER_TEAM, NEED_PER_TEAM,
};
use fm_engine::valuation::params;
use fm_parity::compare::{compare, report};
use fm_parity::{skip, Fixtures, Tolerance};
use serde_json::{json, Value};

/// The valuation params are a process-global: one league at a time.
static LOCK: Mutex<()> = Mutex::new(());

/// `analysis.outlook.Outlook` as dumped by `fm outlook --json` (recommended + per-team labels).
struct FixtureOutlook {
    recommended: Option<String>,
    labels: Map<String, String>,
}

impl OutlookView for FixtureOutlook {
    fn recommended(&self) -> Option<&str> {
        self.recommended.as_deref()
    }
    fn direction(&self, team_id: &str) -> Option<&str> {
        self.labels.get(team_id).map(|s| s.as_str())
    }
}

fn fixture_outlook(fx: &Fixtures) -> Option<FixtureOutlook> {
    let out = fx.try_json("cli/outlook.json").ok()?;
    let o = out.get("output")?.get("outlook")?;
    let labels = o
        .get("teams")?
        .as_array()?
        .iter()
        .filter_map(|t| Some((t.get("team_id")?.as_str()?.to_string(), t.get("label")?.as_str()?.to_string())))
        .collect();
    Some(FixtureOutlook { recommended: o.get("recommended").and_then(|v| v.as_str()).map(str::to_string), labels })
}

/// Compare and remember the failures (all comparisons run, the test fails at the end).
fn check(failures: &mut Vec<String>, name: &str, expected: &Value, actual: &Value, tol: &Tolerance) {
    let diffs = compare(expected, actual, tol);
    if diffs.is_empty() {
        let n = actual.as_array().map_or_else(|| actual.as_object().map_or(0, |o| o.len()), |a| a.len());
        let exact = compare(expected, actual, &Tolerance::exact()).len();
        eprintln!("{name}: ok ({n} items; {exact} numbers differ in the last bits)");
    } else {
        let dir = fm_parity::compare::out_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(format!("{name}.actual.json")), serde_json::to_string_pretty(actual).unwrap());
        let _ = std::fs::write(dir.join(format!("{name}.expected.json")), serde_json::to_string_pretty(expected).unwrap());
        failures.push(format!("{name}: {} diffs\n{}", diffs.len(), report(&diffs[..diffs.len().min(30)])));
    }
}

fn run_league(league: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let test = format!("stage_trades_{league}");
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let snap = fx.snapshot();
    params::install_with_active(snap.params.merged.clone(), Some(snap.params.active.clone()));
    let values = fx.values(&snap);
    let dynasty = fx.dynasty(&snap);
    let ctx = &snap.ctx;
    let tol = Tolerance::default();
    let mut failures: Vec<String> = Vec::new();

    // engines/trades.json: advise's call (dynasty values only)
    let opts = TradeOpts { dynasty_values: dynasty.as_ref(), ..TradeOpts::default() };
    let t = Instant::now();
    let trades = recommend_trades(ctx, &values, &opts, None, None).expect("trades");
    let t_trades = t.elapsed();
    assert!(!trades.is_empty(), "{league}: no trades");
    let trades_json = serde_json::to_value(&trades).unwrap();
    check(&mut failures, &format!("{test}_trades"), &fx.engine("trades"), &trades_json, &tol);

    // deterministic: same result again (parallel search, shared memos)
    let t = Instant::now();
    let again = recommend_trades(ctx, &values, &opts, None, None).expect("trades");
    let t_again = t.elapsed();
    assert_eq!(serde_json::to_string(&trades_json).unwrap(), serde_json::to_string(&serde_json::to_value(&again).unwrap()).unwrap());

    // engines/trades_with_blocks.json
    let engine = EngineOpts { wants: snap.trade_wants.as_ref(), offered: snap.trade_offered.as_ref(), ..EngineOpts::default() };
    let opts_b = TradeOpts { dynasty_values: dynasty.as_ref(), engine: engine.clone(), ..TradeOpts::default() };
    let with_blocks = recommend_trades(ctx, &values, &opts_b, None, None).expect("trades with blocks");
    check(&mut failures, &format!("{test}_trades_with_blocks"), &fx.engine("trades_with_blocks"), &serde_json::to_value(&with_blocks).unwrap(), &tol);

    // engines/need_trades.json (no outlook)
    let t = Instant::now();
    let need = recommend_need_trades(ctx, &values, &opts, None, 5, NEED_PER_TEAM, None).expect("need trades");
    let t_need = t.elapsed();
    check(&mut failures, &format!("{test}_need_trades"), &fx.engine("need_trades"), &serde_json::to_value(&need).unwrap(), &tol);

    // cli/trades.json (engine part): trades + sweet spot + future-only + exploits
    let mut sweet = Vec::new();
    let mut future = Vec::new();
    let contend = dynasty.is_some() && ctx.dynasty_mode.as_str() == "contend";
    let t = Instant::now();
    let recs = recommend_trades(ctx, &values, &opts_b, if contend { Some(&mut future) } else { None }, Some(&mut sweet)).expect("cli trades");
    let exploits = exploit_opportunities(ctx, &values, &opts_b, EXPLOIT_N, EXPLOIT_PER_TEAM).expect("exploits");
    let t_cli = t.elapsed();
    future.truncate(5);
    let wants: Value = serde_json::to_value(snap.trade_wants.clone().unwrap_or_default()).unwrap();
    let actual = json!({"trades": recs, "sweet_spot": sweet, "future_only": future, "exploits": exploits, "trade_block_wants": wants});
    let mut expected = fx.cli_output("trades");
    if let Value::Object(o) = &mut expected {
        o.shift_remove("errors");
        o.shift_remove("meta");
    }
    check(&mut failures, &format!("{test}_cli_trades"), &expected, &actual, &tol);

    // cli/trades_need.json (engine part), with the dumped outlook's directions
    if let Some(outlook) = fixture_outlook(&fx) {
        let needs = find_needs(ctx, &values, Some(&outlook), dynasty.as_ref());
        let recs = recommend_need_trades(ctx, &values, &opts_b, Some(&outlook), 10, NEED_PER_TEAM, Some(needs.clone())).expect("cli need");
        let needs_json: Vec<Value> = needs
            .iter()
            .map(|n| json!({"slot": n.slot, "gap": if n.gap == f64::NEG_INFINITY { Value::Null } else { json!(n.gap) }, "text": n.text, "long_term": n.long_term}))
            .collect();
        let actual = json!({"needs": needs_json, "direction": outlook.recommended, "need_trades": recs});
        let mut expected = fx.cli_output("trades_need");
        if let Value::Object(o) = &mut expected {
            o.shift_remove("errors");
            o.shift_remove("meta");
        }
        check(&mut failures, &format!("{test}_cli_trades_need"), &expected, &actual, &tol);
    }

    // cli/trade_eval.json (engine part): offer summary + recommendation
    let te = fx.cli("trade_eval");
    let args: Vec<String> = te["args"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    let arg = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].clone());
    let me = ctx.my_team().unwrap();
    let give = find_players(&[arg("--give").unwrap().as_str()], &me.players()).expect("give");
    let others: Vec<&fm_core::Player> = ctx.teams.iter().filter(|t| t.team_id != me.team_id).flat_map(|t| t.players_iter()).collect();
    let get = find_players(&[arg("--get").unwrap().as_str()], &others).expect("get");
    let gives: Vec<&str> = give.iter().map(|p| p.cid.as_str()).collect();
    let gets: Vec<&str> = get.iter().map(|p| p.cid.as_str()).collect();
    let owner = ctx.teams.iter().find(|t| !t.owner_is_me && t.players_iter().any(|p| p.cid == gets[0])).unwrap();
    let t = Instant::now();
    let o = evaluate_offer(ctx, &values, &gives, &gets, Some(&owner.team_id), dynasty.as_ref(), &[], &[], &engine).expect("offer");
    let t_offer = t.elapsed();
    let actual = json!({"offer": offer_summary(&o), "recommendation": o.rec});
    let mut expected = te["output"].clone();
    if let Value::Object(o) = &mut expected {
        o.shift_remove("errors");
        o.shift_remove("meta");
    }
    check(&mut failures, &format!("{test}_cli_trade_eval"), &expected, &actual, &tol);

    // pick assets as the trade engine values them vs picks.json (the CLI's own valuation)
    let picks = pick_assets(ctx, &values, dynasty.as_ref());
    let expected_picks = fx.picks();
    let n_engine: usize = picks.values().map(|v| v.len()).sum();
    let n_cli: usize = expected_picks.values().map(|v| v.len()).sum();
    assert_eq!(n_engine, n_cli, "{league}: pick count");
    for (holder, ps) in &expected_picks {
        let mine = picks.get(holder).unwrap_or_else(|| panic!("{league}: picks of {holder}"));
        for (a, b) in ps.iter().zip(mine) {
            assert_eq!((a.year, a.round, &a.original), (b.year, b.round, &b.original), "{league}: pick order of {holder}");
        }
    }

    eprintln!(
        "{league}: trades {:?} (again {:?}), need trades {:?}, cli trades+exploits {:?}, offer {:?}",
        t_trades, t_again, t_need, t_cli, t_offer
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn stage_trades_espn() {
    run_league("espn");
}

#[test]
fn stage_trades_fantrax() {
    run_league("fantrax");
}
