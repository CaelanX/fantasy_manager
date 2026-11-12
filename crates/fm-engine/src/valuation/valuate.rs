//! Rust port of `fantasy_manager/valuation/valuate.py`: league-wide valuation (rates -> FPG ->
//! availability -> schedule -> replacement -> VORP). `PlayerValue` lives in `fm_core::values`.
//! Owner: work package `W4-valuate` (wave 4).
//!
//! See the Python module docstring for the model. Deviations (no behavioural differences):
//! * `valuate_league(ctx, scoring)` takes an already fitted scoring system (`&(dyn ScoringSystem +
//!   Sync)`); [`fit_and_valuate`] does Python's "fit an unfitted categories system first".
//!   The schedule memo (Python `_CTX_MEMO`, odds from the settings data dir) is explicit:
//!   [`valuate_league_with`] takes a [`ScheduleMemo`]; [`valuate_league`] uses one without a data
//!   dir (no betting odds: opponent goals against only).
//! * The optional-signal lookups (`_rookie_lookup`, `_news_signals`, ...) read `ctx.enrich`
//!   (`EnrichState`), which cannot fail, so `warn_signal` is never reached from here; it is kept
//!   (same text) for callers.
//! * `_rates_for(..., rookie=dict)` stored the estimate back into the dict; here the estimate is
//!   returned ([`RatesOut`]).
//! * The per-player loop runs in parallel (rayon) over players: every player's value depends only
//!   on the precomputed league data, results are collected in player order, so the output is the
//!   same as the sequential loop (tested).
//! * `PlayerValue.rates` key order: Python's comes from set unions (hash order); here it is a
//!   fixed union order (`blend` module). Compare rates as maps.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::NaiveDate;
use fm_core::models::{LeagueContext, Player, Reason, StatLine};
use fm_core::normalize::normalize_team;
use fm_core::snapshot::{GoalieGsax, LeagueSeason, PreseasonLine, RoleSignal};
use fm_core::values::{Horizon, PlayerValue, RookieEstimate};
use fm_core::{Map, Set};
use fm_pycompat::fmt::{fixed, general, percent};
use fm_pycompat::num::round_to;
use rayon::prelude::*;
use serde_json::Value;

use super::adjust::{availability_multiplier, season_availability, SeasonLength};
use super::blend::{
    baseline_k, blend_preseason, blend_rates, blend_recency, multi_season_baseline, multi_season_sample,
    preseason_weight, projection_blend_weight, projection_k, recency_weights, shrink_baseline, shrink_inseason,
    shrink_k, Rates, MULTI_SEASON_WEIGHTS, PRESEASON_STATS, PRIOR_SPLITS,
};
use super::params::{self, age_factor};
use super::regression::{
    apply_team_goalie_rates, assist_shrink_rates, gsax_rates, luck_signals, shrink_goals, GsaxExt, ASSIST_SHRINK_CAP, ASSIST_SHRINK_MIN_GP,
    ASSIST_SHRINK_SHARE, GSAX_K_SHOTS, K_GOALS,
};
use super::replacement::{best_slot, replacement_with_fallback, vorp, DEFAULT_SLOTS, ROSTERED_FALLBACK_PCT};
use super::roles::{goalie_news_share, role_change, RoleChange};
use super::rookie::{has_rookie_evidence, is_unproven, rookie_prior, rookie_value, RookieInputs, OFFENSIVE};
use super::schedule::{
    actual_start_share, opp_week_factor, proj_week, schedule_factor, start_share, start_share_parts,
    teammate_shares, ScheduleMemo, TeammateShare, WeekWindow, ACTUAL_SHARE_K, OFFNIGHT_THRESHOLD, OPP_WEEK_REASON,
    ROSTER_ONLY_SHARE, TEAMMATE_CAP,
};
use crate::analysis::team_factors::{team_goalie_rates, TeamGoalieRates, GOALIE_SHOTS_SHRINK_K, GOALIE_WIN_SHRINK_K};
use crate::recommend::base::{free_ir_slots, ir_movable, IR_SLOT_NAMES};
use crate::scoring::{fit_to_context, point_scale, point_scale_disabled, PointScale, Scoring, ScoringSystem};

/// team games of a prior season (2025-26 and the seasons before it had 82); this season's count
/// comes from the schedule ([`SeasonLength`])
pub const PRIOR_TEAM_GAMES: i64 = 82;
pub const PROJ_SHARE_K: i64 = 20;
/// season GP before the xG goal shrink applies (regression.MIN_CURRENT_GP)
pub const XG_MIN_GP: i64 = 5;
pub const XG_CREDIT: &str = "ixG: MoneyPuck.com";
pub const GSAX_CREDIT: &str = "GSAx: MoneyPuck.com";
pub const ONICE_CREDIT: &str = "on-ice shooting %: MoneyPuck.com";
/// out for now (day-to-day is a flat week discount)
pub const WEEK_OUT_STATUSES: [&str; 4] = ["out", "ir", "ltir", "suspended"];
/// ledger team games before actual starts / back-to-back rates are used
pub const MIN_LEDGER_TEAM_GAMES: i64 = 5;

/// `MIN_GP_FOR_MEAN[group]`.
pub fn min_gp_for_mean(group: &str) -> i64 {
    if group == "G" {
        10
    } else {
        20
    }
}

/// Positional means: group -> stat -> mean per-game rate.
pub type Means = Map<String, Rates>;

/// `(rates, prior NHL GP, description)` of the multi-season baseline.
pub type History = (Rates, i64, String);

pub fn position_group(p: &Player) -> &'static str {
    if p.is_goalie() {
        return "G";
    }
    if p.has_position("D") && !p.positions.iter().any(|x| matches!(x.as_str(), "C" | "LW" | "RW" | "F")) {
        return "D";
    }
    "F"
}

/// Mean prior-season per-game rates by position group (F/D/G).
pub fn positional_means(players: &[&Player]) -> Means {
    let mut sums: Map<String, Rates> = Map::new();
    let mut counts: HashMap<&'static str, i64> = HashMap::new();
    for p in players {
        let g = position_group(p);
        let Some(prior) = p.lines.get("prior") else { continue };
        if prior.gp < min_gp_for_mean(g) {
            continue;
        }
        *counts.entry(g).or_insert(0) += 1;
        let acc = sums.entry(g.to_string()).or_default();
        for (k, v) in prior.per_game() {
            *acc.entry(k).or_insert(0.0) += v;
        }
    }
    sums.into_iter()
        .map(|(g, acc)| {
            let n = counts[g.as_str()] as f64;
            let m = acc.into_iter().map(|(k, v)| (k, v / n)).collect();
            (g, m)
        })
        .collect()
}

/// ESPN projection if present, else the prior season (None when neither has games).
pub fn baseline_line(p: &Player) -> Option<&StatLine> {
    if let Some(proj) = p.lines.get("projected") {
        if proj.gp > 0 {
            return Some(proj);
        }
    }
    p.lines.get("prior").filter(|l| l.gp > 0)
}

/// Start year of the current NHL season (from September on, the upcoming one).
pub fn season_start_year(as_of: NaiveDate) -> i64 {
    use chrono::Datelike;
    if as_of.month() >= 9 {
        as_of.year() as i64
    } else {
        as_of.year() as i64 - 1
    }
}

/// Age on Oct 1 of the current / upcoming season (the backtest's age convention).
pub fn season_age(p: &Player, as_of: NaiveDate) -> Option<f64> {
    let b = p.birth_date?;
    let oct1 = NaiveDate::from_ymd_opt(season_start_year(as_of) as i32, 10, 1)?;
    Some(round_to((oct1 - b).num_days() as f64 / 365.25, 3))
}

/// The player's NHL seasons N-1, N-2, N-3 (`prior`, `prior2`, `prior3`).
pub fn history_lines(p: &Player) -> [Option<&StatLine>; 3] {
    PRIOR_SPLITS.map(|s| p.lines.get(s))
}

/// (rates, prior NHL GP, description) of the multi-season baseline (the backtest's `fm_multi`).
pub fn history_baseline(p: &Player, means: &Means, age: Option<f64>) -> History {
    let g = position_group(p);
    let lines = history_lines(p);
    let Some((_, n, gp_total)) = multi_season_sample(&lines, &MULTI_SEASON_WEIGHTS) else {
        return (Rates::new(), 0, String::new());
    };
    let mut rates = multi_season_baseline(&lines, g, means, None);
    let seasons = lines.iter().filter(|l| l.is_some_and(|l| l.gp > 0)).count();
    let mut desc = format!("{gp_total} GP over {seasons} season{}", if seasons != 1 { "s" } else { "" });
    if seasons > 1 {
        desc += " weighted 5/4/3";
    }
    if means.get(g).is_some_and(|m| !m.is_empty()) {
        let k = baseline_k(g);
        desc += &format!(", shrunk {} toward {g} mean (k={})", percent(k / (n + k), 0, false), general(k, None, false));
    }
    let age_prev = age.map(|a| a - 1.0);
    let f = age_factor(g, age_prev, None, None);
    if (f - 1.0).abs() > 1e-9 {
        rates = rates.into_iter().map(|(k, v)| (k, v * f)).collect();
        let a0 = age_prev.map(|a| a.trunc() as i64).unwrap_or(0);
        desc += &format!(", aging {a0}->{} x{}", a0 + 1, fixed(f, 3, false));
    }
    (rates, gp_total, desc)
}

/// (rates, raw baseline line, effective GP, source) of the pre-season baseline (Python
/// `baseline_rates`; see its docstring). `params` / `history` let the harness replay it.
pub fn baseline_rates<'a>(
    p: &'a Player,
    means: &Means,
    age: Option<f64>,
    params: Option<&Value>,
    history: Option<&History>,
) -> (Rates, Option<&'a StatLine>, f64, String) {
    let g = position_group(p);
    let empty = Rates::new();
    let mean = means.get(g).unwrap_or(&empty);
    let computed;
    let (hist, hist_gp, hist_desc) = match history {
        Some(h) => (&h.0, h.1, h.2.as_str()),
        None => {
            computed = history_baseline(p, means, age);
            (&computed.0, computed.1, computed.2.as_str())
        }
    };
    if let Some(proj) = p.lines.get("projected").filter(|l| l.gp > 0) {
        let (prates, n) = shrink_baseline(Some(proj), mean, p.is_goalie(), None, params);
        let mut src = format!("projection ({} GP", proj.gp);
        if n != proj.gp as f64 {
            src += &format!(", counted as {}", fixed(n, 0, false));
        }
        src += ")";
        if !mean.is_empty() {
            let k = projection_k(p.is_goalie(), params);
            src += &format!(" shrunk {} toward {g} mean (k={})", percent(k / (n + k), 0, false), general(k, None, false));
        }
        if !hist.is_empty() {
            let w = projection_blend_weight(hist_gp, params);
            let text =
                format!("{} {src} + {} NHL history ({hist_desc})", percent(w, 0, false), percent(1.0 - w, 0, false));
            return (blend_rates(&prates, hist, w), Some(proj), n, text);
        }
        return (prates, Some(proj), n, src);
    }
    if !hist.is_empty() {
        if history.is_some() {
            return (hist.clone(), None, hist_gp as f64, format!("NHL history ({hist_desc})"));
        }
        let lines = history_lines(p);
        let line = lines.iter().flatten().find(|l| l.gp > 0).copied();
        let n = multi_season_sample(&lines, &MULTI_SEASON_WEIGHTS).map(|s| s.1).unwrap_or(0.0);
        return (hist.clone(), line, n, format!("NHL history ({hist_desc})"));
    }
    // only a player with current-season games borrows the mean as his prior; one with no stats at
    // all stays at NO_DATA rather than being handed an average line
    if !mean.is_empty() && p.gp("season") > 0 {
        return (mean.clone(), None, 0.0, format!("{g} positional mean (no baseline line)"));
    }
    (Rates::new(), None, 0.0, String::new())
}

/// The healthy per-game rates behind `PlayerValue.fpg` (baseline -> season-to-date shrink ->
/// recency blend), without reasons. Pure: the harness replays archived inputs through it.
pub fn player_rates(p: &Player, means: &Means, age: Option<f64>, params: Option<&Value>, history: Option<&History>) -> Rates {
    rates_for(p, means, None, age, params, history, false, None, None).rates
}

