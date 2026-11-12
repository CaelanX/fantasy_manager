//! League commands of `fantasy_manager/cli.py`: `settings` (+ `_print_fit`), `picks`
//! (`_picks_by_holder`, `_pick_json`), `roster`, `waivers` (`_print_protected`), `lineup`
//! (`_daily_lineup`), `injuries`, `sync`. Owner: work package `W9-cli-core`.
//!
//! Deviations: rich markup (colours / bold) is dropped from the human output; `injuries`
//! records statuses with the run's clock (`FM_NOW` / snapshot) instead of `time.time()`.

use std::panic::{catch_unwind, AssertUnwindSafe};

use anyhow::Result;
use clap::Args;
use fm_core::models::{FantasyTeam, LeagueContext, Player};
use fm_core::values::{Horizon, PickAsset};
use fm_core::{DynastyValue, Map, PlayerValue};
use fm_engine::recommend::base::{league_unit, move_scarcity_threshold, moves_left, BASE_MIN_GAIN, no_moves_text, position_limits_text};
use fm_engine::recommend::injuries::{recommend_injuries, History};
use fm_engine::recommend::lineup::{current_total, daily_plan, optimal_lineup, recommend_lineup, starting_slots, DailyPlan};
use fm_engine::recommend::waivers::{recommend_waivers, WaiverDebug, WaiverOpts};
use fm_engine::valuation::picks::{picks_by_holder, value_picks};
use fm_engine::valuation::valuate::replacement_for;
use fm_pycompat::fmt::{fixed, general};
use fm_pycompat::num::{py_sum, round_to};
use fm_pycompat::order::{cmp_f64, first_max_by};
use fm_pycompat::time::strftime_date;
use serde_json::{json, Value};

use crate::common::*;

// --------------------------------------------------------------------------- settings

#[derive(Args, Clone, Debug, Default)]
pub struct SettingsArgs {
    #[command(flatten)]
    pub lo: LeagueOpts,
    /// Fit point values from the platform's own fantasy points (Fantrax) and print a FANTRAX_POINTS line.
    #[arg(long = "fit-points")]
    pub fit_points: bool,
}


fn nan_none(v: Option<f64>) -> Value {
    match v {
        Some(x) if !x.is_nan() => json!(x),
        _ => Value::Null,
    }
}

fn wmap(v: &Value) -> Map<String, f64> {
    v.as_object().map(|o| o.iter().filter_map(|(k, x)| x.as_f64().map(|f| (k.clone(), f))).collect()).unwrap_or_default()
}

/// `_print_fit(lc, provider, as_json)`.
fn print_fit(lc: &LeagueContext, provider: &dyn fm_core::provider::LeagueProvider, as_json: bool) -> Result<()> {
    let res = match provider.fit_points() {
        Ok(r) => r,
        Err(_) => {
            return Err(fail(format!(
                "--fit-points needs per-stat totals and the platform's own fantasy points; the {} provider does not support it (try --league fantrax).",
                lc.provider
            )))
        }
    };
    let ex = provider.extras();
    let source = ex.scoring_source.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "points".into());
    let cfg_mae = ex.config_mae.filter(|x| !x.is_nan());
    let use_mae = ex.weights_mae.filter(|x| !x.is_nan());
    let goalie = (!lc.scoring.goalie_weights.is_empty()).then_some(&lc.scoring.goalie_weights);
    let in_use = fm_core::config::format_points(&lc.scoring.weights, goalie);
    if as_json {
        dump(&json!({"fit": {"weights": res["weights"], "goalie_weights": res["goalie_weights"], "groups": res["groups"],
                              "mae": res["mae"], "n": res["n"], "residual_max": res["residual_max"],
                              "points_line": res["points_line"]},
                     "in_use": {"source": source, "points_line": format!("FANTRAX_POINTS={in_use}"), "mae": nan_none(use_mae)},
                     "configured_mae": nan_none(cfg_mae), "warnings": lc.warnings}));
        return Ok(());
    }
    let n = res["n"].as_i64().unwrap_or(0);
    if n == 0 {
        return Err(fail("No Fantrax lines with both per-stat totals and FPts were loaded; nothing to fit."));
    }
    let mut t = Table::new(format!("Fitted point values ({n} Fantrax lines, least squares)"));
    t.cols(&[("Stat", L), ("Fitted", R), (&format!("In use ({source})"), R)]);
    let mut fitted = wmap(&res["weights"]);
    for (k, v) in wmap(&res["goalie_weights"]) {
        fitted.insert(format!("goalie.{k}"), v);
    }
    let mut used: Map<String, f64> = lc.scoring.weights.clone();
    for (k, v) in &lc.scoring.goalie_weights {
        used.insert(format!("goalie.{k}"), *v);
    }
    // sorted(set(fitted) | set(used), key=(is goalie, -abs(fitted))): set order is arbitrary in
    // Python; ties keep insertion order here
    let mut keys: Vec<String> = fitted.keys().cloned().collect();
    for k in used.keys() {
        if !fitted.contains_key(k) {
            keys.push(k.clone());
        }
    }
    keys.sort_by(|a, b| {
        (a.starts_with("goalie."))
            .cmp(&b.starts_with("goalie."))
            .then(cmp_f64(-fitted.get(a).copied().unwrap_or(0.0).abs(), -fitted.get(b).copied().unwrap_or(0.0).abs()))
    });
    for k in &keys {
        t.row([
            k.clone(),
            fitted.get(k).map(|v| general(*v, None, false)).unwrap_or_else(|| "-".into()),
            used.get(k).map(|v| general(*v, None, false)).unwrap_or_else(|| "-".into()),
        ]);
    }
    t.print();
    echo("Paste into .env:");
    echo(res["points_line"].as_str().unwrap_or(""));
    let mae = res["mae"].as_f64().unwrap_or(f64::NAN);
    let rmax = res["residual_max"].as_f64().unwrap_or(f64::NAN);
    echo(format!("Fit error: mean abs {} FP/G, max {} FP/G over {n} lines", fixed(mae, 3, false), fixed(rmax, 3, false)));
    if let Some(c) = cfg_mae {
        echo(format!(
            "Configured FANTRAX_POINTS: mean abs error {} FP/G{}",
            fixed(c, 3, false),
            if source == "FANTRAX_POINTS" { String::new() } else { format!(" (not used: {source} win)") }
        ));
    }
    if let Some(u) = use_mae {
        if source != "FANTRAX_POINTS" {
            echo(format!("In use ({source}): mean abs error {} FP/G", fixed(u, 3, false)));
            echo(format!("FANTRAX_POINTS={in_use}"));
        }
    }
    footer(lc, &[]);
    Ok(())
}

