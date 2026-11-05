//! Analysis commands of `fantasy_manager/cli.py` (lines 2160-2619): `schedule` (+ `--playoffs`,
//! `--stream`, `--season`), `playoffs`, `outlook`, `matchup`, `plan`, `claims`.
//! Owner: work package `W10-cli-analysis`.
//!
//! Deviations: rich markup is dropped from the human output and tables are comfy-table boxes.

use std::panic::{catch_unwind, AssertUnwindSafe};

use anyhow::Result;
use clap::Args;
use fm_core::models::LeagueContext;
use fm_core::values::PlayerValue;
use fm_core::{DynastyValue, Map};
use fm_engine::analysis::matchup::{current_matchup, MatchupOpts};
use fm_engine::analysis::outlook::{build_outlook, outlook_json, recommendation_text, FUTURE_YEARS, SIMS};
use fm_engine::analysis::playoffs::{playoff_plan, PlayoffPlan, PlayoffPlayer, TARGETS};
use fm_engine::analysis::schedule_grid::{
    default_week, playoff_weeks, season_grid, streaming_targets, week_rows, Grid, PlayoffWeeks, StreamingOpts, StreamingPlan, WeekView,
    OFFNIGHT_THRESHOLD,
};
use fm_engine::recommend::claims::plan_claims;
use fm_engine::recommend::injuries::History;
use fm_engine::recommend::plan::{archive_plan, build_week_plan, PlanOpts};
use fm_providers::enrich::{enrich_context, EnrichOptions};
use fm_providers::pipeline::Loaded;
use fm_pycompat::fmt::{fixed, general, percent};
use fm_pycompat::time::{date_fromisoformat, strftime_date};
use serde_json::{json, Value};

use crate::common::*;

fn say(s: impl AsRef<str>) {
    echo(s);
}

fn pct_opt(p: Option<f64>) -> String {
    p.map(|v| percent(v, 0, false)).unwrap_or_else(|| "-".into())
}

/// A win-probability change in percentage points (`_pp`).
fn pp(delta: Option<f64>) -> String {
    delta.map(|d| format!("{}%", fixed(d * 100.0, 0, true))).unwrap_or_else(|| "-".into())
}

fn with_meta(mut v: Value, lc: &LeagueContext) -> Value {
    if let Value::Object(o) = &mut v {
        o.insert("meta".into(), meta(lc));
    }
    v
}

// --------------------------------------------------------------------------- schedule

#[derive(Args, Clone, Debug)]
pub struct ScheduleArgs {
    /// Any date in the week (YYYY-MM-DD); default: this week.
    #[arg(long = "week")]
    pub week: Option<String>,
    /// Games per NHL team in the fantasy playoffs.
    #[arg(long = "playoffs")]
    pub playoffs: bool,
    /// Free agents to stream this week, by slot.
    #[arg(long = "stream")]
    pub stream: bool,
    /// Season-long team x week game counts.
    #[arg(long = "season")]
    pub season: bool,
    /// Streaming targets per slot.
    #[arg(long = "limit", default_value_t = 10)]
    pub limit: usize,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

/// `_load_schedule_ctx(league, need_values)`: (lc, values, provider).
fn load_schedule_ctx(
    cli: &Cli,
    league: &str,
    need_values: bool,
) -> Result<(LeagueContext, Map<String, PlayerValue>, Box<dyn fm_core::provider::LeagueProvider>)> {
    if need_values {
        let d = cli.load_all(league, false, None)?;
        let Loaded { lc, values, provider, .. } = d;
        return Ok((lc, values, provider));
    }
    let (mut lc, provider) = cli.load_context(league, None)?;
    // a snapshot already carries the enriched context
    if cli.snapshot.is_none() {
        let cache = cli.cache()?;
        let r = catch_unwind(AssertUnwindSafe(|| {
            enrich_context(&mut lc, &cli.settings, Some(cache), false, EnrichOptions { clock: cli.clock, ..Default::default() })
        }));
        if let Err(p) = r {
            let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "panic".into());
            lc.warnings.push(format!("NHL enrichment failed: {msg}"));
        }
    }
    Ok((lc, Map::new(), provider))
}

