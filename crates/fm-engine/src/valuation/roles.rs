//! Rust port of `fantasy_manager/valuation/roles.py`: role multipliers from Daily Faceoff lineups
//! and news, shared by the rookie model and the role-change step for established skaters, plus
//! goalie job news on the start share. Both are applied by `valuation::valuate`.
//! Owner: work package `W3-valuation-schedule` (wave 3).
//!
//! Signals are [`Signal`]s (the Python dicts); `role_change` / `goalie_news_share` accept any
//! [`AsSignal`] slice (e.g. `EnrichState::signals_for(p)`).

use chrono::NaiveDate;
use fm_core::models::{Player, Reason};
use fm_pycompat::fmt::{fixed, general, percent};
use fm_pycompat::num::round_to;

use super::rookie::{dfo_signals, fmt_signal, usable_news, AsSignal, Signal};

pub const ROLE_BOOST: f64 = 1.15;
pub const ROLE_MINOR_BOOST: f64 = 1.05;
pub const DEPTH_PENALTY: f64 = 0.92;
pub const SCRATCH_MULT: f64 = 0.85;
pub const TOP_KINDS: [&str; 3] = ["top_line", "pp1", "first_line_pairing"];
pub const MINOR_KINDS: [&str; 2] = ["pp2", "extended_role"];

/// share of a role change that carries into the rest of the season
pub const ROLE_SEASON_WEIGHT: f64 = 0.5;
pub const ESTABLISHED_NEWS_MAX_AGE_DAYS: i64 = 10;
pub const SCRATCH_NEWS_MAX_AGE_DAYS: i64 = 7;
/// share of team PP time behind a usual PP1 / PP2 (no snapshots)
pub const PP1_SHARE: f64 = 0.5;
pub const PP2_SHARE: f64 = 0.2;

pub const START_NEWS_MAX_AGE_DAYS: i64 = 14;
pub const START_NEWS_WEIGHT: f64 = 0.5;

/// `START_NEWS_TARGET[(kind, direction)]`
pub fn start_news_target(kind: &str, direction: i64) -> Option<f64> {
    match (kind, direction) {
        ("goalie_role", 1) => Some(0.70),
        ("goalie_role", -1) => Some(0.30),
        ("goalie_split", 1) => Some(0.50),
        _ => None,
    }
}

const LINE_NEWS: [&str; 3] = ["top_line", "first_line_pairing", "extended_role"];
const PP_NEWS: [&str; 2] = ["pp1", "pp2"];

/// `role_multiplier(signals)`: (multiplier, the signals behind it) by the module tiers: a top role
/// x1.15 (not stacked), PP2 / second line / top-six x1.05, a depth line x0.92, else 1.0.
pub fn role_multiplier(signals: &[Signal]) -> (f64, Vec<Signal>) {
    let pick = |f: &dyn Fn(&Signal) -> bool| -> Vec<Signal> { signals.iter().filter(|d| f(d)).cloned().collect() };
    let top = pick(&|d| TOP_KINDS.contains(&d.kind.as_str()) && d.direction > 0);
    if !top.is_empty() {
        return (ROLE_BOOST, top);
    }
    let minor = pick(&|d| MINOR_KINDS.contains(&d.kind.as_str()) && d.direction > 0);
    if !minor.is_empty() {
        return (ROLE_MINOR_BOOST, minor);
    }
    let depth = pick(&|d| d.kind == "top_line" && d.direction < 0);
    if !depth.is_empty() {
        return (DEPTH_PENALTY, depth);
    }
    (1.0, Vec::new())
}

fn line_kinds(line: Option<&str>) -> Vec<Signal> {
    match line.unwrap_or("").to_lowercase().as_str() {
        "f1" => vec![Signal::kd("top_line", 1)],
        "d1" => vec![Signal::kd("first_line_pairing", 1)],
        "f2" | "d2" => vec![Signal::kd("extended_role", 1)],
        "f4" | "d3" | "d4" => vec![Signal::kd("top_line", -1)],
        _ => vec![],
    }
}

fn pp_kinds(pp: Option<&str>) -> Vec<Signal> {
    let pp = pp.unwrap_or("").to_lowercase();
    if pp == "pp1" || pp == "pp2" {
        vec![Signal::kd(&pp, 1)]
    } else {
        vec![]
    }
}

fn news_line(d: &Signal) -> (Vec<Signal>, String) {
    if d.direction > 0 {
        (vec![Signal::kd(&d.kind, 1)], d.kind.replace('_', " "))
    } else {
        (vec![Signal::kd("top_line", -1)], "depth role".to_string())
    }
}

fn news_pp(d: &Signal) -> (Vec<Signal>, String) {
    if d.kind == "pp1" && d.direction > 0 {
        (pp_kinds(Some("pp1")), "PP1".to_string())
    } else {
        (pp_kinds(Some("pp2")), "PP2".to_string())
    }
}