pub fn settings(cli: &mut Cli, a: &SettingsArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let (lc, provider) = cli.load_context(&league, None)?;
    if a.fit_points {
        return print_fit(&lc, provider.as_ref(), as_json);
    }
    let mut lc = lc;
    let extra: Vec<String> = match catch_unwind(AssertUnwindSafe(|| provider.describe_settings())) {
        Ok(v) => v,
        Err(_) => {
            lc.warnings.push("describe_settings failed: panic".into());
            vec![]
        }
    };
    let my = lc.my_team().ok();
    if as_json {
        let teams: Vec<Value> = lc
            .teams
            .iter()
            .map(|t| json!({"team_id": t.team_id, "name": t.name, "record": t.record, "mine": t.owner_is_me}))
            .collect();
        dump(&json!({"provider": lc.provider, "league_id": lc.league_id, "season": lc.season, "name": lc.name,
                     "scoring": to_json(&lc.scoring), "roster_shape": lc.roster_shape,
                     "position_limits": lc.position_limits, "max_roster_size": lc.max_roster_size,
                     "matchup_period": lc.matchup_period, "my_team": my.map(|t| t.name.clone()),
                     "dynasty": lc.dynasty, "keeper_horizon_years": lc.keeper_horizon_years,
                     "dynasty_mode": lc.dynasty_mode.as_str(), "dynasty_mode_source": lc.dynasty_mode_source,
                     "teams": teams,
                     "free_agents_loaded": lc.free_agents.len(), "provider_settings": extra,
                     "moves": moves_json(&lc), "stat_limits": lc.stat_limits,
                     "params_source": params_source(), "warnings": lc.warnings}));
        return Ok(());
    }
    echo(format!("{}  ({} league {}, season {})", lc.name, lc.provider, lc.league_id, lc.season));
    let dyn_txt = if lc.dynasty {
        format!("   Dynasty: yes ({}-year horizon, mode {})", lc.keeper_horizon_years, lc.dynasty_mode.as_str())
    } else {
        String::new()
    };
    let mp = lc.matchup_period.map(|m| m.to_string()).unwrap_or_else(|| "None".into());
    echo(format!("Scoring: {}   Matchup period: {mp}{dyn_txt}", lc.scoring.kind.as_str()));
    if lc.scoring.kind == fm_core::models::ScoringKind::Points {
        let mut t = Table::new("Point values");
        t.cols(&[("Stat", L), ("Points", R)]);
        let mut items: Vec<(&String, &f64)> = lc.scoring.weights.iter().collect();
        items.sort_by(|a, b| cmp_f64(-a.1.abs(), -b.1.abs()));
        for (k, v) in items {
            t.row([k.clone(), general(*v, None, false)]);
        }
        for (k, v) in &lc.scoring.goalie_weights {
            t.row([format!("{k} (goalies)"), general(*v, None, false)]);
        }
        t.print();
    } else {
        echo(format!("Categories: {}", lc.scoring.categories.join(", ")));
    }
    let mut shape: Vec<(&String, &i64)> = lc.roster_shape.iter().collect();
    shape.sort_by_key(|(s, _)| slot_order(s));
    echo(format!("Roster: {}", shape.iter().map(|(s, n)| format!("{s}x{n}")).collect::<Vec<_>>().join("  ")));
    echo(format!(
        "Roster maximums: {}",
        position_limits_text(&lc).filter(|s| !s.is_empty()).unwrap_or_else(|| "none reported by the provider".into())
    ));
    echo(stat_limits_line(&lc));
    echo(moves_line(&lc));
    echo(format!("My team: {}", my.map(|t| t.name.as_str()).unwrap_or("?")));
    let mut tt = Table::new("Teams");
    tt.cols(&[("ID", L), ("Team", L), ("W-L-T", L)]);
    for team in &lc.teams {
        let rec = team.record.map(|(w, l, t)| format!("{w}-{l}-{t}")).unwrap_or_default();
        tt.row([team.team_id.clone(), team.name.clone(), rec]);
    }
    tt.print();
    echo(format!("Free agents loaded: {}", lc.free_agents.len()));
    if !extra.is_empty() {
        echo("League rules detected by the provider");
        for line in &extra {
            echo(format!("  {line}"));
        }
    }
    if !lc.dynasty && lc.provider == "fantrax" {
        echo(format!("{} (used only when dynasty valuation is on)", mode_line(&lc)));
    }
    echo(format!("Valuation params: {} (`fm harness params`)", params_source()));
    footer(&lc, &[]);
    Ok(())
}

// --------------------------------------------------------------------------- picks