fn week_table(view: &WeekView) -> Table {
    let mut tbl = Table::new(format!(
        "NHL schedule, week of {} - {} (off-night = fewer than {} games, marked *)",
        strftime_date(view.start, "%a %b %d"),
        strftime_date(view.end, "%a %b %d"),
        view.threshold
    ));
    tbl.col("Team", L);
    for ((d, n), off) in view.days.iter().zip(&view.league_games).zip(&view.off_days) {
        tbl.col(format!("{}\n{n} gm{}", strftime_date(*d, "%a %m/%d"), if *off { " *" } else { "" }), R);
    }
    tbl.cols(&[("G", R), ("Off", R), ("B2B", R)]);
    for r in &view.rows {
        let mut row: Vec<String> = vec![r.team.clone()];
        for c in &r.cells {
            match c {
                None => row.push("-".into()),
                Some(c) => {
                    let opp = c.opp.clone().filter(|o| !o.is_empty()).unwrap_or_else(|| "x".into());
                    row.push(format!("{opp}{}{}", if c.off { "*" } else { "" }, if c.b2b { "^" } else { "" }));
                }
            }
        }
        row.extend([r.games.to_string(), r.offnights.to_string(), r.b2b.to_string()]);
        tbl.row(row);
    }
    tbl
}

fn stream_tables(plan: &StreamingPlan) -> Vec<Table> {
    let mut out = Vec::new();
    let mut teams = Table::new(format!(
        "Best NHL teams to stream from ({} - {})",
        strftime_date(plan.from_day, "%a %b %d"),
        strftime_date(plan.week_end, "%a %b %d")
    ));
    teams.cols(&[("Team", L), ("Games", R), ("Off-night", R), ("B2B", R), ("Healthy FAs", R)]);
    for r in &plan.teams {
        teams.row([
            r.team.clone(),
            r.games.to_string(),
            r.offnights.to_string(),
            r.b2b.to_string(),
            r.free_agents.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
        ]);
    }
    out.push(teams);
    for (slot, targets) in &plan.by_slot {
        let mut tbl = Table::new(format!(
            "Streaming targets: {slot} (proj = per-game x games x (1 + {} x off-nights))",
            general(plan.bonus, None, false)
        ));
        tbl.cols(&[("Player", L), ("Pos", L), ("NHL", L), ("GP", R), ("Off", R), ("FP/G", R), ("Proj", R), ("Own%", R), ("Opponents", L)]);
        for t in targets {
            let mut name = t.name.clone();
            if t.status != "healthy" {
                name.push_str(&format!(" ({})", t.status));
            }
            if let Some(nd) = t.needs_drop.as_deref().filter(|n| !n.is_empty()) {
                name.push_str(&format!(" ({nd})"));
            }
            tbl.row([
                name,
                t.pos.clone(),
                t.team.clone().filter(|x| !x.is_empty()).unwrap_or_else(|| "-".into()),
                t.games.to_string(),
                t.offnights.to_string(),
                fmtv(Some(t.per_game)),
                fmtv(Some(t.proj)),
                t.pct_owned.map(|v| fixed(v, 0, false)).unwrap_or_else(|| "-".into()),
                t.opps.join(" "),
            ]);
        }
        if targets.is_empty() {
            tbl.row(["(no free agents with games)", "", "", "", "", "", "", "", ""]);
        }
        out.push(tbl);
    }
    out
}

fn playoff_table(po: &PlayoffWeeks) -> Table {
    let nums: Vec<String> = po.periods.iter().map(|p| p.number.to_string()).collect();
    let mut tbl = Table::new(format!("Fantasy playoffs: games per NHL team (periods {}; {})", nums.join(", "), po.source));
    tbl.cols(&[("#", R), ("Team", L)]);
    for p in &po.periods {
        tbl.col(format!("P{}\n{}", p.number, strftime_date(p.start, "%m/%d")), R);
    }
    tbl.cols(&[("Total", R), ("Off-night", R), ("B2B", R), ("vs avg", R)]);
    for (i, t) in po.teams.iter().enumerate() {
        let mut row = vec![(i + 1).to_string(), t.team.clone()];
        row.extend(t.games.iter().map(|g| g.to_string()));
        row.extend([t.total.to_string(), t.total_offnights.to_string(), t.total_b2b.to_string(), fixed(t.total as f64 - po.avg_total, 1, true)]);
        tbl.row(row);
    }
    tbl
}