/// The rookie-model inputs of one unproven skater (`_rookie_lookup` dict).
#[derive(Clone, Copy, Debug)]
pub struct RookieLookup<'a> {
    pub history: &'a [LeagueSeason],
    pub signals: &'a [RoleSignal],
    pub as_of: Option<NaiveDate>,
}

/// What `_rates_for` returns (plus the rookie estimate Python stored back into its dict).
#[derive(Clone, Debug, Default)]
pub struct RatesOut {
    pub rates: Rates,
    pub reasons: Vec<Reason>,
    pub estimate: Option<RookieEstimate>,
}

/// `_rates_for(p, means, scoring, age, params, history, explain, preseason, rookie)`.
#[allow(clippy::too_many_arguments)]
pub fn rates_for(
    p: &Player,
    means: &Means,
    scoring: Option<&dyn ScoringSystem>,
    age: Option<f64>,
    params: Option<&Value>,
    history: Option<&History>,
    explain: bool,
    preseason: Option<&PreseasonLine>,
    rookie: Option<&RookieLookup<'_>>,
) -> RatesOut {
    let mut reasons: Vec<Reason> = Vec::new();
    let season = p.lines.get("season");
    let (mut base_rates, base_line, _, mut base_src) = baseline_rates(p, means, age, params, history);
    let gp = season.map(|s| s.gp).unwrap_or(0);
    let mut rookie_reasons: Vec<Reason> = Vec::new();
    let mut estimate: Option<RookieEstimate> = None;
    if let Some(rk) = rookie.filter(|_| !p.is_goalie()) {
        let est = rookie_estimate(p, means, age, &base_rates, gp, rk, scoring);
        if est.rates != base_rates {
            base_src = if !base_src.is_empty() {
                format!("rookie blend of {base_src}")
            } else {
                "rookie priors (NHLe / pedigree, no projection)".to_string()
            };
        }
        base_rates = est.rates.clone();
        rookie_reasons = est.reasons.clone();
        estimate = Some(est);
    }
    let mut pre_base: Option<Rates> = None; // baseline before the preseason blend
    let mut pre_w = 0.0;
    if let Some(pre) = preseason.filter(|_| !p.is_goalie() && !base_rates.is_empty()) {
        pre_w = preseason_weight(pre.gp, gp);
        if pre_w > 0.0 {
            let blended = blend_preseason(&base_rates, &pre.per_game(), pre_w, &PRESEASON_STATS);
            pre_base = Some(std::mem::replace(&mut base_rates, blended));
        }
    }
    // the k of the scoring stats; the volume stats may have their own (`shrink_inseason`)
    let k = shrink_k(p.is_goalie(), params);
    let mut xg: Option<(f64, f64, f64)> = None;
    let shrunk = match season.filter(|_| gp > 0) {
        Some(season) => {
            let mut season_rates = season.per_game();
            if xg_applies(p, Some(gp)) && season_rates.contains_key("G") {
                let before = season_rates["G"];
                season_rates = shrink_goals(&season_rates, p.ixg_per_game, gp as f64, K_GOALS);
                xg = Some((before, p.ixg_per_game.unwrap_or(0.0), season_rates["G"]));
            }
            shrink_inseason(&season_rates, gp as f64, &base_rates, p.is_goalie(), params)
        }
        None => base_rates.clone(),
    };
    let (l30, l15, l7) = (p.lines.get("last30"), p.lines.get("last15"), p.lines.get("last7"));
    let rates = blend_recency(&shrunk, l30, l15, l7, params);
    let Some(scoring) = scoring.filter(|_| explain) else {
        return RatesOut { rates, reasons, estimate };
    };
    if !base_rates.is_empty() {
        let base_fpg = scoring.value(pre_base.as_ref().unwrap_or(&base_rates));
        let raw = match base_line {
            Some(l) => format!(" (raw {})", fixed(scoring.value(&l.per_game()), 2, false)),
            None => String::new(),
        };
        reasons.push(
            Reason::new("BASELINE", format!("Baseline from {base_src}: {} FPG{raw}", fixed(base_fpg, 2, false)))
                .with_value(Some(base_fpg)),
        );
    }
    reasons.extend(rookie_reasons);
    if let Some(pre) = preseason.filter(|l| !p.is_goalie() && l.gp > 0) {
        reasons.push(preseason_reason(pre, pre_w, pre_base.as_ref(), &base_rates, gp, scoring));
    }
    if let Some((g_rate, ixg_rate, new_g)) = xg {
        reasons.push(
            Reason::new(
                "XG_SHRINK",
                format!(
                    "Goals {}/GP vs ixG {}/GP over {gp} GP this season: goal rate shrunk to {}/GP (k={} games; {XG_CREDIT})",
                    fixed(g_rate, 2, false),
                    fixed(ixg_rate, 2, false),
                    fixed(new_g, 2, false),
                    general(K_GOALS, None, false)
                ),
            )
            .with_value(Some(g_rate))
            .with_baseline(Some(ixg_rate)),
        );
    }
    if gp > 0 {
        let w = if !base_rates.is_empty() { gp as f64 / (gp as f64 + k) } else { 1.0 };
        reasons.push(
            Reason::new(
                "SHRINK_GP",
                format!(
                    "{gp} GP this season carry {} weight vs baseline (k={})",
                    percent(w, 0, false),
                    general(k, None, false)
                ),
            )
            .with_value(Some(gp as f64))
            .with_baseline(Some(k)),
        );
    }
    let gpo = |l: Option<&StatLine>| l.map(|l| l.gp).unwrap_or(0);
    let w = recency_weights(gpo(l30), gpo(l15), gpo(l7), params);
    if w["season"] < 0.999 {
        reasons.push(
            Reason::new(
                "RECENCY",
                format!(
                    "Recency weights season {} / L30 {} / L15 {} / L7 {} (recent form is weighted lightly: in the \
                     backtest it barely predicts the rest of the season)",
                    fixed(w["season"], 2, false),
                    fixed(w["last30"], 2, false),
                    fixed(w["last15"], 2, false),
                    fixed(w["last7"], 2, false)
                ),
            )
            .with_value(Some(1.0 - w["season"])),
        );
    }
    if rates.is_empty() {
        reasons.push(Reason::new("NO_DATA", "No current, projected or prior stats available"));
    }
    RatesOut { rates, reasons, estimate }
}

/// `_rookie_estimate`: `rookie.rookie_value` for an unproven skater (the NHL rows of his league
/// history are left out when the baseline already carries his NHL seasons).
fn rookie_estimate(
    p: &Player,
    means: &Means,
    age: Option<f64>,
    base_rates: &Rates,
    season_gp: i64,
    rookie: &RookieLookup<'_>,
    scoring: Option<&dyn ScoringSystem>,
) -> RookieEstimate {
    let as_of = rookie.as_of;
    let hist_gp = if PRIOR_SPLITS.iter().any(|s| p.gp(s) > 0) { history_baseline(p, means, age).1 } else { 0 };
    let before = as_of.map(|d| {
        let start = season_start_year(d);
        start * 10000 + start + 1
    });
    let prior = rookie_prior(p, rookie.history, None, age, hist_gp <= 0, before);
    let proj = p.lines.get("projected");
    let empty = Rates::new();
    rookie_value(
        p,
        Some(&prior),
        rookie.signals,
        RookieInputs {
            provider_projection: Some(base_rates),
            deployment: Some(p),
            baseline_gp: hist_gp as f64,
            projection_gp: proj.filter(|l| l.gp > 0).map(|l| l.gp),
            mean: Some(means.get(position_group(p)).unwrap_or(&empty)),
            season_gp,
            as_of,
            scoring,
        },
    )
}

/// One `ctx.warnings` entry per optional valuation signal that failed (deduplicated by signal
/// name). `exc` = (exception type name, message).
pub fn warn_signal(warnings: &mut Vec<String>, signal: &str, exc: Option<(&str, &str)>) {
    let prefix = format!("Valuation: {signal} unavailable");
    if warnings.iter().any(|w| w.starts_with(&prefix)) {
        return;
    }
    let detail = match exc {
        Some((ty, msg)) => format!(" ({ty}: {})", msg.chars().take(100).collect::<String>()),
        None => String::new(),
    };
    warnings.push(format!("{prefix}{detail}; values computed without it"));
}

/// `FM_ROOKIE_MODEL` switch (default on).
pub fn rookie_model_enabled() -> bool {
    let v = std::env::var("FM_ROOKIE_MODEL").unwrap_or_else(|_| "1".to_string());
    !matches!(v.trim().to_lowercase().as_str(), "0" | "false" | "no" | "off")
}

/// `_rookie_lookup(ctx)`: cid -> rookie inputs of the context's unproven skaters with rookie
/// evidence ({} with FM_ROOKIE_MODEL=0).
pub fn rookie_lookup(ctx: &LeagueContext) -> HashMap<String, RookieLookup<'_>> {
    let mut out = HashMap::new();
    if !rookie_model_enabled() {
        return out;
    }
    for p in ctx.all_players() {
        if p.is_goalie() || !is_unproven(p) {
            continue;
        }
        let (hist, sigs) = (ctx.enrich.history_for(p), ctx.enrich.signals_for(p));
        if has_rookie_evidence(p, hist, sigs) {
            out.insert(p.cid.clone(), RookieLookup { history: hist, signals: sigs, as_of: Some(ctx.as_of) });
        }
    }
    out
}

/// `_news_signals(ctx)`: cid -> news role signals registered by the enrich rookie step.
pub fn news_signals(ctx: &LeagueContext) -> HashMap<String, &[RoleSignal]> {
    let mut out = HashMap::new();
    for p in ctx.all_players() {
        let sigs = ctx.enrich.signals_for(p);
        if !sigs.is_empty() {
            out.insert(p.cid.clone(), sigs);
        }
    }
    out
}

/// `rookie._scale(rates, mult)`: offensive rates x mult.
fn scale_offense(rates: &Rates, mult: f64) -> Rates {
    rates.iter().map(|(k, v)| (k.clone(), if OFFENSIVE.contains(&k.as_str()) { v * mult } else { *v })).collect()
}

/// `_preseason_reason`: reason PRESEASON (value = PTS/GP, baseline = GP).
fn preseason_reason(
    line: &PreseasonLine,
    w: f64,
    before: Option<&Rates>,
    after: &Rates,
    season_gp: i64,
    scoring: &dyn ScoringSystem,
) -> Reason {
    let ppg = line.pts_per_game();
    let mut text = format!("Preseason {} ({} PTS/GP)", line.summary(), fixed(ppg, 2, false));
    match before {
        Some(before) if w > 0.0 => {
            text += &format!(
                ": blended at {} into the baseline ({} -> {} FPG; capped at 25%: preseason opponents and usage are weak evidence",
                percent(w, 0, false),
                fixed(scoring.value(before), 2, false),
                fixed(scoring.value(after), 2, false)
            );
            if season_gp > 0 {
                text += &format!(", fading over the first 20 GP, {season_gp} played)");
            } else {
                text += ")";
            }
        }
        _ if after.is_empty() => text += ": not blended (no baseline to adjust)",
        _ => text += &format!(": no longer blended ({season_gp} regular-season GP)"),
    }
    Reason::new("PRESEASON", text).with_value(Some(round_to(ppg, 4))).with_baseline(Some(line.gp as f64))
}

/// `nhl.current_season(as_of)`: season id like 20262027.
fn current_season(as_of: NaiveDate) -> i64 {
    let y = season_start_year(as_of);
    y * 10000 + (y + 1)
}

/// `_preseason_lookup(ctx)` (`preseason_enrich.preseason_lines`): cid -> preseason line of the
/// context's unproven skaters ({} when none were loaded).
pub fn preseason_lookup(ctx: &LeagueContext) -> HashMap<String, &PreseasonLine> {
    let mut out = HashMap::new();
    if ctx.enrich.preseason.is_empty() {
        return out;
    }
    let season = current_season(ctx.as_of);
    for p in ctx.all_players() {
        if p.is_goalie() || !is_unproven(p) {
            continue;
        }
        if let Some(ln) = ctx.enrich.preseason_line(p, season) {
            if ln.gp > 0 {
                out.insert(p.cid.clone(), ln);
            }
        }
    }
    out
}