/// `_picks_by_holder(lc, values, dyn)`: draft picks by holder team id ({} outside dynasty
/// leagues with pick data); a failure is a warning.
pub fn picks_by_holder_of(
    lc: &mut LeagueContext,
    values: &Map<String, PlayerValue>,
    dyn_values: Option<&Map<String, DynastyValue>>,
) -> Map<String, Vec<PickAsset>> {
    let Some(d) = dyn_values else { return Map::new() };
    if lc.draft_picks.is_empty() {
        return Map::new();
    }
    let ctx: &LeagueContext = lc;
    match catch_unwind(AssertUnwindSafe(|| picks_by_holder(value_picks(ctx, values, Some(d))))) {
        Ok(m) => m,
        Err(_) => {
            lc.warnings.push("Draft pick values failed: panic".into());
            Map::new()
        }
    }
}

/// `_pick_json(a)`.
pub fn pick_json(a: &PickAsset) -> Value {
    json!({"year": a.year, "round": a.round, "holder": a.holder, "original": a.original,
           "original_name": a.original_name, "expected_slot": round_to(a.expected_slot, 2),
           "overall": round_to(a.overall, 1), "value": round_to(a.value, 2), "net": round_to(a.net, 2),
           "years_ahead": a.years_ahead, "inferred": a.inferred})
}

fn picks_total(ps: &[PickAsset]) -> f64 {
    py_sum(ps.iter().map(|a| a.value))
}

#[derive(Args, Clone, Debug, Default)]
pub struct PicksArgs {
    #[command(flatten)]
    pub lo: LeagueOpts,
    #[command(flatten)]
    pub mode: ModeOpt,
}


pub fn picks(cli: &mut Cli, a: &PicksArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let mut d = cli.load_all(&league, false, a.mode.val())?;
    let by_holder = picks_by_holder_of(&mut d.lc, &d.values, d.dyn_values.as_ref());
    let lc = &d.lc;
    let names: Map<String, String> = lc.teams.iter().map(|t| (t.team_id.clone(), t.name.clone())).collect();
    let name_of = |tid: &str| names.get(tid).cloned().unwrap_or_else(|| tid.to_string());
    let me = lc.my_team()?.team_id.clone();
    if as_json {
        let teams: Vec<Value> = by_holder
            .iter()
            .map(|(tid, ps)| {
                json!({"team": name_of(tid), "team_id": tid, "me": *tid == me,
                       "total": round_to(picks_total(ps), 2), "picks": ps.iter().map(pick_json).collect::<Vec<_>>()})
            })
            .collect();
        dump(&json!({"teams": teams, "errors": lc.warnings}));
        return Ok(());
    }
    if by_holder.is_empty() {
        echo("No draft pick data: picks are valued in dynasty leagues whose provider reports them (Fantrax).");
        footer(lc, &[]);
        return Ok(());
    }
    let mut order: Vec<&String> = by_holder.keys().collect();
    order.sort_by(|a, b| cmp_f64(-picks_total(&by_holder[*a]), -picks_total(&by_holder[*b])));
    let me_tag = |tid: &str| if tid == me { " (me)" } else { "" };
    let mut summary = Table::new("Draft capital by team");
    summary.cols(&[("Team", L), ("Picks", R), ("Total value", R), ("Total net", R), ("Best pick", L)]);
    for tid in &order {
        let ps = &by_holder[*tid];
        let Some(best) = first_max_by(ps.iter(), |x, y| cmp_f64(x.value, y.value)) else { continue };
        summary.row([
            format!("{}{}", name_of(tid), me_tag(tid)),
            ps.len().to_string(),
            fixed(picks_total(ps), 1, false),
            fixed(py_sum(ps.iter().map(|a| a.net)), 1, false),
            format!("{} {}", best.short(Some(tid)), fixed(best.value, 1, false)),
        ]);
    }
    summary.print();
    let mut seq: Vec<&str> = vec![me.as_str()];
    seq.extend(order.iter().map(|s| s.as_str()).filter(|t| *t != me));
    for tid in seq {
        let mut ps: Vec<&PickAsset> = by_holder.get(tid).map(|v| v.iter().collect()).unwrap_or_default();
        if ps.is_empty() {
            continue;
        }
        ps.sort_by_key(|a| (a.year, a.round));
        let mut tbl = Table::new(format!("{}{}", name_of(tid), me_tag(tid)));
        tbl.cols(&[("Pick", L), ("Exp. slot", R), ("Overall", R), ("Value", R), ("Net", R)]);
        for a in ps {
            tbl.row([
                format!("{}{}", a.short(Some(tid)), if a.inferred { " *" } else { "" }),
                fixed(a.expected_slot, 1, false),
                fixed(a.overall, 0, false),
                fixed(a.value, 1, false),
                fixed(a.net, 1, false),
            ]);
        }
        tbl.print();
    }
    if by_holder.values().flatten().any(|a| a.inferred) {
        echo("* holder inferred: Fantrax did not report that team's picks, so it is assumed to hold its own");
    }
    echo(
        "Expected slot: reverse standings (next draft: this season's lineup strength blended with the record so far; \
         later drafts: dynasty strength, pulled toward the middle). Value: the dynasty value of the league's prospect \
         at that overall rank x 0.75, x 0.8 per year ahead. Net: over the roster spot the draftee will take (what \
         trades charge per added player); trades count picks at net for you and at full value for the other manager. \
         Model-only: no consensus source for pick values.",
    );
    footer(lc, &[]);
    Ok(())
}

// --------------------------------------------------------------------------- roster

#[derive(Args, Clone, Debug, Default)]
pub struct RosterArgs {
    /// Show every team, not just mine.
    #[arg(long = "all-teams")]
    pub all_teams: bool,
    /// Also fetch NHL game logs for recent form.
    #[arg(long = "deep")]
    pub deep: bool,
    #[command(flatten)]
    pub lo: LeagueOpts,
    #[command(flatten)]
    pub mode: ModeOpt,
}


