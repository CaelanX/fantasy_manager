//! Rust port of `fantasy_manager/providers/xg_enrich.py`: fill MoneyPuck expected-goals fields on
//! a LeagueContext's skaters (and goalie GSAx). Owner: work package `W4-enrich` (wave 4).
//!
//! For every skater with an NHL id the season used is the current one once he has
//! `MIN_CURRENT_GP` (5) MoneyPuck games, else the prior season, else a small current sample:
//! `ixg_per_game`, `goals_minus_ixg` (situation `all`), `onice_sh_pct` / `onice_xg_pct`
//! (situation `5on5`), `xg_split`. Goalies get a regressed GSAx registered per (current season
//! start year, NHL id). Header guard: a season file missing a column a signal reads skips that
//! signal for that season with a warning.
//!
//! Deviations: the registries `_GSAX` / `_NORMS` are `ctx.enrich.gsax` / `ctx.enrich.xg_norms`;
//! `today` (Python `date.today()` inside `MoneyPuckClient.from_cache`) is a parameter; the
//! `players=` override is not ported (always `ctx.all_players()`).

use std::collections::HashSet;
use std::sync::Arc;

use chrono::NaiveDate;
use fm_core::models::LeagueContext;
use fm_core::snapshot::GoalieGsax;
use fm_core::Map;
use fm_engine::valuation::regression::{regressed_gsax, GsaxSeason, GSAX_K_SHOTS, GSAX_SEASON_WEIGHTS, MIN_CURRENT_GP};
use fm_http::HttpCache;
use fm_pycompat::num::{py_sum, round_to};
use fm_sources::moneypuck::{
    current_start_year, goalie_calibration, onice_sh_norms, start_year, Kind, MoneyPuckClient, MpGoalie, MpSkater,
    CORE_COLUMNS, CREDIT, GOALIE_GSAX_COLUMNS, SKATER_ONICE_COLUMNS, SKATER_XG_COLUMNS,
};

use crate::enrich::{ctx_players_mut, exc_name};

/// `XgEnrichResult`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct XgEnrichResult {
    /// (current, prior) start years
    pub seasons: (i64, i64),
    /// cid -> start year used
    pub filled: Map<String, i64>,
    /// cids filled from the prior season
    pub from_prior: HashSet<String>,
    /// current season, < MIN_CURRENT_GP GP
    pub small_sample: HashSet<String>,
    /// start year -> skater rows (all)
    pub rows: Map<i64, usize>,
    /// 5v5 norms of the season used most
    pub onice_sh_norms: Map<String, f64>,
    pub errors: Vec<String>,
    /// cid -> regressed GSAx
    pub goalies: Map<String, GoalieGsax>,
    pub goalie_errors: Vec<String>,
    /// header-guard warnings
    pub missing: Vec<String>,
}

impl XgEnrichResult {
    pub fn count(&self) -> usize {
        self.filled.len()
    }
}

fn label(y: i64) -> String {
    let next = (y + 1).to_string();
    format!("{y}-{}", &next[next.len().saturating_sub(2)..])
}

fn err_text(e: &anyhow::Error) -> String {
    fm_pycompat::text::py_slice_str(&e.to_string(), None, Some(120)).to_string()
}

/// Fill the MoneyPuck fields; `xg_ok` / `onice_ok` false (a required column missing from the
/// file) leave that signal's fields unset.
pub fn apply_xg(p: &mut fm_core::models::Player, row: &MpSkater, five: Option<&MpSkater>, xg_ok: bool, onice_ok: bool) {
    if xg_ok {
        p.ixg_per_game = if row.gp > 0 { Some(round_to(row.ixg / row.gp as f64, 4)) } else { None };
        p.goals_minus_ixg = Some(round_to(row.goals - row.ixg, 2));
    }
    let src = five.unwrap_or(row);
    if onice_ok {
        p.onice_sh_pct = src.onice_sh_pct.map(|v| round_to(v, 4));
    }
    p.onice_xg_pct = src.onice_xg_pct;
}

type SeasonRows = (Map<i64, MpSkater>, Map<i64, MpSkater>);

fn season_rows(client: &MoneyPuckClient<'_>, year: i64) -> anyhow::Result<SeasonRows> {
    let rows = client.skaters(year, None, false)?;
    let mut all: Map<i64, MpSkater> = Map::new();
    let mut five: Map<i64, MpSkater> = Map::new();
    for r in rows {
        if r.situation == "all" {
            all.insert(r.nhl_id, r);
        } else if r.situation == "5on5" {
            five.insert(r.nhl_id, r);
        }
    }
    Ok((all, five))
}