/// True when the in-season xG goal shrink applies (Python `xg_applies(p, gp)`).
pub fn xg_applies(p: &Player, gp: Option<i64>) -> bool {
    if p.is_goalie() || p.ixg_per_game.is_none() {
        return false;
    }
    let gp = gp.unwrap_or_else(|| p.gp("season"));
    if gp < XG_MIN_GP {
        return false;
    }
    matches!(p.xg_split.as_deref(), None | Some("season"))
}

/// Current-season games played per NHL team: scheduled dates before as_of, or (when that is
/// lower) the most season GP of any player on that team.
pub fn team_games_played(ctx: &LeagueContext) -> Map<String, i64> {
    let mut out: Map<String, i64> = Map::new();
    for (team, dates) in &ctx.schedule {
        let set: HashSet<&NaiveDate> = dates.iter().collect();
        out.insert(team.clone(), set.into_iter().filter(|d| **d < ctx.as_of).count() as i64);
    }
    for p in ctx.all_players() {
        if let Some(team) = normalize_team(p.team.as_deref()) {
            let gp = p.gp("season");
            let e = out.entry(team).or_insert(0);
            *e = (*e).max(gp);
        }
    }
    out
}

/// Start-share denominators per split: this season's team GP, a full season otherwise (last
/// season's 82 for "prior", this season's `season_games` for "projected"; Python used 82 for both).
pub fn goalie_team_games(p: &Player, team_gp: &Map<String, i64>, season_games: f64) -> Map<String, f64> {
    let mut out: Map<String, f64> = Map::new();
    out.insert("prior".into(), PRIOR_TEAM_GAMES as f64);
    out.insert("projected".into(), season_games);
    if let Some(team) = normalize_team(p.team.as_deref()) {
        if let Some(n) = team_gp.get(&team) {
            out.insert("season".into(), *n as f64);
        }
    }
    out
}

/// Goalie workload from a full-season projection: (start share, projected fantasy points per
/// team game or None, reason text), or None without a usable projection. The projection covers
/// this season, so its starts and points are spread over `season_games` (the schedule's games per
/// team), where Python divided by 82.
pub fn projected_workload(
    p: &Player,
    team_gp: Option<&Map<String, i64>>,
    season_games: f64,
) -> Option<(f64, Option<f64>, String)> {
    if !p.is_goalie() {
        return None;
    }
    let proj = p.lines.get("projected").filter(|l| l.gp > 0)?;
    let starts = proj.stats.get("GS").copied().unwrap_or(proj.gp as f64);
    let share = (starts / season_games).min(1.0);
    let mut text = format!(
        "Projected start share {} ({} projected starts / {})",
        percent(share, 0, false),
        fixed(starts, 0, false),
        fixed(season_games, 0, false)
    );
    let team = normalize_team(p.team.as_deref());
    let mut tg = match (&team, team_gp) {
        (Some(t), Some(m)) => m.get(t).copied().unwrap_or(0) as f64,
        _ => 0.0,
    };
    if let Some(season) = p.lines.get("season").filter(|l| l.gp > 0) {
        let gs = season.stats.get("GS").copied().unwrap_or(season.gp as f64);
        tg = tg.max(season.gp as f64);
        let k = PROJ_SHARE_K as f64;
        let blended = (gs + k * share) / (tg + k);
        text += &format!(
            ", blended with {} GS / {} team GP this season (k={PROJ_SHARE_K}) -> {}",
            fixed(gs, 0, false),
            fixed(tg, 0, false),
            percent(blended, 0, false)
        );
        return Some((blended, None, text));
    }
    let fpts = proj.stats.get("FPTS").copied();
    let per_team_game = fpts.map(|f| f / season_games);
    if let Some(f) = fpts {
        text += &format!(
            "; projected {} FPts / {} team games",
            fixed(f, 0, false),
            fixed(season_games, 0, false)
        );
    }
    Some((share, per_team_game, text))
}

/// The projection's own start share: projected GS (else GP) / `season_games`, this season's games
/// per team (0 without a projection).
pub fn projection_share(p: &Player, season_games: f64) -> f64 {
    match p.lines.get("projected").filter(|l| l.gp > 0) {
        None => 0.0,
        Some(proj) => (proj.stats.get("GS").copied().unwrap_or(proj.gp as f64) / season_games).min(1.0),
    }
}

/// `_gsax_lookup(ctx)`: cid -> usable regressed GSAx of the context's goalies.
pub fn gsax_lookup(ctx: &LeagueContext) -> HashMap<String, &GoalieGsax> {
    let year = season_start_year(ctx.as_of);
    let mut out = HashMap::new();
    for p in ctx.all_players() {
        if !p.is_goalie() {
            continue;
        }
        if let Some(est) = ctx.enrich.goalie_gsax(p, year) {
            if est.usable() {
                out.insert(p.cid.clone(), est);
            }
        }
    }
    out
}

fn season_span(years: &[i64]) -> String {
    let lab = |y: i64| {
        let s = (y + 1).to_string();
        let tail: String = s.chars().skip(s.chars().count().saturating_sub(2)).collect();
        format!("{y}-{tail}")
    };
    let (Some(lo), Some(hi)) = (years.iter().min(), years.iter().max()) else { return String::new() };
    if lo == hi {
        lab(*lo)
    } else {
        format!("{}..{}", lab(*lo), lab(*hi))
    }
}

/// League save % of the fantasy data (goalie positional means: SV / SA, else 1 - GA / SA).
pub fn league_save_pct(means: &Means) -> Option<f64> {
    let g = means.get("G")?;
    let sa = *g.get("SA")?;
    if sa == 0.0 || sa <= 0.0 {
        return None;
    }
    if let Some(sv) = g.get("SV") {
        return Some(sv / sa);
    }
    g.get("GA").map(|ga| 1.0 - ga / sa)
}

fn shot_quality(est: &GoalieGsax) -> String {
    match est.league_sv_pct {
        None => format!("expected save % {}", fixed(est.xsv_pct, 3, false)),
        Some(l) => format!("{} save % points vs league", fixed((est.xsv_pct - l) * 100.0, 2, true)),
    }
}

/// Goalie rates with the save component replaced by the regressed GSAx estimate and reason GSAX.
pub fn apply_gsax(
    rates: &Rates,
    est: &GoalieGsax,
    scoring: &dyn ScoringSystem,
    league_sv: Option<f64>,
) -> (Rates, Option<Reason>) {
    let (new, before, after) = gsax_rates(rates, est, league_sv);
    let (Some(before), Some(after)) = (before, after) else { return (rates.clone(), None) };
    let (f0, f1) = (scoring.value(rates), scoring.value(&new));
    let text = format!(
        "MoneyPuck GSAx {} on {} shots ({}), regressed to {} per 100 shots (seasons weighted 1/0.8/0.6, k={} shots; \
         shot quality against {}): save % {} -> {}, {} -> {} FPG ({GSAX_CREDIT})",
        fixed(est.raw_gsax, 1, true),
        fixed(est.raw_shots, 0, false),
        season_span(&est.seasons),
        fixed(est.per_shot * 100.0, 2, true),
        general(GSAX_K_SHOTS, None, false),
        shot_quality(est),
        fixed(before, 3, false),
        fixed(after, 3, false),
        fixed(f0, 2, false),
        fixed(f1, 2, false)
    );
    let r = Reason::new("GSAX", text)
        .with_value(Some(round_to(est.per_shot, 5)))
        .with_baseline(Some(round_to(before, 4)));
    (new, Some(r))
}

/// Goalie rates with wins and shots against per start taken from his current team's goalies this
/// season (reason TEAM_GOALIE); his own save % is kept. This departs from the Python, which
/// projected wins and shots against from the goalie's own history.
pub fn team_goalie(rates: &Rates, team: &str, t: &TeamGoalieRates, scoring: &dyn ScoringSystem) -> (Rates, Option<Reason>) {
    let new = apply_team_goalie_rates(rates, t.wins_per_start, t.shots_per_start);
    if new == *rates {
        return (new, None);
    }
    let (f0, f1) = (scoring.value(rates), scoring.value(&new));
    let get = |r: &Rates, k: &str| r.get(k).copied().unwrap_or(0.0);
    let text = format!(
        "Wins and shots against from {team}'s goalies this season ({} starts: {} wins and {} shots per start, \
         shrunk toward the league average with k={} and k={} starts), own save % kept: W {} -> {}/GP, \
         SA {} -> {}/GP, {} -> {} FPG",
        fixed(t.starts, 0, false),
        fixed(t.wins_per_start, 3, false),
        fixed(t.shots_per_start, 1, false),
        general(GOALIE_WIN_SHRINK_K, None, false),
        general(GOALIE_SHOTS_SHRINK_K, None, false),
        fixed(get(rates, "W"), 3, false),
        fixed(get(&new, "W"), 3, false),
        fixed(get(rates, "SA"), 1, false),
        fixed(get(&new, "SA"), 1, false),
        fixed(f0, 2, false),
        fixed(f1, 2, false)
    );
    (new, Some(Reason::new("TEAM_GOALIE", text).with_value(Some(f1)).with_baseline(Some(f0))))
}

/// In-season assist-luck shrink (reason ASSIST_SHRINK).
pub fn assist_shrink(p: &Player, rates: &Rates, scoring: &dyn ScoringSystem, norms: Option<&Map<String, f64>>) -> (Rates, Option<Reason>) {
    let Some(onice) = p.onice_sh_pct else { return (rates.clone(), None) };
    if p.is_goalie() || rates.is_empty() || !rates.contains_key("A") {
        return (rates.clone(), None);
    }
    let gp = p.gp("season");
    if gp < ASSIST_SHRINK_MIN_GP || !matches!(p.xg_split.as_deref(), None | Some("season")) {
        return (rates.clone(), None);
    }
    let norms = norms.filter(|n| !n.is_empty());
    let Some(sig) = luck_signals(p, norms, None, None, None) else { return (rates.clone(), None) };
    if sig.from_prior || sig.assist_luck_fpg == 0.0 {
        return (rates.clone(), None);
    }
    let (mut new, mut delta) = assist_shrink_rates(rates, Some(&sig), ASSIST_SHRINK_SHARE);
    let f0 = scoring.value(rates);
    let mut f1 = scoring.value(&new);
    let cap = ASSIST_SHRINK_CAP * f0.abs();
    let capped = (f1 - f0).abs() > cap;
    if capped {
        delta *= cap / (f1 - f0).abs();
        new = rates.clone();
        new["A"] += delta;
        if let Some(pts) = new.get_mut("PTS") {
            *pts += delta;
        }
        f1 = scoring.value(&new);
    }
    if delta.abs() < 1e-6 {
        return (rates.clone(), None);
    }
    let norm = onice - sig.onice_sh_pct_delta.unwrap_or(0.0);
    let a_pg = match p.lines.get("season") {
        Some(line) if gp != 0 => line.stats.get("A").copied().unwrap_or(0.0) / gp as f64,
        _ => 0.0,
    };
    let mut text = format!(
        "{} A/GP this season at {} 5v5 on-ice shooting (norm {}): assist luck {}/GP, {} of it x confidence {} \
         removed -> assist rate {} -> {}/GP, {} -> {} FPG",
        fixed(a_pg, 2, false),
        percent(onice, 1, false),
        percent(norm, 1, false),
        fixed(sig.assist_luck_fpg, 2, true),
        percent(ASSIST_SHRINK_SHARE, 0, false),
        fixed(sig.confidence, 2, false),
        fixed(rates["A"], 2, false),
        fixed(new["A"], 2, false),
        fixed(f0, 2, false),
        fixed(f1, 2, false)
    );
    if capped {
        text += &format!(" (capped at {} of FPG)", percent(ASSIST_SHRINK_CAP, 0, false));
    }
    text += &format!(" ({ONICE_CREDIT})");
    let r = Reason::new("ASSIST_SHRINK", text)
        .with_value(Some(round_to(delta, 4)))
        .with_baseline(Some(round_to(sig.assist_luck_fpg, 4)));
    (new, Some(r))
}