pub fn roster(cli: &mut Cli, a: &RosterArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let mut d = cli.load_all(&league, a.deep, a.mode.val())?;
    let by_holder = picks_by_holder_of(&mut d.lc, &d.values, d.dyn_values.as_ref());
    let lc = &d.lc;
    let values = &d.values;
    let dynv = d.dyn_values.as_ref();
    let teams: Vec<&FantasyTeam> = if a.all_teams { lc.teams.iter().collect() } else { vec![lc.my_team()?] };
    if as_json {
        let out: Vec<Value> = teams
            .iter()
            .map(|t| {
                let slots: Vec<Value> = t
                    .slots
                    .iter()
                    .filter_map(|s| {
                        let p = s.player.as_ref()?;
                        Some(json!({"slot": s.slot, "starting": s.starting,
                                    "value": values.get(&p.cid).map(to_json).unwrap_or(Value::Null),
                                    "dynasty": dyn_json(dynv.and_then(|m| m.get(&p.cid)))}))
                    })
                    .collect();
                let empty: Vec<&str> =
                    t.slots.iter().filter(|s| s.player.is_none() && s.starting).map(|s| s.slot.as_str()).collect();
                let mut o = serde_json::Map::new();
                o.insert("team".into(), json!(t.name));
                o.insert("slots".into(), Value::Array(slots));
                o.insert("empty_starting".into(), json!(empty));
                if !by_holder.is_empty() {
                    let picks: Vec<Value> =
                        by_holder.get(&t.team_id).map(|v| v.iter().map(pick_json).collect()).unwrap_or_default();
                    o.insert("draft_picks".into(), Value::Array(picks));
                }
                Value::Object(o)
            })
            .collect();
        dump(&Value::Array(out));
        return Ok(());
    }
    let vl = vlabel(lc);
    for team in &teams {
        let mut tbl = Table::new(format!("{}{}", team.name, if team.owner_is_me { " (me)" } else { "" }));
        let mut cols: Vec<(String, Justify)> = vec![
            ("Slot".into(), L),
            ("Player".into(), L),
            ("Pos".into(), L),
            ("Age".into(), R),
            ("NHL".into(), L),
            ("Status".into(), L),
            ("GP".into(), R),
            (format!("{vl} szn"), R),
            (format!("{vl} wk"), R),
            ("G/7d".into(), R),
            ("Proj wk".into(), R),
            ("VORP".into(), R),
        ];
        if dynv.is_some() {
            cols.push(("Dynasty".into(), R));
        }
        for (c, j) in &cols {
            tbl.col(c.clone(), *j);
        }
        let mut slots: Vec<_> = team.slots.iter().filter(|s| s.player.is_some()).collect();
        slots.sort_by_key(|s| slot_order(&s.slot));
        for s in slots {
            let Some(p) = s.player.as_ref() else { continue };
            let Some(pv) = values.get(&p.cid) else { continue };
            let ag = age(p, lc, dynv);
            let name = format!("{}{}", p.name, if is_prospect(p, ag) { "*" } else { "" });
            let mut row = vec![
                s.slot.clone(),
                name,
                pos(p),
                ag.map(|x| (x.trunc() as i64).to_string()).unwrap_or_else(|| "-".into()),
                p.team.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "-".into()),
                status(p),
                p.gp("season").to_string(),
                fmtv(Some(pv.fpg_season)),
                fmtv(Some(pv.fpg_week)),
                pv.games_next7.map(|g| g.to_string()).unwrap_or_else(|| "-".into()),
                fmtv(pv.proj_week),
                fmt_signed(Some(pv.vorp)),
            ];
            if let Some(dm) = dynv {
                row.push(fmtv(dyn_value(dm.get(&p.cid))));
            }
            tbl.row(row);
        }
        let mut empty: Vec<_> = team.slots.iter().filter(|s| s.player.is_none() && s.starting).collect();
        empty.sort_by_key(|s| slot_order(&s.slot));
        for s in empty {
            let mut row = vec![s.slot.clone(), "(empty starting slot)".to_string()];
            row.extend(std::iter::repeat("-".to_string()).take(cols.len() - 2));
            tbl.row(row);
        }
        tbl.print();
        if let Some(mine) = by_holder.get(&team.team_id).filter(|v| !v.is_empty()) {
            let mut sorted: Vec<&PickAsset> = mine.iter().collect();
            sorted.sort_by_key(|a| (a.year, a.round));
            echo(format!(
                "Draft picks ({}, dynasty value {}, {} net of roster spots): {}",
                mine.len(),
                fixed(picks_total(mine), 1, false),
                fixed(py_sum(mine.iter().map(|a| a.net)), 1, false),
                sorted
                    .iter()
                    .map(|a| format!("{} {}", a.short(Some(&team.team_id)), fixed(a.value, 1, false)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if !by_holder.is_empty() {
        echo("Pick values: expected slot from standings / roster strength, worth that rank of the league's prospects less a draft-class discount (`fm picks`)");
    }
    let repl = replacement_for(lc, values, Horizon::Season);
    echo(format!(
        "Replacement FPG: {}",
        repl.iter().map(|(s, v)| format!("{s} {}", fixed(*v, 2, false))).collect::<Vec<_>>().join("  ")
    ));
    echo(format!("* prospect: age <= {PROSPECT_MAX_AGE} and fewer than {PROSPECT_MAX_GP} NHL games"));
    if values.values().any(|v| v.proj_week.is_some()) {
        echo(format!(
            "{vl} wk: schedule-adjusted, not a per-game rate (Proj wk / avg team games this week, so 4 games incl. off-nights reads above {vl})"
        ));
    }
    echo(moves_line(lc));
    footer(lc, &[]);
    Ok(())
}

// --------------------------------------------------------------------------- waivers

#[derive(Args, Clone, Debug)]
pub struct WaiversArgs {
    /// Max recommendations.
    #[arg(long = "limit", short = 'n', default_value_t = 10)]
    pub limit: usize,
    /// Value over the next week (schedule-adjusted: games this week vs the average team, not a per-game rate) or rest of season.
    #[arg(long = "horizon", value_enum, default_value = "season")]
    pub horizon: HorizonName,
    /// Also fetch NHL game logs for recent form.
    #[arg(long = "deep")]
    pub deep: bool,
    #[command(flatten)]
    pub lo: LeagueOpts,
    #[command(flatten)]
    pub mode: ModeOpt,
}


pub fn waivers(cli: &mut Cli, a: &WaiversArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, a.deep, a.mode.val())?;
    let lc = &d.lc;
    let values = &d.values;
    let h = a.horizon.horizon();
    let mut blocked: Vec<WaiverDebug> = Vec::new();
    let opts = WaiverOpts { limit: a.limit, horizon: h, dynasty_values: d.dyn_values.as_ref(), ..WaiverOpts::default() };
    let recs = recommend_waivers(lc, values, &opts, Some(&mut blocked), &cli.now(), &cli.memo())?;
    if as_json {
        dump(&recs_json(&recs));
        return Ok(());
    }
    if recs.is_empty() {
        if moves_left(lc) == Some(0) {
            echo(format!("{}.", no_moves_text(lc)));
        } else {
            let thr = move_scarcity_threshold(lc, false).filter(|t| *t != 0.0).unwrap_or(BASE_MIN_GAIN * league_unit(lc));
            let mut msg = format!("No free agent beats your weakest same-position player by more than {} FPG", fixed(thr, 1, false));
            if d.dyn_values.as_ref().is_some_and(|m| !m.is_empty()) {
                msg += " while also clearing the dynasty-value bar (x1.15, x1.5 for protected players)";
            }
            echo(format!("{msg}."));
        }
        print_protected(&blocked);
        echo(moves_line(lc));
        footer(lc, &[]);
        return Ok(());
    }
    let mut tbl = Table::new(format!("Waiver targets ({} horizon)", a.horizon.as_str())).lines();
    tbl.cols(&[("#", R), ("Add", L), ("Drop", L), ("Gain", R), ("Strength", R), ("Why", L)]);
    let fpg = |p: &Player| values.get(&p.cid).map(|v| fixed(v.fpg_for(h), 2, false)).unwrap_or_else(|| "-".into());
    for (i, r) in recs.iter().enumerate() {
        let Some(ad) = r.add.first() else { continue };
        let mut add = format!("{} ({}, {})\n{} FPG", ad.name, pos(ad), ad.team.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "FA".into()), fpg(ad));
        if let Some(w) = ad.waiver_until {
            if r.reasons.iter().any(|x| x.code == "WAIVER_CLAIM") {
                add += &format!("\nW ({}): claim", strftime_date(w, "%a"));
            }
        }
        let drop = match r.drop.first() {
            None => r
                .reasons
                .iter()
                .find(|x| x.code == "IR_MOVE")
                .map(|x| x.text.clone())
                .unwrap_or_else(|| "(no drop: open roster spot)".into()),
            Some(dp) => format!("{} ({})\n{} FPG", dp.name, pos(dp), fpg(dp)),
        };
        let lines: Vec<String> =
            r.reasons.iter().filter(|x| x.code != "FPG_ADD" && x.code != "FPG_DROP").map(|x| x.text.clone()).collect();
        tbl.row([rank_cell(r, i + 1), add, drop, gain_cell(r), strength_cell(r), why(r, &lines)]);
    }
    tbl.print();
    print_protected(&blocked);
    echo(moves_line(lc));
    footer(lc, &[]);
    Ok(())
}

/// `_print_protected(blocked)`: one line per protected player that blocked a waiver drop.
pub fn print_protected(blocked: &[WaiverDebug]) {
    let mut by_drop: Map<String, Vec<&WaiverDebug>> = Map::new();
    let mut churn: Map<String, Vec<&WaiverDebug>> = Map::new();
    let capped: Vec<&WaiverDebug> = blocked.iter().filter(|b| b.reason.code == "POSITION_CAP").collect();
    for b in blocked {
        let key = b.drop.clone().unwrap_or_default();
        if b.reason.code == "CHURN_GUARD" {
            churn.entry(key).or_default().push(b);
        } else if b.reason.code != "POSITION_CAP" {
            by_drop.entry(key).or_default().push(b);
        }
    }
    let plural = |n: usize| if n != 1 { "s" } else { "" };
    for items in churn.values() {
        if let Some(best) = first_max_by(items.iter(), |x, y| cmp_f64(x.add_value.unwrap_or(0.0), y.add_value.unwrap_or(0.0))) {
            echo(format!("Churn guard: {} (blocked {} pickup{})", best.reason.text, items.len(), plural(items.len())));
        }
    }
    for items in by_drop.values() {
        if let Some(best) = first_max_by(items.iter(), |x, y| cmp_f64(x.add_value.unwrap_or(0.0), y.add_value.unwrap_or(0.0))) {
            echo(format!("Protected: {} (blocked {} pickup{})", best.reason.text, items.len(), plural(items.len())));
        }
    }
    for b in capped.iter().take(3) {
        echo(format!("Position limit: {}", b.reason.text));
    }
}

// --------------------------------------------------------------------------- lineup

#[derive(Args, Clone, Debug)]
pub struct LineupArgs {
    /// Optimize projected week points or season FPG.
    #[arg(long = "horizon", value_enum, default_value = "week")]
    pub horizon: HorizonName,
    /// Also fetch NHL game logs for recent form.
    #[arg(long = "deep")]
    pub deep: bool,
    #[command(flatten)]
    pub lo: LeagueOpts,
}


fn opt_json<T: serde::Serialize>(x: Option<T>) -> Value {
    x.map(|v| to_json(&v)).unwrap_or(Value::Null)
}

pub fn lineup(cli: &mut Cli, a: &LineupArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, a.deep, None)?;
    let lc = &d.lc;
    let values = &d.values;
    let h = a.horizon.horizon();
    let team = lc.my_team()?;
    let slots = starting_slots(&lc.roster_shape);
    let now = cli.now();
    let memo = cli.memo();
    let plan = if h == Horizon::Week { daily_plan(lc, team, values, None, &now, &memo) } else { None };
    if let Some(plan) = plan {
        let recs = recommend_lineup(lc, values, h, &now, &memo)?;
        return daily_lineup(lc, team, &slots, &plan, &recs, as_json);
    }
    let (assign, total) = optimal_lineup(team, values, &lc.roster_shape, h);
    let cur_total = current_total(team, values, h);
    let recs = recommend_lineup(lc, values, h, &now, &memo)?;
    let where_: Map<&str, &str> =
        team.slots.iter().filter_map(|s| s.player.as_ref().map(|p| (p.cid.as_str(), s.slot.as_str()))).collect();
    let by_cid: Map<&str, &Player> = team.players_iter().map(|p| (p.cid.as_str(), p)).collect();
    struct Row<'a> {
        slot: &'a str,
        cid: Option<&'a str>,
        value: Option<f64>,
        games: Option<i64>,
    }
    let rows: Vec<Row> = slots
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            let cid = assign.get(&i).map(|c| c.as_str());
            let pv = cid.and_then(|c| values.get(c));
            Row { slot, cid, value: pv.map(|v| v.lineup_value(h)), games: pv.and_then(|v| v.games_next7) }
        })
        .collect();
    if as_json {
        let optimal: Vec<Value> = rows
            .iter()
            .map(|r| {
                let p = r.cid.and_then(|c| by_cid.get(c));
                json!({"slot": r.slot, "cid": r.cid, "name": p.map(|p| p.name.clone()),
                       "current_slot": r.cid.and_then(|c| where_.get(c).copied()),
                       "value": r.value, "games_next7": r.games})
            })
            .collect();
        dump(&json!({"horizon": a.horizon.as_str(), "optimal": optimal, "optimal_total": total, "current_total": cur_total,
                     "recommendations": recs_json(&recs), "meta": meta(lc)}));
        return Ok(());
    }
    let unit = if h == Horizon::Week && values.values().any(|v| v.proj_week.is_some()) { "proj pts" } else { "FPG" };
    let mut tbl = Table::new(format!("Optimal lineup ({}, {unit})", a.horizon.as_str()));
    tbl.cols(&[("Slot", L), ("Player", L), ("Pos", L), ("NHL", L), ("Status", L), ("G/7d", R), ("Value", R), ("Now", L)]);
    for r in &rows {
        let Some(p) = r.cid.and_then(|c| by_cid.get(c)) else {
            tbl.row([r.slot, "(empty)", "", "", "", "", "", ""]);
            continue;
        };
        let now_slot = r.cid.and_then(|c| where_.get(c).copied()).unwrap_or("-");
        tbl.row([
            r.slot.to_string(),
            p.name.clone(),
            pos(p),
            p.team.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "-".into()),
            status(p),
            r.games.map(|g| g.to_string()).unwrap_or_else(|| "-".into()),
            fmtv(r.value),
            now_slot.to_string(),
        ]);
    }
    tbl.print();
    echo(format!(
        "Current lineup: {}   Optimal: {}   Gain: {}",
        fixed(cur_total, 2, false),
        fixed(total, 2, false),
        fixed(total - cur_total, 2, true)
    ));
    if !recs.is_empty() {
        rec_table(&recs, "Lineup changes").print();
    } else {
        echo("Your lineup is already optimal.");
    }
    footer(lc, &[]);
    Ok(())
}

