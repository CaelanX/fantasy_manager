//! Rust port of `fantasy_manager/recommend/flags.py`: sell-high / buy-low flags (recent form vs a
//! shrunk season baseline, luck from shooting% / SV% or MoneyPuck xG), role-change alerts (NHL
//! TOI / PP-share deployment trends; `providers.deployment_enrich.role_change_alerts` lives here as
//! [`role_change_alerts`]), preseason-standout alerts and rookie role-signal alerts.
//!
//! Owner: work package `W7-injuries-flags-alerts-matchup`.
//!
//! Deviations from Python (behaviour-preserving):
//! * `details` (deployment summaries) is `Map<cid or NHL id as string, Map<String, Value>>`; Python
//!   keys it by cid or int NHL id.
//! * The registries (`preseason_enrich.preseason_lines`, `rookie_enrich.all_signals`) are read from
//!   `ctx.enrich`; `lines` / `signals` overrides keep the Python keyword arguments.
//! * The `try: ... except Exception: pass` around the preseason / rookie alerts has nothing to
//!   catch in Rust (pure lookups).

use std::collections::HashMap;

use fm_core::models::{GainUnits, LeagueContext, Player, Reason, RecKind, Recommendation};
use fm_core::snapshot::{PreseasonLine, RoleSignal};
use fm_core::unproven::is_unproven;
use fm_core::values::PlayerValue;
use fm_core::{Cid, Map};
use fm_pycompat::fmt::{fixed, percent, repr_f64};
use fm_pycompat::num::round_to;
use fm_pycompat::order::{cmp_f64, sort_desc_by};
use serde_json::Value;

use crate::recommend::strength::{apply_ranks, apply_strength, interp};
use crate::scoring::{fit_to_context, from_config, Scoring, ScoringLike};
use crate::valuation::blend::shrunk_rates;
use crate::valuation::regression::luck_signals;
use crate::valuation::rookie::{usable_news, Signal};
use crate::valuation::valuate::preseason_lookup;

pub const MIN_GP_L15: i64 = 5;
pub const SELL_FORM: f64 = 1.35;
pub const BUY_FORM: f64 = 0.7;
pub const SH_SPIKE: f64 = 1.6;
pub const SV_SPIKE: f64 = 0.015;
pub const SOG_STABLE: f64 = 0.9;
pub const MIN_BASE_SOG: f64 = 20.0;
pub const MIN_BASE_FPG: f64 = 0.05;
pub const HEALTHY: [&str; 2] = ["healthy", "unknown"];
/// goals - ixG per game that counts as luck (sell-high threshold)
pub const XG_LUCK_GPG: f64 = 0.06;
/// luck multiplier = 1 + 10 x |goals - ixG| per game (0.06 -> 1.6, like SH_SPIKE)
pub const XG_LUCK_SCALE: f64 = 10.0;
pub const XG_CREDIT: &str = "MoneyPuck.com";
/// predicted_gain of a flag is graded over the next 28 days
pub const FLAG_HORIZON_DAYS: i64 = 28;

type Stats = Map<String, f64>;

fn baseline_line(p: &Player) -> Option<&fm_core::models::StatLine> {
    if let Some(proj) = p.lines.get("projected").filter(|l| l.gp > 0) {
        return Some(proj);
    }
    p.lines.get("prior").filter(|l| l.gp > 0)
}

/// (G/SOG, SOG) pooled over the splits that carry shots.
fn shooting(p: &Player, splits: &[&str]) -> (Option<f64>, f64) {
    let (mut g, mut sog) = (0.0, 0.0);
    for s in splits {
        if let Some(ln) = p.lines.get(*s) {
            let shots = ln.stats.get("SOG").copied().unwrap_or(0.0);
            if shots != 0.0 {
                g += ln.stats.get("G").copied().unwrap_or(0.0);
                sog += shots;
            }
        }
    }
    (if sog > 0.0 { Some(g / sog) } else { None }, sog)
}

fn baseline_shooting(p: &Player) -> (Option<f64>, f64) {
    let (pct, sog) = shooting(p, &["season", "prior"]);
    if pct.is_none() || sog < MIN_BASE_SOG {
        let (pct2, sog2) = shooting(p, &["projected"]);
        if pct2.is_some() && sog2 >= sog {
            return (pct2, sog2);
        }
    }
    (pct, sog)
}

fn svpct(stats: &Stats) -> Option<f64> {
    if let Some(v) = stats.get("SVPCT") {
        return Some(if *v > 1.0 { v / 100.0 } else { *v });
    }
    let sa = stats.get("SA").copied().filter(|s| *s != 0.0)?;
    let sv = stats.get("SV").copied().unwrap_or_else(|| sa - stats.get("GA").copied().unwrap_or(0.0));
    Some(sv / sa)
}

/// (form_ratio, fpg_l15, fpg_base, shrunk season rates) or None when not measurable.
fn form(p: &Player, scoring: &Scoring) -> Option<(f64, f64, f64, Stats)> {
    let l15 = p.lines.get("last15").filter(|l| l.gp >= MIN_GP_L15)?;
    let rates = shrunk_rates(p.lines.get("season"), baseline_line(p), p.is_goalie(), None);
    if rates.is_empty() {
        return None;
    }
    let base = scoring.value(&rates);
    if base <= MIN_BASE_FPG {
        return None;
    }
    let fpg15 = scoring.value(&l15.per_game());
    Some((fpg15 / base, fpg15, base, rates))
}