/// `(x or "no PP").upper().replace("NO PP", "no PP")`
fn pp_label(pp: Option<&str>) -> String {
    let s = match pp {
        Some(s) if !s.is_empty() => s,
        _ => "no PP",
    };
    s.to_uppercase().replace("NO PP", "no PP")
}

fn sign_char(direction: i64) -> &'static str {
    if direction > 0 {
        "+"
    } else {
        "-"
    }
}

fn truthy(s: Option<&str>) -> bool {
    s.is_some_and(|s| !s.is_empty())
}

/// `RoleChange`: the established-skater role change.
#[derive(Clone, Debug, PartialEq)]
pub struct RoleChange {
    /// on the scoring rates (this week's role)
    pub mult: f64,
    /// what the rest of the season keeps of it
    pub season_mult: f64,
    /// scratch: x SCRATCH_MULT on the week value only
    pub week_extra: f64,
    pub reasons: Vec<Reason>,
}

impl Default for RoleChange {
    fn default() -> Self {
        RoleChange { mult: 1.0, season_mult: 1.0, week_extra: 1.0, reasons: Vec::new() }
    }
}

/// `role_change(p, news_signals, as_of, team_pp_listed=, team_lineup_listed=)`: the established-
/// skater role change, or None when nothing moves. `team_pp_listed` / `team_lineup_listed`: Daily
/// Faceoff lists PP units / a lineup for his team.
pub fn role_change<S: AsSignal>(
    p: &Player,
    news_signals: &[S],
    as_of: Option<NaiveDate>,
    team_pp_listed: bool,
    team_lineup_listed: bool,
) -> Option<RoleChange> {
    let news = usable_news(news_signals, as_of, ESTABLISHED_NEWS_MAX_AGE_DAYS);
    let mut out = RoleChange::default();
    // usual role
    let (usual_line, usual_pp, usual_txt): (Option<Vec<Signal>>, Vec<Signal>, String) =
        if let Some(ul) = p.usual_line.as_deref().filter(|s| !s.is_empty()) {
            (
                Some(line_kinds(Some(ul))),
                pp_kinds(p.usual_pp_unit.as_deref()),
                format!("{}, {}", ul.to_uppercase(), pp_label(p.usual_pp_unit.as_deref())),
            )
        } else if let Some(share) = p.pp_share {
            let tier = if share >= PP1_SHARE {
                Some("pp1")
            } else if share >= PP2_SHARE {
                Some("pp2")
            } else {
                None
            };
            (None, pp_kinds(tier), format!("{} ({} of team PP time)", pp_label(tier), percent(share, 0, false)))
        } else {
            (None, Vec::new(), String::new())
        };
    let has_line = truthy(p.line.as_deref());
    if usual_line.is_some() || p.pp_share.is_some() {
        let dfo: Vec<Signal> =
            if has_line { dfo_signals(p).into_iter().filter(|d| d.kind != "nhl_roster").collect() } else { vec![] };
        let mut evidence: Vec<Reason> = Vec::new();
        let dfo_reason = |d: &Signal| {
            Reason::new("ROLE_DFO", d.quote.clone())
                .with_value(Some(d.direction as f64))
                .with_baseline(Some(d.confidence))
        };
        // line part
        let (cur_line, line_txt): (Vec<Signal>, Option<String>) = match &usual_line {
            None => (vec![], None),
            Some(_) if has_line => {
                evidence.extend(dfo.iter().filter(|d| !PP_NEWS.contains(&d.kind.as_str())).map(dfo_reason));
                (line_kinds(p.line.as_deref()), Some(p.line.as_deref().unwrap_or("").to_uppercase()))
            }
            Some(ul) => match news.iter().find(|d| LINE_NEWS.contains(&d.kind.as_str())) {
                Some(ln) => {
                    let (kinds, txt) = news_line(ln);
                    evidence.push(
                        Reason::new(
                            "ROLE_NEWS",
                            format!("{} {}: {}", ln.kind.replace('_', " "), sign_char(ln.direction), fmt_signal(ln)),
                        )
                        .with_value(Some(ln.direction as f64))
                        .with_baseline(Some(ln.confidence)),
                    );
                    (kinds, Some(txt + " (news)"))
                }
                None => (ul.clone(), None),
            },
        };
        // PP part
        let (cur_pp, pp_txt): (Vec<Signal>, Option<String>) = if has_line && team_pp_listed {
            evidence.extend(dfo.iter().filter(|d| PP_NEWS.contains(&d.kind.as_str())).map(dfo_reason));
            (pp_kinds(p.pp_unit.as_deref()), Some(pp_label(p.pp_unit.as_deref())))
        } else {
            match news.iter().find(|d| PP_NEWS.contains(&d.kind.as_str())) {
                Some(pn) => {
                    let (kinds, txt) = news_pp(pn);
                    evidence.push(
                        Reason::new("ROLE_NEWS", format!("{} {}: {}", pn.kind, sign_char(pn.direction), fmt_signal(pn)))
                            .with_value(Some(pn.direction as f64))
                            .with_baseline(Some(pn.confidence)),
                    );
                    (kinds, Some(txt + " (news)"))
                }
                None => (usual_pp.clone(), None),
            }
        };
        let before_sigs: Vec<Signal> = usual_line.clone().unwrap_or_default().into_iter().chain(usual_pp).collect();
        let after_sigs: Vec<Signal> = cur_line.into_iter().chain(cur_pp).collect();
        let (before, _) = role_multiplier(&before_sigs);
        let (after, _) = role_multiplier(&after_sigs);
        let r = after / before;
        if (r - 1.0).abs() > 1e-9 {
            out.mult = r;
            out.season_mult = 1.0 + (r - 1.0) * ROLE_SEASON_WEIGHT;
            let now_txt: Vec<&str> = [line_txt.as_deref(), pp_txt.as_deref()]
                .into_iter()
                .flatten()
                .filter(|t| !t.is_empty())
                .collect();
            let mut reasons = evidence;
            reasons.push(
                Reason::new(
                    if r > 1.0 { "ROLE_BOOST" } else { "ROLE_DEPTH" },
                    format!(
                        "Role change: usually {usual_txt}, now {}: x{} on scoring rates this week, x{} rest of season",
                        now_txt.join(", "),
                        fixed(r, 2, false),
                        fixed(out.season_mult, 2, false)
                    ),
                )
                .with_value(Some(round_to(r, 4)))
                .with_baseline(Some(1.0)),
            );
            out.reasons = reasons;
        }
    }
    // scratch: healthy, not in his team's Daily Faceoff lineup, after a scratch
    if p.status.is_in(&["healthy", "unknown"]) && !has_line && team_lineup_listed {
        let sc = usable_news(news_signals, as_of, SCRATCH_NEWS_MAX_AGE_DAYS).into_iter().find(|d| d.kind == "scratched");
        let why = if p.line_change.as_deref().unwrap_or("").to_lowercase() == "scratched" {
            Some("Daily Faceoff: dropped out of his team's lineup".to_string())
        } else {
            sc.filter(|s| s.direction < 0).map(|s| format!("scratched: {}", fmt_signal(&s)))
        };
        if let Some(why) = why {
            out.week_extra = SCRATCH_MULT;
            out.reasons.push(
                Reason::new(
                    "ROLE_SCRATCH",
                    format!("{why}; not in the lineup now: week value x{}", general(SCRATCH_MULT, None, false)),
                )
                .with_value(Some(SCRATCH_MULT))
                .with_baseline(Some(1.0)),
            );
        }
    }
    if out.mult == 1.0 && out.week_extra == 1.0 {
        return None;
    }
    Some(out)
}

