//! Rust port of `fantasy_manager/valuation/regression.py`: luck / regression signals from expected
//! goals (MoneyPuck), in-season assist-luck shrink, goalie GSAx save talent.
//! Owner: work package `W2-valuation-base` (wave 2).
//!
//! `GoalieGsax` itself lives in `fm_core::snapshot`; its methods are the [`GsaxExt`] extension
//! trait here. `regressed_gsax` takes its season rows as [`GsaxSeason`].
//! `luck_signals(.., scoring, ..)` takes `Option<&dyn ScoringLike>` (a `Scoring`, `ScoringConfig` or
//! a plain weights `Map`), like Python's `scoring: Any`.

use fm_core::models::Player;
use fm_core::snapshot::GoalieGsax;
use fm_core::Map;

use crate::scoring::ScoringLike;

pub const K_GOALS: f64 = 15.0;
pub const MIN_CURRENT_GP: i64 = 5;
pub const CONFIDENCE_K: f64 = 20.0;
pub const PRIOR_CONFIDENCE: f64 = 0.5;
pub const ASSIST_LUCK_CLAMP: (f64, f64) = (-1.0, 0.6);
pub const DEFAULT_ONICE_SH_PCT: [(&str, f64); 2] = [("F", 0.0956), ("D", 0.0947)];
pub const GOAL_LUCK_REALIZED: f64 = 0.59;
pub const ASSIST_LUCK_REALIZED: f64 = 0.36;

pub const ASSIST_SHRINK_SHARE: f64 = 0.35;
pub const ASSIST_SHRINK_MIN_GP: i64 = 10;
pub const ASSIST_SHRINK_CAP: f64 = 0.15;

pub const GSAX_SEASON_WEIGHTS: [f64; 3] = [1.0, 0.8, 0.6];
pub const GSAX_K_SHOTS: f64 = 3000.0;
pub const GSAX_MIN_SHOTS: f64 = 500.0;
pub const GSAX_SV_BOUNDS: (f64, f64) = (0.85, 0.95);