fn season_table(grid: &Grid) -> Table {
    let mut tbl = Table::new("Games per NHL team per fantasy week (Mon-Sun)");
    tbl.col("Team", L);
    for w in &grid.weeks {
        tbl.col(strftime_date(*w, "%m/%d"), R);
    }
    tbl.col("Total", R);
    for t in &grid.teams {
        let mut row = vec![t.team.clone()];
        row.extend(t.games.iter().map(|g| g.to_string()));
        row.push(t.total_games.to_string());
        tbl.row(row);
    }
    let mut last = vec!["NHL games".to_string()];
    last.extend(grid.league_games.iter().map(|n| n.to_string()));
    last.push(String::new());
    tbl.row(last);
    tbl
}

pub fn schedule_cmd(cli: &mut Cli, a: &ScheduleArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let day = match a.week.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(t) => match date_fromisoformat(t) {
            Some(d) => Some(d),
            None => return Err(fail(format!("--week expects a date like 2026-10-12, got {:?}", a.week.as_deref().unwrap_or("")))),
        },
    };
    let (mut lc, values, provider) = load_schedule_ctx(cli, &league, a.stream)?;
    if lc.schedule.is_empty() {
        lc.warnings.push("No NHL schedule loaded (offline or the schedule request failed).".to_string());
    }
    let start = day.unwrap_or_else(|| default_week(&lc));
    let view = week_rows(&lc, start, OFFNIGHT_THRESHOLD, 7);
    let plan = if a.stream {
        Some(streaming_targets(&lc, &values, view.start, &StreamingOpts { limit: a.limit, ..StreamingOpts::default() }))
    } else if !lc.schedule.is_empty() {
        // the team half of the streaming plan needs no values
        Some(streaming_targets(&lc, &Map::new(), view.start, &StreamingOpts { limit: 0, ..StreamingOpts::default() }))
    } else {
        None
    };
    let po = if a.playoffs { Some(playoff_weeks(&lc, Some(provider.as_ref()), None, OFFNIGHT_THRESHOLD)) } else { None };
    let grid = if a.season { Some(season_grid(&lc, None, None, OFFNIGHT_THRESHOLD)) } else { None };
    if as_json {
        dump(&json!({"week": to_json(&view), "streaming": plan.as_ref().map(to_json), "playoffs": po.as_ref().map(to_json),
                     "season": grid.as_ref().map(to_json), "meta": meta(&lc)}));
        return Ok(());
    }
    if !lc.schedule.is_empty() {
        week_table(&view).print();
        say("* off-night game (fewer than 8 NHL games that night); ^ second night of a back-to-back; @XXX = away.");
    }
    if let Some(plan) = &plan {
        let tables = stream_tables(plan);
        tables[0].print();
        if a.stream {
            for t in &tables[1..] {
                t.print();
            }
        }
    }
    if let Some(po) = &po {
        for n in &po.notes {
            say(n);
        }
        if !po.teams.is_empty() {
            playoff_table(po).print();
        }
    }
    if let Some(g) = &grid {
        if !g.teams.is_empty() {
            season_table(g).print();
        }
    }
    footer(&lc, &[]);
    Ok(())
}

// --------------------------------------------------------------------------- playoffs

#[derive(Args, Clone, Debug)]
pub struct PlayoffsArgs {
    /// Players per list.
    #[arg(long = "limit", default_value_t = TARGETS)]
    pub limit: usize,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

fn playoff_player_table(title: &str, rows: &[PlayoffPlayer], plan: &PlayoffPlan, kind: &str) -> Table {
    let mut tbl = Table::new(title);
    tbl.cols(&[("Player", L), ("Pos", L), ("NHL", L)]);
    if kind == "trade" {
        tbl.col("Team", L);
    }
    for p in &plan.periods {
        tbl.col(format!("P{}", p.number), R);
    }
    tbl.cols(&[("GP", R), ("Off", R), ("vs avg", R), ("Per GP", R)]);
    if kind == "hold" {
        tbl.col("Starts", R);
    }
    tbl.col(if kind == "sell" { "Lost" } else { "Playoff pts" }, R);
    for r in rows {
        let last = if kind == "sell" { fmtv(r.cost) } else { fixed(r.points, 1, true) };
        let mut name = r.name.clone();
        if r.status != "healthy" {
            name.push_str(&format!(" ({})", r.status));
        }
        let mut row = vec![name, r.pos.clone(), r.team.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "-".into())];
        if kind == "trade" {
            row.push(r.owner.clone().filter(|o| !o.is_empty()).unwrap_or_else(|| "-".into()));
        }
        row.extend(r.games.iter().map(|g| g.to_string()));
        row.extend([r.total.to_string(), r.offnights.to_string(), fixed(r.vs_avg, 1, true), fixed(r.per_game, 2, false)]);
        if kind == "hold" {
            row.push(r.starts.unwrap_or(0).to_string());
        }
        row.push(last);
        tbl.row(row);
    }
    tbl
}

