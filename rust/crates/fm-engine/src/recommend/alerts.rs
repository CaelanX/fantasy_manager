//! Rust port of `fantasy_manager/recommend/alerts.py`: line / power-play / goalie-start alerts
//! from the Daily Faceoff snapshot (kind "alert", `strength` set directly).
//!
//! Owner: work package `W7-injuries-flags-alerts-matchup`.
//!
//! Deviations from Python (behaviour-preserving):
//! * `team_meta` defaults to `ctx.enrich.team_meta_for(ctx.as_of)` (Python: the
//!   `lines_enrich._TEAM_META` module registry).
//! * `weak_teams` is a [`WeakTeams`] enum (a mapping team -> goals per game, or a list of teams)
//!   instead of a duck-typed argument.
//! * `values` is the typed values map (Python `_fa_rank_value` reads `fpg_season` / `fpg` with
//!   getattr; a `PlayerValue` always has `fpg_season`).

use std::sync::OnceLock;

use fm_core::models::{LeagueContext, Player, Reason, RecKind, Recommendation};
use fm_core::normalize::normalize_team;
use fm_core::values::PlayerValue;
use fm_core::{Cid, Map, PyDateTime};
use fm_pycompat::fmt::fixed;
use fm_pycompat::order::cmp_f64;
use fm_pycompat::time::strftime;
use regex::Regex;
use serde_json::Value;

use crate::recommend::base::{cap_text, capped_positions, moves_budget_reason, moves_left, no_moves_text};

pub const PP1_PROMOTION: f64 = 7.0;
pub const TOP_LINE_PROMOTION: f64 = 5.0;
pub const DEMOTION: f64 = 4.0;
pub const CONFIRMED_START: f64 = 6.0;
/// free-agent line promotions reported per run
pub const FA_LIMIT: usize = 5;
pub const FA_GOALIE_LIMIT: usize = 3;
pub const WEAK_OFFENSE_TEAMS: usize = 10;
pub const MIN_TEAMS_FOR_RANK: usize = 16;
pub const MIN_TEAM_GP: i64 = 10;

fn game_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bstarts ([A-Z]{2,4})@([A-Z]{2,4})\b").expect("static regex"))
}

fn strength_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\bDFO (Confirmed|Likely|Expected)\b").expect("static regex"))
}

fn top_line_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^(?:F[234] -> F1|D[234] -> D1)$").expect("static regex"))
}

fn parts(change: Option<&str>) -> Vec<&str> {
    change.unwrap_or("").split(';').map(|c| c.trim()).filter(|c| !c.is_empty()).collect()
}

/// [(trigger, strength)] for one player's `line_change`.
pub fn classify(p: &Player) -> Vec<(&'static str, f64)> {
    let mut out = Vec::new();
    for part in parts(p.line_change.as_deref()) {
        let up = part.to_uppercase();
        if up.ends_with("-> PP1") && !up.starts_with("PP1") {
            out.push(("pp1_promotion", PP1_PROMOTION));
        } else if up.starts_with("PP1 ->") {
            out.push(("pp1_demotion", DEMOTION));
        } else if top_line_re().is_match(&up)
            || (part == "new to lineup" && matches!(p.line.as_deref().unwrap_or(""), "f1" | "d1"))
        {
            out.push(("top_line", TOP_LINE_PROMOTION));
        } else if part == "scratched" {
            out.push(("scratched", DEMOTION));
        }
    }
    out
}

type TeamMeta = Map<String, Map<String, Value>>;

/// (source, "Nov 04 16:12 UTC") from the Daily Faceoff team meta.
fn team_meta(team: Option<&str>, meta: Option<&TeamMeta>) -> (Option<String>, Option<String>) {
    let Some(m) = meta.and_then(|tm| tm.get(team.unwrap_or(""))).filter(|m| !m.is_empty()) else {
        return (None, None);
    };
    let when_txt = match m.get("updated_at") {
        Some(Value::String(s)) => match PyDateTime::parse(&s.replace('Z', "+00:00")) {
            Some(dt) => Some(strftime(dt.local, "%b %d %H:%M UTC")),
            None => (!s.is_empty()).then(|| s.clone()),
        },
        Some(Value::Null) | None => None,
        Some(Value::Bool(false)) => None,
        Some(v) => Some(v.to_string()),
    };
    let src = match m.get("source") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(v) => Some(v.to_string()),
    };
    (src, when_txt)
}