/// True when the season file has every `required` column (`miss` = the missing ones, from
/// `client.missing(year, kind, required)`); else a warning and false.
fn guard(warnings: &mut Vec<String>, res: &mut XgEnrichResult, miss: &[String], year: i64, kind: Kind, what: &str) -> bool {
    if miss.is_empty() {
        return true;
    }
    let msg = format!(
        "MoneyPuck {} {}.csv is missing column(s) {}: {what} skipped for that season",
        label(year),
        kind.as_str(),
        miss.join(", ")
    );
    res.missing.push(msg.clone());
    if !warnings.contains(&msg) {
        warnings.push(msg);
    }
    false
}

/// Fill ixG fields for `ctx.all_players()`. `seasons` = (current, prior) as start years or NHL
/// season ids (default: from `ctx.as_of`). `client` defaults to a MoneyPuckClient reading
/// through `cache` (12 h / 30 d TTLs), else the uncached network client.
pub fn enrich_xg(
    ctx: &mut LeagueContext,
    cache: Option<&Arc<HttpCache>>,
    seasons: Option<(i64, i64)>,
    client: Option<&MoneyPuckClient<'_>>,
    today: NaiveDate,
) -> XgEnrichResult {
    match client {
        Some(c) => xg_with(ctx, seasons, c),
        None => {
            let own = match cache {
                Some(c) => MoneyPuckClient::from_cache(c.as_ref(), Some(today)),
                None => MoneyPuckClient::with_default_fetch(),
            };
            xg_with(ctx, seasons, &own)
        }
    }
}

/// (current, prior) start years: `seasons` (start years or NHL season ids) or from `as_of`.
pub fn xg_seasons(as_of: NaiveDate, seasons: Option<(i64, i64)>) -> (i64, i64) {
    match seasons {
        Some((a, b)) => (start_year(a), start_year(b)),
        None => {
            let cur = current_start_year(Some(as_of));
            (cur, cur - 1)
        }
    }
}

/// One season's skater file: rows by situation and the header guards' missing columns.
#[derive(Debug)]
pub struct SkaterYear {
    rows: SeasonRows,
    core_missing: Vec<String>,
    xg_missing: Vec<String>,
    onice_missing: Vec<String>,
}

/// One season's goalie file (situation `all`) and the GSAx header guard's missing columns.
#[derive(Debug)]
pub struct GoalieYear {
    rows: Vec<MpGoalie>,
    missing: Vec<String>,
}

/// Everything `enrich_xg` reads from MoneyPuck, loaded without the context (so it can be
/// fetched and parsed on another thread while the earlier enrichment steps run). Failures are
/// kept as the warning text `enrich_xg` would have produced at that point.
#[derive(Debug)]
pub struct XgData {
    pub seasons: (i64, i64),
    goalies: Vec<(i64, Result<GoalieYear, String>)>,
    skaters: Vec<(i64, Result<SkaterYear, String>)>,
}

/// Load the goalie files of `cur_y` and the `GSAX_SEASON_WEIGHTS` seasons before it, then the
/// skater files of `cur_y` and `prior_y`, in the order `enrich_xg` reads them.
pub fn load_xg_data(client: &MoneyPuckClient<'_>, seasons: (i64, i64)) -> XgData {
    let (cur_y, prior_y) = seasons;
    let required: Vec<&str> = CORE_COLUMNS.iter().chain(GOALIE_GSAX_COLUMNS.iter()).copied().collect();
    let goalies = (0..GSAX_SEASON_WEIGHTS.len() as i64)
        .map(|i| cur_y - i)
        .map(|y| {
            let r = match client.goalies(y, Some("all")) {
                Ok(rows) => Ok(GoalieYear { rows, missing: client.missing(y, Kind::Goalies, &required).unwrap_or_default() }),
                Err(e) => Err(format!("MoneyPuck goalies {}: {}: {}", label(y), exc_name(&e), err_text(&e))),
            };
            (y, r)
        })
        .collect();
    let core: Vec<&str> = CORE_COLUMNS.to_vec();
    let skaters = [cur_y, prior_y]
        .into_iter()
        .map(|y| {
            let r = match season_rows(client, y) {
                Ok(rows) => {
                    let miss = |req: &[&str]| client.missing(y, Kind::Skaters, req).unwrap_or_default();
                    Ok(SkaterYear {
                        rows,
                        core_missing: miss(&core),
                        xg_missing: miss(&SKATER_XG_COLUMNS),
                        onice_missing: miss(&SKATER_ONICE_COLUMNS),
                    })
                }
                Err(e) => Err(format!("MoneyPuck {}: {}: {}", label(y), exc_name(&e), err_text(&e))),
            };
            (y, r)
        })
        .collect();
    XgData { seasons, goalies, skaters }
}