/// `_onice_norms(ctx)`.
pub fn onice_norms(ctx: &LeagueContext) -> Map<String, f64> {
    ctx.enrich.onice_norms(season_start_year(ctx.as_of))
}

fn ledger_actual(ctx: &LeagueContext, cid: &str) -> Option<(i64, i64)> {
    ctx.goalie_actual_starts.get(cid).copied().filter(|a| a.1 >= MIN_LEDGER_TEAM_GAMES)
}

/// (start share before teammate adjustments, has evidence) - the same share `valuate_league`
/// computes. `has evidence` is false when the share is the bare prior.
pub fn goalie_base_share(p: &Player, ctx: &LeagueContext, team_gp: &Map<String, i64>, season_games: f64) -> (f64, bool) {
    let work = projected_workload(p, Some(team_gp), season_games);
    let actual = ledger_actual(ctx, &p.cid);
    if let Some(work) = work {
        if let Some((s, g)) = actual {
            return (actual_start_share(projection_share(p, season_games), s as f64, g as f64, ACTUAL_SHARE_K), true);
        }
        return (work.0, true);
    }
    let tg = goalie_team_games(p, team_gp, season_games);
    if let Some((s, g)) = actual {
        return (start_share(p, None, None, Some(&tg), Some((s as f64, g as f64)), ACTUAL_SHARE_K), true);
    }
    let (share, _, games) = start_share_parts(p, None, None, Some(&tg), &["season", "prior"]);
    (share, games > 0.0)
}

/// `_teammate_shares(ctx, team_gp, window)`: cid -> TeammateShare for goalies whose higher-share
/// teammate is absent.
pub fn teammate_share_lookup(
    ctx: &LeagueContext,
    team_gp: &Map<String, i64>,
    window: Option<&WeekWindow>,
) -> Map<String, TeammateShare> {
    let mut base: Map<String, f64> = Map::new();
    let mut weights: Map<String, f64> = Map::new();
    let season_games = SeasonLength::of(&ctx.schedule).games;
    for p in ctx.all_players() {
        if !p.is_goalie() {
            continue;
        }
        let (share, evidence) = goalie_base_share(p, ctx, team_gp, season_games);
        base.insert(p.cid.clone(), share);
        if !evidence {
            weights.insert(p.cid.clone(), share.min(ROSTER_ONLY_SHARE));
        }
    }
    teammate_shares(ctx, &base, window, Some(&weights), TEAMMATE_CAP)
}

/// `_slots(ctx)`.
pub fn slots(ctx: &LeagueContext) -> Vec<String> {
    let s: Vec<String> = ["C", "LW", "RW", "F", "D", "G"]
        .iter()
        .filter(|s| ctx.roster_shape.contains_key(**s))
        .map(|s| s.to_string())
        .collect();
    if s.is_empty() {
        DEFAULT_SLOTS.iter().map(|s| s.to_string()).collect()
    } else {
        s
    }
}

/// `_replacement(ctx, values, horizon)`: (levels, fallback slots).
pub fn replacement(ctx: &LeagueContext, values: &Map<String, PlayerValue>, horizon: Horizon) -> (Map<String, f64>, Set<String>) {
    let fa_ids: HashSet<&str> = ctx.free_agents.iter().map(|p| p.cid.as_str()).collect();
    let mut fa: Map<String, f64> = Map::new();
    for p in &ctx.free_agents {
        if let Some(v) = values.get(&p.cid) {
            fa.insert(p.cid.clone(), v.fpg_for(horizon));
        }
    }
    let mut rostered: Map<String, f64> = Map::new();
    for t in &ctx.teams {
        for p in t.players_iter() {
            if fa_ids.contains(p.cid.as_str()) {
                continue;
            }
            if let Some(v) = values.get(&p.cid) {
                rostered.insert(p.cid.clone(), v.fpg_for(horizon));
            }
        }
    }
    let players: HashMap<String, &Player> = values.iter().map(|(c, pv)| (c.clone(), &pv.player)).collect();
    replacement_with_fallback(&fa, &rostered, &players, &slots(ctx), ROSTERED_FALLBACK_PCT)
}

/// Replacement levels (for display): top-3 free-agent mean per slot, or the rostered 10th
/// percentile for a slot with no free agents at all.
pub fn replacement_for(ctx: &LeagueContext, values: &Map<String, PlayerValue>, horizon: Horizon) -> Map<String, f64> {
    replacement(ctx, values, horizon).0
}

/// Slots whose replacement level came from the rostered-percentile fallback.
pub fn replacement_fallback_slots(ctx: &LeagueContext, values: &Map<String, PlayerValue>, horizon: Horizon) -> Set<String> {
    replacement(ctx, values, horizon).1
}

/// Per-league data computed once before the per-player loop.
struct League<'a> {
    ctx: &'a LeagueContext,
    memo: &'a ScheduleMemo,
    scoring: &'a (dyn ScoringSystem + Sync),
    means: Means,
    team_gp: Map<String, i64>,
    season: SeasonLength,
    window: Option<WeekWindow>,
    teammates: Map<String, TeammateShare>,
    preseason: HashMap<String, &'a PreseasonLine>,
    rookies: HashMap<String, RookieLookup<'a>>,
    news: HashMap<String, &'a [RoleSignal]>,
    gsax: HashMap<String, &'a GoalieGsax>,
    goalie_teams: Map<String, TeamGoalieRates>,
    league_sv: Option<f64>,
    norms: Map<String, f64>,
    pp_teams: HashSet<Option<String>>,
    lineup_teams: HashSet<Option<String>>,
    ss_prior: f64,
    ss_k: f64,
}

fn pct0(x: f64) -> String {
    percent(x, 0, false)
}
fn f2(x: f64) -> String {
    fixed(x, 2, false)
}