pub fn playoffs_cmd(cli: &mut Cli, a: &PlayoffsArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let p = playoff_plan(lc, &d.values, Some(d.provider.as_ref()), None, a.limit, OFFNIGHT_THRESHOLD);
    if as_json {
        dump(&with_meta(p.to_json(), lc));
        return Ok(());
    }
    if let (Some(first), Some(last)) = (p.periods.first(), p.periods.last()) {
        let nums: Vec<String> = p.periods.iter().map(|x| x.number.to_string()).collect();
        say(format!(
            "Fantasy playoffs: periods {} ({} - {}, {} days; {})",
            nums.join(", "),
            strftime_date(first.start, "%b %d"),
            strftime_date(last.end, "%b %d"),
            p.days,
            p.source
        ));
    }
    match p.deadline {
        Some(dl) => {
            let left = p.days_to_deadline.unwrap_or(0);
            let when = if left > 0 {
                format!("{left} days left")
            } else if left == 0 {
                "today".to_string()
            } else {
                "passed".to_string()
            };
            say(format!("Trade deadline: {} ({when}; {})", strftime_date(dl, "%a %b %d, %Y"), p.deadline_source.as_deref().unwrap_or("")));
        }
        None => say(format!("Trade deadline: {}.", p.deadline_source.as_deref().filter(|s| !s.is_empty()).unwrap_or("not found in the league settings"))),
    }
    if !p.my_team.is_empty() {
        say(format!(
            "{}: {} projected playoff pts (league average {}; rank {} of {}). Average NHL team: {} games.",
            p.my_team,
            fixed(p.my_points, 1, false),
            fixed(p.league_avg_points, 1, false),
            p.rank.map(|r| r.to_string()).unwrap_or_else(|| "None".into()),
            p.n_teams,
            fixed(p.avg_team_games, 1, false)
        ));
        if !p.groups.is_empty() {
            let g: Vec<String> = p.groups.iter().map(|g| format!("{} {} / {}", g.group, fixed(g.mine, 0, false), fixed(g.league_avg, 1, false))).collect();
            say(format!("Playoff starts by position (yours / league average): {}", g.join(", ")));
        }
    }
    let hold: Vec<PlayoffPlayer> = p.hold.iter().take(a.limit).cloned().collect();
    for (title, rows, kind) in [
        ("Hold: your playoff contributors", &hold, "hold"),
        ("Sell: weak playoff schedules", &p.sell, "sell"),
        ("Acquire: free agents", &p.acquire_fa, "fa"),
        ("Acquire before the deadline: other teams' players", &p.acquire_trade, "trade"),
    ] {
        if !rows.is_empty() {
            playoff_player_table(title, rows, &p, kind).print();
        }
    }
    if p.sell.is_empty() {
        say("Sell: none of your players has a playoff schedule 2+ games below the average NHL team.");
    }
    say("Playoff pts: the best lineup each playoff day with healthy per-game values; for acquisitions, what he adds to your lineup (no drop).");
    for n in &p.notes {
        say(n);
    }
    footer(lc, &[]);
    Ok(())
}

// --------------------------------------------------------------------------- outlook