fn source_text(team: Option<&str>, meta: Option<&TeamMeta>) -> String {
    let (src, when) = team_meta(team, meta);
    let bits: Vec<String> =
        [src, when.map(|w| format!("updated {w}"))].into_iter().flatten().filter(|b| !b.is_empty()).collect();
    if bits.is_empty() {
        "Daily Faceoff".into()
    } else {
        format!("Daily Faceoff ({})", bits.join(", "))
    }
}

fn fa_rank_value(p: &Player, values: Option<&Map<Cid, PlayerValue>>) -> f64 {
    match values.and_then(|v| v.get(&p.cid)) {
        Some(pv) => pv.fpg_season,
        None => p.pct_owned.unwrap_or(0.0),
    }
}

/// False only when the NHL schedule is loaded for his team and has no game on as_of.
pub fn plays_today(ctx: &LeagueContext, p: &Player) -> bool {
    let team = normalize_team(p.team.as_deref()).unwrap_or_default();
    match ctx.schedule.get(&team).filter(|d| !d.is_empty()) {
        None => true,
        Some(dates) => dates.contains(&ctx.as_of),
    }
}

pub fn opponent_today(ctx: &LeagueContext, p: &Player) -> Option<String> {
    let team = normalize_team(p.team.as_deref());
    let key = team.clone().unwrap_or_default();
    if let Some(opp) = ctx.opponents.get(&key).and_then(|m| m.get(&ctx.as_of)).filter(|o| !o.is_empty()) {
        return Some(opp.trim_start_matches('@').to_string());
    }
    let caps = game_re().captures(p.start_source.as_deref().unwrap_or(""))?;
    let team = team?;
    let (away, home) = (&caps[1], &caps[2]);
    if team == away {
        Some(home.to_string())
    } else if team == home {
        Some(away.to_string())
    } else {
        None
    }
}

/// {team: goals per game} for the `n` lowest-scoring NHL teams (empty when too few teams have
/// data).
pub fn weak_offenses(ctx: &LeagueContext, n: usize) -> Map<String, f64> {
    let skaters: Vec<(&Player, String)> = ctx
        .all_players()
        .into_iter()
        .filter(|p| !p.is_goalie())
        .filter_map(|p| normalize_team(p.team.as_deref()).map(|t| (p, t)))
        .collect();
    let season_gp = skaters.iter().map(|(p, _)| p.gp("season")).max().unwrap_or(0);
    let split = if season_gp >= MIN_TEAM_GP { "season" } else { "prior" };
    let mut goals: Map<String, f64> = Map::new();
    let mut games: Map<String, i64> = Map::new();
    for (p, t) in &skaters {
        let Some(line) = p.lines.get(split).filter(|l| l.gp > 0) else { continue };
        *goals.entry(t.clone()).or_insert(0.0) += line.stats.get("G").copied().unwrap_or(0.0);
        let g = games.entry(t.clone()).or_insert(0);
        *g = (*g).max(line.gp);
    }
    let mut gpg: Vec<(String, f64)> = goals
        .iter()
        .filter_map(|(t, g)| games.get(t).filter(|n| **n != 0).map(|n| (t.clone(), g / *n as f64)))
        .collect();
    if gpg.len() < MIN_TEAMS_FOR_RANK {
        return Map::new();
    }
    gpg.sort_by(|a, b| cmp_f64(a.1, b.1));
    gpg.into_iter().take(n).collect()
}

fn rec(p: &Player, title: String, strength: f64, reasons: Vec<Reason>) -> Recommendation {
    let mut r = Recommendation::new(RecKind::Alert, strength, title);
    r.subjects = vec![p.clone()];
    r.reasons = reasons;
    r.strength = Some(strength);
    r
}