fn xg_with(ctx: &mut LeagueContext, seasons: Option<(i64, i64)>, client: &MoneyPuckClient<'_>) -> XgEnrichResult {
    let data = load_xg_data(client, xg_seasons(ctx.as_of, seasons));
    enrich_xg_with_data(ctx, data)
}

/// [`enrich_xg`] over already loaded MoneyPuck data ([`load_xg_data`]): same fields, notes and
/// warnings, in the same order.
pub fn enrich_xg_with_data(ctx: &mut LeagueContext, xd: XgData) -> XgEnrichResult {
    let (cur_y, prior_y) = xd.seasons;
    let mut res = XgEnrichResult { seasons: (cur_y, prior_y), ..Default::default() };

    enrich_goalies(ctx, xd.goalies, &mut res, cur_y);

    let mut data: Map<i64, SeasonRows> = Map::new();
    let mut xg_ok: Map<i64, bool> = Map::new();
    let mut onice_ok: Map<i64, bool> = Map::new();
    for (y, r) in xd.skaters {
        let sy = match r {
            Ok(sy) => {
                res.rows.insert(y, sy.rows.0.len());
                sy
            }
            Err(msg) => {
                // network / parse failure: keep going with what we have
                res.errors.push(msg.clone());
                ctx.warnings.push(msg);
                data.insert(y, (Map::new(), Map::new()));
                continue;
            }
        };
        if !guard(&mut ctx.warnings, &mut res, &sy.core_missing, y, Kind::Skaters, "expected goals") {
            data.insert(y, (Map::new(), Map::new()));
            continue;
        }
        data.insert(y, sy.rows);
        let x = guard(&mut ctx.warnings, &mut res, &sy.xg_missing, y, Kind::Skaters, "the xG goal shrink (XG_SHRINK)");
        xg_ok.insert(y, x);
        let o = guard(&mut ctx.warnings, &mut res, &sy.onice_missing, y, Kind::Skaters, "on-ice shooting % luck");
        onice_ok.insert(y, o);
    }
    if !data.values().any(|d| !d.0.is_empty()) {
        return res;
    }

    let empty: SeasonRows = (Map::new(), Map::new());
    let cur_d = data.get(&cur_y).unwrap_or(&empty);
    let prior_d = data.get(&prior_y).unwrap_or(&empty);
    for p in ctx_players_mut(ctx) {
        let Some(pid) = p.nhl_id() else { continue };
        if p.is_goalie() {
            continue;
        }
        let cur = cur_d.0.get(&pid);
        let prior = prior_d.0.get(&pid);
        let (use_, y) = if let Some(c) = cur.filter(|c| c.gp >= MIN_CURRENT_GP) {
            (c, cur_y)
        } else if let Some(pr) = prior.filter(|pr| pr.gp > 0) {
            res.from_prior.insert(p.cid.clone());
            (pr, prior_y)
        } else if let Some(c) = cur.filter(|c| c.gp > 0) {
            res.small_sample.insert(p.cid.clone());
            (c, cur_y)
        } else {
            continue;
        };
        let five = data.get(&y).and_then(|d| d.1.get(&pid));
        apply_xg(p, use_, five, *xg_ok.get(&y).unwrap_or(&true), *onice_ok.get(&y).unwrap_or(&true));
        p.xg_split = Some(if res.from_prior.contains(&p.cid) { "prior" } else { "season" }.to_string());
        res.filled.insert(p.cid.clone(), y);
    }

    let used: Vec<i64> = [cur_y, prior_y]
        .into_iter()
        .filter(|y| data.get(y).is_some_and(|d| !d.1.is_empty()) && *onice_ok.get(y).unwrap_or(&true))
        .collect();
    if !used.is_empty() {
        let count = |y: i64| res.filled.values().filter(|v| **v == y).count();
        let main = fm_pycompat::order::first_max_by(used.iter().copied(), |a, b| count(*a).cmp(&count(*b))).expect("non-empty");
        res.onice_sh_norms = onice_sh_norms(data[&main].1.values(), 200.0);
        if !res.onice_sh_norms.is_empty() {
            Arc::make_mut(&mut ctx.enrich).xg_norms.insert(cur_y, res.onice_sh_norms.clone());
        }
    }
    if !res.filled.is_empty() {
        let n_cur = res.filled.values().filter(|v| **v == cur_y).count();
        let mut parts: Vec<String> = Vec::new();
        if n_cur > 0 {
            parts.push(format!("{} for {n_cur} skaters", label(cur_y)));
        }
        if !res.from_prior.is_empty() {
            parts.push(format!(
                "{} for {} with < {MIN_CURRENT_GP} GP in {}",
                label(prior_y),
                res.from_prior.len(),
                label(cur_y)
            ));
        }
        ctx.source_notes.push(format!("{CREDIT} ({})", parts.join("; ")));
    }
    res
}