fn day_label(d: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    format!("{} {}", strftime_date(d, "%a %b"), d.day())
}

/// `_daily_lineup(lc, team, slots, plan, recs, as_json)`.
fn daily_lineup(
    lc: &LeagueContext,
    team: &FantasyTeam,
    slots: &[String],
    plan: &DailyPlan,
    recs: &[fm_core::models::Recommendation],
    as_json: bool,
) -> Result<()> {
    let by_cid: Map<&str, &Player> = team.players_iter().map(|p| (p.cid.as_str(), p)).collect();
    let where_: Map<&str, &str> =
        team.slots.iter().filter_map(|s| s.player.as_ref().map(|p| (p.cid.as_str(), s.slot.as_str()))).collect();
    let name = |c: &str| by_cid.get(c).map(|p| p.name.clone()).unwrap_or_default();
    let Some(today) = plan.days.first() else { return Ok(()) };
    let days: Vec<(Vec<String>, Vec<String>)> = plan
        .days
        .iter()
        .map(|d| {
            let start: Vec<String> = d.assign.values().map(|c| name(c)).collect();
            let no_slot: Vec<String> = d
                .playing
                .iter()
                .filter(|c| d.values.get(*c).copied().unwrap_or(0.0) > 0.0 && !d.assign.values().any(|x| x == *c))
                .map(|c| name(c))
                .collect();
            (start, no_slot)
        })
        .collect();
    if as_json {
        let rows: Vec<Value> = slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                let cid = today.assign.get(&i).map(|c| c.as_str());
                json!({"slot": slot, "cid": cid, "current_slot": cid.and_then(|c| where_.get(c).copied()),
                       "value": opt_json(cid.and_then(|c| today.values.get(c).copied())),
                       "locked": cid.is_some_and(|c| today.locked.contains(c))})
            })
            .collect();
        let days_json: Vec<Value> = plan
            .days
            .iter()
            .zip(&days)
            .map(|(d, (start, no_slot))| {
                json!({"date": d.day.to_string(), "optimal": d.optimal, "current": d.current,
                       "start": start, "no_slot": no_slot})
            })
            .collect();
        dump(&json!({"horizon": "week", "basis": "daily", "today": rows, "days": days_json,
                     "optimal_total": plan.optimal(), "current_total": plan.current(),
                     "recommendations": recs_json(recs), "meta": meta(lc)}));
        return Ok(());
    }
    let mut tbl = Table::new(format!("Today's lineup ({}, proj pts)", day_label(today.day)));
    tbl.cols(&[("Slot", L), ("Player", L), ("Pos", L), ("NHL", L), ("Status", L), ("Today", R), ("Left", R), ("Now", L)]);
    for (i, slot) in slots.iter().enumerate() {
        let Some(p) = today.assign.get(&i).and_then(|c| by_cid.get(c.as_str())) else {
            tbl.row([slot.as_str(), "(empty: no one free with a game)", "", "", "", "", "", ""]);
            continue;
        };
        let now_slot = where_.get(p.cid.as_str()).copied().unwrap_or("-");
        tbl.row([
            slot.clone(),
            p.name.clone(),
            pos(p),
            p.team.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "-".into()),
            status(p),
            fmtv(today.values.get(&p.cid).copied()),
            plan.remaining_starts(&p.cid).to_string(),
            format!("{now_slot}{}", if today.locked.contains(&p.cid) { " (locked)" } else { "" }),
        ]);
    }
    tbl.print();
    let mut wk = Table::new("Rest of the matchup (best lineup each day)");
    wk.cols(&[("Day", L), ("Starts", R), ("Proj", R), ("As set", R), ("Game but no slot", L)]);
    for (d, (start, no_slot)) in plan.days.iter().zip(&days) {
        let ns = if no_slot.is_empty() { "-".to_string() } else { no_slot.join(", ") };
        wk.row([day_label(d.day), start.len().to_string(), fmtv(Some(d.optimal)), fmtv(Some(d.current)), ns]);
    }
    wk.print();
    let (opt, cur) = (plan.optimal(), plan.current());
    echo(format!(
        "Rest of matchup: {} pts set daily vs {} with today's lineup left as is ({})",
        fixed(opt, 2, false),
        fixed(cur, 2, false),
        fixed(opt - cur, 2, true)
    ));
    if !recs.is_empty() {
        rec_table(recs, "Lineup changes for today").print();
    } else {
        echo("Today's lineup is already optimal.");
    }
    footer(lc, &[]);
    Ok(())
}

