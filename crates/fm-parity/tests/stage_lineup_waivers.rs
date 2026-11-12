//! Lineup / waivers parity (work package W6-lineup-waivers): each engine called on the snapshot
//! with the fixture values must reproduce `fixtures/<league>/engines/{lineup,waivers,
//! matchup_waivers,trending_alerts}.json`. Also: the parallel waiver loop is deterministic, the
//! timings of the hot engines are printed (`--nocapture`), and when a Python goalie-week golden is
//! available (`FM_W6_GOLDEN_DIR/<league>_goalie_week.json`, not part of the dumped fixtures yet) the
//! goalie plan is compared too.

use std::sync::Mutex;
use std::time::Instant;

use fm_core::values::Horizon;
use fm_engine::scoring::disable_point_scale;
use fm_engine::recommend::started::disable_started_games;
use fm_engine::analysis::goalie_plan::{goalie_week, GoalieWeekOpts};
use fm_engine::recommend::lineup::{disable_goalie_matchup, recommend_lineup};
use fm_engine::recommend::waivers::{recommend_matchup_waivers, recommend_trending_alerts, recommend_waivers, WaiverOpts};
use fm_engine::valuation::params;
use fm_engine::valuation::schedule::ScheduleMemo;
use fm_parity::{assert_parity, skip, Fixtures, Tolerance};
use serde_json::Value;

/// The valuation params are a process-global: one league at a time.
static LOCK: Mutex<()> = Mutex::new(());

fn run_league(league: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // the fixtures come from Python, whose lineup valued a goalie start without the opponent and
    // floored it at zero
    disable_goalie_matchup(true);
    // and used the ESPN-point thresholds unscaled in every league, with no error ranges
    disable_point_scale(true);
    disable_started_games(true);
    let test = format!("stage_lineup_waivers_{league}");
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let snap = fx.snapshot();
    params::install_with_active(snap.params.merged.clone(), Some(snap.params.active.clone()));
    let values = fx.values(&snap);
    let dynasty = fx.dynasty(&snap);
    let memo = ScheduleMemo::new(Some(Fixtures::datadir_path()));
    let ctx = &snap.ctx;
    let now = &snap.now;
    let tol = Tolerance::default();

    let t = Instant::now();
    let lineup = recommend_lineup(ctx, &values, Horizon::Week, now, &memo).expect("lineup");
    let t_lineup = t.elapsed();
    assert_parity!(&format!("{test}_lineup"), fx.engine("lineup"), serde_json::to_value(&lineup).unwrap(), tol.clone());

    let opts = WaiverOpts { dynasty_values: dynasty.as_ref(), ..WaiverOpts::default() };
    let t = Instant::now();
    let waivers = recommend_waivers(ctx, &values, &opts, None, now, &memo).expect("waivers");
    let t_waivers = t.elapsed();
    let waivers_json = serde_json::to_value(&waivers).unwrap();
    assert_parity!(&format!("{test}_waivers"), fx.engine("waivers"), waivers_json.clone(), tol.clone());

    let t = Instant::now();
    let matchup = recommend_matchup_waivers(ctx, &values, 5, dynasty.as_ref(), &[], now, &memo).expect("matchup waivers");
    let t_matchup = t.elapsed();
    let matchup_json = serde_json::to_value(&matchup).unwrap();
    assert_parity!(&format!("{test}_matchup_waivers"), fx.engine("matchup_waivers"), matchup_json.clone(), tol.clone());

    let trending = recommend_trending_alerts(ctx, Some(&values), 10, Horizon::Season);
    assert_parity!(
        &format!("{test}_trending_alerts"),
        fx.engine("trending_alerts"),
        serde_json::to_value(&trending).unwrap(),
        tol.clone()
    );

    // rayon over free agents: deterministic
    let again = recommend_matchup_waivers(ctx, &values, 5, dynasty.as_ref(), &[], now, &memo).unwrap();
    assert_eq!(serde_json::to_string(&matchup_json).unwrap(), serde_json::to_string(&serde_json::to_value(&again).unwrap()).unwrap());
    let again = recommend_waivers(ctx, &values, &opts, None, now, &memo).unwrap();
    assert_eq!(serde_json::to_string(&waivers_json).unwrap(), serde_json::to_string(&serde_json::to_value(&again).unwrap()).unwrap());

    eprintln!(
        "{league}: lineup {:?}, waivers {:?}, matchup waivers {:?} ({} free agents)",
        t_lineup,
        t_waivers,
        t_matchup,
        ctx.free_agents.len()
    );

    // goalie week vs a Python golden when one is provided
    if let Some(dir) = std::env::var_os("FM_W6_GOLDEN_DIR") {
        let path = std::path::Path::new(&dir).join(format!("{league}_goalie_week.json"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            let expected: Value = serde_json::from_str(&text).expect("golden json");
            let gopts = GoalieWeekOpts {
                data_dir: Some(Fixtures::datadir_path()),
                dynasty_values: dynasty.as_ref(),
                ..GoalieWeekOpts::default()
            };
            let gw = goalie_week(ctx, &values, &gopts, now, &memo);
            assert_parity!(&format!("{test}_goalie_week"), expected, serde_json::to_value(&gw).unwrap(), tol.clone());
            eprintln!("{league}: goalie_week matches {}", path.display());
        }
    }
}

#[test]
fn stage_lineup_waivers_espn() {
    run_league("espn");
}

#[test]
fn stage_lineup_waivers_fantrax() {
    run_league("fantrax");
}