fn group_map(pos: &str) -> Option<&'static str> {
    Some(match pos {
        "C" | "LW" | "RW" | "F" | "W" => "F",
        "D" => "D",
        "G" => "G",
        _ => return None,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct LuckSignals {
    /// games behind the MoneyPuck line used
    pub gp: i64,
    /// the line is last season's (current GP < 5)
    pub from_prior: bool,
    pub ixg_per_game: f64,
    pub goals_per_game: f64,
    /// season total
    pub goals_minus_ixg: f64,
    /// goals / ixG (None when ixG == 0)
    pub sh_pct_vs_xg_ratio: Option<f64>,
    /// on-ice sh% minus the positional norm
    pub onice_sh_pct_delta: Option<f64>,
    pub goal_luck_fpg: f64,
    pub assist_luck_fpg: f64,
    /// goal_luck_fpg + assist_luck_fpg (+ = running hot)
    pub expected_regression_fpg: f64,
    /// 0..1
    pub confidence: f64,
}

impl LuckSignals {
    /// The part of the luck that historically went away the following season.
    pub fn next_season_drop_fpg(&self) -> f64 {
        self.goal_luck_fpg * GOAL_LUCK_REALIZED + self.assist_luck_fpg * ASSIST_LUCK_REALIZED
    }
}

pub fn group_of(player: &Player) -> &'static str {
    if player.is_goalie() {
        return "G";
    }
    if player.positions.iter().any(|p| p == "D")
        && !player.positions.iter().any(|p| matches!(p.as_str(), "C" | "LW" | "RW" | "F"))
    {
        return "D";
    }
    "F"
}

fn norm(baseline: Option<&Map<String, f64>>, player: &Player) -> Option<f64> {
    let mut base: Map<String, f64> = DEFAULT_ONICE_SH_PCT.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    if let Some(b) = baseline {
        for (k, v) in b {
            base.insert(k.clone(), *v);
        }
    }
    let g = group_of(player);
    if let Some(v) = base.get(g) {
        return Some(*v);
    }
    for pos in &player.positions {
        if let Some(v) = base.get(pos.as_str()) {
            return Some(*v);
        }
        if let Some(v) = group_map(pos).and_then(|g| base.get(g)) {
            return Some(*v);
        }
    }
    None
}

/// (points per goal, points per assist) of a points league (`PTS` added to both); (1.0, 1.0) for
/// category / roto scoring or None.
pub fn league_weights(scoring: Option<&dyn ScoringLike>) -> (f64, f64) {
    let Some(s) = scoring else { return (1.0, 1.0) };
    if !matches!(s.scoring_kind(), None | Some("points")) {
        return (1.0, 1.0);
    }
    let Some(w) = s.scoring_weights().filter(|w| !w.is_empty()) else { return (1.0, 1.0) };
    let pts = w.get("PTS").copied().unwrap_or(0.0);
    (w.get("G").copied().unwrap_or(0.0) + pts, w.get("A").copied().unwrap_or(0.0) + pts)
}

/// (GP, from_prior, split) of the season the MoneyPuck fields describe: this season once it has
/// `MIN_CURRENT_GP` games, else last season.
pub fn xg_sample(player: &Player) -> (i64, bool, &'static str) {
    let cur = player.gp("season");
    if cur >= MIN_CURRENT_GP {
        return (cur, false, "season");
    }
    let prior = player.gp("prior");
    if prior > 0 {
        return (prior, true, "prior");
    }
    (cur, false, "season")
}

/// Luck signals of a skater (None without MoneyPuck data or games). `gp` / `from_prior` override
/// the sample inferred by [`xg_sample`].
pub fn luck_signals(
    player: &Player,
    baseline_sh_pct_by_position: Option<&Map<String, f64>>,
    scoring: Option<&dyn ScoringLike>,
    gp: Option<i64>,
    from_prior: Option<bool>,
) -> Option<LuckSignals> {
    let (ixg_pg, gmx_raw) = match (player.ixg_per_game, player.goals_minus_ixg) {
        (Some(a), Some(b)) if !player.is_goalie() => (a, b),
        _ => return None,
    };
    let (mut n, mut prior, mut split) = xg_sample(player);
    if let Some(g) = gp {
        n = g;
    }
    if let Some(fp) = from_prior {
        prior = fp;
        split = if prior { "prior" } else { "season" };
    }
    if n <= 0 {
        return None;
    }
    let nf = n as f64;
    let (w_goal, w_assist) = league_weights(scoring);
    let ixg = ixg_pg * nf;
    let gmx = gmx_raw;
    let goals = ixg + gmx;
    let goal_luck = gmx / nf * w_goal;

    let nrm = norm(baseline_sh_pct_by_position, player);
    let mut delta = None;
    let mut assist_luck = 0.0;
    if let (Some(oi), Some(nrm)) = (player.onice_sh_pct, nrm) {
        delta = Some(oi - nrm);
        if let Some(line) = player.lines.get(split) {
            if line.gp > 0 && oi > 0.0 {
                let a_pg = line.stats.get("A").copied().unwrap_or(0.0) / line.gp as f64;
                let (lo, hi) = ASSIST_LUCK_CLAMP;
                let frac = hi.min(lo.max(1.0 - nrm / oi));
                assist_luck = a_pg * frac * w_assist;
            }
        }
    }

    let conf = nf / (nf + CONFIDENCE_K) * (if prior { PRIOR_CONFIDENCE } else { 1.0 });
    Some(LuckSignals {
        gp: n,
        from_prior: prior,
        ixg_per_game: ixg_pg,
        goals_per_game: goals / nf,
        goals_minus_ixg: gmx,
        sh_pct_vs_xg_ratio: if ixg > 0.0 { Some(goals / ixg) } else { None },
        onice_sh_pct_delta: delta,
        goal_luck_fpg: goal_luck,
        assist_luck_fpg: assist_luck,
        expected_regression_fpg: goal_luck + assist_luck,
        confidence: conf,
    })
}

/// Rates with `G` shrunk toward `ixg_per_game`: (gp*G + k*ixg)/(gp + k); `PTS` moves by the same
/// amount. Unchanged without an ixG rate, a `G` rate or games; `k=inf` replaces goals with ixG.
pub fn shrink_goals(rates: &Map<String, f64>, ixg_per_game: Option<f64>, gp: f64, k: f64) -> Map<String, f64> {
    let mut out = rates.clone();
    let (Some(ixg), Some(&g)) = (ixg_per_game, out.get("G")) else { return out };
    if gp <= 0.0 {
        return out;
    }
    let new = if k == f64::INFINITY { ixg } else { (gp * g + k * ixg) / (gp + k) };
    let delta = new - g;
    out.insert("G".to_string(), new);
    if let Some(p) = out.get_mut("PTS") {
        *p += delta;
    }
    out
}

/// `rates` with goals shrunk toward the ixG-implied rate of `signals`; a copy when None.
pub fn regressed_rates(rates: &Map<String, f64>, signals: Option<&LuckSignals>, k: f64) -> Map<String, f64> {
    match signals {
        None => rates.clone(),
        Some(s) => shrink_goals(rates, Some(s.ixg_per_game), s.gp as f64, k),
    }
}

/// (rates with `A` (and `PTS`) moved by `-share x assist_luck x confidence`, the change in A/GP).
/// `signals` must be computed without league weights (assists / game).
pub fn assist_shrink_rates(rates: &Map<String, f64>, signals: Option<&LuckSignals>, share: f64) -> (Map<String, f64>, f64) {
    let mut out = rates.clone();
    let Some(s) = signals else { return (out, 0.0) };
    if !out.contains_key("A") || s.assist_luck_fpg == 0.0 {
        return (out, 0.0);
    }
    let a = out["A"];
    let mut delta = -share * s.assist_luck_fpg * s.confidence;
    delta = delta.max(-a); // never below zero assists
    out["A"] += delta;
    if let Some(p) = out.get_mut("PTS") {
        *p += delta;
    }
    (out, delta)
}

// --------------------------------------------------------------------------- goalie GSAx

/// Extension methods of `fm_core::snapshot::GoalieGsax` (Python `GoalieGsax` properties).
pub trait GsaxExt {
    fn usable(&self) -> bool;
    /// Projected save % on MoneyPuck's scale (clamped).
    fn sv_pct(&self) -> f64;
    /// Projected save % re-anchored on `league_sv`; `sv_pct` when either league level is unknown.
    fn sv_pct_on(&self, league_sv: Option<f64>) -> f64;
}

impl GsaxExt for GoalieGsax {
    fn usable(&self) -> bool {
        self.shots >= GSAX_MIN_SHOTS
    }

    fn sv_pct(&self) -> f64 {
        let (lo, hi) = GSAX_SV_BOUNDS;
        hi.min(lo.max(self.xsv_pct + self.per_shot))
    }

    fn sv_pct_on(&self, league_sv: Option<f64>) -> f64 {
        match (league_sv, self.league_sv_pct) {
            (Some(l), Some(mp)) if 0.8 < l && l < 1.0 => {
                let (lo, hi) = GSAX_SV_BOUNDS;
                hi.min(lo.max(l + (self.xsv_pct - mp) + self.per_shot))
            }
            _ => self.sv_pct(),
        }
    }
}

/// One season row for [`regressed_gsax`]: (start year, calibrated xGA, GA, shots on goal, GP[, league sv]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GsaxSeason {
    pub year: i64,
    pub xga: f64,
    pub ga: f64,
    pub sa: f64,
    pub gp: f64,
    pub league_sv: Option<f64>,
}

