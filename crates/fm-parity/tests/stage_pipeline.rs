//! Load-pipeline parity (work package W5-provider-pipeline).
//!
//! * `pipeline_from_snapshot_<league>`: `pipeline::from_snapshot(fixtures/<league>)` (snapshot
//!   context + SnapshotProvider + snapshot params) reproduces `values.json`, `dynasty.json` and
//!   `picks.json`; the SnapshotProvider replays remote calls from the tape when present.
//! * `pipeline_end_to_end_<league>`: the whole `_load_all` replayed offline from the HTTP tape
//!   (tape-backed provider -> dynasty mode from the data dir's prefs -> enrich -> valuate ->
//!   dynasty) reproduces the snapshot context and the same values / dynasty / picks. Params are
//!   installed from the (temp) data dir like a real run. Skips when the tape or harness.db is
//!   absent (point `FM_PARITY_FIXTURES` at the full set). Prints per-stage timings.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use fm_core::provider::LeagueProvider;
use fm_core::{DynastyValue, Map, PickAsset, PlayerValue};
use fm_engine::scoring::disable_point_scale;
use fm_engine::recommend::started::disable_started_games;
use fm_engine::valuation::adjust::{pin_season_length, SeasonLength};
use fm_engine::valuation::valuate::{disable_ir_fill, disable_team_goalie};
use fm_engine::valuation::dynasty::disable_positional_dynasty;
use fm_engine::valuation::params;
use fm_engine::valuation::picks::{picks_by_holder, value_picks};
use fm_http::{HttpCache, Tape};
use fm_parity::{assert_parity, normalize, skip, Fixtures, Tolerance};
use fm_providers::enrich::Clock;
use fm_providers::pipeline::{self, LoadOptions, Loaded, ParamsSource};
use fm_providers::snapshot::{snapshot_settings, tape_provider};
use fm_pycompat::num::round_to;
use serde_json::{json, Value};

/// The valuation params are a process-global: one league at a time.
static LOCK: Mutex<()> = Mutex::new(());

const AGE_RE: &str = r"(?:just now|\b\d+(?:s|m|h|d) ago)";

/// `dump_fixtures.pv_json`
fn values_json(values: &Map<String, PlayerValue>) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(c, pv)| {
                let mut v = serde_json::to_value(pv).unwrap();
                let o = v.as_object_mut().unwrap();
                o.shift_remove("player");
                o.insert("cid".into(), Value::String(pv.player.cid.clone()));
                o.insert("share_source".into(), serde_json::to_value(&pv.share_source).unwrap());
                o.insert("share_starts".into(), serde_json::to_value(pv.share_starts).unwrap());
                o.insert("share_team_games".into(), serde_json::to_value(pv.share_team_games).unwrap());
                o.insert("rookie".into(), serde_json::to_value(&pv.rookie).unwrap());
                (c.clone(), v)
            })
            .collect(),
    )
}

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

/// `cli._pick_json`
fn pick_json(a: &PickAsset) -> Value {
    json!({"year": a.year, "round": a.round, "holder": a.holder, "original": a.original,
           "original_name": a.original_name, "expected_slot": round_to(a.expected_slot, 2),
           "overall": round_to(a.overall, 1), "value": round_to(a.value, 2), "net": round_to(a.net, 2),
           "years_ahead": a.years_ahead, "inferred": a.inferred})
}

/// values / dynasty / picks of a Loaded against the league's fixtures.
fn check_outputs(test: &str, fx: &Fixtures, d: &Loaded) {
    let expected = fx.json("values.json");
    assert_eq!(expected.as_object().map(|o| o.len()), Some(d.values.len()), "{test}: player count");
    assert_parity!(&format!("{test}_values"), expected, values_json(&d.values), normalize::set_order_fields(Tolerance::default()));
    let exp_dyn = fx.json("dynasty.json");
    match &d.dyn_values {
        None => {
            assert!(exp_dyn.is_null(), "{test}: dynasty values expected");
            assert_eq!(fx.json("picks.json"), json!({}));
        }
        Some(dv) => {
            assert_parity!(&format!("{test}_dynasty"), exp_dyn, dyn_json(dv), Tolerance::default());
            let by_holder = if d.lc.draft_picks.is_empty() {
                Map::new()
            } else {
                picks_by_holder(value_picks(&d.lc, &d.values, Some(dv)))
            };
            let actual = Value::Object(
                by_holder.iter().map(|(k, v)| (k.clone(), Value::Array(v.iter().map(pick_json).collect()))).collect(),
            );
            assert_parity!(&format!("{test}_picks"), fx.json("picks.json"), actual, Tolerance::default());
        }
    }
}

fn from_snapshot(league: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // the fixtures come from Python, which assumed 82 games in 186 days whatever the schedule said
    pin_season_length(Some(SeasonLength::DEFAULT));
    // and valued an injured player's missed games at zero even when an IR slot held him
    disable_ir_fill(true);
    // and used the ESPN-point thresholds unscaled in every league, with no error ranges
    disable_point_scale(true);
    disable_started_games(true);
    // and projected goalie wins and shots against from the goalie, not his team
    disable_team_goalie(true);
    // and valued the dynasty future with one replacement level and a rising goalie curve
    disable_positional_dynasty(true);
    let test = format!("pipeline_from_snapshot_{league}");
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let dir = Fixtures::root().join(league);
    let d = pipeline::from_snapshot(&dir, Some(&Fixtures::datadir_path())).expect("from_snapshot");
    let snap = fx.snapshot();
    assert_eq!(d.lc.dynasty_mode, snap.ctx.dynasty_mode);
    assert_eq!(d.clock.today, snap.today);
    assert_eq!(d.provider.name(), league);
    assert_eq!(d.provider.extras(), &snap.provider);
    check_outputs(&test, &fx, &d);

    // remote calls replay the tape (lazy tape-backed provider)
    if fx.tape_path().is_some() {
        let acts = d.provider.activity(None).expect("activity from the tape");
        if league == "fantrax" {
            assert!(!d.provider.describe_settings().is_empty(), "fantrax describe_settings");
            let lineups = d.provider.lineup_snapshot(None).expect("lineup snapshot");
            assert!(!lineups.is_empty());
        }
        eprintln!("{test}: {} activity items replayed", acts.len());
    } else {
        assert!(d.provider.activity(None).is_err(), "no tape: remote calls fail like a cache miss");
    }
}