/// (goals - ixG per game, XG_LUCK reason) from MoneyPuck fields, None without them.
pub fn xg_luck(p: &Player, scoring: Option<&fm_core::models::ScoringConfig>) -> Option<(f64, Reason)> {
    let sig = luck_signals(p, None, scoring.map(|s| s as &dyn ScoringLike), None, None)?;
    if sig.gp <= 0 {
        return None;
    }
    let gpg = sig.goals_minus_ixg / sig.gp as f64;
    let unit = if scoring.is_some_and(|s| s.kind.as_str() == "points") { "FPG" } else { "goals/GP" };
    let prior = if sig.from_prior { "last season, " } else { "" };
    let text = format!(
        "Goals {}/GP vs ixG {}/GP over {} GP ({prior}{} goals vs expected): about {} {unit} of finishing luck ({XG_CREDIT})",
        fixed(sig.goals_per_game, 2, false),
        fixed(sig.ixg_per_game, 2, false),
        sig.gp,
        fixed(sig.goals_minus_ixg, 1, true),
        fixed(sig.goal_luck_fpg, 2, true),
    );
    Some((gpg, Reason { code: "XG_LUCK".into(), text, value: Some(round_to(sig.goal_luck_fpg, 4)), baseline: Some(0.0) }))
}

fn pv_weight(values: &Map<Cid, PlayerValue>, p: &Player) -> f64 {
    values.get(&p.cid).map(|pv| pv.vorp).unwrap_or(0.0).max(0.0) + 0.5
}

fn form_reason(fpg15: f64, base: f64, ratio: f64, baseline: f64) -> Reason {
    Reason {
        code: "FORM_RATIO".into(),
        text: format!(
            "L15 {} FPG vs {} shrunk season baseline ({}x)",
            fixed(fpg15, 2, false),
            fixed(base, 2, false),
            fixed(ratio, 2, false)
        ),
        value: Some(ratio),
        baseline: Some(baseline),
    }
}

fn gp_reason(gp: i64) -> Reason {
    Reason {
        code: "GP_L15".into(),
        text: format!("{gp} GP in the last 15 days"),
        value: Some(gp as f64),
        baseline: Some(MIN_GP_L15 as f64),
    }
}

/// Sell-high (my players) and buy-low (other teams' players) flags, best first. `scoring`
/// defaults to the league's scoring fitted on the context.
pub fn recommend_flags(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    scoring: Option<&Scoring>,
    limit: Option<usize>,
) -> Result<Vec<Recommendation>, fm_core::CoreError> {
    let owned;
    let scoring = match scoring {
        Some(s) => s,
        None => {
            let mut s = from_config(&ctx.scoring);
            fit_to_context(&mut s, ctx);
            owned = s;
            &owned
        }
    };
    let me = ctx.my_team()?;
    let mut recs: Vec<Recommendation> = Vec::new();

    for p in me.players_iter() {
        let Some((ratio, fpg15, base, _)) = form(p, scoring) else { continue };
        if ratio <= SELL_FORM {
            continue;
        }
        let l15 = &p.lines["last15"];
        let mut reasons = vec![form_reason(fpg15, base, ratio, SELL_FORM), gp_reason(l15.gp)];
        let luck;
        if p.is_goalie() {
            let cur = svpct(&l15.stats);
            let bl = baseline_line(p);
            let mut rf = p.lines.get("season").and_then(|l| svpct(&l.stats));
            if rf.is_none() {
                rf = bl.and_then(|l| svpct(&l.stats));
            }
            let (Some(cur), Some(rf)) = (cur, rf) else { continue };
            if cur - rf < SV_SPIKE {
                continue;
            }
            reasons.push(Reason {
                code: "SAVE_PCT".into(),
                text: format!("L15 SV% {} vs baseline {}", fixed(cur, 3, false), fixed(rf, 3, false)),
                value: Some(cur),
                baseline: Some(rf),
            });
            luck = 1.0 + (cur - rf) * 20.0;
        } else if let Some((gpg, reason)) = xg_luck(p, Some(&ctx.scoring)) {
            // expected goals replace the shooting% test
            if gpg < XG_LUCK_GPG {
                continue;
            }
            reasons.push(reason);
            luck = 1.0 + XG_LUCK_SCALE * gpg;
        } else {
            let (cur, _) = shooting(p, &["last15"]);
            let (rf, _) = baseline_shooting(p);
            let (Some(cur), Some(rf)) = (cur, rf.filter(|r| *r != 0.0)) else { continue };
            if cur / rf <= SH_SPIKE {
                continue;
            }
            reasons.push(Reason {
                code: "SHOOTING_PCT".into(),
                text: format!(
                    "L15 shooting {} vs baseline {} ({}x)",
                    percent(cur, 1, false),
                    percent(rf, 1, false),
                    fixed(cur / rf, 1, false)
                ),
                value: Some(cur),
                baseline: Some(rf),
            });
            luck = cur / rf;
        }
        if let Some(pv) = values.get(&p.cid) {
            reasons.push(Reason {
                code: "VORP".into(),
                text: format!("Season VORP {}", fixed(pv.vorp, 2, true)),
                value: Some(pv.vorp),
                baseline: None,
            });
        }
        let mut r = Recommendation::new(
            RecKind::SellHigh,
            (ratio - 1.0) * luck.min(3.0) * pv_weight(values, p),
            format!("Sell high on {}", p.name),
        );
        r.drop = vec![p.clone()];
        r.predicted_gain = Some(round_to(base - fpg15, 4));
        r.gain_units = Some(GainUnits::SeasonFpg);
        r.horizon_days = Some(FLAG_HORIZON_DAYS);
        r.reasons = reasons;
        recs.push(r);
    }

    for t in &ctx.teams {
        if t.team_id == me.team_id {
            continue;
        }
        for p in t.players_iter() {
            if p.is_goalie() || !p.status.is_in(&HEALTHY) {
                continue;
            }
            if values.get(&p.cid).is_some_and(|pv| pv.vorp <= 0.0) {
                continue;
            }
            let Some((ratio, fpg15, base, rates)) = form(p, scoring) else { continue };
            if ratio >= BUY_FORM {
                continue;
            }
            let l15 = &p.lines["last15"];
            let sog15 = l15.stats.get("SOG").copied().unwrap_or(0.0) / l15.gp as f64;
            let Some(sog_base) = rates.get("SOG").copied().filter(|s| *s != 0.0) else { continue };
            if sog15 < SOG_STABLE * sog_base {
                continue;
            }
            let mut reasons = vec![
                form_reason(fpg15, base, ratio, BUY_FORM),
                Reason {
                    code: "SOG_RATE".into(),
                    text: format!("Still {} SOG/GP vs {} baseline", fixed(sog15, 2, false), fixed(sog_base, 2, false)),
                    value: Some(sog15),
                    baseline: Some(sog_base),
                },
                gp_reason(l15.gp),
            ];
            let mut bonus = 1.0;
            if let Some((gpg, reason)) = xg_luck(p, Some(&ctx.scoring)) {
                if gpg >= XG_LUCK_GPG {
                    continue; // cold overall but still finishing above ixG
                }
                reasons.push(reason);
                bonus = 1.0 + 2.0f64.min(XG_LUCK_SCALE * (-gpg).max(0.0));
            } else {
                let (cur, _) = shooting(p, &["last15"]);
                let (rf, _) = baseline_shooting(p);
                if let (Some(cur), Some(rf)) = (cur, rf.filter(|r| *r != 0.0)) {
                    reasons.push(Reason {
                        code: "SHOOTING_PCT".into(),
                        text: format!("L15 shooting {} vs baseline {}", percent(cur, 1, false), percent(rf, 1, false)),
                        value: Some(cur),
                        baseline: Some(rf),
                    });
                }
            }
            let mut r = Recommendation::new(
                RecKind::BuyLow,
                (1.0 - ratio) * bonus * pv_weight(values, p),
                format!("Buy low on {} ({})", p.name, t.name),
            );
            r.add = vec![p.clone()];
            r.counterparty = Some(t.name.clone());
            r.predicted_gain = Some(round_to(base - fpg15, 4));
            r.gain_units = Some(GainUnits::SeasonFpg);
            r.horizon_days = Some(FLAG_HORIZON_DAYS);
            r.reasons = reasons;
            recs.push(r);
        }
    }

    sort_desc_by(&mut recs, |a, b| cmp_f64(a.score, b.score));
    if let Some(n) = limit.filter(|n| *n > 0) {
        recs.truncate(n);
    }
    apply_strength(&mut recs);
    apply_ranks(&mut recs);
    Ok(recs)
}