impl GsaxSeason {
    pub fn new(year: i64, xga: f64, ga: f64, sa: f64, gp: f64, league_sv: Option<f64>) -> Self {
        GsaxSeason { year, xga, ga, sa, gp, league_sv }
    }
}

/// `seasons` most recent first, at most `weights.len()` used (a missing season keeps its slot).
/// None without shots.
pub fn regressed_gsax(nhl_id: i64, seasons: &[GsaxSeason], k: f64, weights: &[f64]) -> Option<GoalieGsax> {
    let (mut g, mut s, mut rg, mut rs, mut wx, mut wl) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut used: Vec<i64> = Vec::new();
    let mut spg: Option<f64> = None;
    let mut have_league = true;
    for (w, row) in weights.iter().zip(seasons.iter()) {
        if row.sa <= 0.0 {
            continue;
        }
        used.push(row.year);
        g += w * (row.xga - row.ga);
        s += w * row.sa;
        wx += w * row.xga;
        match row.league_sv {
            None => have_league = false,
            Some(lsv) => wl += w * row.sa * lsv,
        }
        rg += row.xga - row.ga;
        rs += row.sa;
        if spg.is_none() && row.gp > 0.0 {
            spg = Some(row.sa / row.gp);
        }
    }
    if s <= 0.0 {
        return None;
    }
    Some(GoalieGsax {
        nhl_id,
        seasons: used,
        gsax: g,
        shots: s,
        raw_gsax: rg,
        raw_shots: rs,
        xsv_pct: 1.0 - wx / s,
        per_shot: g / (s + k),
        shots_per_game: spg,
        league_sv_pct: if have_league { Some(wl / s) } else { None },
    })
}