// --------------------------------------------------------------------------- injuries

#[derive(Args, Clone, Debug)]
pub struct InjuriesArgs {
    /// Save today's statuses as the baseline for the next run.
    #[arg(long = "record", overrides_with = "no_record")]
    pub record: bool,
    /// Do not save today's statuses.
    #[arg(long = "no-record", overrides_with = "record")]
    pub no_record: bool,
    #[command(flatten)]
    pub lo: LeagueOpts,
}


pub fn injuries(cli: &mut Cli, a: &InjuriesArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let record = !a.no_record;
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let history = cli.history()?;
    let first_run = history.last()?.is_empty();
    let recs = recommend_injuries(lc, &d.values, History::Store(&history), Horizon::Week, None, &cli.now(), &cli.memo())?;
    let written = if record { history.record(lc.teams.iter().flat_map(|t| t.players_iter()), Some(cli.clock.now))? } else { 0 };
    let team = lc.my_team()?;
    let hurt: Vec<(&str, &Player)> = team
        .slots
        .iter()
        .filter_map(|s| s.player.as_ref().map(|p| (s.slot.as_str(), p)))
        .filter(|(_, p)| !p.status.is_in(&["healthy", "unknown"]))
        .collect();
    if as_json {
        let injured: Vec<Value> = hurt
            .iter()
            .map(|(slot, p)| json!({"slot": slot, "cid": p.cid, "name": p.name, "status": p.status.as_str(), "note": p.status_note}))
            .collect();
        dump(&json!({"injured": injured, "recommendations": recs_json(&recs),
                     "first_snapshot": first_run, "status_changes_recorded": written, "meta": meta(lc)}));
        return Ok(());
    }
    if !hurt.is_empty() {
        let mut tbl = Table::new(format!("{}: injured / unavailable", team.name));
        tbl.cols(&[("Slot", L), ("Player", L), ("Pos", L), ("NHL", L), ("Status", L), ("Note", L)]);
        for (slot, p) in &hurt {
            tbl.row([
                slot.to_string(),
                p.name.clone(),
                pos(p),
                p.team.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "-".into()),
                status(p),
                p.status_note.clone().unwrap_or_default(),
            ]);
        }
        tbl.print();
    } else {
        echo("No injured players on your roster.");
    }
    if !recs.is_empty() {
        rec_table(&recs, "Injury actions").print();
    } else if first_run {
        echo("First snapshot saved; status changes will be reported from the next run.");
    }
    footer(lc, &[]);
    Ok(())
}

