//! Rust port of `fantasy_manager/providers/deployment_enrich.py`: fill every skater's deployment
//! fields (`toi_per_game`, `pp_toi_per_game`, `pp_share`, `toi_trend`, `pp_share_trend`) from the
//! harness ledger's per-game rows, with the NHL season reports as fallback.
//! Owner: work package `W4-enrich` (wave 4).
//!
//! Deviation: `role_change_alerts` (a thin wrapper over `recommend.flags.recommend_role_alerts`)
//! lives in `fm_engine::recommend::flags` (package notes).

use std::collections::BTreeSet;

use anyhow::Result;
use chrono::NaiveDate;
use fm_core::models::{LeagueContext, Player};
use fm_core::Map;
use fm_harness::deployment::{deployment_summaries, Prior, DEFAULT_LAST_N, MIN_BASE_GP};
use fm_harness::ledger::{Ledger, Row};
use fm_pycompat::num::round_to;
use fm_sources::nhl::{current_season, prior_season, NhlClient, NhlSkaterDeploymentSeason};
use serde_json::Value;

use crate::enrich::{ctx_players_mut, exc_name};

fn minutes(seconds: Option<f64>) -> Option<f64> {
    seconds.map(|s| round_to(s / 60.0, 3))
}

fn season_report(
    nhl: &NhlClient,
    season: i64,
    warnings: &mut Vec<String>,
    label: &str,
) -> Map<i64, NhlSkaterDeploymentSeason> {
    match nhl.skater_deployment_season(Some(season), 2) {
        Ok(m) => m,
        Err(e) => {
            // best effort, like every enrichment step
            warnings.push(format!("Deployment ({label} season report) unavailable: {}: {e}", exc_name(&e)));
            Map::new()
        }
    }
}

fn fill_from_report(p: &mut Player, rep: &NhlSkaterDeploymentSeason) {
    p.toi_per_game = minutes(rep.toi_per_game);
    p.pp_toi_per_game = minutes(rep.pp_toi_per_game);
    p.pp_share = rep.pp_share.map(|v| round_to(v, 4));
    p.toi_trend = None;
    p.pp_share_trend = None;
}

/// `enrich_deployment`'s result: counts + `details` (cid -> deployment summary).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeploymentResult {
    pub skaters: i64,
    pub ledger: i64,
    pub season_report: i64,
    pub prior_season: i64,
    pub missing: i64,
    pub trends: i64,
    pub details: Map<String, Map<String, Value>>,
}

fn f(row: &Row, k: &str) -> Option<f64> {
    row.get(k).and_then(Value::as_f64)
}

/// Set the deployment fields of every skater with an NHL id (in place; see the module docs).
pub fn enrich_deployment(
    ctx: &mut LeagueContext,
    ledger: Option<&Ledger>,
    as_of: Option<NaiveDate>,
    nhl: Option<&NhlClient>,
    last_n: usize,
    fallback: bool,
) -> Result<DeploymentResult> {
    let as_of = as_of.unwrap_or(ctx.as_of);
    let ids: Vec<i64> = {
        let s: BTreeSet<i64> =
            ctx.all_players().iter().filter(|p| !p.is_goalie()).filter_map(|p| p.nhl_id()).collect();
        s.into_iter().collect()
    };
    let season = current_season(as_of);
    let mut cur: Map<i64, NhlSkaterDeploymentSeason> = Map::new();
    let mut prior: Map<i64, NhlSkaterDeploymentSeason> = Map::new();
    if let Some(n) = nhl.filter(|_| fallback && !ids.is_empty()) {
        cur = season_report(n, season, &mut ctx.warnings, "current");
    }
    let mut rows_gp: Map<i64, i64> = Map::new();
    if let Some(l) = ledger.filter(|_| !ids.is_empty()) {
        for (pid, s) in deployment_summaries(l, &ids, as_of, last_n, None)? {
            rows_gp.insert(pid, s.get("gp").and_then(Value::as_i64).unwrap_or(0));
        }
    }
    // last season's averages: the trend baseline early in the season and the preseason fallback
    let need_prior = ids.iter().any(|pid| rows_gp.get(pid).copied().unwrap_or(0) < (last_n + MIN_BASE_GP) as i64);
    if let Some(n) = nhl.filter(|_| fallback && need_prior) {
        prior = season_report(n, prior_season(season), &mut ctx.warnings, "prior");
    }
    let priors: Map<i64, Prior> = prior
        .iter()
        .filter(|(_, r)| r.gp > 0)
        .map(|(pid, r)| {
            let mut m: Prior = Map::new();
            m.insert("toi".into(), minutes(r.toi_per_game));
            m.insert("pp_share".into(), r.pp_share);
            (*pid, m)
        })
        .collect();
    let summaries: Map<i64, Row> = match ledger.filter(|_| !ids.is_empty()) {
        Some(l) => deployment_summaries(l, &ids, as_of, last_n, Some(&priors))?,
        None => Map::new(),
    };

    let mut out = DeploymentResult::default();
    for p in ctx_players_mut(ctx) {
        let Some(pid) = p.nhl_id() else { continue };
        if p.is_goalie() {
            continue;
        }
        out.skaters += 1;
        let s = summaries.get(&pid);
        let rep = cur.get(&pid);
        let s_gp = s.and_then(|s| s.get("gp")).and_then(Value::as_i64).unwrap_or(0);
        if let Some(s) = s.filter(|s| !s.is_empty() && s_gp > 0) {
            p.toi_per_game = f(s, "season_toi");
            p.pp_toi_per_game = f(s, "season_pp_toi");
            p.pp_share = f(s, "season_pp_share");
            match rep {
                Some(rep) if rep.gp > s_gp => {
                    // the ledger missed some days
                    fill_from_report(p, rep);
                    out.season_report += 1;
                }
                _ => out.ledger += 1,
            }
            p.toi_trend = f(s, "toi_trend");
            p.pp_share_trend = f(s, "pp_share_trend");
            out.details.insert(p.cid.clone(), s.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
            if f(s, "toi_trend").is_some() || f(s, "pp_share_trend").is_some() {
                out.trends += 1;
            }
        } else if let Some(rep) = rep.filter(|r| r.gp > 0) {
            fill_from_report(p, rep);
            out.season_report += 1;
        } else if let Some(pr) = prior.get(&pid).filter(|r| r.gp > 0) {
            fill_from_report(p, pr);
            out.prior_season += 1;
        } else {
            out.missing += 1;
        }
    }
    Ok(out)
}

/// `enrich_deployment` with the Python defaults (`last_n=DEFAULT_LAST_N`, `fallback=True`).
pub fn enrich_deployment_default(ctx: &mut LeagueContext, ledger: Option<&Ledger>, nhl: Option<&NhlClient>) -> Result<DeploymentResult> {
    enrich_deployment(ctx, ledger, None, nhl, DEFAULT_LAST_N, true)
}