/// (goalie rates with the save component replaced by `est`, save % before, save % after).
///
/// With SA = his shots against per game, the projected save % becomes `est.sv_pct_on(league_sv)`, so
/// `GA = SA x (1 - sv)`, `SV = SA x sv`, `SVPCT = sv` and GAA scales with GA. Only keys already in
/// `rates` change. Unchanged (and (None, None)) without a shot volume or a save stat.
pub fn gsax_rates(
    rates: &Map<String, f64>,
    est: &GoalieGsax,
    league_sv: Option<f64>,
) -> (Map<String, f64>, Option<f64>, Option<f64>) {
    let mut out = rates.clone();
    let mut sa = out.get("SA").copied().filter(|x| *x != 0.0);
    if sa.is_none() {
        if let (Some(sv), Some(ga)) = (out.get("SV"), out.get("GA")) {
            sa = Some(*sv + *ga);
        }
    }
    let mut sa = sa.filter(|x| *x != 0.0);
    if sa.is_none() {
        sa = est.shots_per_game.filter(|x| *x != 0.0);
    }
    let sa = match sa {
        Some(x) if x > 0.0 => x,
        _ => return (out, None, None),
    };
    let before = if let Some(&pct) = out.get("SVPCT") {
        if pct > 1.0 {
            pct / 100.0
        } else {
            pct
        }
    } else if let Some(&sv) = out.get("SV") {
        sv / sa
    } else if let Some(&ga) = out.get("GA") {
        1.0 - ga / sa
    } else {
        return (out, None, None);
    };
    let after = est.sv_pct_on(league_sv);
    if let Some(&pct) = out.get("SVPCT") {
        out["SVPCT"] = if pct > 1.0 { after * 100.0 } else { after };
    }
    if out.contains_key("SV") {
        out["SV"] = sa * after;
    }
    if out.contains_key("GA") {
        out["GA"] = sa * (1.0 - after);
    }
    if let Some(&gaa) = out.get("GAA") {
        if before < 1.0 {
            out["GAA"] = gaa * (1.0 - after) / (1.0 - before);
        }
    }
    (out, Some(before), Some(after))
}