/// `goalie_news_share(share, news_signals, as_of)`: (start share after job news, START_NEWS
/// reason or None). The newest goalie_role / goalie_split signal (<= 14 days) moves the share
/// `START_NEWS_WEIGHT` x confidence of the way to its target, only in the news' direction.
pub fn goalie_news_share<S: AsSignal>(share: f64, news_signals: &[S], as_of: Option<NaiveDate>) -> (f64, Option<Reason>) {
    let news: Vec<Signal> = usable_news(news_signals, as_of, START_NEWS_MAX_AGE_DAYS)
        .into_iter()
        .filter(|d| start_news_target(&d.kind, d.direction).is_some())
        .collect();
    let Some(d) = news.first() else { return (share, None) };
    let target = start_news_target(&d.kind, d.direction).expect("filtered");
    if (d.kind == "goalie_role" && (target - share) * d.direction as f64 <= 0.0) || (target - share).abs() < 1e-9 {
        return (share, None); // already past the target in the news' direction
    }
    let w = START_NEWS_WEIGHT * d.confidence;
    let new = share + w * (target - share);
    let label = match (d.kind.as_str(), d.direction) {
        ("goalie_role", 1) => "won the job",
        ("goalie_role", -1) => "lost the job",
        _ => "tandem",
    };
    let reason = Reason::new(
        "START_NEWS",
        format!(
            "Goalie news ({label}): {}; start share {} -> {} ({} of the way to {})",
            fmt_signal(d),
            percent(share, 0, false),
            percent(new, 0, false),
            percent(w, 0, false),
            percent(target, 0, false)
        ),
    )
    .with_value(Some(round_to(new, 4)))
    .with_baseline(Some(round_to(share, 4)));
    (new, Some(reason))
}