impl League<'_> {
    /// (his value, his season availability).
    fn value_player(&self, p: &Player) -> (PlayerValue, f64) {
        let ctx = self.ctx;
        let scoring = self.scoring;
        let rk = self.rookies.get(&p.cid);
        let out = rates_for(
            p,
            &self.means,
            Some(scoring as &dyn ScoringSystem),
            season_age(p, ctx.as_of),
            None,
            None,
            true,
            self.preseason.get(&p.cid).copied(),
            rk,
        );
        let (mut rates, mut reasons, est) = (out.rates, out.reasons, out.estimate);
        let val = |r: &Rates| if r.is_empty() { 0.0 } else { scoring.value(r) };
        let fpg_pre = val(&rates);
        let team_key = normalize_team(p.team.as_deref()).unwrap_or_default();
        if let Some(t) = self.goalie_teams.get(&team_key).filter(|_| p.is_goalie() && !rates.is_empty()) {
            let (r, why) = team_goalie(&rates, &team_key, t, scoring);
            rates = r;
            reasons.extend(why);
        }
        if p.is_goalie() && !rates.is_empty() && self.gsax.contains_key(&p.cid) {
            let (r, why) = apply_gsax(&rates, self.gsax[&p.cid], scoring, self.league_sv);
            rates = r;
            reasons.extend(why);
        } else if !p.is_goalie() && !rates.is_empty() {
            let (r, why) = assist_shrink(p, &rates, scoring, Some(&self.norms));
            rates = r;
            reasons.extend(why);
        }
        let fpg_shift = val(&rates) - fpg_pre; // TEAM_GOALIE / GSAX / ASSIST_SHRINK change
        let mut role: Option<RoleChange> = None;
        if rk.is_none() && !p.is_goalie() && !rates.is_empty() && !is_unproven(p) {
            let team = normalize_team(p.team.as_deref());
            let news: &[RoleSignal] = self.news.get(&p.cid).copied().unwrap_or(&[]);
            role = role_change(
                p,
                news,
                Some(ctx.as_of),
                self.pp_teams.contains(&team),
                self.lineup_teams.contains(&team),
            );
            if let Some(r) = &role {
                rates = scale_offense(&rates, r.mult);
                reasons.extend(r.reasons.iter().cloned());
            }
        }
        let fpg = val(&rates);
        let team_dates: &[NaiveDate] = ctx.schedule.get(&team_key).map(|v| v.as_slice()).unwrap_or(&[]);
        let status = p.status.as_str();
        let (a_season, ret_est) =
            season_availability(status, p.status_note.as_deref(), ctx.as_of, Some(team_dates), ctx.season_start, self.season, None);
        let mut a_week = availability_multiplier(status, Horizon::Week, None);
        let ret_day = ret_est.as_ref().and_then(|e| e.return_date).filter(|_| WEEK_OUT_STATUSES.contains(&status));
        if let (Some(ret_day), Some(window)) = (ret_day, &self.window) {
            // out now, back within the window: his games after
            let end = window.end();
            let games: HashSet<NaiveDate> =
                team_dates.iter().copied().filter(|d| window.start <= *d && *d <= end).collect();
            a_week = if games.is_empty() {
                0.0
            } else {
                games.iter().filter(|d| **d >= ret_day).count() as f64 / games.len() as f64
            };
        }
        let raw = match p.lines.get("season").filter(|s| s.gp != 0) {
            Some(season) => format!(", season raw {}", f2(scoring.value(&season.per_game()))),
            None => String::new(),
        };
        reasons.insert(0, Reason::new("FPG_BLEND", format!("Blended {} FPG{raw}", f2(fpg))).with_value(Some(fpg)));
        if !matches!(status, "healthy" | "unknown") {
            let note = match p.status_note.as_deref() {
                Some(n) if !n.is_empty() => format!(" ({n})"),
                _ => String::new(),
            };
            reasons.push(
                Reason::new(
                    "STATUS",
                    format!("Status {status}{note}: x{} this week, x{} rest of season", f2(a_week), f2(a_season)),
                )
                .with_value(Some(a_season))
                .with_baseline(Some(1.0)),
            );
            if let Some(e) = &ret_est {
                reasons.push(
                    Reason::new("RETURN_ESTIMATE", e.text())
                        .with_value(Some(round_to(e.games_missed, 2)))
                        .with_baseline(Some(round_to(e.games_remaining, 2))),
                );
            }
        }
        let mut share = 1.0;
        let mut season_value: Option<f64> = None;
        let mut share_source: Option<&str> = None;
        let mut share_starts: Option<f64> = None;
        let mut share_team_games: Option<f64> = None;
        if p.is_goalie() {
            let (ss_prior, ss_k) = (self.ss_prior, self.ss_k);
            let work = projected_workload(p, Some(&self.team_gp), self.season.games);
            let actual = ledger_actual(ctx, &p.cid);
            if let Some((w_share, mut per_team_game, text)) = work {
                share = w_share;
                if let Some(ptg) = per_team_game.as_mut() {
                    // the projection's own FPts: carry the GSAx change
                    *ptg += fpg_shift * share;
                }
                if let Some((a0, a1)) = actual {
                    // this season's ledger starts replace the stat-line blend (k=10)
                    let proj_share = projection_share(p, self.season.games);
                    let new_share = actual_start_share(proj_share, a0 as f64, a1 as f64, ACTUAL_SHARE_K);
                    if let Some(ptg) = per_team_game.as_mut() {
                        if share > 0.0 {
                            *ptg *= new_share / share;
                        }
                    }
                    let sv = per_team_game.unwrap_or(fpg * new_share) * a_season;
                    season_value = Some(sv);
                    share_source = Some("actual");
                    reasons.push(
                        Reason::new(
                            "START_ACTUAL",
                            format!(
                                "Start share {} from {a0} starts in {a1} team games this season (ledger), shrunk toward \
                                 his projected share {} (k={}; projection estimate {}): season value {} -> {} FPG",
                                pct0(new_share),
                                pct0(proj_share),
                                general(ACTUAL_SHARE_K, None, false),
                                pct0(share),
                                f2(fpg * a_season),
                                f2(sv)
                            ),
                        )
                        .with_value(Some(new_share))
                        .with_baseline(Some(share)),
                    );
                    share = new_share;
                } else {
                    let sv = per_team_game.unwrap_or(fpg * share) * a_season;
                    season_value = Some(sv);
                    share_source = Some("projection");
                    reasons.push(
                        Reason::new(
                            "START_SHARE",
                            format!("{text}: season value {} -> {} FPG", f2(fpg * a_season), f2(sv)),
                        )
                        .with_value(Some(share))
                        .with_baseline(Some(ss_prior)),
                    );
                }
            } else {
                let tg_map = goalie_team_games(p, &self.team_gp, self.season.games);
                let (s, gs, tg) = start_share_parts(p, None, None, Some(&tg_map), &["season", "prior"]);
                share = s;
                if let Some((a0, a1)) = actual {
                    let line_share = share;
                    share = start_share(p, None, None, Some(&tg_map), Some((a0 as f64, a1 as f64)), ACTUAL_SHARE_K);
                    share_source = Some("actual");
                    reasons.push(
                        Reason::new(
                            "START_ACTUAL",
                            format!(
                                "Start share {} from {a0} starts in {a1} team games this season (ledger), shrunk toward \
                                 his prior share (k=10; stat-line estimate {}): season value {} -> {} FPG",
                                pct0(share),
                                pct0(line_share),
                                f2(fpg * a_season),
                                f2(fpg * a_season * share)
                            ),
                        )
                        .with_value(Some(share))
                        .with_baseline(Some(line_share)),
                    );
                } else {
                    share_source = Some("history");
                    share_starts = Some(gs);
                    share_team_games = Some(tg);
                    reasons.push(
                        Reason::new(
                            "START_SHARE",
                            format!(
                                "Projected start share {} ({} GS / {} team GP, k={} toward {}): season value {} -> {} FPG",
                                pct0(share),
                                fixed(gs, 0, false),
                                fixed(tg, 0, false),
                                general(ss_k, None, false),
                                pct0(ss_prior),
                                f2(fpg * a_season),
                                f2(fpg * a_season * share)
                            ),
                        )
                        .with_value(Some(share))
                        .with_baseline(Some(ss_prior)),
                    );
                }
            }
            if share_source != Some("actual") {
                // ledger starts beat news
                let news: &[RoleSignal] = self.news.get(&p.cid).copied().unwrap_or(&[]);
                let (new_share, why) = goalie_news_share(share, news, Some(ctx.as_of));
                if let Some(why) = why {
                    if let Some(sv) = season_value.as_mut() {
                        if share > 0.0 {
                            *sv *= new_share / share;
                        }
                    }
                    share = new_share;
                    reasons.push(why);
                }
            }
        }
        let (mut share_week, mut share_season) = (share, share);
        if let Some(tm) = self.teammates.get(&p.cid).filter(|_| p.is_goalie()) {
            share_week = tm.week;
            share_season = tm.season;
            if let Some(sv) = season_value {
                season_value =
                    Some(if share > 0.0 { sv * share_season / share } else { fpg * share_season * a_season });
            }
            share_source = Some("teammate");
            share_starts = None;
            share_team_games = None;
            reasons.push(
                Reason::new("TEAMMATE_OUT", tm.text(p))
                    .with_value(Some(round_to(share_week, 4)))
                    .with_baseline(Some(round_to(share, 4))),
            );
        }
        let (mut gp_share, mut week_share) = (1.0, 1.0);
        if let Some(r) = &role {
            // a role change: the week keeps all of it, the season part of it
            gp_share = r.season_mult / r.mult;
            week_share = r.week_extra;
        }
        if let Some(e) = &est {
            gp_share = e.gp_expectation;
            week_share = e.week_share;
            if gp_share < 1.0 || week_share < 1.0 {
                let wk = if week_share < 1.0 { format!(", week value x{}", f2(week_share)) } else { String::new() };
                reasons.push(
                    Reason::new("ROOKIE_GAMES", format!("Rookie games share: season value x{}{wk}", f2(gp_share)))
                        .with_value(Some(gp_share))
                        .with_baseline(Some(1.0)),
                );
            }
        }
        let fpg_season = season_value.unwrap_or(fpg * a_season * share_season) * gp_share;
        let mut pv = PlayerValue {
            player: p.clone(),
            fpg,
            fpg_season,
            ir_fill: 0.0,
            fpg_error: 0.0,
            fpg_week: fpg * a_week * share_week * week_share,
            vorp: 0.0,
            vorp_week: 0.0,
            proj_week: None,
            games_next7: None,
            offnight_next7: None,
            start_share: if p.is_goalie() { Some(share_week) } else { None },
            return_date: ret_day,
            avail_week: a_week,
            share_source: share_source.map(str::to_string),
            share_starts,
            share_team_games,
            horizon_values: Map::new(),
            rates,
            reasons,
            rookie: est,
        };
        let Some(window) = &self.window else { return (pv, a_season) };
        let b2b = if p.is_goalie() { ctx.b2b_second_night.get(&team_key).copied() } else { None };
        let sf = schedule_factor(
            p,
            &ctx.schedule,
            &ctx.games_per_day,
            window,
            OFFNIGHT_THRESHOLD,
            Some(share_week),
            b2b.map(|b| b.0),
        );
        let Some(sf) = sf else { return (pv, a_season) };
        if let Some((bp, seen_n)) = b2b {
            if (sf.start_share - share_week).abs() > 1e-4 {
                let seen = format!("{seen_n} seen{}", if seen_n >= 5 { "" } else { ", league prior until 5" });
                pv.reasons.push(
                    Reason::new(
                        "B2B",
                        format!(
                            "Back-to-back second nights {}: the #1 starts them with p={} ({seen}); week start share {} -> {}",
                            window.label(),
                            f2(bp),
                            pct0(share_week),
                            pct0(sf.start_share)
                        ),
                    )
                    .with_value(Some(sf.start_share))
                    .with_baseline(Some(share_week)),
                );
            }
        }
        let mut pw = proj_week(fpg, a_week, sf.games, sf.offnight, None) * sf.start_share * week_share;
        let opp = if sf.games != 0 { opp_week_factor(ctx, self.memo, p, Some(window)) } else { 1.0 };
        if (opp - 1.0).abs() > 1e-4 {
            pw *= opp;
            pv.reasons.push(
                Reason::new(
                    OPP_WEEK_REASON,
                    format!(
                        "Opponents {}: week value x{} (market implied goals, else opponent goals against)",
                        window.label(),
                        f2(opp)
                    ),
                )
                .with_value(Some(opp))
                .with_baseline(Some(1.0)),
            );
        }
        pv.proj_week = Some(pw);
        pv.games_next7 = Some(sf.games);
        pv.offnight_next7 = Some(sf.offnight);
        pv.fpg_week = pw / window.avg_team_games;
        let pre = if window.preseason { " (preseason: first 7 days from opening night)" } else { "" };
        pv.reasons.push(
            Reason::new(
                "GAMES_NEXT7",
                format!(
                    "{} games {}{pre}, {} on off-nights (avg team {})",
                    sf.games,
                    window.label(),
                    sf.offnight,
                    fixed(window.avg_team_games, 1, false)
                ),
            )
            .with_value(Some(sf.games as f64))
            .with_baseline(Some(window.avg_team_games)),
        );
        let rk_week = if week_share < 1.0 {
            format!(
                " (x{} {})",
                f2(week_share),
                if pv.rookie.is_some() { "rookie games share" } else { "after a scratch" }
            )
        } else {
            String::new()
        };
        pv.reasons.push(
            Reason::new("PROJ_WEEK", format!("Projected {} pts over {}{rk_week}", fixed(pw, 1, false), window.label()))
                .with_value(Some(pw)),
        );
        (pv, a_season)
    }
}

/// Value every rostered player and free agent in the league (no betting odds; see
/// [`valuate_league_with`]). `scoring` must already be fitted (see [`fit_and_valuate`]).
pub fn valuate_league(ctx: &mut LeagueContext, scoring: &(dyn ScoringSystem + Sync)) -> Map<String, PlayerValue> {
    valuate_league_with(ctx, scoring, &ScheduleMemo::new(None))
}

/// `valuate_league` with an unfitted categories / roto system fitted on the league's rostered
/// players first (Python does this inside `valuate_league`).
pub fn fit_and_valuate(ctx: &mut LeagueContext, scoring: &mut Scoring, memo: &ScheduleMemo) -> Map<String, PlayerValue> {
    if scoring.as_categories().is_some_and(|c| !c.fitted()) {
        fit_to_context(scoring, ctx);
    }
    valuate_league_with(ctx, scoring, memo)
}

/// `valuate_league(ctx, scoring)` with an explicit schedule memo (odds from its data dir).
pub fn valuate_league_with(
    ctx: &mut LeagueContext,
    scoring: &(dyn ScoringSystem + Sync),
    memo: &ScheduleMemo,
) -> Map<String, PlayerValue> {
    // Python appends `warn_signal` entries here when an optional signal raises; the Rust lookups
    // read ctx.enrich and cannot fail, so ctx is only read below.
    let ctx: &LeagueContext = ctx;
    let players = ctx.all_players();
    let means = positional_means(&players);
    let team_gp = team_games_played(ctx);
    let window = memo.window(ctx).filter(|w| w.avg_team_games > 0.0);
    let teammates = teammate_share_lookup(ctx, &team_gp, window.as_ref());
    let preseason = preseason_lookup(ctx);
    let rookies = rookie_lookup(ctx);
    let news = news_signals(ctx);
    let gsax = gsax_lookup(ctx);
    let league_sv = if gsax.is_empty() { None } else { league_save_pct(&means) };
    let norms = onice_norms(ctx);
    let pp_teams: HashSet<Option<String>> = players
        .iter()
        .filter(|p| p.pp_unit.as_deref().is_some_and(|s| !s.is_empty()))
        .map(|p| normalize_team(p.team.as_deref()))
        .collect();
    let lineup_teams: HashSet<Option<String>> = players
        .iter()
        .filter(|p| p.line.as_deref().is_some_and(|s| !s.is_empty()))
        .map(|p| normalize_team(p.team.as_deref()))
        .collect();
    let league = League {
        ctx,
        memo,
        scoring,
        means,
        team_gp,
        season: SeasonLength::of(&ctx.schedule),
        window,
        teammates,
        preseason,
        rookies,
        news,
        gsax,
        goalie_teams: if TEAM_GOALIE_OFF.load(Ordering::Relaxed) { Map::new() } else { team_goalie_rates(ctx) },
        league_sv,
        norms,
        pp_teams,
        lineup_teams,
        ss_prior: params::start_share_prior(None),
        ss_k: params::start_share_k(None),
    };
    let pvs: Vec<(PlayerValue, f64)> = players.par_iter().map(|p| league.value_player(p)).collect();
    let mut partial: Map<String, PlayerValue> = Map::with_capacity(pvs.len());
    let mut a_season: Map<String, f64> = Map::with_capacity(pvs.len());
    for (pv, a) in pvs {
        a_season.insert(pv.player.cid.clone(), a);
        partial.insert(pv.player.cid.clone(), pv);
    }
    finish_vorp(ctx, &mut partial, &a_season);
    partial
}

/// Typical error of a player's per-game value in league points: the backtest's mean absolute
/// error for his group (`preseason.mae_at_fitted_k_multi`: F 0.23, D 0.21, G 0.58 ESPN points)
/// times the league's [`PointScale`] for skaters or goalies; 0 after `disable_point_scale`.
pub fn projection_error(p: &Player, scale: &PointScale) -> f64 {
    if point_scale_disabled() {
        return 0.0;
    }
    params::projection_mae(position_group(p), None) * scale.for_player(p)
}

static IR_FILL_OFF: AtomicBool = AtomicBool::new(false);
static TEAM_GOALIE_OFF: AtomicBool = AtomicBool::new(false);

/// Turn the team goalie rates ([`team_goalie`]) off (true) or back on for every later valuation in
/// this process. Only the Python parity tests and the CLI's hidden `--from-snapshot` mode turn them
/// off: their fixtures come from Python, which projected wins and shots from the goalie himself.
pub fn disable_team_goalie(off: bool) {
    TEAM_GOALIE_OFF.store(off, Ordering::Relaxed);
}