/// Goalie per-game rates with the team-dependent parts taken from his current team: `W =
/// wins_per_start x starts per game` and `SA = shots_per_start x starts per game`, where starts per
/// game is his own GS rate (1 without one, capped at 1). His own save % (SV / (SV + GA), else
/// SV / SA or 1 - GA / SA) is kept, so `SV = SA x sv`, `GA = SA x (1 - sv)` and GAA scales with GA. Only keys already in
/// `rates` change; without a W rate they are returned unchanged. Not in the Python.
pub fn apply_team_goalie_rates(rates: &Map<String, f64>, wins_per_start: f64, shots_per_start: f64) -> Map<String, f64> {
    let mut out = rates.clone();
    if !out.contains_key("W") {
        return out;
    }
    let per_gp = out.get("GS").copied().unwrap_or(1.0).clamp(0.0, 1.0);
    out.insert("W".to_string(), wins_per_start * per_gp);
    // SV + GA first: blended rates can carry an SA rate from different lines than SV and GA (a
    // Fantrax projection has no SA), and his save % must come from the same shots as his saves
    let sa_old = (|| Some(out.get("SV")? + out.get("GA")?))().or_else(|| out.get("SA").copied()).filter(|x| *x > 0.0);
    let Some(sa_old) = sa_old else { return out };
    let sv = match (out.get("SV"), out.get("GA")) {
        (Some(sv), _) => sv / sa_old,
        (None, Some(ga)) => 1.0 - ga / sa_old,
        (None, None) => return out,
    };
    let sa = shots_per_start * per_gp;
    let ga_old = out.get("GA").copied();
    if out.contains_key("SA") {
        out.insert("SA".to_string(), sa);
    }
    if out.contains_key("SV") {
        out.insert("SV".to_string(), sa * sv);
    }
    if out.contains_key("GA") {
        out.insert("GA".to_string(), sa * (1.0 - sv));
    }
    if let (Some(gaa), Some(ga_old)) = (out.get("GAA").copied(), ga_old.filter(|g| *g > 0.0)) {
        out.insert("GAA".to_string(), gaa * sa * (1.0 - sv) / ga_old);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(xs: &[(&str, f64)]) -> Map<String, f64> {
        xs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} vs {b}");
    }

    #[test]
    fn team_goalie_rates_replace_wins_and_shots_and_keep_save_pct() {
        let r = m(&[("W", 0.4), ("GS", 0.9), ("SA", 28.0), ("SV", 25.2), ("GA", 2.8), ("GAA", 2.8), ("SO", 0.05)]);
        let out = apply_team_goalie_rates(&r, 0.55, 30.0);
        close(out["W"], 0.55 * 0.9);
        close(out["SA"], 27.0);
        close(out["SV"], 24.3); // .900 of 27 shots
        close(out["GA"], 2.7);
        close(out["GAA"], 2.8 * 2.7 / 2.8);
        close(out["SO"], 0.05);
        // no SA key: shots are SV + GA, and no GS rate means every game is a start
        let out = apply_team_goalie_rates(&m(&[("W", 0.5), ("SV", 27.0), ("GA", 3.0)]), 0.45, 25.0);
        close(out["W"], 0.45);
        close(out["SV"], 22.5);
        close(out["GA"], 2.5);
        assert!(!out.contains_key("SA"));
        // an SA rate from other lines than SV and GA: save % from SV / (SV + GA) = 24 / 26
        let out = apply_team_goalie_rates(&m(&[("W", 0.5), ("SA", 30.0), ("SV", 24.0), ("GA", 2.0)]), 0.5, 26.0);
        close(out["SA"], 26.0);
        close(out["SV"], 24.0);
        close(out["GA"], 2.0);
        // no wins rate: unchanged
        let r = m(&[("SV", 27.0), ("GA", 3.0)]);
        assert_eq!(apply_team_goalie_rates(&r, 0.45, 25.0), r);
    }
}