#[derive(Args, Clone, Debug)]
pub struct OutlookArgs {
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn outlook_cmd(cli: &mut Cli, a: &OutlookArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let o = build_outlook(lc, &d.values, d.dyn_values.as_ref(), Some(d.provider.as_ref()), SIMS);
    if as_json {
        json_out(json!({"outlook": outlook_json(&o)}), Some(lc), &d.errors);
        return Ok(());
    }
    say(format!("Outlook: {}", recommendation_text(&o)));
    for r in &o.reasons {
        say(format!("  - {r}"));
    }
    let mut tbl = Table::new("League outlook (best lineup per game; playoff odds approximate)");
    tbl.cols(&[("Team", L), ("Record", R), ("Now", R), ("#", R)]);
    if o.dynasty {
        for y in 1..=FUTURE_YEARS {
            tbl.col(format!("+{y}y"), R);
        }
        tbl.col("Future #", R);
    }
    tbl.cols(&[("Playoffs", R), ("Direction", L)]);
    for r in &o.teams {
        let name = if r.mine() { format!("{} (me)", r.team.name) } else { r.team.name.clone() };
        let record = r.record.map(|(w, l, t)| format!("{w}-{l}-{t}")).unwrap_or_else(|| "-".into());
        let mut row = vec![name, record, fixed(r.now, 1, false), r.now_rank.to_string()];
        if o.dynasty {
            row.extend(r.future.iter().map(|x| fixed(*x, 1, false)));
            row.push(r.future_rank.map(|x| x.to_string()).unwrap_or_else(|| "-".into()));
        }
        row.push(pct_opt(r.playoff_odds));
        row.push(r.label.clone());
        tbl.row(row);
    }
    tbl.print();
    for (title, rows) in [("Sell", &o.sell), ("Buy", &o.buy)] {
        if !rows.is_empty() {
            say(format!("{title} ({})", o.recommended.as_deref().unwrap_or("None")));
            for a in rows {
                let owner = a.owner.as_deref().filter(|x| !x.is_empty()).map(|x| format!(" ({x})")).unwrap_or_default();
                say(format!("  - {}{owner}: {}", a.player.name, a.why));
            }
        }
    }
    for n in &o.notes {
        say(n);
    }
    footer(lc, &d.errors);
    Ok(())
}

// --------------------------------------------------------------------------- matchup

#[derive(Args, Clone, Debug)]
pub struct MatchupArgs {
    /// Random seed for the win-probability draws.
    #[arg(long = "seed", allow_negative_numbers = true)]
    pub seed: Option<i64>,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn matchup_cmd(cli: &mut Cli, a: &MatchupArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let opts = MatchupOpts { seed: a.seed, ..MatchupOpts::default() };
    let m = current_matchup(lc, Some(d.provider.as_ref()), Some(&d.values), opts, &cli.now(), &cli.memo())?;
    if as_json {
        dump(&with_meta(m.to_json(), lc));
        return Ok(());
    }
    let mut when = match m.period {
        Some(p) if p != 0 => format!("period {p}"),
        _ => "this period".to_string(),
    };
    if let (Some(s), Some(e)) = (m.start, m.end) {
        when.push_str(&format!(", {} - {} ({} days left)", strftime_date(s, "%a %b %d"), strftime_date(e, "%a %b %d"), m.days_left));
    }
    say(format!(
        "{} vs {} - {when}{}",
        m.my_team,
        m.opponent_team.as_deref().filter(|t| !t.is_empty()).unwrap_or("unknown opponent"),
        if m.playoffs { " (playoffs)" } else { "" }
    ));
    let mut tbl = Table::new("Matchup");
    tbl.cols(&[("", L), ("So far", R), ("Proj remaining", R), ("Proj total", R), ("Games left", R), ("SD", R)]);
    tbl.row([
        m.my_team.clone(),
        fmtv(Some(m.my_points_so_far)),
        fmtv(Some(m.my_projected_remaining)),
        fmtv(Some(m.my_projected_total())),
        m.my_games_left.to_string(),
        fmtv(Some(m.my_sd)),
    ]);
    if let Some(opp) = m.opponent_team.as_deref().filter(|t| !t.is_empty()) {
        tbl.row([
            opp.to_string(),
            fmtv(Some(m.their_points_so_far)),
            fmtv(Some(m.their_projected_remaining)),
            fmtv(Some(m.their_projected_total())),
            m.their_games_left.to_string(),
            fmtv(Some(m.their_sd)),
        ]);
    }
    tbl.print();
    if let Some(w) = m.win_probability {
        say(format!(
            "Win probability: {} ({} draws) - stance: {}   margin {}",
            percent(w, 0, false),
            m.draws,
            m.stance.as_deref().unwrap_or("None"),
            fixed(m.margin(), 1, true)
        ));
    }
    if !m.gap_by_position.is_empty() {
        let mut gt = Table::new("Projected remaining by position");
        gt.cols(&[("Pos", L), ("Mine", R), ("Theirs", R), ("Gap", R)]);
        for (g, x) in &m.gap_by_position {
            gt.row([g.clone(), fmtv(Some(x.mine)), fmtv(Some(x.theirs)), fmt_signed(Some(x.gap()))]);
        }
        gt.print();
    }
    if !m.key_players_theirs.is_empty() {
        let mut kt = Table::new("Their key players");
        kt.cols(&[("Player", L), ("Tag", L), ("Pos", L), ("NHL", L), ("Status", L), ("GP left", R), ("Proj", R), ("Note", L)]);
        for k in &m.key_players_theirs {
            kt.row([
                k.name.clone(),
                k.tag.clone(),
                k.pos.clone(),
                k.team.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "-".into()),
                k.status.clone(),
                k.games_left.to_string(),
                fmtv(Some(k.proj_remaining)),
                k.note.clone().unwrap_or_default(),
            ]);
        }
        kt.print();
    }
    for adv in &m.advice {
        say(format!("- {adv}"));
    }
    say(format!("Lineups: {}; scores: {}.", m.lineup_basis, m.source));
    for w in &m.warnings {
        say(format!("! {w}"));
    }
    footer(lc, &[]);
    Ok(())
}

// --------------------------------------------------------------------------- plan

#[derive(Args, Clone, Debug)]
pub struct PlanArgs {
    /// Don't save the plan for grading.
    #[arg(long = "no-archive")]
    pub no_archive: bool,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn plan_cmd(cli: &mut Cli, a: &PlanArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let history = cli.history()?;
    let now = cli.now();
    let data_dir = cli.data_dir().to_path_buf();
    let p = {
        let h = History::Store(&history);
        let opts = PlanOpts {
            dynasty_values: d.dyn_values.as_ref(),
            provider: Some(d.provider.as_ref()),
            history: &h,
            data_dir: Some(&data_dir),
            ..PlanOpts::default()
        };
        build_week_plan(lc, &d.values, &opts, &now, &cli.memo())
    };
    drop(history);
    if !a.no_archive {
        archive_plan(&p, &data_dir, Some(&lc.league_id), &now);
    }
    if as_json {
        dump(&with_meta(p.to_json(), lc));
        return Ok(());
    }
    let when = match (p.start, p.end) {
        (Some(s), Some(e)) => format!("{} - {}, ", strftime_date(s, "%a %b %d"), strftime_date(e, "%a %b %d")),
        _ => String::new(),
    };
    say(format!(
        "{} vs {} - {when}{} days left",
        p.my_team,
        p.opponent.as_deref().filter(|t| !t.is_empty()).unwrap_or("unknown opponent"),
        p.days_left
    ));
    if !p.available {
        for n in &p.notes {
            say(format!("- {n}"));
        }
        footer(lc, &[]);
        return Ok(());
    }
    let moves = p.moves_left.map(|m| m.to_string()).unwrap_or_else(|| "unlimited".into());
    say(format!(
        "Score {} - {} · projected {} - {} with the plan · moves left {moves}{}",
        fixed(p.my_so_far, 1, false),
        fixed(p.their_so_far, 1, false),
        fixed(p.my_so_far + p.proj_plan, 1, false),
        fixed(p.their_so_far + p.their_remaining, 1, false),
        p.stance.as_deref().filter(|s| !s.is_empty()).map(|s| format!(" · stance {s}")).unwrap_or_default()
    ));
    say(format!("Win probability: {} with quick lineup each day -> {} with the plan", pct_opt(p.win_as_set), pct_opt(p.win_plan)));
    if let Some(since) = &p.since {
        say(format!("Since your last plan{}", if since.at.is_empty() { String::new() } else { format!(" ({})", since.at) }));
        for line in since.done.iter().chain(&since.roster).chain(&since.changed).chain(&since.kept).chain(&since.new) {
            say(format!("- {line}"));
        }
    }
    let mut steps = Table::new("Plan steps beyond quick lineup (points over the rest of the matchup)");
    steps.cols(&[("#", R), ("When", L), ("Do this", L), ("Pts", R), ("Win", R), ("Why", L)]);
    let mut n = 0;
    for act in &p.today {
        n += 1;
        let gain = match act.gain {
            None => "-".to_string(),
            Some(g) if act.gain_units.as_deref() == Some("matchup_pts") => fixed(g, 1, true),
            Some(g) => format!("{} ({})", fixed(g, 2, true), act.gain_units.as_deref().unwrap_or("")),
        };
        steps.row([n.to_string(), "Today".to_string(), act.title.clone(), gain, "-".to_string(), act.headline.clone().unwrap_or_default()]);
    }
    for m in &p.moves {
        n += 1;
        let mut what = format!("{}{}", if m.claim { "Claim " } else { "Add " }, m.add.name);
        if let Some(dr) = &m.drop {
            what.push_str(&format!(", drop {}", dr.name));
        }
        if let Some(ir) = &m.to_ir {
            what.push_str(&format!(", {} to IR", ir.name));
        }
        let mut when = strftime_date(m.day, "%a");
        if m.effective != m.day {
            when.push_str(&format!(" (counts {})", strftime_date(m.effective, "%a")));
        }
        let mut why = match m.status.as_str() {
            "new" => "New · ".to_string(),
            "kept" => "Kept from your last plan · ".to_string(),
            _ => String::new(),
        };
        if !m.note.is_empty() {
            why.push_str(&format!("{} · ", m.note));
        }
        why.push_str(&m.headline);
        if !m.timing.is_empty() {
            why.push_str(&format!(" · {}", m.timing));
        }
        if let Some(mk) = m.market.as_deref().filter(|x| !x.is_empty()) {
            why.push_str(&format!(" · {mk}"));
        }
        for r in m.reasons.iter().take(1) {
            why.push_str(&format!(" · {r}"));
        }
        steps.row([n.to_string(), when, what, format!("+{}", fixed(m.gain_pts, 1, false)), pp(m.win_delta), why]);
    }
    if n > 0 {
        steps.print();
    } else {
        say("Quick lineup each day is all this week needs.");
    }
    let mut days = Table::new("Day by day (press quick lineup each day; extra steps below)");
    days.cols(&[("Day", L), ("Plan", L), ("Proj", R), ("Quick lineup", R)]);
    for line in &p.days {
        days.row([strftime_date(line.day, "%a %b %d"), line.text.clone(), fmtv(Some(line.pts_plan)), fmtv(Some(line.pts_as_set))]);
    }
    days.print();
    say(format!(
        "Plan total: {} pts remaining vs {} with quick lineup ({} from the moves)",
        fixed(p.proj_plan, 1, false),
        fixed(p.proj_as_set, 1, false),
        fixed(p.proj_plan - p.proj_as_set, 1, true)
    ));
    for w in &p.net_week {
        say(w);
    }
    for w in &p.watch {
        say(format!("Watch: {w}"));
    }
    for w in &p.claim_watch {
        say(format!("Watch: {w} - see fm claims"));
    }
    for note in &p.notes {
        say(note);
    }
    footer(lc, &[]);
    Ok(())
}

// --------------------------------------------------------------------------- claims

#[derive(Args, Clone, Debug)]
pub struct ClaimsArgs {
    /// Claims to list (also capped by the moves left).
    #[arg(short = 'n', default_value_t = 5)]
    pub n: usize,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn claims_cmd(cli: &mut Cli, a: &ClaimsArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let dyn_values: Option<&Map<String, DynastyValue>> = d.dyn_values.as_ref();
    let cp = plan_claims(lc, &d.values, dyn_values, Some(d.provider.as_ref()), &cli.now(), a.n, &cli.memo())?;
    if as_json {
        dump(&with_meta(to_json(&cp), lc));
        return Ok(());
    }
    for line in &cp.rules {
        say(line);
    }
    let head = format!("Moves left: {}", cp.moves_left.map(|m| m.to_string()).unwrap_or_else(|| "unlimited".into()));
    say(format!("{head}{}", cp.priority.as_deref().filter(|p| !p.is_empty()).map(|p| format!(" · your waiver priority {p}")).unwrap_or_default()));
    if !cp.claims.is_empty() {
        let has_bid = cp.claims.iter().any(|c| c.bid.is_some());
        let mut t = Table::new("Claims to place (in this order)");
        t.cols(&[("#", R), ("Add", L), ("Drop", L), ("Type", L), ("Counts from", L), ("Lineup games", L), ("Gain", R)]);
        if has_bid {
            t.col("Bid", R);
        }
        t.col("Why", L);
        for c in &cp.claims {
            let mut kind = match (&c.kind[..], c.clears) {
                ("claim", Some(cl)) => format!("claim, clears {}", strftime_date(cl, "%a")),
                _ => c.kind.clone(),
            };
            if c.horizon == "matchup" {
                kind.push_str(" · this matchup");
            }
            let drop = match (&c.drop, &c.to_ir) {
                (Some(dr), _) => dr.name.clone(),
                (None, Some(ir)) => format!("{} to IR", ir.name),
                _ => "open spot".to_string(),
            };
            let games: Vec<String> = c.games.iter().map(|g| strftime_date(*g, "%a")).collect();
            let games_txt = if games.is_empty() { "-".to_string() } else { games.join(", ") };
            let gain = match c.gain {
                None => "-".to_string(),
                Some(g) => match c.gain_units.as_deref() {
                    Some("matchup_pts") => format!("+{} pts", fixed(g, 1, false)),
                    Some("season_fpg") => format!("+{} FPG", fixed(g, 2, false)),
                    u => format!("+{} ({})", fixed(g, 1, false), u.unwrap_or("None")),
                },
            };
            let mut row = vec![
                c.order.to_string(),
                format!("{} ({})", c.add.name, c.add.team.as_deref().filter(|t| !t.is_empty()).unwrap_or("FA")),
                drop,
                kind,
                format!("{} {}", strftime_date(c.effective, "%a %b"), chrono::Datelike::day(&c.effective)),
                format!("{games_txt} ({} games in 7 days)", c.games_7d),
                gain,
            ];
            if has_bid {
                row.push(c.bid.map(|b| format!("${}", general(b, None, false))).unwrap_or_else(|| "-".into()));
            }
            row.push(c.headline.clone());
            t.row(row);
        }
        t.print();
    }
    for note in &cp.notes {
        say(note);
    }
    footer(lc, &[]);
    Ok(())
}

impl crate::app::Analysis {
    pub fn commands(&self) -> Vec<crate::app::CommandSpec> {
        vec![
            crate::command_spec!("schedule", 240, ScheduleArgs, "NHL schedule grid for a fantasy week (off-nights, back-to-backs), streaming targets and fantasy-playoff schedule strength.", schedule_cmd),
            crate::command_spec!("playoffs", 250, PlayoffsArgs, "Fantasy-playoff planner: your roster's playoff games and projected points, players to hold, sell and acquire before the trade deadline.", playoffs_cmd),
            crate::command_spec!("outlook", 260, OutlookArgs, "Contention window: every team's lineup strength now and over the next seasons, playoff odds, and whether to contend, stay balanced or rebuild.", outlook_cmd),
            crate::command_spec!("matchup", 270, MatchupArgs, "This period's head-to-head matchup: score, projected remaining points, win probability and advice.", matchup_cmd),
            crate::command_spec!("plan", 280, PlanArgs, "This week's action plan: today's moves, which pickups to make on which day, the lineup for each remaining day, and the points / win probability each step adds.", plan_cmd),
            crate::command_spec!("claims", 290, ClaimsArgs, "Claims to place now, in order: who to add, the drop that keeps the roster legal, when each clears and counts, and a FAAB bid if the league uses one.", claims_cmd),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct T<A: Args + Send + Sync + 'static> {
        #[command(flatten)]
        a: A,
    }

    #[test]
    fn args_parse() {
        T::<ScheduleArgs>::command().debug_assert();
        T::<ClaimsArgs>::command().debug_assert();
        T::<MatchupArgs>::command().debug_assert();
        let c = T::<ClaimsArgs>::parse_from(["t", "-n", "3"]);
        assert_eq!(c.a.n, 3);
        let s = T::<ScheduleArgs>::parse_from(["t", "--stream", "--week", "2026-10-12", "--limit", "4"]);
        assert!(s.a.stream && s.a.limit == 4 && s.a.week.as_deref() == Some("2026-10-12"));
        assert_eq!(T::<MatchupArgs>::parse_from(["t", "--seed", "7"]).a.seed, Some(7));
    }

    #[test]
    fn pp_formats_points() {
        assert_eq!(pp(Some(0.034)), "+3%");
        assert_eq!(pp(None), "-");
    }
}