/// Turn the IR fill ([`ir_holders`]) off (true) or back on for every later valuation in this
/// process. Only the Python parity tests and the CLI's hidden `--from-snapshot` mode turn it off:
/// their fixtures come from Python, which valued an injured player's missed games at zero.
pub fn disable_ir_fill(off: bool) {
    IR_FILL_OFF.store(off, Ordering::Relaxed);
}

/// Rostered players whose roster spot is refilled while they are out: everyone in an IR slot, and
/// on each team the players the waiver and trade engines would move into its free IR slots
/// ([`ir_movable`], best first, as many as there are free slots).
pub fn ir_holders(ctx: &LeagueContext, values: &Map<String, PlayerValue>) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    for t in &ctx.teams {
        for s in t.slots.iter().filter(|s| IR_SLOT_NAMES.contains(&s.slot.as_str())) {
            out.extend(s.player.as_ref().map(|p| p.cid.clone()));
        }
        let free = free_ir_slots(t, &ctx.roster_shape).max(0) as usize;
        out.extend(ir_movable(t, Some(values)).into_iter().take(free).map(|p| p.cid.clone()));
    }
    out
}

/// Replacement levels for both horizons, the IR fill, then VORP / horizon values / VORP reasons.
///
/// IR fill: a player who is out and held in an IR slot (or can be moved into a free one) does not
/// block a roster spot, so the spot gets replacement-level production for the games he misses.
/// His season value becomes availability x FPG + (1 - availability) x replacement, i.e. `fpg_season`
/// plus (1 - availability) x the replacement level of his VORP slot (floored at 0: an unfilled
/// slot scores nothing). Python had no fill; [`disable_ir_fill`] restores that for parity.
///
/// Each value also gets its projection error ([`projection_error`]), which Python did not have. A
/// non-points league gets none: the backtest errors are in fantasy points, not category units.
fn finish_vorp(ctx: &LeagueContext, partial: &mut Map<String, PlayerValue>, a_season: &Map<String, f64>) {
    let (repl_season, fb_season) = replacement(ctx, partial, Horizon::Season);
    let (repl_week, fb_week) = replacement(ctx, partial, Horizon::Week);
    let fallback: HashSet<&String> = fb_season.iter().chain(fb_week.iter()).collect();
    let pct = (ROSTERED_FALLBACK_PCT * 100.0).trunc() as i64;
    let held = if IR_FILL_OFF.load(Ordering::Relaxed) { HashSet::new() } else { ir_holders(ctx, partial) };
    let scale = point_scale(ctx);
    let points = ctx.scoring.kind == fm_core::models::ScoringKind::Points;
    for pv in partial.values_mut() {
        pv.fpg_error = if points { projection_error(&pv.player, &scale) } else { 0.0 };
        if pv.player.is_goalie() && pv.fpg > 0.0 {
            // the backtest error is per start, but a goalie's season value is per team game
            // (fpg x start share x availability), so his error shrinks by the same factor
            pv.fpg_error *= (pv.fpg_season / pv.fpg).clamp(0.0, 1.0);
        }
        let pos = &pv.player.positions;
        let missed = 1.0 - a_season.get(&pv.player.cid).copied().unwrap_or(1.0);
        if missed > 0.0 && held.contains(&pv.player.cid) {
            if let Some(slot) = best_slot(pos, &repl_season) {
                let r = repl_season[&slot].max(0.0);
                pv.ir_fill = missed * r;
                pv.fpg_season += pv.ir_fill;
                pv.reasons.push(
                    Reason::new(
                        "IR_FILL",
                        format!(
                            "IR slot: his roster spot is refilled while he is out, +{} FPG ({} of the season at {slot} \
                             replacement {})",
                            f2(pv.ir_fill),
                            pct0(missed),
                            f2(r)
                        ),
                    )
                    .with_value(Some(pv.ir_fill))
                    .with_baseline(Some(r)),
                );
            }
        }
        pv.vorp = vorp(pv.fpg_season, pos, &repl_season);
        pv.vorp_week = vorp(pv.fpg_week, pos, &repl_week);
        let slot = best_slot(pos, &repl_season);
        let mut hv: Map<String, f64> = Map::with_capacity(5);
        hv.insert("season".into(), pv.fpg_season);
        hv.insert("week".into(), pv.fpg_week);
        hv.insert("vorp_season".into(), pv.vorp);
        hv.insert("vorp_week".into(), pv.vorp_week);
        if let Some(pw) = pv.proj_week {
            hv.insert("proj_week".into(), pw);
        }
        pv.horizon_values = hv;
        if let Some(slot) = slot {
            let r = repl_season[&slot];
            pv.reasons.push(
                Reason::new("VORP", format!("{} FPG over {slot} replacement ({})", fixed(pv.vorp, 2, true), f2(r)))
                    .with_value(Some(pv.vorp))
                    .with_baseline(Some(r)),
            );
            if fallback.contains(&slot) {
                pv.reasons.push(
                    Reason::new(
                        "REPL_FALLBACK",
                        format!("No free agents at {slot}: replacement is the rostered {pct}th percentile ({})", f2(r)),
                    )
                    .with_value(Some(r)),
                );
            }
        }
    }
}