fn line_alert(p: &Player, mine: bool, meta: Option<&TeamMeta>) -> Option<Recommendation> {
    let mut triggers = classify(p);
    if !mine {
        triggers.retain(|t| t.0 == "pp1_promotion" || t.0 == "top_line");
    }
    if triggers.is_empty() {
        return None;
    }
    let has = |k: &str| triggers.iter().any(|t| t.0 == k);
    let strength = triggers.iter().map(|t| t.1).fold(f64::NEG_INFINITY, f64::max);
    let team = normalize_team(p.team.as_deref());
    let src = source_text(team.as_deref(), meta);
    let change = p.line_change.as_deref().unwrap_or("None");
    let mut reasons = vec![Reason {
        code: "LINE_CHANGE".into(),
        text: format!("{}: {change} per {src}", p.name),
        value: Some(strength),
        baseline: None,
    }];
    let pp = p.pp_unit.as_deref().filter(|u| !u.is_empty());
    if pp.is_some() || has("pp1_demotion") {
        let unit = pp.map(|u| u.to_uppercase()).unwrap_or_else(|| "no power-play unit".into());
        let value = pp.and_then(|u| u.chars().last()).and_then(|c| c.to_digit(10)).map(|d| d as f64);
        reasons.push(Reason { code: "PP_UNIT".into(), text: format!("{} now on {unit} ({src})", p.name), value, baseline: None });
    }
    let who = if mine { p.name.clone() } else { format!("{} (free agent)", p.name) };
    let title = if has("pp1_promotion") {
        format!("{who} promoted to PP1 ({change}): {}", if mine { "start him / hold" } else { "waiver watch" })
    } else if has("top_line") {
        format!("{who} moved up to the top unit ({change}): {}", if mine { "role boost" } else { "waiver watch" })
    } else if has("pp1_demotion") {
        format!("{who} dropped off PP1 ({change}): sell / bench caution")
    } else {
        format!("{who} out of the lineup ({change}): bench caution")
    };
    Some(rec(p, title, strength, reasons))
}

/// MOVE_BUDGET note for a streamer pickup; None when moves are unlimited.
fn budget_reason(ctx: &LeagueContext) -> Option<Reason> {
    if moves_left(ctx) == Some(0) {
        return Some(Reason { code: "MOVE_BUDGET".into(), text: no_moves_text(ctx), value: Some(0.0), baseline: None });
    }
    moves_budget_reason(ctx)
}

fn strength_word(p: &Player) -> String {
    strength_re()
        .captures(p.start_source.as_deref().unwrap_or(""))
        .map(|c| c[1].to_lowercase())
        .unwrap_or_else(|| "confirmed".into())
}

fn start_alert(p: &Player, ctx: &LeagueContext, mine: bool, weak: &Map<String, f64>) -> Option<Recommendation> {
    let confirmed = p.confirmed_start?;
    if !plays_today(ctx, p) {
        return None;
    }
    let word = strength_word(p);
    let opp = opponent_today(ctx, p);
    let vs = opp.as_ref().map(|o| format!(" vs {o}")).unwrap_or_default();
    let src = p.start_source.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "Daily Faceoff".into());
    if confirmed {
        let in_weak = opp.as_ref().is_some_and(|o| weak.contains_key(o));
        let title = if mine {
            format!("{} {word} to start tonight{vs}: start him", p.name)
        } else {
            if !in_weak {
                return None;
            }
            format!("{} (free agent) {word} to start tonight{vs} (weak offense): streamer", p.name)
        };
        let mut reasons = vec![Reason { code: "CONFIRMED_START".into(), text: src, value: Some(1.0), baseline: None }];
        if !mine && in_weak {
            let o = opp.as_deref().unwrap_or("");
            let gpg = weak[o];
            let txt = if gpg != 0.0 {
                format!("{o} scores {} goals per game (bottom-{} offense)", fixed(gpg, 2, false), weak.len())
            } else {
                format!("{o} is a weak offense")
            };
            reasons.push(Reason {
                code: "OPPONENT".into(),
                text: txt,
                value: if gpg != 0.0 { Some(gpg) } else { None },
                baseline: None,
            });
        }
        if !mine && !ctx.position_limits.is_empty() {
            let capped: Vec<&str> = match ctx.my_team() {
                Ok(t) => capped_positions(t, ctx, p),
                Err(_) => vec![],
            };
            if !capped.is_empty() {
                reasons.push(Reason::new(
                    "POSITION_CAP",
                    format!(
                        "{} reached: streaming him needs a {} drop",
                        capped.iter().map(|k| cap_text(ctx, k, None)).collect::<Vec<_>>().join(", "),
                        capped.join("/")
                    ),
                ));
            }
        }
        if !mine {
            if let Some(b) = budget_reason(ctx) {
                reasons.push(b);
            }
        }
        return Some(rec(p, title, CONFIRMED_START, reasons));
    }
    if mine && word == "confirmed" {
        return Some(rec(
            p,
            format!("{} not starting tonight{vs}: bench caution", p.name),
            DEMOTION,
            vec![Reason { code: "CONFIRMED_START".into(), text: src, value: Some(0.0), baseline: None }],
        ));
    }
    None
}