// --------------------------------------------------------------------------- sync

#[derive(Args, Clone, Debug, Default)]
pub struct SyncArgs {
    /// List player matches waiting for review.
    #[arg(long = "review")]
    pub review: bool,
    /// Pin a match, e.g. espn:123=8478402 (repeatable).
    #[arg(long = "confirm")]
    pub confirm: Vec<String>,
    /// Clear the HTTP cache before loading.
    #[arg(long = "refresh")]
    pub refresh: bool,
    /// Also fetch NHL game logs.
    #[arg(long = "deep")]
    pub deep: bool,
    #[command(flatten)]
    pub lo: LeagueOpts,
}


pub fn sync(cli: &mut Cli, a: &SyncArgs) -> Result<()> {
    use fm_sources::matching::crosswalk::{parse_confirm, Crosswalk};

    let (league, as_json) = cli.opts(&a.lo);
    let mut out = serde_json::Map::new();
    if !a.confirm.is_empty() {
        let xw = Crosswalk::new(cli.data_dir())?;
        let mut done = Vec::new();
        for spec in &a.confirm {
            let (src, sid, nid) = match parse_confirm(spec) {
                Ok(x) => x,
                Err(e) => {
                    xw.close();
                    return Err(fail(e));
                }
            };
            done.push(xw.confirm(&src, &sid, nid)?);
        }
        xw.close();
        if !as_json {
            for r in &done {
                let nm = if r.name.is_empty() { "?" } else { r.name.as_str() };
                let nid = r.nhl_id.map(|n| n.to_string()).unwrap_or_else(|| "None".into());
                echo(format!("Confirmed {}:{} ({nm}) -> NHL {nid}", r.source, r.source_id));
            }
        }
        out.insert("confirmed".into(), Value::Array(done.iter().map(to_json).collect()));
    }
    if a.review {
        let xw = Crosswalk::new(cli.data_dir())?;
        let rows = xw.pending(true);
        xw.close();
        let rows = rows?;
        out.insert("pending".into(), Value::Array(rows.iter().map(to_json).collect()));
        if !as_json {
            if rows.is_empty() {
                echo("No player matches need review.");
            } else {
                let mut tbl = Table::new("Player matches to review");
                tbl.cols(&[("Player", L), ("Team", L), ("Source id", L), ("Best NHL match", L), ("NHL id", L), ("Score", L), ("State", L)]);
                for r in &rows {
                    tbl.row([
                        r.name.clone(),
                        r.team.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "-".into()),
                        format!("{}:{}", r.source, r.source_id),
                        r.candidate_name.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "-".into()),
                        r.nhl_id.filter(|n| *n != 0).map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
                        r.score.filter(|s| *s != 0.0).map(|s| fixed(s, 0, false)).unwrap_or_else(|| "-".into()),
                        r.confidence.clone(),
                    ]);
                }
                tbl.print();
                echo("Confirm with: fm sync --confirm SOURCE:ID=NHLID");
            }
        }
    }
    if a.refresh || !(a.review || !a.confirm.is_empty()) {
        let cache = cli.cache()?;
        if a.refresh {
            cache.clear(None)?;
            fm_providers::registry::clear_league_cache("fantrax", &cli.settings, Some(&cache))?;
            if !as_json {
                echo("HTTP and Fantrax caches cleared.");
            }
        }
        let d = cli.load_all_with(&league, a.deep, Some(cache), None)?;
        let players = d.lc.all_players();
        let with_id = players.iter().filter(|p| p.nhl_id().is_some()).count();
        out.insert(
            "sync".into(),
            json!({"players": players.len(), "with_nhl_id": with_id, "valued": d.values.len(), "meta": meta(&d.lc)}),
        );
        if !as_json {
            echo(format!("Synced {}: {} players, {with_id} linked to NHL ids.", d.lc.name, players.len()));
            footer(&d.lc, &[]);
        }
    }
    if as_json {
        dump(&Value::Object(out));
    }
    Ok(())
}