// --------------------------------------------------------------------------- role-change alerts
//
// Deployment changes from the NHL per-game TOI / PP reports (Player.toi_trend / pp_share_trend =
// last-N GP minus the baseline). See the Python module for the thresholds and the 0-10 strength.

pub const ROLE_TOI_DELTA: f64 = 1.5;
pub const ROLE_PP_DELTA: f64 = 0.15;
pub const PP1_FROM: f64 = 0.2;
pub const PP1_TO: f64 = 0.5;
pub const ROLE_TOI_SCALE: [(f64, f64); 4] = [(0.0, 0.0), (1.5, 4.0), (3.0, 7.0), (5.0, 10.0)];
pub const ROLE_PP_SCALE: [(f64, f64); 4] = [(0.0, 0.0), (0.15, 4.0), (0.3, 7.0), (0.5, 10.0)];
pub const PP1_FLOOR: f64 = 7.0;
pub const BOTH_BONUS: f64 = 1.0;
pub const FA_LABEL: &str = "FA";

/// Deployment summaries keyed by cid (or NHL id as a string).
pub type Details = Map<String, Map<String, Value>>;

/// Recent / baseline / trend for TOI and PP share plus the thresholds crossed (`role_change`).
#[derive(Clone, Debug, PartialEq)]
pub struct RoleChange {
    pub toi: Option<f64>,
    pub toi_base: Option<f64>,
    pub toi_trend: Option<f64>,
    pub pp: Option<f64>,
    pub pp_base: Option<f64>,
    pub pp_trend: Option<f64>,
    /// `last_n` of the summary (JSON value as given)
    pub n: Option<Value>,
    pub toi_hit: bool,
    pub pp_hit: bool,
    pub pp1: bool,
    pub strength: f64,
}

type Numbers = (Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<Value>);

fn num(d: &Map<String, Value>, k: &str) -> Option<f64> {
    d.get(k).and_then(|v| v.as_f64())
}

/// `_role_numbers`: with a deployment summary the exact numbers; otherwise baseline = the season
/// value on the Player and recent = baseline + trend.
fn role_numbers(p: &Player, d: Option<&Map<String, Value>>) -> Numbers {
    if let Some(d) = d.filter(|d| !d.is_empty()) {
        return (
            num(d, "toi_avg"),
            num(d, "baseline_toi"),
            num(d, "toi_trend"),
            num(d, "pp_share_avg"),
            num(d, "baseline_pp_share"),
            num(d, "pp_share_trend"),
            d.get("last_n").filter(|v| !v.is_null()).cloned(),
        );
    }
    let (tt, pt) = (p.toi_trend, p.pp_share_trend);
    (
        match (tt, p.toi_per_game) {
            (Some(t), Some(b)) => Some(b + t),
            _ => None,
        },
        p.toi_per_game,
        tt,
        match (pt, p.pp_share) {
            (Some(t), Some(b)) => Some(b + t),
            _ => None,
        },
        p.pp_share,
        pt,
        None,
    )
}

/// Which role thresholds `p` crossed in direction `sign` (+1 up, -1 down), with the numbers and
/// the 0-10 strength; None when none did.
pub fn role_change(p: &Player, d: Option<&Map<String, Value>>, sign: i32) -> Option<RoleChange> {
    let (toi, toi_base, tt, pp, pp_base, pt, n) = role_numbers(p, d);
    let sg = sign as f64;
    let toi_hit = tt.is_some_and(|t| sg * t >= ROLE_TOI_DELTA);
    let pp_hit = pt.is_some_and(|t| sg * t >= ROLE_PP_DELTA);
    let mut pp1 = false;
    if let (Some(now), Some(base)) = (pp, pp_base) {
        pp1 = if sign > 0 { base < PP1_FROM && now >= PP1_TO } else { base >= PP1_TO && now < PP1_FROM };
    }
    if !(toi_hit || pp_hit || pp1) {
        return None;
    }
    let mut s: f64 = 0.0;
    if toi_hit {
        s = s.max(interp(tt.unwrap_or(0.0).abs(), &ROLE_TOI_SCALE));
    }
    if pp_hit {
        s = s.max(interp(pt.unwrap_or(0.0).abs(), &ROLE_PP_SCALE));
    }
    if pp1 {
        s = s.max(PP1_FLOOR);
    }
    if toi_hit && (pp_hit || pp1) {
        s += BOTH_BONUS;
    }
    Some(RoleChange {
        toi,
        toi_base,
        toi_trend: tt,
        pp,
        pp_base,
        pp_trend: pt,
        n,
        toi_hit,
        pp_hit,
        pp1,
        strength: round_to(s.min(10.0), 2),
    })
}