/// Regressed GSAx for the context's goalies (with NHL ids) from the goalie files of `cur_y` and
/// the seasons before it (one per `GSAX_SEASON_WEIGHTS` entry). Registered for valuation;
/// failures are warnings.
fn enrich_goalies(ctx: &mut LeagueContext, files: Vec<(i64, Result<GoalieYear, String>)>, res: &mut XgEnrichResult, cur_y: i64) {
    {
        let st = Arc::make_mut(&mut ctx.enrich);
        st.gsax.retain(|k, _| k.0 != cur_y);
    }
    let goalies: Vec<(String, i64)> =
        ctx.all_players().iter().filter(|p| p.is_goalie()).filter_map(|p| p.nhl_id().map(|n| (p.cid.clone(), n))).collect();
    if goalies.is_empty() {
        return;
    }
    let years: Vec<i64> = (0..GSAX_SEASON_WEIGHTS.len() as i64).map(|i| cur_y - i).collect();
    // year -> nhl id -> (row, xGA calibration, league save %)
    type YearRows = Map<i64, (MpGoalie, f64, Option<f64>)>;
    let mut by_year: Map<i64, YearRows> = Map::new();
    for (y, file) in files {
        let rows = match file {
            Ok(GoalieYear { rows, missing }) => {
                if !guard(&mut ctx.warnings, res, &missing, y, Kind::Goalies, "goalie GSAx") {
                    continue;
                }
                rows
            }
            Err(msg) => {
                // network / parse failure: GSAx from the seasons we have
                res.goalie_errors.push(msg.clone());
                ctx.warnings.push(msg);
                continue;
            }
        };
        let cal = goalie_calibration(rows.iter(), 20000.0, (0.9, 1.1));
        let sa = py_sum(rows.iter().map(|r| r.sa));
        let lsv = if sa > 0.0 { Some(1.0 - py_sum(rows.iter().map(|r| r.ga)) / sa) } else { None };
        let m: YearRows = rows.into_iter().map(|r| (r.nhl_id, (r, cal, lsv))).collect();
        by_year.insert(y, m);
    }
    if !by_year.values().any(|m| !m.is_empty()) {
        return;
    }
    for (cid, pid) in goalies {
        let seasons: Vec<GsaxSeason> = years
            .iter()
            .map(|&y| match by_year.get(&y).and_then(|m| m.get(&pid)) {
                None => GsaxSeason::new(y, 0.0, 0.0, 0.0, 0.0, None),
                Some((r, cal, lsv)) => GsaxSeason::new(y, r.xga * cal, r.ga, r.sa, r.gp as f64, *lsv),
            })
            .collect();
        let Some(est) = regressed_gsax(pid, &seasons, GSAX_K_SHOTS, &GSAX_SEASON_WEIGHTS) else { continue };
        Arc::make_mut(&mut ctx.enrich).gsax.insert((cur_y, est.nhl_id), est.clone());
        res.goalies.insert(cid, est);
    }
    if !res.goalies.is_empty() {
        let used: Vec<String> =
            years.iter().filter(|y| by_year.get(*y).is_some_and(|m| !m.is_empty())).map(|y| label(*y)).collect();
        ctx.source_notes.push(format!("{CREDIT}: goalie GSAx for {} goalies ({})", res.goalies.len(), used.join(", ")));
    }
}