impl crate::app::League {
    /// This module's commands (see `app` for the registry).
    pub fn commands(&self) -> Vec<crate::app::CommandSpec> {
        vec![
            crate::command_spec!("settings", 10, SettingsArgs, "Show detected league settings: scoring, point values, roster shape, teams.", settings),
            crate::command_spec!("picks", 20, PicksArgs, "Every team's draft picks with their dynasty value (dynasty leagues with pick data, Fantrax).", picks),
            crate::command_spec!("keepers", 25, crate::cmd_keepers::KeepersArgs, "Predicted keepers for every team, your bubble, the redraft pool and what your picks could land (keeper leagues).", crate::cmd_keepers::keepers),
            crate::command_spec!("roster", 30, RosterArgs, "Show roster(s) with per-game value, week projection, VORP (and dynasty value).", roster),
            crate::command_spec!("waivers", 40, WaiversArgs, "Free-agent pickups that beat your weakest same-position player.", waivers),
            crate::command_spec!("lineup", 50, LineupArgs, "Optimal starting lineup and start/sit changes.", lineup),
            crate::command_spec!("injuries", 60, InjuriesArgs, "Injury alerts for your roster: status changes, IR moves and IR activations.", injuries),
            crate::command_spec!("sync", 70, SyncArgs, "Refresh league + NHL data and manage the player id crosswalk.", sync),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injuries_record_flags() {
        use clap::{CommandFactory, Parser};
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            a: InjuriesArgs,
        }
        T::command().debug_assert();
        assert!(T::parse_from(["t", "--no-record"]).a.no_record);
        assert!(!T::parse_from(["t"]).a.no_record);
        assert!(!T::parse_from(["t", "--no-record", "--record"]).a.no_record);
    }
}