/// Python `str(v)` / f-string of a JSON scalar (ints print as ints, floats with repr).
fn value_text(v: &Value) -> String {
    match v {
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.to_string(),
            None => repr_f64(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        other => other.to_string(),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn role_reasons(c: &RoleChange) -> Vec<Reason> {
    let window = match c.n.as_ref().filter(|v| truthy(v)) {
        Some(n) => format!("last {} GP", value_text(n)),
        None => "recent games".to_string(),
    };
    let mut out = Vec::new();
    if c.toi_hit || c.toi_trend.is_some_and(|t| t.abs() >= ROLE_TOI_DELTA) {
        let base = c.toi_base.map(|b| format!(" vs {} baseline", fixed(b, 1, false))).unwrap_or_default();
        let now = c.toi.map(|t| format!("{} min", fixed(t, 1, false))).unwrap_or_else(|| "TOI".into());
        out.push(Reason {
            code: "ROLE_TOI".into(),
            text: format!(
                "TOI {now} per game over the {window}{base} ({} min)",
                fixed(c.toi_trend.unwrap_or(0.0), 1, true)
            ),
            value: c.toi,
            baseline: c.toi_base,
        });
    }
    if c.pp_hit || c.pp1 {
        let base = c.pp_base.map(|b| format!(" vs {} baseline", percent(b, 0, false))).unwrap_or_default();
        let now = c.pp.map(|x| percent(x, 0, false)).unwrap_or_else(|| "?".into());
        let trend = c.pp_trend.map(|t| format!(" ({} pts)", fixed(t * 100.0, 0, true))).unwrap_or_default();
        let tag = if c.pp1 {
            if c.pp.unwrap_or(0.0) >= PP1_TO {
                " - PP1 usage"
            } else {
                " - off PP1"
            }
        } else {
            ""
        };
        out.push(Reason {
            code: "ROLE_PP".into(),
            text: format!("PP share {now} of team PP time over the {window}{base}{trend}{tag}"),
            value: c.pp,
            baseline: c.pp_base,
        });
    }
    out
}

/// cid -> (fantasy team name, mine) for rostered players (first team wins).
fn owners(ctx: &LeagueContext) -> HashMap<&str, (&str, bool)> {
    let me = ctx.my_team().ok().map(|t| t.team_id.as_str());
    let mut owner: HashMap<&str, (&str, bool)> = HashMap::new();
    for t in &ctx.teams {
        for p in t.players_iter() {
            owner.entry(p.cid.as_str()).or_insert((t.name.as_str(), me == Some(t.team_id.as_str())));
        }
    }
    owner
}

fn where_text(team_name: &str, mine: bool) -> String {
    if team_name == FA_LABEL {
        "free agent".into()
    } else if mine {
        "my team".into()
    } else {
        team_name.to_string()
    }
}

fn sort_alerts(recs: &mut [Recommendation]) {
    recs.sort_by(|a, b| cmp_f64(-a.strength.unwrap_or(0.0), -b.strength.unwrap_or(0.0)).then_with(|| a.title.cmp(&b.title)));
}

/// Role-change alerts for every skater with deployment trends: my players (up and down), other
/// teams' players and free agents (up only), plus the preseason-standout and rookie role-signal
/// alerts. `details` defaults to `ctx.deployment_details`.
pub fn recommend_role_alerts(ctx: &LeagueContext, details: Option<&Details>, limit: Option<usize>) -> Vec<Recommendation> {
    let details: Option<&Details> = details.or(if ctx.deployment_details.is_empty() { None } else { Some(&ctx.deployment_details) });
    let details = details.filter(|d| !d.is_empty());
    let owner = owners(ctx);
    let mut recs: Vec<Recommendation> = Vec::new();
    for p in ctx.all_players() {
        let nhl_key = p.nhl_id().map(|i| i.to_string());
        let in_details =
            details.is_some_and(|d| d.contains_key(&p.cid) || nhl_key.as_ref().is_some_and(|k| d.contains_key(k)));
        if p.is_goalie() || (p.toi_trend.is_none() && p.pp_share_trend.is_none() && !in_details) {
            continue;
        }
        let d: Option<&Map<String, Value>> = details.and_then(|dm| {
            dm.get(&p.cid).filter(|x| !x.is_empty()).or_else(|| nhl_key.as_ref().and_then(|k| dm.get(k)))
        });
        let (team_name, mine) = owner.get(p.cid.as_str()).copied().unwrap_or((FA_LABEL, false));
        let mut sign = 1;
        let mut c = role_change(p, d, 1);
        if c.is_none() && mine {
            sign = -1;
            c = role_change(p, d, -1);
        }
        let Some(c) = c else { continue };
        let mut what: Vec<String> = Vec::new();
        if c.toi_hit {
            what.push(format!("TOI {}", fixed(c.toi_trend.unwrap_or(0.0), 1, true)));
        }
        if c.pp1 {
            what.push(if sign > 0 { "PP1".into() } else { "off PP1".into() });
        } else if c.pp_hit {
            what.push(format!("PP share {} pts", fixed(c.pp_trend.unwrap_or(0.0) * 100.0, 0, true)));
        }
        let title = format!(
            "{}: {} ({}; {})",
            if sign > 0 { "Role up" } else { "Role loss" },
            p.name,
            what.join(", "),
            where_text(team_name, mine)
        );
        let mut r = Recommendation::new(RecKind::Alert, c.strength, title);
        r.subjects = vec![p.clone()];
        r.counterparty = Some(team_name.to_string());
        r.reasons = role_reasons(&c);
        r.strength = Some(c.strength);
        recs.push(r);
    }
    // preseason standouts and rookie news-role signals ride along
    recs.extend(recommend_preseason_alerts(ctx, None, None));
    recs.extend(recommend_rookie_role_alerts(ctx, None, None));
    sort_alerts(&mut recs);
    if let Some(n) = limit.filter(|n| *n > 0) {
        recs.truncate(n);
    }
    apply_ranks(&mut recs);
    recs
}

/// `providers.deployment_enrich.role_change_alerts`: see [`recommend_role_alerts`].
pub fn role_change_alerts(ctx: &LeagueContext, details: Option<&Details>) -> Vec<Recommendation> {
    recommend_role_alerts(ctx, details, None)
}

// --------------------------------------------------------------------------- preseason standouts

pub const PRESEASON_MIN_GP: i64 = 3;
pub const PRESEASON_PTS_PG: f64 = 1.0;
pub const PRESEASON_TOI: f64 = 16.0;
pub const PRESEASON_MAX_SEASON_GP: i64 = 5;
pub const PRESEASON_STRENGTH: (f64, f64) = (4.0, 6.0);

fn ramp(x: f64, x0: f64, x1: f64, y0: f64, y1: f64) -> f64 {
    y0 + (y1 - y0) * 1.0f64.min(0.0f64.max((x - x0) / (x1 - x0)))
}

/// Thresholds and 4-6 strength of a preseason line.
#[derive(Clone, Debug, PartialEq)]
pub struct Standout {
    pub pts_pg: f64,
    pub toi: Option<f64>,
    pub pts_hit: bool,
    pub toi_hit: bool,
    pub strength: f64,
}

/// None when the line is not a standout.
pub fn preseason_standout(line: Option<&PreseasonLine>) -> Option<Standout> {
    let line = line.filter(|l| l.gp >= PRESEASON_MIN_GP)?;
    let (ppg, toi) = (line.pts_per_game(), line.toi_per_game());
    let pts_hit = ppg >= PRESEASON_PTS_PG;
    let toi_hit = toi.is_some_and(|t| t >= PRESEASON_TOI);
    if !(pts_hit || toi_hit) {
        return None;
    }
    let mut s: f64 = 0.0;
    if pts_hit {
        s = s.max(ramp(ppg, PRESEASON_PTS_PG, 2.0, 4.5, 6.0));
    }
    if toi_hit {
        s = s.max(ramp(toi.unwrap_or(0.0), PRESEASON_TOI, 20.0, 4.0, 5.0));
    }
    if pts_hit && toi_hit {
        s += 0.5;
    }
    let (lo, hi) = PRESEASON_STRENGTH;
    Some(Standout { pts_pg: ppg, toi, pts_hit, toi_hit, strength: round_to(hi.min(lo.max(s)), 2) })
}

fn pp_label(p: &Player) -> String {
    match p.pp_unit.as_deref().filter(|s| !s.is_empty()) {
        Some(u) => u.to_uppercase(),
        None => {
            if p.line.as_deref().is_some_and(|l| !l.is_empty()) {
                "no PP unit".into()
            } else {
                "PP unit unknown".into()
            }
        }
    }
}

fn career_text(p: &Player) -> String {
    match p.career_gp {
        Some(g) => format!("{g} career NHL GP"),
        None => "no NHL season with 20+ GP".into(),
    }
}

/// "Preseason standout" alerts. `lines` = {cid: PreseasonLine} overrides the registry.
pub fn recommend_preseason_alerts(
    ctx: &LeagueContext,
    lines: Option<&Map<String, PreseasonLine>>,
    limit: Option<usize>,
) -> Vec<Recommendation> {
    let reg;
    let lookup = |cid: &str| -> Option<&PreseasonLine> {
        match lines {
            Some(l) => l.get(cid),
            None => None,
        }
    };
    let registry: Option<HashMap<String, &PreseasonLine>> = if lines.is_none() {
        reg = preseason_lookup(ctx);
        Some(reg)
    } else {
        None
    };
    let empty = match (&registry, lines) {
        (Some(r), _) => r.is_empty(),
        (None, Some(l)) => l.is_empty(),
        _ => true,
    };
    if empty {
        return vec![];
    }
    let owner = owners(ctx);
    let mut recs: Vec<Recommendation> = Vec::new();
    for p in ctx.all_players() {
        let line = match &registry {
            Some(r) => r.get(&p.cid).copied(),
            None => lookup(&p.cid),
        };
        let Some(line) = line else { continue };
        if p.is_goalie() || !is_unproven(p) || p.gp("season") >= PRESEASON_MAX_SEASON_GP {
            continue;
        }
        let Some(c) = preseason_standout(Some(line)) else { continue };
        let (team_name, mine) = owner.get(p.cid.as_str()).copied().unwrap_or((FA_LABEL, false));
        let toi = c.toi.map(|t| format!("TOI {}", fixed(t, 1, false))).unwrap_or_else(|| "TOI n/a".into());
        let title = format!(
            "Preseason standout: {} ({} GP, {} PTS/GP, {toi}, {}; {})",
            p.name,
            line.gp,
            fixed(c.pts_pg, 2, false),
            pp_label(p),
            where_text(team_name, mine)
        );
        let mut reasons = vec![Reason {
            code: "PRESEASON".into(),
            text: format!("Preseason {} ({} PTS/GP)", line.summary(), fixed(c.pts_pg, 2, false)),
            value: Some(round_to(c.pts_pg, 4)),
            baseline: Some(PRESEASON_PTS_PG),
        }];
        if let Some(t) = c.toi {
            reasons.push(Reason {
                code: "PRESEASON_TOI".into(),
                text: format!("{} min TOI per preseason game", fixed(t, 1, false)),
                value: Some(round_to(t, 2)),
                baseline: Some(PRESEASON_TOI),
            });
        }
        if let Some(u) = p.pp_unit.as_deref().filter(|s| !s.is_empty()) {
            reasons.push(Reason::new("PP_UNIT", format!("Daily Faceoff lists him on {}", u.to_uppercase())));
        }
        reasons.push(Reason {
            code: "UNPROVEN".into(),
            text: format!(
                "Unproven: {}; preseason is a weak signal (blended at most 25% into his value)",
                career_text(p)
            ),
            value: p.career_gp.map(|g| g as f64),
            baseline: None,
        });
        let mut r = Recommendation::new(RecKind::Alert, c.strength, title);
        r.subjects = vec![p.clone()];
        r.counterparty = Some(team_name.to_string());
        r.reasons = reasons;
        r.strength = Some(c.strength);
        recs.push(r);
    }
    sort_alerts(&mut recs);
    if let Some(n) = limit.filter(|n| *n > 0) {
        recs.truncate(n);
    }
    recs
}

// --------------------------------------------------------------------------- rookie role signals

pub const ROOKIE_ALERT_MAX_AGE_DAYS: i64 = 10;
pub const ROOKIE_ALERT_STRENGTH: (f64, f64) = (5.0, 7.0);
pub const ROOKIE_NEG_STRENGTH: f64 = 5.0;
pub const ROOKIE_KIND_BONUS: [(&str, f64); 3] = [("pp1", 1.0), ("top_line", 0.5), ("nhl_roster", 0.0)];
pub const ROOKIE_NEG_KINDS: [&str; 6] = ["ahl_demotion", "junior_return", "scratched", "top_line", "pp1", "nhl_roster"];

fn kind_bonus(kind: &str) -> Option<f64> {
    ROOKIE_KIND_BONUS.iter().find(|(k, _)| *k == kind).map(|(_, b)| *b)
}

/// 0-10 strength of a positive rookie role signal, None when the kind does not alert.
pub fn rookie_signal_strength(sig: &Signal) -> Option<f64> {
    if sig.direction <= 0 {
        return None;
    }
    let bonus = kind_bonus(&sig.kind)?;
    let (lo, hi) = ROOKIE_ALERT_STRENGTH;
    let s = lo + bonus + 2.5 * (sig.confidence - 0.6);
    Some(round_to(hi.min(lo.max(s)), 2))
}

/// "Rookie role signal" alerts. `signals` = {cid: [RoleSignal]} overrides the enrich registry.
pub fn recommend_rookie_role_alerts(
    ctx: &LeagueContext,
    signals: Option<&Map<String, Vec<RoleSignal>>>,
    limit: Option<usize>,
) -> Vec<Recommendation> {
    let signals = signals.unwrap_or(&ctx.enrich.rookie_signals);
    if signals.is_empty() {
        return vec![];
    }
    let owner = owners(ctx);
    let mut recs: Vec<Recommendation> = Vec::new();
    for p in ctx.all_players() {
        let Some(sigs) = signals.get(&p.cid).filter(|s| !s.is_empty()) else { continue };
        if p.is_goalie() || !is_unproven(p) {
            continue;
        }
        let usable: Vec<Signal> = usable_news(sigs, Some(ctx.as_of), ROOKIE_ALERT_MAX_AGE_DAYS)
            .into_iter()
            .filter(|d| kind_bonus(&d.kind).is_some() || ROOKIE_NEG_KINDS.contains(&d.kind.as_str()))
            .collect();
        if usable.is_empty() {
            continue;
        }
        let (team_name, mine) = owner.get(p.cid.as_str()).copied().unwrap_or((FA_LABEL, false));
        let pos: Vec<&Signal> = usable.iter().filter(|d| rookie_signal_strength(d).is_some()).collect();
        let neg: Vec<&Signal> =
            usable.iter().filter(|d| d.direction < 0 && ROOKIE_NEG_KINDS.contains(&d.kind.as_str())).collect();
        let latest = &usable[0]; // newest first
        let (sig, strength): (&Signal, f64) = if latest.direction > 0 && !pos.is_empty() {
            (pos[0], rookie_signal_strength(pos[0]).unwrap_or(0.0))
        } else if mine && !neg.is_empty() {
            (neg[0], ROOKIE_NEG_STRENGTH)
        } else if !pos.is_empty() {
            (pos[0], rookie_signal_strength(pos[0]).unwrap_or(0.0))
        } else {
            continue;
        };
        let when = match &sig.published {
            Some(p) => p.local.format("%Y-%m-%d").to_string(),
            None => "undated".to_string(),
        };
        let src = sig.source_or_news();
        let title = format!("Rookie role signal: {} \u{2014} {} ({src}, {when})", p.name, sig.quote);
        let where_ = where_text(team_name, mine);
        let kind = sig.kind.replace('_', " ");
        let origin = if sig.origin.is_empty() { "rules" } else { sig.origin.as_str() };
        let mut reasons = vec![
            Reason {
                code: "ROLE_NEWS".into(),
                text: format!(
                    "{kind} {} ({origin}, confidence {}; {where_}): \"{}\"",
                    if sig.direction > 0 { "+" } else { "-" },
                    fixed(sig.confidence, 2, false),
                    sig.quote
                ),
                value: Some(sig.direction as f64),
                baseline: Some(sig.confidence),
            },
            Reason {
                code: "UNPROVEN".into(),
                text: format!(
                    "Unproven: {}; news is the earliest role signal for a rookie (the valuation's rookie model uses it too)",
                    career_text(p)
                ),
                value: p.career_gp.map(|g| g as f64),
                baseline: None,
            },
        ];
        let dfo: Vec<String> = [p.line.as_deref(), p.pp_unit.as_deref()]
            .into_iter()
            .flatten()
            .filter(|x| !x.is_empty())
            .map(|x| x.to_uppercase())
            .collect();
        if !dfo.is_empty() {
            reasons.push(Reason::new("ROLE_DFO", format!("Daily Faceoff: {}", dfo.join(" / "))));
        }
        let mut r = Recommendation::new(RecKind::Alert, strength, title);
        r.subjects = vec![p.clone()];
        r.counterparty = Some(team_name.to_string());
        r.reasons = reasons;
        r.strength = Some(strength);
        recs.push(r);
    }
    sort_alerts(&mut recs);
    if let Some(n) = limit.filter(|n| *n > 0) {
        recs.truncate(n);
    }
    recs
}

#[cfg(test)]
mod tests {
    //! Port of tests/test_flags.py (intent) + role / preseason alert checks.
    use super::*;
    use serde_json::json;

    pub(crate) fn player(cid: &str, pos: &[&str], lines: Value) -> Player {
        serde_json::from_value(json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": "EDM",
            "positions": pos, "status": "healthy", "lines": lines}))
        .unwrap()
    }

    fn line(split: &str, gp: i64, g_pg: f64, a_pg: f64, sog_pg: f64) -> Value {
        let n = gp as f64;
        json!({"split": split, "gp": gp, "stats": {"G": g_pg * n, "A": a_pg * n, "SOG": sog_pg * n, "GP": n}})
    }

    /// Baseline 1.0 FPG (0.4 G + 0.6 A), 4 SOG/GP -> 10% shooting in season and projection.
    fn skater(cid: &str, l15: Option<(i64, f64, f64, f64)>, status: &str, season_gp: i64) -> Player {
        let mut lines = json!({"season": line("season", season_gp, 0.4, 0.6, 4.0),
                               "projected": line("projected", 82, 0.4, 0.6, 4.0)});
        if let Some((gp, g, a, s)) = l15 {
            lines["last15"] = line("last15", gp, g, a, s);
        }
        let mut p = player(cid, &["C"], lines);
        p.status = fm_core::models::Status::parse(status).unwrap();
        p
    }

    fn val(p: &Player, vorp: f64) -> PlayerValue {
        serde_json::from_value(json!({"player": p, "fpg": 1.0, "fpg_season": 1.0, "fpg_week": 1.0, "vorp": vorp,
            "vorp_week": vorp, "horizon_values": {"season": 1.0, "week": 1.0, "vorp_season": vorp, "vorp_week": vorp}}))
        .unwrap()
    }

    const HOT_LUCKY: (i64, f64, f64, f64) = (6, 0.6, 0.9, 3.0);
    const HOT_REAL: (i64, f64, f64, f64) = (6, 0.6, 0.9, 6.0);
    const COLD_STABLE: (i64, f64, f64, f64) = (6, 0.1, 0.4, 4.0);
    const COLD_SHOTS_DOWN: (i64, f64, f64, f64) = (6, 0.1, 0.4, 2.0);

    fn build(mine: Vec<Player>, theirs: Vec<Player>, fas: Vec<Player>) -> (LeagueContext, Map<Cid, PlayerValue>) {
        let slots = |ps: Vec<Player>| -> Vec<Value> {
            ps.into_iter().map(|p| json!({"slot": "C", "player": p, "starting": true})).collect()
        };
        let ctx: LeagueContext = serde_json::from_value(json!({"provider": "test", "league_id": "1", "season": 2027,
            "name": "T", "scoring": {"kind": "points", "weights": {"G": 1.0, "A": 1.0}}, "roster_shape": {"C": 4, "BN": 2},
            "teams": [{"team_id": "1", "name": "Mine", "owner_is_me": true, "slots": slots(mine)},
                      {"team_id": "2", "name": "Rivals", "owner_is_me": false, "slots": slots(theirs)}],
            "free_agents": fas, "matchup_period": 1, "as_of": "2026-11-01"}))
        .unwrap();
        let values = ctx.all_players().into_iter().map(|p| (p.cid.clone(), val(p, 1.0))).collect();
        (ctx, values)
    }

    fn reason<'a>(r: &'a Recommendation, code: &str) -> &'a Reason {
        r.reasons.iter().find(|x| x.code == code).unwrap()
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn sell_high_fires_on_lucky_hot_streak() {
        let (ctx, values) = build(
            vec![skater("lucky", Some(HOT_LUCKY), "healthy", 30), skater("real", Some(HOT_REAL), "healthy", 30)],
            vec![],
            vec![],
        );
        let recs = recommend_flags(&ctx, &values, None, None).unwrap();
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!((r.kind, r.drop[0].cid.as_str()), (RecKind::SellHigh, "lucky"));
        assert!(close(reason(r, "FORM_RATIO").value, 1.5));
        assert!(close(reason(r, "SHOOTING_PCT").value, 0.20));
        assert!(close(reason(r, "SHOOTING_PCT").baseline, 0.10));
        assert_eq!(reason(r, "GP_L15").value, Some(6.0));
        assert!(r.counterparty.is_none() && r.score > 0.0);
    }

    #[test]
    fn sell_high_only_for_my_players() {
        let (ctx, values) = build(vec![], vec![skater("theirs_lucky", Some(HOT_LUCKY), "healthy", 30)], vec![]);
        assert!(recommend_flags(&ctx, &values, None, None).unwrap().is_empty());
    }

    #[test]
    fn buy_low_only_for_other_teams_players() {
        let (ctx, values) = build(
            vec![skater("my_cold", Some(COLD_STABLE), "healthy", 30)],
            vec![
                skater("their_cold", Some(COLD_STABLE), "healthy", 30),
                skater("their_shots_down", Some(COLD_SHOTS_DOWN), "healthy", 30),
                skater("their_hurt", Some(COLD_STABLE), "out", 30),
            ],
            vec![skater("fa_cold", Some(COLD_STABLE), "healthy", 30)],
        );
        let recs = recommend_flags(&ctx, &values, None, None).unwrap();
        let got: Vec<(RecKind, &str)> = recs.iter().map(|r| (r.kind, r.add[0].cid.as_str())).collect();
        assert_eq!(got, vec![(RecKind::BuyLow, "their_cold")]);
        let r = &recs[0];
        assert_eq!(r.counterparty.as_deref(), Some("Rivals"));
        assert!(close(reason(r, "FORM_RATIO").value, 0.5));
        assert!(close(reason(r, "SOG_RATE").value, 4.0) && close(reason(r, "SOG_RATE").baseline, 4.0));
        reason(r, "GP_L15");
        reason(r, "SHOOTING_PCT");
    }

    #[test]
    fn small_l15_sample_is_skipped() {
        let (ctx, values) = build(
            vec![skater("lucky", Some((4, 0.6, 0.9, 3.0)), "healthy", 30)],
            vec![skater("cold", Some((4, 0.1, 0.4, 4.0)), "healthy", 30)],
            vec![],
        );
        assert!(recommend_flags(&ctx, &values, None, None).unwrap().is_empty());
    }

    #[test]
    fn buy_low_skips_replacement_level_players() {
        let (ctx, mut values) = build(vec![], vec![skater("scrub", Some(COLD_STABLE), "healthy", 30)], vec![]);
        let p = values["scrub"].player.clone();
        values.insert("scrub".into(), val(&p, -0.2));
        assert!(recommend_flags(&ctx, &values, None, None).unwrap().is_empty());
    }

    #[test]
    fn form_ratio_uses_shrunk_season_baseline() {
        let mut p = skater("x", Some((6, 0.9, 1.2, 3.0)), "healthy", 10);
        p.lines.insert("season".into(), serde_json::from_value(line("season", 10, 0.6, 0.9, 4.0)).unwrap());
        let (ctx, values) = build(vec![p], vec![], vec![]);
        let recs = recommend_flags(&ctx, &values, None, None).unwrap();
        let ratio = reason(&recs[0], "FORM_RATIO").value.unwrap();
        // K_INSEASON["skater"] = 25
        let k = 25.0;
        assert!((ratio - 2.1 / ((10.0 * 1.5 + k * 1.0) / (10.0 + k))).abs() < 1e-9, "{ratio}");
        let mut p2 = skater("y", Some(HOT_LUCKY), "healthy", 10);
        p2.lines.insert("season".into(), serde_json::from_value(line("season", 10, 0.6, 0.9, 4.0)).unwrap());
        let (ctx2, values2) = build(vec![p2], vec![], vec![]);
        assert!(recommend_flags(&ctx2, &values2, None, None).unwrap().is_empty());
    }

    #[test]
    fn flag_predicted_gain_is_regression_to_baseline() {
        let (ctx, values) = build(
            vec![skater("lucky", Some(HOT_LUCKY), "healthy", 30)],
            vec![skater("cold", Some(COLD_STABLE), "healthy", 30)],
            vec![],
        );
        let recs = recommend_flags(&ctx, &values, None, None).unwrap();
        let sell = recs.iter().find(|r| r.kind == RecKind::SellHigh).unwrap();
        let buy = recs.iter().find(|r| r.kind == RecKind::BuyLow).unwrap();
        assert_eq!(sell.gain_units, Some(GainUnits::SeasonFpg));
        assert_eq!(buy.gain_units, Some(GainUnits::SeasonFpg));
        assert_eq!((sell.horizon_days, buy.horizon_days), (Some(28), Some(28)));
        assert!(sell.predicted_gain.unwrap() < 0.0 && 0.0 < buy.predicted_gain.unwrap());
        assert!((sell.predicted_gain.unwrap() - (1.0 - 1.5)).abs() <= 0.2);
        assert!(reason(sell, "FORM_RATIO").text.contains("baseline"));
        assert!(sell.strength.unwrap() > 4.0);
    }

    #[test]
    fn role_change_thresholds_and_strength() {
        let mut p = player("p", &["C"], json!({}));
        p.toi_per_game = Some(15.0);
        p.toi_trend = Some(3.0);
        p.pp_share = Some(0.1);
        p.pp_share_trend = Some(0.45);
        let c = role_change(&p, None, 1).unwrap();
        assert!(c.toi_hit && c.pp_hit && c.pp1);
        // TOI 3.0 -> 7, PP 0.45 -> 9.5, + both bonus, capped at 10
        assert_eq!(c.strength, round_to((interp(0.45, &ROLE_PP_SCALE) + 1.0).min(10.0), 2));
        assert!(role_change(&p, None, -1).is_none());
        p.toi_trend = Some(-2.0);
        p.pp_share_trend = None;
        let down = role_change(&p, None, -1).unwrap();
        assert!(down.toi_hit && !down.pp_hit);
        assert_eq!(down.strength, round_to(interp(2.0, &ROLE_TOI_SCALE), 2));
    }

    #[test]
    fn role_alerts_up_for_others_and_loss_for_mine() {
        let mut mine = player("mine", &["C"], json!({}));
        mine.toi_per_game = Some(18.0);
        mine.toi_trend = Some(-2.5);
        let mut rival = player("rival", &["C"], json!({}));
        rival.toi_per_game = Some(14.0);
        rival.toi_trend = Some(2.0);
        let mut rival_down = player("rd", &["C"], json!({}));
        rival_down.toi_per_game = Some(14.0);
        rival_down.toi_trend = Some(-3.0);
        let (ctx, _) = build(vec![mine], vec![rival, rival_down], vec![]);
        let recs = recommend_role_alerts(&ctx, None, None);
        let titles: Vec<&str> = recs.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["Role loss: mine (TOI -2.5; my team)", "Role up: rival (TOI +2.0; Rivals)"]);
        assert_eq!(recs[1].counterparty.as_deref(), Some("Rivals"));
        assert_eq!(recs[0].reasons[0].code, "ROLE_TOI");
        assert_eq!(recs[0].reasons[0].text, "TOI 15.5 min per game over the recent games vs 18.0 baseline (-2.5 min)");
    }

    #[test]
    fn preseason_standout_strength() {
        let ln: PreseasonLine = serde_json::from_value(json!({"nhl_id": 1, "season": 20262027, "gp": 4,
            "stats": {"G": 3.0, "A": 3.0, "PTS": 6.0, "GP": 4.0}, "toi_seconds": 4.0 * 17.0 * 60.0, "toi_games": 4}))
        .unwrap();
        let c = preseason_standout(Some(&ln)).unwrap();
        assert!(c.pts_hit && c.toi_hit);
        assert_eq!(c.strength, 5.75); // 1.5 PTS/GP -> 5.25, + 0.5 (TOI also hit)
        assert_eq!(ln.summary(), "4 GP: 3 G, 3 A, 6 PTS, 17.0 TOI");
    }
}