#[test]
fn pipeline_from_snapshot_espn() {
    from_snapshot("espn");
}

#[test]
fn pipeline_from_snapshot_fantrax() {
    from_snapshot("fantrax");
}

fn end_to_end(league: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // the fixtures come from Python, which assumed 82 games in 186 days whatever the schedule said
    pin_season_length(Some(SeasonLength::DEFAULT));
    // and valued an injured player's missed games at zero even when an IR slot held him
    disable_ir_fill(true);
    // and used the ESPN-point thresholds unscaled in every league, with no error ranges
    disable_point_scale(true);
    disable_started_games(true);
    // and projected goalie wins and shots against from the goalie, not his team
    disable_team_goalie(true);
    // and valued the dynasty future with one replacement level and a rising goalie curve
    disable_positional_dynasty(true);
    let test = format!("pipeline_end_to_end_{league}");
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let Some(tape_path) = fx.tape_path() else { return skip(&test, "tape.json.gz missing (set FM_PARITY_FIXTURES)") };
    if Fixtures::harness_db().is_none() {
        return skip(&test, "_datadir/harness.db missing (set FM_PARITY_FIXTURES)");
    }
    let snap = fx.snapshot();
    let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let settings = snapshot_settings(&snap, &data_dir);
    let t0 = Instant::now();
    let tape = Tape::load(&tape_path).expect("tape loads");
    let t_tape = t0.elapsed();
    let cache = Arc::new(HttpCache::from_tape(tape.clone()));
    let now = snap.now.to_utc().expect("aware snapshot clock");
    let clock = Clock { now: now.timestamp() as f64 + now.timestamp_subsec_micros() as f64 / 1e6, today: snap.today };
    let provider: Box<dyn LeagueProvider> = tape_provider(league, settings.clone(), tape, snap.today).expect("provider");
    let opts = LoadOptions { cache: Some(cache), clock: Some(clock), params: ParamsSource::DataDir, ..Default::default() };

    let t1 = Instant::now();
    let d = pipeline::load_all_with(provider, &settings, &opts).expect("load_all");
    let t_pipe = t1.elapsed();
    eprintln!("{test}: tape load {t_tape:?}, pipeline (provider load + enrich + valuate + dynasty) {t_pipe:?}");

    // a real run's params: packaged (the fixture data dir has no harness override)
    assert_eq!(params::current().merged, snap.params.merged, "{test}: params");
    assert!(d.errors.is_empty());

    // the enriched context equals the dumped one (dumped after valuation)
    let mut exp_ctx = fx.json("snapshot.json")["ctx"].clone();
    exp_ctx.as_object_mut().unwrap().shift_remove("deployment_details");
    let tol = Tolerance::default()
        .with_normalize("$.source_notes[*]", AGE_RE, "<age>")
        .with_ignore("$.sources[*].fetched_at")
        .with_ignore("$.sources[*].age_seconds");
    assert_parity!(&format!("{test}_ctx"), exp_ctx, serde_json::to_value(&d.lc).unwrap(), tol);
    assert_eq!(d.provider.extras().warnings, snap.provider.warnings);
    check_outputs(&test, &fx, &d);
}

#[test]
fn pipeline_end_to_end_espn() {
    end_to_end("espn");
}

#[test]
fn pipeline_end_to_end_fantrax() {
    end_to_end("fantrax");
}

/// Live-network timing of enrichment from a cold cache (both leagues' raw contexts, one fresh
/// temp data dir: ESPN cold, then Fantrax with the NHL data already cached). Not a parity check
/// (live data). Run:
/// `cargo test -p fm-parity --test stage_pipeline live_enrich_timing -- --ignored --nocapture`,
/// once with `FM_FETCH_WORKERS=1` (sequential, like Python) and once without.
#[test]
#[ignore]
fn live_enrich_timing() {
    use fm_core::config::Settings;
    use fm_core::models::LeagueContext;
    use fm_providers::enrich::{enrich_context, EnrichOptions};

    let (dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let settings = Settings { fm_data_dir: dir.clone(), ..Settings::default() };
    let cache = Arc::new(HttpCache::open(&dir, false).expect("cache"));
    for league in ["espn", "fantrax"] {
        let fx = Fixtures::league(league);
        let mut ctx: LeagueContext = serde_json::from_value(fx.raw_ctx()).expect("raw_ctx parses");
        let t = Instant::now();
        enrich_context(&mut ctx, &settings, Some(cache.clone()), false, EnrichOptions { clock: Clock::system(), ..Default::default() });
        eprintln!(
            "live_enrich_timing {league}: {:?} (workers {}), {} warnings",
            t.elapsed(),
            fm_providers::enrich::fetch_workers(),
            ctx.warnings.len()
        );
        for w in &ctx.warnings {
            eprintln!("  warning: {w}");
        }
    }
}