/// tests/test_valuation_quality.py and the valuation parts of tests/test_goalie_gsax.py.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use fm_core::snapshot::EnrichState;
    use serde_json::json;

    use super::super::dynasty::test_support::*;
    use super::super::dynasty::{apply_dynasty, production_rate};
    use super::*;
    use crate::scoring::PointsScoring;

    fn w(xs: &[(&str, f64)]) -> PointsScoring {
        PointsScoring::new(xs.iter().map(|(k, v)| (k.to_string(), *v)).collect(), None)
    }
    fn goalie_w() -> PointsScoring {
        w(&[("W", 3.0), ("SV", 0.2), ("GA", -1.0), ("SO", 2.0)])
    }

    /// test_valuation_quality.goalie
    fn goalie(cid: &str, gp: i64, svpct: f64, w_rate: f64, split: &str, gs: Option<f64>) -> Player {
        let sa = 30.0 * gp as f64;
        let sv = svpct * sa;
        let stats = json!({"GS": gs.unwrap_or(gp as f64), "SA": sa, "SV": sv, "GA": sa - sv, "W": w_rate * gp as f64,
                           "SO": 0.05 * gp as f64, "SVPCT": svpct});
        player(json!({"cid": cid, "team": "EDM", "positions": ["G"], "lines": {split: line(split, gp, stats)}}))
    }

    fn dman(cid: &str, gp: i64, pts_pg: f64) -> Player {
        player(json!({"cid": cid, "team": "EDM", "positions": ["D"],
                      "lines": {"prior": line("prior", gp, json!({"PTS": pts_pg * gp as f64, "GP": gp}))}}))
    }

    fn qctx(rostered: &[&Player], fas: &[&Player], dynasty: bool) -> LeagueContext {
        let slots: Vec<(&str, &Player)> = rostered.iter().map(|p| ("BN", *p)).collect();
        ctx(json!({"roster_shape": {"D": 2, "G": 2, "BN": 10}, "teams": [team("1", "me", true, &slots)],
                   "free_agents": fas, "matchup_period": 1, "as_of": "2026-09-28", "dynasty": dynasty}))
    }

    fn val(c: &mut LeagueContext, s: &PointsScoring) -> Map<String, PlayerValue> {
        valuate_league(c, s)
    }

    fn has(pv: &PlayerValue, code: &str) -> bool {
        pv.reason(code).is_some()
    }

    #[test]
    fn three_gp_hot_goalie_values_below_sixty_gp_starter() {
        let hot = goalie("hot", 3, 0.950, 1.0, "prior", None);
        let starter = goalie("starter", 60, 0.915, 0.55, "prior", None);
        let fillers: Vec<Player> = (0..4).map(|i| goalie(&format!("f{i}"), 40, 0.900, 0.45, "prior", None)).collect();
        let mut rost = vec![&starter];
        rost.extend(fillers.iter());
        let mut c = qctx(&rost, &[&hot], false);
        let s = goalie_w();
        let vals = val(&mut c, &s);
        let raw_hot = s.value(&hot.lines["prior"].per_game());
        let raw_starter = s.value(&starter.lines["prior"].per_game());
        assert!(raw_hot > raw_starter);
        assert!(vals["hot"].fpg < raw_hot);
        assert!(vals["hot"].fpg_season < vals["starter"].fpg_season);
        assert!(vals["hot"].vorp < vals["starter"].vorp);
        let share_hot = (3.0 + 5.0) / (82.0 + 10.0);
        close(vals["hot"].start_share.unwrap(), share_hot);
        close(vals["hot"].fpg_season, vals["hot"].fpg * share_hot);
        assert!(has(&vals["hot"], "START_SHARE") && has(&vals["hot"], "BASELINE"));
    }

    #[test]
    fn five_gp_defenseman_values_below_seventy_gp_one() {
        let small = dman("small", 5, 1.2);
        let big = dman("big", 70, 0.8);
        let fillers: Vec<Player> = (0..5).map(|i| dman(&format!("f{i}"), 60, 0.3)).collect();
        let mut rost = vec![&big];
        rost.extend(fillers.iter());
        let mut c = qctx(&rost, &[&small], false);
        let vals = val(&mut c, &w(&[("PTS", 1.0)]));
        assert!(vals["small"].fpg < vals["big"].fpg);
        let mean = (0.8 + 5.0 * 0.3) / 6.0;
        let k = baseline_k("D");
        close(vals["small"].fpg, (5.0 * 1.2 + k * mean) / (5.0 + k));
        close(vals["big"].fpg, (70.0 * 0.8 + k * mean) / (70.0 + k));
    }

    #[test]
    fn season_only_small_sample_shrinks_toward_mean() {
        let lucky = goalie("lucky", 1, 0.970, 1.0, "season", None);
        let fillers: Vec<Player> = (0..3).map(|i| goalie(&format!("f{i}"), 40, 0.905, 0.5, "prior", None)).collect();
        let rost: Vec<&Player> = fillers.iter().collect();
        let mut c = qctx(&rost, &[&lucky], false);
        let s = goalie_w();
        let vals = val(&mut c, &s);
        let raw = s.value(&lucky.lines["season"].per_game());
        assert!(vals["lucky"].fpg < 0.5 * raw + 0.5 * vals["f0"].fpg);
    }

    #[test]
    fn valuation_marks_fallback_replacement() {
        let gs: Vec<Player> =
            (0..5).map(|i| goalie(&format!("g{i}"), 50, 0.90 + i as f64 * 0.003, 0.5, "prior", None)).collect();
        let fa = dman("fa", 60, 0.3);
        let dd = dman("d", 60, 0.5);
        let mut rost: Vec<&Player> = gs.iter().collect();
        rost.push(&dd);
        let mut c = qctx(&rost, &[&fa], false);
        let vals = val(&mut c, &w(&[("W", 3.0), ("SV", 0.2), ("GA", -1.0), ("SO", 2.0), ("PTS", 1.0)]));
        let r = vals["g4"].reason("REPL_FALLBACK").expect("REPL_FALLBACK");
        assert!(r.value.unwrap() > 0.0);
        assert!(r.text.contains("rostered 10th percentile"));
        assert!(!has(&vals["d"], "REPL_FALLBACK"));
    }

    #[test]
    fn dynasty_does_not_carry_injury_into_future_years() {
        let mut hurt = dman("hurt", 70, 0.8);
        hurt.status = fm_core::models::Status::Ir;
        let fine = dman("fine", 70, 0.8);
        let mut c = qctx(&[&hurt, &fine], &[], true);
        let vals = val(&mut c, &w(&[("PTS", 1.0)]));
        let dynv = apply_dynasty(&vals, &c, None, None);
        assert!(vals["hurt"].fpg_season < vals["fine"].fpg_season);
        let gap = vals["fine"].fpg_season - vals["hurt"].fpg_season;
        close(dynv["fine"].value - dynv["hurt"].value, gap);
    }

    /// Two identical injured defensemen, one IR slot: the one in it is worth a x FPG + (1 - a) x
    /// replacement, the one on the bench a x FPG; a second IR slot refills the bench one too.
    #[test]
    fn ir_slot_refills_an_injured_players_missed_games() {
        let hurt = |cid: &str| {
            let mut p = dman(cid, 70, 0.8);
            p.status = fm_core::models::Status::Ir;
            p
        };
        let (stashed, benched, hurt_fa) = (hurt("stashed"), hurt("benched"), hurt("hurt_fa"));
        let fas: Vec<Player> = (0..3).map(|i| dman(&format!("fa{i}"), 60, 0.3)).chain([hurt_fa]).collect();
        let league = |ir_slots: i64| {
            ctx(json!({"roster_shape": {"D": 2, "BN": 3, "IR": ir_slots},
                       "teams": [team("1", "me", true, &[("IR", &stashed), ("BN", &benched)])],
                       "free_agents": fas, "matchup_period": 1, "as_of": "2026-09-28"}))
        };
        let pts = w(&[("PTS", 1.0)]);
        let vals = val(&mut league(1), &pts);
        let (s, b) = (&vals["stashed"], &vals["benched"]);
        let a = s.reason("STATUS").unwrap().value.unwrap();
        let r = s.reason("VORP").unwrap().baseline.unwrap();
        assert!(0.0 < a && a < 1.0 && r > 0.0, "a={a} r={r}");
        close(s.fpg, b.fpg);
        close(b.fpg_season, a * b.fpg);
        assert_eq!(b.ir_fill, 0.0);
        assert!(!has(b, "IR_FILL"));
        close(s.fpg_season, a * s.fpg + (1.0 - a) * r);
        close(s.ir_fill, (1.0 - a) * r);
        close(s.vorp, a * (s.fpg - r));
        assert_eq!(s.reason("IR_FILL").unwrap().baseline, Some(r));
        // an injured free agent has no IR slot of his own: his missed games stay at zero
        close(vals["hurt_fa"].fpg_season, a * vals["hurt_fa"].fpg);
        // a free IR slot: the bench player can be moved there
        let vals = val(&mut league(2), &pts);
        close(vals["benched"].fpg_season, a * vals["benched"].fpg + (1.0 - a) * r);
    }

    #[test]
    fn player_without_any_stats_is_not_given_the_mean() {
        let blank = player(json!({"cid": "blank", "team": "EDM", "positions": ["G"]}));
        let fillers: Vec<Player> = (0..3).map(|i| goalie(&format!("f{i}"), 40, 0.905, 0.5, "prior", None)).collect();
        let rost: Vec<&Player> = fillers.iter().collect();
        let mut c = qctx(&rost, &[&blank], false);
        let vals = val(&mut c, &goalie_w());
        assert_eq!(vals["blank"].fpg, 0.0);
        assert!(has(&vals["blank"], "NO_DATA"));
    }

    #[test]
    fn dynasty_upside_ignores_small_samples() {
        assert_eq!(production_rate(&dman("tiny", 3, 1.5)), None);
        close(production_rate(&dman("real", 30, 0.5)).unwrap(), 0.5);
    }

    fn proj_goalie(cid: &str, prior_gp: i64, proj_gp: i64, fpts: f64) -> Player {
        let mut g = goalie(cid, prior_gp, 0.905, 0.5, "prior", None);
        let pg = proj_gp as f64;
        let l: StatLine = serde_json::from_value(line("projected", proj_gp, json!({"GP": proj_gp, "W": 0.5 * pg,
            "SV": 25.0 * pg, "GA": 2.8 * pg, "FPTS": fpts}))).unwrap();
        g.lines.insert("projected".into(), l);
        g
    }

    #[test]
    fn goalie_workload_comes_from_the_projection() {
        let blackwood = proj_goalie("blackwood", 39, 42, 42.0 * 6.5);
        let skinner = proj_goalie("skinner", 50, 25, 25.0 * 6.2);
        let fillers: Vec<Player> = (0..3).map(|i| goalie(&format!("f{i}"), 40, 0.900, 0.5, "prior", None)).collect();
        let mut rost = vec![&blackwood];
        rost.extend(fillers.iter());
        let mut c = qctx(&rost, &[&skinner], false);
        let vals = val(&mut c, &goalie_w());
        let (b, s) = (&vals["blackwood"], &vals["skinner"]);
        close(b.start_share.unwrap(), 42.0 / 82.0);
        close(s.start_share.unwrap(), 25.0 / 82.0);
        close(b.fpg_season, 42.0 * 6.5 / 82.0);
        close(s.fpg_season, 25.0 * 6.2 / 82.0);
        assert!(b.fpg_season > s.fpg_season && b.vorp > s.vorp);
        assert!(b.reason("START_SHARE").unwrap().text.contains("42 projected starts"));
        assert_eq!(b.share_source.as_deref(), Some("projection"));
        let plain = goalie("plain", 50, 0.905, 0.5, "prior", None);
        let mut rost = vec![&plain];
        rost.extend(fillers.iter());
        let mut c = qctx(&rost, &[], false);
        let vals = val(&mut c, &goalie_w());
        close(vals["plain"].start_share.unwrap(), (50.0 + 5.0) / (82.0 + 10.0));
        assert_eq!(vals["plain"].share_source.as_deref(), Some("history"));
        assert_eq!(vals["plain"].share_starts, Some(50.0));
        assert_eq!(vals["plain"].share_team_games, Some(82.0));
    }

    #[test]
    fn goalie_projection_share_blends_with_this_seasons_starts() {
        let mut g = goalie("g", 10, 0.91, 0.5, "season", Some(10.0));
        g.lines.insert("projected".into(), serde_json::from_value(line("projected", 41, json!({"GP": 41}))).unwrap());
        let tg: Map<String, i64> = [("EDM".to_string(), 12)].into_iter().collect();
        let (share, per_game, _) = projected_workload(&g, Some(&tg), 82.0).unwrap();
        assert!(per_game.is_none());
        close(share, (10.0 + PROJ_SHARE_K as f64 * 0.5) / (12.0 + PROJ_SHARE_K as f64));
        assert!(projected_workload(&dman("d", 50, 0.5), None, 82.0).is_none());
    }

    /// An 84-game schedule for EDM: every other day from 2026-10-01.
    fn schedule_84() -> Map<String, Vec<NaiveDate>> {
        let first = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let dates = (0..84).map(|i| first + chrono::Duration::days(2 * i)).collect();
        [("EDM".to_string(), dates)].into_iter().collect()
    }

    #[test]
    fn goalie_projection_is_spread_over_the_schedules_games_per_team() {
        let blackwood = proj_goalie("blackwood", 39, 42, 42.0 * 6.5);
        let plain = goalie("plain", 50, 0.905, 0.5, "prior", None);
        let fillers: Vec<Player> = (0..3).map(|i| goalie(&format!("f{i}"), 40, 0.900, 0.5, "prior", None)).collect();
        let mut rost = vec![&blackwood, &plain];
        rost.extend(fillers.iter());
        let mut c = qctx(&rost, &[], false);
        c.schedule = schedule_84();
        let vals = val(&mut c, &goalie_w());
        let b = &vals["blackwood"];
        close(b.start_share.unwrap(), 42.0 / 84.0);
        close(b.fpg_season, 42.0 * 6.5 / 84.0);
        let text = &b.reason("START_SHARE").unwrap().text;
        assert!(text.contains("50% (42 projected starts / 84)") && text.contains("273 FPts / 84 team games"), "{text}");
        // last season's starts are still counted against last season's 82 team games
        close(vals["plain"].start_share.unwrap(), (50.0 + 5.0) / (82.0 + 10.0));
        assert_eq!(vals["plain"].share_team_games, Some(82.0));
        // ledger starts shrink toward the same 84-game projected share
        let mut g = proj_goalie("g", 39, 63, 63.0 * 6.5);
        g.lines.shift_remove("prior");
        let mut c = qctx(&[&g], &[], false);
        c.schedule = schedule_84();
        c.goalie_actual_starts.insert("g".into(), (2, 10));
        let expect = (2.0 + 10.0 * (63.0 / 84.0)) / (10.0 + 10.0);
        close(val(&mut c, &goalie_w())["g"].start_share.unwrap(), expect);
        close(goalie_base_share(&g, &c, &team_games_played(&c), 84.0).0, expect);
    }

    fn fwd(cid: &str, lines: &[(&str, i64, f64)], birth: Option<&str>) -> Player {
        let ls: serde_json::Map<String, serde_json::Value> = lines
            .iter()
            .map(|(s, gp, pg)| (s.to_string(), line(s, *gp, json!({"PTS": pg * *gp as f64, "GP": gp}))))
            .collect();
        player(json!({"cid": cid, "team": "EDM", "positions": ["C"], "birth_date": birth, "lines": ls}))
    }

    fn means() -> Means {
        [("F".to_string(), [("PTS".to_string(), 0.5)].into_iter().collect())].into_iter().collect()
    }

    #[test]
    fn history_only_baseline_is_three_seasons_weighted_and_shrunk() {
        let p = fwd("h", &[("prior", 60, 0.9), ("prior2", 80, 0.6), ("prior3", 40, 0.3)], None);
        let (rates, line, n, src) = baseline_rates(&p, &means(), None, None, None);
        let wg = 5.0 * 60.0 + 4.0 * 80.0 + 3.0 * 40.0;
        let raw = (5.0 * 60.0 * 0.9 + 4.0 * 80.0 * 0.6 + 3.0 * 40.0 * 0.3) / wg;
        let k = baseline_k("F");
        close(rates["PTS"], (wg / 5.0 * raw + k * 0.5) / (wg / 5.0 + k));
        assert_eq!(line.unwrap().split.as_str(), "prior");
        close(n, wg / 5.0);
        assert!(src.starts_with("NHL history (180 GP over 3 seasons weighted 5/4/3"), "{src}");
    }

    #[test]
    fn age_factor_applies_to_the_history_baseline() {
        let p = fwd("h", &[("prior", 80, 0.6)], Some("2005-01-01"));
        let as_of = d(2026, 9, 28);
        let age = season_age(&p, as_of);
        let (young, _, _, src) = baseline_rates(&p, &means(), age, None, None);
        let (flat, _, _, _) = baseline_rates(&p, &means(), None, None, None);
        let f = age_factor("F", age.map(|a| a - 1.0), None, None);
        assert!(f > 1.0);
        close(young["PTS"], flat["PTS"] * f);
        assert!(src.contains(&format!("x{}", fixed(f, 3, false))));
        let old = fwd("o", &[("prior", 80, 0.6)], Some("1992-01-01"));
        assert!(baseline_rates(&old, &means(), season_age(&old, as_of), None, None).0["PTS"] < flat["PTS"]);
        let g = player(json!({"cid": "g", "team": "EDM", "positions": ["G"], "birth_date": "1990-01-01",
                              "lines": {"prior": line("prior", 50, json!({"W": 25.0, "GP": 50}))}}));
        let gm: Means = [("G".to_string(), [("W".to_string(), 0.5)].into_iter().collect())].into_iter().collect();
        assert_eq!(baseline_rates(&g, &gm, Some(36.0), None, None).0, baseline_rates(&g, &gm, None, None, None).0);
    }

    #[test]
    fn projection_blends_60_40_with_history_rookie_projection_only() {
        let m = means();
        let proj_rates = |p: &Player| shrink_baseline(p.lines.get("projected"), &m["F"], false, None, None).0["PTS"];
        let vet = fwd("v", &[("projected", 80, 1.0), ("prior", 70, 0.6), ("prior2", 10, 0.6)], None);
        let hist = baseline_rates(&fwd("v2", &[("prior", 70, 0.6), ("prior2", 10, 0.6)], None), &m, None, None, None).0["PTS"];
        let (rates, line, _, src) = baseline_rates(&vet, &m, None, None, None);
        assert_eq!(line.unwrap().split.as_str(), "projected");
        close(rates["PTS"], 0.6 * proj_rates(&vet) + 0.4 * hist);
        assert!(src.starts_with("60% projection") && src.contains("40% NHL history"), "{src}");
        let rookie = fwd("r", &[("projected", 70, 0.7)], None);
        close(baseline_rates(&rookie, &m, None, None, None).0["PTS"], proj_rates(&rookie));
        let part = fwd("p", &[("projected", 70, 0.7), ("prior", 20, 0.3)], None);
        let hist_p = baseline_rates(&fwd("p2", &[("prior", 20, 0.3)], None), &m, None, None, None).0["PTS"];
        close(baseline_rates(&part, &m, None, None, None).0["PTS"], 0.8 * proj_rates(&part) + 0.2 * hist_p);
    }

    // -- goalie GSAx (tests/test_goalie_gsax.py, valuation) ------------------------------------

    fn ggoalie(cid: &str, nhl: Option<i64>, gp: i64, svpct: f64, split: &str, gs: Option<f64>) -> Player {
        let sa = 30.0 * gp as f64;
        let sv = svpct * sa;
        let stats = json!({"GS": gs.unwrap_or(gp as f64), "SA": sa, "SV": sv, "GA": sa - sv, "W": 0.5 * gp as f64,
                           "SO": 0.05 * gp as f64, "SVPCT": svpct});
        let ids = match nhl {
            Some(n) => json!({"nhl": n.to_string()}),
            None => json!({}),
        };
        player(json!({"cid": cid, "ids": ids, "team": "EDM", "positions": ["G"], "lines": {split: line(split, gp, stats)}}))
    }

    fn est(nhl: i64, per_100: f64, shots: f64) -> GoalieGsax {
        let k = GSAX_K_SHOTS;
        GoalieGsax {
            nhl_id: nhl,
            seasons: vec![2025],
            gsax: per_100 / 100.0 * (shots + k),
            shots,
            raw_gsax: per_100 / 100.0 * (shots + k),
            raw_shots: shots,
            xsv_pct: 0.900,
            per_shot: per_100 / 100.0,
            shots_per_game: Some(28.0),
            league_sv_pct: Some(0.900),
        }
    }

    fn gctx(players: &[&Player], gsax: &[GoalieGsax]) -> LeagueContext {
        let slots: Vec<(&str, &Player)> = players.iter().map(|p| ("BN", *p)).collect();
        let mut c = ctx(json!({"scoring": {"kind": "points", "weights": {"W": 3.0, "SV": 0.2, "GA": -1.0, "SO": 2.0}},
                               "roster_shape": {"G": 2, "BN": 10}, "teams": [team("1", "me", true, &slots)],
                               "matchup_period": 1, "as_of": "2026-10-20"}));
        let mut st = EnrichState::default();
        for e in gsax {
            st.gsax.insert((2026, e.nhl_id), e.clone());
        }
        c.enrich = Arc::new(st);
        c
    }

    #[test]
    fn a_goalie_who_changed_teams_takes_wins_and_shots_from_his_new_team() {
        // three great seasons' worth of wins on his old team, ten starts with NJD this season
        let moved = player(json!({"cid": "moved", "team": "NJD", "positions": ["G"], "lines": {
            "prior": line("prior", 60, json!({"GS": 60, "W": 42, "SA": 1800, "SV": 1638, "GA": 162})),
            "season": line("season", 10, json!({"GS": 10, "W": 3, "SA": 300, "SV": 270, "GA": 30}))}}));
        let wpg = player(json!({"cid": "wpg", "team": "WPG", "positions": ["G"], "lines": {
            "season": line("season", 10, json!({"GS": 10, "W": 7, "SA": 250, "SV": 230, "GA": 20}))}}));
        let mut c = gctx(&[&moved, &wpg], &[]);
        let base = val(&mut c, &goalie_w());
        assert!(!has(&base["moved"], "TEAM_GOALIE"));
        assert!(base["moved"].rates["W"] > 0.6);
        let days: Vec<NaiveDate> = (1..=10).map(|d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap()).collect();
        c.schedule.insert("NJD".into(), days.clone());
        c.schedule.insert("WPG".into(), days);
        let vals = val(&mut c, &goalie_w());
        let (v, b) = (&vals["moved"], &base["moved"]);
        assert!(has(v, "TEAM_GOALIE"));
        // league 10 W and 550 SA over 20 starts; NJD 3 W and 300 SA over 10 starts, shrunk with
        // k=60 (wins) and k=25 (shots); every game was a start, so per start = per game
        close(v.rates["W"], (3.0 + 60.0 * 0.5) / 70.0);
        close(v.rates["SA"], (300.0 + 25.0 * 27.5) / 35.0);
        close(vals["wpg"].rates["W"], (7.0 + 60.0 * 0.5) / 70.0);
        let sv = |r: &Rates| r["SV"] / (r["SV"] + r["GA"]);
        close(sv(&v.rates), sv(&b.rates));
        assert!(v.fpg < b.fpg);
    }

    #[test]
    fn gsax_replaces_the_raw_save_component() {
        let (good, bad, plain) =
            (ggoalie("good", Some(1), 60, 0.905, "prior", None), ggoalie("bad", Some(2), 60, 0.905, "prior", None),
             ggoalie("plain", Some(3), 60, 0.905, "prior", None));
        let fillers: Vec<Player> =
            (0..3).map(|i| ggoalie(&format!("f{i}"), Some(10 + i), 60, 0.905, "prior", None)).collect();
        let mut ps = vec![&good, &bad, &plain];
        ps.extend(fillers.iter());
        let mut c = gctx(&ps, &[est(1, 0.6, 4000.0), est(2, -0.6, 4000.0)]);
        let vals = val(&mut c, &goalie_w());
        let (g, b, p) = (&vals["good"], &vals["bad"], &vals["plain"]);
        assert!(has(g, "GSAX") && has(b, "GSAX") && !has(p, "GSAX"));
        assert!(g.rates["GA"] < p.rates["GA"] && p.rates["GA"] < b.rates["GA"]);
        assert!(g.fpg > p.fpg && p.fpg > b.fpg);
        let r = g.reason("GSAX").unwrap();
        close(r.value.unwrap(), 0.006);
        assert!((r.baseline.unwrap() - 0.905).abs() < 1e-3);
        assert!(r.text.contains("MoneyPuck.com") && r.text.contains("+0.60 per 100 shots"), "{}", r.text);
        close(g.rates["SVPCT"], 0.911);
        close(g.rates["SV"] + g.rates["GA"], 30.0);
        assert_eq!(g.rates["W"], p.rates["W"]);
    }

    #[test]
    fn gsax_does_not_double_count_a_hot_raw_save_pct() {
        let hot = ggoalie("hot", Some(1), 60, 0.940, "prior", None);
        let fillers: Vec<Player> =
            (0..4).map(|i| ggoalie(&format!("f{i}"), Some(10 + i), 60, 0.900, "prior", None)).collect();
        let mut ps = vec![&hot];
        ps.extend(fillers.iter());
        let mut c = gctx(&ps, &[est(1, 0.0, 4000.0)]);
        let v = &val(&mut c, &goalie_w())["hot"];
        let mean_sv = (0.940 + 4.0 * 0.900) / 5.0;
        close(v.rates["SVPCT"], mean_sv);
        assert!(v.reason("GSAX").unwrap().baseline.unwrap() > v.rates["SVPCT"]);
    }

    #[test]
    fn gsax_needs_enough_shots_and_applies_preseason_projection_value() {
        let small = ggoalie("small", Some(5), 60, 0.905, "prior", None);
        let mut c = gctx(&[&small], &[est(5, 1.0, 300.0)]);
        assert!(!has(&val(&mut c, &goalie_w())["small"], "GSAX"));
        let proj: StatLine = serde_json::from_value(line("projected", 60, json!({"GP": 60, "GS": 58, "SA": 1700,
            "SV": 1550, "GA": 150, "W": 32, "SO": 3, "FPTS": 400}))).unwrap();
        let mut g = ggoalie("g", Some(6), 60, 0.905, "prior", None);
        g.lines.insert("projected".into(), proj);
        let base = val(&mut gctx(&[&g], &[]), &goalie_w())["g"].clone();
        let up = val(&mut gctx(&[&g], &[est(6, 0.6, 4000.0)]), &goalie_w())["g"].clone();
        assert!(has(&up, "GSAX"));
        let shift = up.fpg - base.fpg;
        assert!(shift > 0.0);
        close(up.fpg_season, base.fpg_season + shift * base.start_share.unwrap());
    }

    #[test]
    fn ledger_starts_blend_into_a_projected_share() {
        let proj: StatLine = serde_json::from_value(line("projected", 60, json!({"GP": 60, "GS": 60, "SA": 1700,
            "SV": 1550, "GA": 150, "W": 32}))).unwrap();
        let mut g = ggoalie("g", None, 8, 0.905, "season", Some(8.0));
        g.lines.insert("projected".into(), proj);
        let mut c = gctx(&[&g], &[]);
        let plain = val(&mut c, &goalie_w())["g"].clone();
        assert_eq!(plain.share_source.as_deref(), Some("projection"));
        assert!(!has(&plain, "START_ACTUAL"));
        c.goalie_actual_starts.insert("g".into(), (2, 10));
        let v = val(&mut c, &goalie_w())["g"].clone();
        let expect = (2.0 + 10.0 * (60.0 / 82.0)) / (10.0 + 10.0);
        close(v.start_share.unwrap(), expect);
        assert_eq!(v.share_source.as_deref(), Some("actual"));
        assert!(has(&v, "START_ACTUAL") && !has(&v, "START_SHARE"));
        assert!(v.start_share < plain.start_share);
        close(v.fpg_season, v.fpg * expect);
        let (s, ev) = goalie_base_share(&g, &c, &team_games_played(&c), 82.0);
        close(s, expect);
        assert!(ev);
        c.goalie_actual_starts.insert("g".into(), (2, 4));
        assert!(!has(&val(&mut c, &goalie_w())["g"], "START_ACTUAL"));
    }

    #[test]
    fn ledger_starts_scale_a_preseason_projection_value() {
        let g = player(json!({"cid": "g", "team": "EDM", "positions": ["G"],
            "lines": {"projected": line("projected", 60, json!({"GP": 60, "GS": 41, "W": 25, "FPTS": 328}))}}));
        let mut c = gctx(&[&g], &[]);
        c.goalie_actual_starts.insert("g".into(), (0, 6));
        let v = &val(&mut c, &goalie_w())["g"];
        let expect = (0.0 + 10.0 * 0.5) / (6.0 + 10.0);
        close(v.start_share.unwrap(), expect);
        close(v.fpg_season, 328.0 / 82.0 * expect / 0.5);
    }

    #[test]
    fn warn_signal_once_per_signal() {
        let mut w: Vec<String> = vec![];
        warn_signal(&mut w, "goalie GSAx", Some(("RuntimeError", "kaput")));
        warn_signal(&mut w, "goalie GSAx", Some(("RuntimeError", "again")));
        warn_signal(&mut w, "preseason lines", None);
        assert_eq!(
            w,
            vec![
                "Valuation: goalie GSAx unavailable (RuntimeError: kaput); values computed without it".to_string(),
                "Valuation: preseason lines unavailable; values computed without it".to_string(),
            ]
        );
    }

    #[test]
    fn season_helpers() {
        assert_eq!(season_start_year(d(2026, 9, 1)), 2026);
        assert_eq!(season_start_year(d(2027, 4, 1)), 2026);
        assert_eq!(current_season(d(2026, 11, 4)), 20262027);
        assert_eq!(season_span(&[2024, 2025]), "2024-25..2025-26");
        assert_eq!(season_span(&[2025]), "2025-26");
        assert_eq!(season_span(&[]), "");
    }

    #[test]
    fn projection_error_is_the_groups_backtest_error_in_league_points() {
        // the packaged fit: F 0.23, D 0.20981, G 0.57728 ESPN points per game
        let fit = params::load_packaged(None);
        assert_eq!(
            ["F", "D", "G"].map(|g| params::projection_mae(g, Some(&fit))),
            [0.23, 0.20981, 0.57728]
        );
        let scale = PointScale { skater: 2.0, goalie: 3.0 };
        let mae = |g| params::projection_mae(g, None);
        close(projection_error(&dman("d", 30, 0.5), &scale), 2.0 * mae("D"));
        close(projection_error(&goalie("g", 30, 0.905, 0.5, "prior", None), &scale), 3.0 * mae("G"));
        assert!(projection_error(&goalie("g", 30, 0.905, 0.5, "prior", None), &PointScale::REFERENCE) > mae("F"));
    }
}