/// `weak_teams` override of [`recommend_line_alerts`].
pub enum WeakTeams<'a> {
    Map(&'a Map<String, f64>),
    List(&'a [String]),
}

/// Keyword arguments of `recommend_line_alerts`.
pub struct LineAlertOpts<'a> {
    /// {team: {"source", "updated_at"}}; None = the last `enrich_lines` run for `ctx.as_of`
    pub team_meta: Option<&'a TeamMeta>,
    /// overrides [`weak_offenses`]
    pub weak_teams: Option<WeakTeams<'a>>,
    pub fa_limit: usize,
    pub fa_goalie_limit: usize,
}

impl Default for LineAlertOpts<'_> {
    fn default() -> Self {
        LineAlertOpts { team_meta: None, weak_teams: None, fa_limit: FA_LIMIT, fa_goalie_limit: FA_GOALIE_LIMIT }
    }
}

/// Line / unit / goalie-start alerts, strongest first.
pub fn recommend_line_alerts(
    ctx: &LeagueContext,
    values: Option<&Map<Cid, PlayerValue>>,
    opts: &LineAlertOpts<'_>,
) -> Vec<Recommendation> {
    let Ok(mine_team) = ctx.my_team() else { return vec![] };
    let meta: Option<&TeamMeta> = match opts.team_meta {
        Some(m) => Some(m),
        None => ctx.enrich.team_meta_for(ctx.as_of),
    };
    // {p.cid: p for p in players}.values(): first position, last value
    let mut uniq: Map<&str, &Player> = Map::new();
    for p in mine_team.players_iter() {
        uniq.insert(p.cid.as_str(), p);
    }
    let mine: Vec<&Player> = uniq.values().copied().collect();
    let is_mine = |cid: &str| uniq.contains_key(cid);
    let fas: Vec<&Player> = ctx.free_agents.iter().filter(|p| !is_mine(&p.cid)).collect();
    let mut recs: Vec<Recommendation> = Vec::new();

    for p in &mine {
        if let Some(r) = line_alert(p, true, meta) {
            recs.push(r);
        }
    }
    let mut fa_line: Vec<Recommendation> = fas
        .iter()
        .filter(|p| p.line_change.as_deref().is_some_and(|c| !c.is_empty()))
        .filter_map(|p| line_alert(p, false, meta))
        .collect();
    fa_line.sort_by(|a, b| {
        cmp_f64(-a.strength.unwrap_or(0.0), -b.strength.unwrap_or(0.0))
            .then_with(|| cmp_f64(-fa_rank_value(&a.subjects[0], values), -fa_rank_value(&b.subjects[0], values)))
    });
    recs.extend(fa_line.into_iter().take(opts.fa_limit));

    let weak: Map<String, f64> = match &opts.weak_teams {
        None => {
            if fas.iter().any(|p| p.is_goalie() && p.confirmed_start == Some(true)) {
                weak_offenses(ctx, WEAK_OFFENSE_TEAMS)
            } else {
                Map::new()
            }
        }
        Some(WeakTeams::Map(m)) => (*m).clone(),
        Some(WeakTeams::List(ts)) => {
            ts.iter().map(|t| (normalize_team(Some(t)).unwrap_or_else(|| t.clone()), 0.0)).collect()
        }
    };
    let seen: Vec<String> = recs.iter().map(|r| r.subjects[0].cid.clone()).collect();
    for p in &mine {
        if p.is_goalie() && !seen.contains(&p.cid) {
            if let Some(r) = start_alert(p, ctx, true, &weak) {
                recs.push(r);
            }
        }
    }
    let mut fa_starts: Vec<Recommendation> =
        fas.iter().filter(|p| p.is_goalie()).filter_map(|p| start_alert(p, ctx, false, &weak)).collect();
    fa_starts.sort_by(|a, b| cmp_f64(-fa_rank_value(&a.subjects[0], values), -fa_rank_value(&b.subjects[0], values)));
    recs.extend(fa_starts.into_iter().take(opts.fa_goalie_limit));

    recs.sort_by(|a, b| {
        cmp_f64(-a.strength.unwrap_or(0.0), -b.strength.unwrap_or(0.0))
            .then_with(|| (!is_mine(&a.subjects[0].cid)).cmp(&!is_mine(&b.subjects[0].cid)))
            .then_with(|| a.title.cmp(&b.title))
    });
    let n = recs.len() as i64;
    for (i, r) in recs.iter_mut().enumerate() {
        r.rank_in_kind = Some(i as i64 + 1);
        r.kind_total = Some(n);
    }
    recs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(cid: &str, pos: &[&str], change: Option<&str>, line: Option<&str>, pp: Option<&str>) -> Player {
        serde_json::from_value(json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": "EDM",
            "positions": pos, "status": "healthy", "lines": {}, "line_change": change, "line": line, "pp_unit": pp}))
        .unwrap()
    }

    fn ctx(mine: Vec<Player>, fas: Vec<Player>) -> LeagueContext {
        let slots: Vec<Value> = mine.into_iter().map(|p| json!({"slot": "BN", "player": p, "starting": false})).collect();
        serde_json::from_value(json!({"provider": "t", "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {"G": 1.0}}, "roster_shape": {"C": 1},
            "teams": [{"team_id": "1", "name": "me", "owner_is_me": true, "slots": slots}],
            "free_agents": fas, "matchup_period": 1, "as_of": "2026-11-04"}))
        .unwrap()
    }

    #[test]
    fn classify_triggers() {
        let x = p("x", &["C"], Some("F3 -> F1; PP2 -> PP1"), Some("f1"), Some("pp1"));
        assert_eq!(classify(&x), vec![("top_line", 5.0), ("pp1_promotion", 7.0)]);
        let y = p("y", &["C"], Some("PP1 -> no PP"), None, None);
        assert_eq!(classify(&y), vec![("pp1_demotion", 4.0)]);
        let z = p("z", &["C"], Some("new to lineup"), Some("d1"), None);
        assert_eq!(classify(&z), vec![("top_line", 5.0)]);
        let s = p("s", &["C"], Some("scratched"), None, None);
        assert_eq!(classify(&s), vec![("scratched", 4.0)]);
    }

    #[test]
    fn line_alerts_mine_and_free_agents() {
        let mine = p("mine", &["C"], Some("PP1 -> PP2"), Some("f2"), Some("pp2"));
        let fa_up = p("fa_up", &["C"], Some("PP2 -> PP1"), Some("f2"), Some("pp1"));
        let fa_down = p("fa_down", &["C"], Some("PP1 -> PP2"), Some("f2"), Some("pp2"));
        let c = ctx(vec![mine], vec![fa_up, fa_down]);
        let recs = recommend_line_alerts(&c, None, &LineAlertOpts::default());
        let titles: Vec<&str> = recs.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "fa_up (free agent) promoted to PP1 (PP2 -> PP1): waiver watch",
                "mine dropped off PP1 (PP1 -> PP2): sell / bench caution",
            ]
        );
        assert_eq!(recs[0].reasons[1].value, Some(1.0));
        assert_eq!(recs[1].reasons[0].text, "mine: PP1 -> PP2 per Daily Faceoff");
        assert_eq!((recs[1].rank_in_kind, recs[1].kind_total), (Some(2), Some(2)));
    }

    #[test]
    fn goalie_start_alerts() {
        let mut g = p("g", &["G"], None, None, None);
        g.confirmed_start = Some(true);
        g.start_source = Some("DFO Likely: g starts EDM@CGY".into());
        let mut fa_g = p("fa_g", &["G"], None, None, None);
        fa_g.confirmed_start = Some(true);
        fa_g.start_source = Some("DFO Confirmed: fa_g starts EDM@CGY".into());
        let c = ctx(vec![g], vec![fa_g]);
        let weak: Map<String, f64> = [("CGY".to_string(), 2.1)].into_iter().collect();
        let opts = LineAlertOpts { weak_teams: Some(WeakTeams::Map(&weak)), ..LineAlertOpts::default() };
        let recs = recommend_line_alerts(&c, None, &opts);
        let titles: Vec<&str> = recs.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["g likely to start tonight vs CGY: start him", "fa_g (free agent) confirmed to start tonight vs CGY (weak offense): streamer"]
        );
        assert_eq!(recs[1].reasons[1].text, "CGY scores 2.10 goals per game (bottom-1 offense)");
        // not a weak offense: no free-agent streamer alert
        let recs = recommend_line_alerts(&c, None, &LineAlertOpts::default());
        assert_eq!(recs.len(), 1);
    }
}
