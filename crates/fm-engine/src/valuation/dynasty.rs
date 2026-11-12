//! Rust port of `fantasy_manager/valuation/dynasty.py`: dynasty / keeper value (production-by-age
//! trajectory over a weighted multi-year horizon, draft-pedigree boost, market prior, keeper-league
//! mode). `DynastyValue` lives in `fm_core::values`.
//! Owner: work package `W4-valuate` (wave 4).
//!
//! See the Python module docstring for the model. Deviations (no behavioural differences):
//! * `PRODUCTION_CURVES` F / D come from `params::dynasty_age_curves()` at call time (Python reads
//!   them once at import); [`ProductionCurves::current`] snapshots them per `apply_dynasty` call.
//! * `ages` / `mode` keyword arguments are `Option`s; `values` is always `Map<Cid, PlayerValue>`.

use std::sync::atomic::{AtomicBool, Ordering};

use chrono::NaiveDate;
use fm_core::models::{FantasyTeam, LeagueContext, Player, Reason};
use fm_core::values::{DynastyValue, Horizon, PlayerValue};
use fm_core::Map;
use fm_pycompat::fmt::{fixed, general, percent};
use fm_pycompat::num::py_sum;
use fm_pycompat::order::{cmp_f64, sort_desc_by};

use super::params;
use super::replacement::best_slot;
use super::valuate::replacement_for;

pub const DISCOUNT: f64 = 0.8;
pub const MAX_GROWTH: f64 = 2.5;
pub const MIN_GP_FOR_RATE: i64 = 10;
pub const DEFAULT_MODE: &str = "contend";

/// `MODE_WEIGHTS[mode]`: (year weights, terminal factor); None for an unknown mode.
pub fn mode_base(mode: &str) -> Option<(&'static [f64], f64)> {
    Some(match mode {
        "contend" => (&[1.0, 0.65, 0.45], 0.8),
        "balanced" => (&[1.0, 0.8, 0.64], 1.5),
        "rebuild" => (&[0.7, 0.9, 0.9], 2.0),
        _ => return None,
    })
}

/// Hand-set goalie production curve. Flat at the peak for every age up to 32: goalies get no youth
/// growth term (07 implication 3; goalie performance "varies little by age"), unlike the Python
/// curve, which rose from 0.55 at 18 to 1.00 at 27. The decline after 32 is unchanged.
pub const GOALIE_CURVE: [(f64, f64); 4] = [(18.0, 1.00), (32.0, 1.00), (34.0, 0.85), (36.0, 0.70)];
/// The Python goalie curve, used only while [`disable_positional_dynasty`] is on.
const PYTHON_GOALIE_CURVE: [(f64, f64); 8] =
    [(18.0, 0.55), (20.0, 0.65), (22.0, 0.75), (24.0, 0.85), (27.0, 1.00), (32.0, 1.00), (34.0, 0.85), (36.0, 0.70)];

static POSITIONAL_OFF: AtomicBool = AtomicBool::new(false);

/// Turn the positional dynasty rules (keeper spot at the player's own VORP slot, flat goalie curve,
/// goalie and defenseman pedigree) off (true) or back on for every later valuation in this
/// process. Only the Python parity tests and the CLI's hidden `--from-snapshot` mode turn them
/// off: their fixtures come from Python, which used one spot, one pedigree rule and a rising
/// goalie curve for every position.
pub fn disable_positional_dynasty(off: bool) {
    POSITIONAL_OFF.store(off, Ordering::Relaxed);
}

pub(crate) fn positional_off() -> bool {
    POSITIONAL_OFF.load(Ordering::Relaxed)
}

pub const PEDIGREE_MAX_AGE: f64 = 23.0;
pub const PEDIGREE_FADE_AGE: f64 = 25.0;
pub const PEDIGREE_PROVEN_GP: f64 = 200.0;
/// Defensemen show their real level at about 400 career NHL games, against 200 for forwards
/// (07 section 3 and implication 5, Dobber's breakout threshold), so their pedigree fades over 400.
pub const PEDIGREE_PROVEN_GP_D: f64 = 400.0;
/// Share of the skater first-round pedigree premium a goalie drafted in rounds 1-2 keeps. File 07
/// says only that goalie pedigree is "much flatter by slot and weaker overall" (implication 4) and
/// gives no size, so this is a judgment: it borrows the weak goalie draft-position to career-games
/// correlation, |r| = 0.24 (07 "Goalie prospects", Canucks Army), as the scale of "weaker". A
/// correlation is not a share of a premium, and nothing validates the number. Goalie outcomes are
/// level by slot after round 2 (Schuckers), so every goalie taken in rounds 1-2 gets
/// 1 + 0.24 x (1.10 - 1) = 1.024 and later goalies get none.
pub const GOALIE_PEDIGREE_SHARE: f64 = 0.24;
/// The skater first-round tier of [`pedigree_base`], the basis of the goalie premium.
const FIRST_ROUND_PEDIGREE: f64 = 1.10;

pub const ROOKIE_UPSIDE_FLOOR: f64 = 0.6;

pub const MARKET_WEIGHT: f64 = 0.35;
pub const MARKET_MIN_PLAYERS: usize = 50;

/// `PRODUCTION_CURVES`: F and D (fitted, from params), G (hand-set).
#[derive(Clone, Debug)]
pub struct ProductionCurves {
    pub f: Vec<(f64, f64)>,
    pub d: Vec<(f64, f64)>,
    pub g: Vec<(f64, f64)>,
}

impl ProductionCurves {
    /// The curves of the current params.
    pub fn current() -> Self {
        let c = params::dynasty_age_curves(None);
        ProductionCurves {
            f: c.get("F").cloned().unwrap_or_default(),
            d: c.get("D").cloned().unwrap_or_default(),
            g: if positional_off() { PYTHON_GOALIE_CURVE.to_vec() } else { GOALIE_CURVE.to_vec() },
        }
    }

    /// `PRODUCTION_CURVES.get(group, PRODUCTION_CURVES["F"])`.
    pub fn get(&self, group: &str) -> &[(f64, f64)] {
        match group {
            "D" => &self.d,
            "G" => &self.g,
            _ => &self.f,
        }
    }

    /// Production relative to the positional peak (unknown group -> F); 1.0 when age is unknown.
    pub fn production_curve(&self, group: &str, age: Option<f64>) -> f64 {
        let Some(age) = age else { return 1.0 };
        let pts = self.get(group);
        let (Some(first), Some(last)) = (pts.first(), pts.last()) else { return 1.0 };
        if age <= first.0 {
            return first.1;
        }
        if age >= last.0 {
            return last.1;
        }
        for w in pts.windows(2) {
            let ((a0, m0), (a1, m1)) = (w[0], w[1]);
            if a0 <= age && age <= a1 {
                return m0 + (m1 - m0) * (age - a0) / (a1 - a0);
            }
        }
        1.0 // unreachable
    }

    /// curve(age + years) / curve(age); 1.0 when age is unknown.
    pub fn growth_ratio(&self, group: &str, age: Option<f64>, years: f64) -> f64 {
        let Some(a) = age else { return 1.0 };
        self.production_curve(group, Some(a + years)) / self.production_curve(group, Some(a))
    }

    /// Future-season multiplier on today's healthy FPG: growth ratio x pedigree, capped at MAX_GROWTH.
    pub fn trajectory_factor(&self, group: &str, age: Option<f64>, years: f64, pedigree: f64) -> f64 {
        MAX_GROWTH.min(self.growth_ratio(group, age, years) * pedigree)
    }
}

pub fn position_group(p: &Player) -> &'static str {
    super::valuate::position_group(p)
}

/// `production_curve(group, age)` on the current params.
pub fn production_curve(group: &str, age: Option<f64>) -> f64 {
    ProductionCurves::current().production_curve(group, age)
}

/// Backwards-compatible name.
pub fn age_multiplier(group: &str, age: Option<f64>) -> f64 {
    production_curve(group, age)
}

pub fn growth_ratio(group: &str, age: Option<f64>, years: f64) -> f64 {
    ProductionCurves::current().growth_ratio(group, age, years)
}

pub fn trajectory_factor(group: &str, age: Option<f64>, years: f64, pedigree: f64) -> f64 {
    ProductionCurves::current().trajectory_factor(group, age, years, pedigree)
}

/// (year weights for years 0..years-1, terminal factor) for a dynasty mode. Longer horizons
/// extend the list geometrically with its last ratio (0.8 for a flat tail).
pub fn mode_weights(mode: Option<&str>, years: i64) -> (Vec<f64>, f64) {
    let (base, terminal) = mode_base(mode.filter(|m| !m.is_empty()).unwrap_or(DEFAULT_MODE))
        .unwrap_or_else(|| mode_base(DEFAULT_MODE).expect("default mode"));
    let mut w: Vec<f64> = base.to_vec();
    let n = w.len();
    let mut ratio = if n >= 2 && w[n - 2] > 0.0 { w[n - 1] / w[n - 2] } else { DISCOUNT };
    if ratio >= 1.0 {
        ratio = DISCOUNT;
    }
    while (w.len() as i64) < years {
        let last = *w.last().unwrap();
        w.push(last * ratio);
    }
    w.truncate(years.max(1) as usize);
    (w, terminal)
}

/// (multiplier, label) from draft position alone.
pub fn pedigree_base(p: &Player) -> (f64, String) {
    let (ov, rnd) = (p.draft_overall, p.draft_round);
    if let Some(o) = ov.filter(|o| *o <= 3) {
        return (1.30, format!("#{o} overall pick"));
    }
    if let Some(o) = ov.filter(|o| *o <= 10) {
        return (1.20, format!("#{o} overall pick"));
    }
    if ov.is_some_and(|o| o <= 32) || rnd == Some(1) {
        return (1.10, ov.map(|o| format!("1st-round pick (#{o})")).unwrap_or_else(|| "1st-round pick".into()));
    }
    if rnd == Some(2) {
        return (1.03, ov.map(|o| format!("2nd-round pick (#{o})")).unwrap_or_else(|| "2nd-round pick".into()));
    }
    (1.0, if ov.is_some() { "later-round pick" } else { "undrafted / unknown" }.into())
}

/// Draft-pedigree boost to the growth trajectory, fading to 1.0 by age 25 and by 200 career NHL
/// games (400 for defensemen). Goalies get only the flat, weak premium of
/// [`GOALIE_PEDIGREE_SHARE`]. Unlike Python, which used the skater tiers and 200 games for every
/// position. (1.0, None) when it does not apply.
pub fn pedigree_multiplier(p: &Player, age: Option<f64>) -> (f64, Option<String>) {
    let (mut base, label) = pedigree_base(p);
    let group = position_group(p);
    let off = positional_off();
    if !off && group == "G" && base > 1.0 {
        base = 1.0 + (FIRST_ROUND_PEDIGREE - 1.0) * GOALIE_PEDIGREE_SHARE;
    }
    let proven_gp = if !off && group == "D" { PEDIGREE_PROVEN_GP_D } else { PEDIGREE_PROVEN_GP };
    let Some(age) = age else { return (1.0, None) };
    if base <= 1.0 || age >= PEDIGREE_FADE_AGE {
        return (1.0, None);
    }
    let f_age = if age <= PEDIGREE_MAX_AGE {
        1.0
    } else {
        (PEDIGREE_FADE_AGE - age) / (PEDIGREE_FADE_AGE - PEDIGREE_MAX_AGE)
    };
    let gp = p.career_gp.unwrap_or(0);
    let f_gp = (1.0 - gp as f64 / proven_gp).max(0.0);
    let mult = 1.0 + (base - 1.0) * f_age * f_gp;
    if mult <= 1.0 + 1e-9 {
        return (1.0, None);
    }
    let year = match p.draft_year {
        Some(y) if y != 0 => format!(" {y}"),
        _ => String::new(),
    };
    (mult, Some(format!("{label}{year}, {gp} NHL GP")))
}

pub fn age_on(birth_date: Option<NaiveDate>, as_of: NaiveDate) -> Option<f64> {
    birth_date.map(|b| (as_of - b).num_days() as f64 / 365.25)
}

/// PTS/GP for skaters, SV% for goalies from the first of season / prior / projected with at least
/// MIN_GP_FOR_RATE games; None when every line is a small sample.
pub fn production_rate(p: &Player) -> Option<f64> {
    let line = ["season", "prior", "projected"].iter().filter_map(|s| p.lines.get(*s)).find(|l| l.gp >= MIN_GP_FOR_RATE)?;
    let st = &line.stats;
    if p.is_goalie() {
        let mut sv = st.get("SVPCT").copied();
        if sv.is_none() {
            if let Some(&sa) = st.get("SA").filter(|x| **x != 0.0) {
                sv = Some(st.get("SV").copied().unwrap_or(sa - st.get("GA").copied().unwrap_or(0.0)) / sa);
            }
        }
        return sv;
    }
    let mut pts = st.get("PTS").copied();
    if pts.is_none() && (st.contains_key("G") || st.contains_key("A")) {
        pts = Some(st.get("G").copied().unwrap_or(0.0) + st.get("A").copied().unwrap_or(0.0));
    }
    pts.map(|x| x / line.gp as f64)
}

/// Trajectory value: w_0 * fpg + sum_{y>=1} w_y * later * g(y) * ped + terminal term.
pub fn horizon_value(
    fpg: f64,
    group: &str,
    age: Option<f64>,
    years: i64,
    later_fpg: Option<f64>,
    mode: Option<&str>,
    pedigree: f64,
) -> f64 {
    let curves = ProductionCurves::current();
    trajectory(&curves, fpg, group, age, years.max(1), later_fpg.unwrap_or(fpg), mode, pedigree).0
}

/// `_trajectory`: (total, flat total without growth / pedigree, terminal term).
#[allow(clippy::too_many_arguments)]
fn trajectory(
    curves: &ProductionCurves,
    fpg: f64,
    group: &str,
    age: Option<f64>,
    years: i64,
    later: f64,
    mode: Option<&str>,
    ped: f64,
) -> (f64, f64, f64) {
    let (w, terminal_factor) = mode_weights(mode, years);
    let mut total = w[0] * fpg;
    let mut flat = w[0] * fpg;
    for y in 1..years {
        total += w[y as usize] * later * curves.trajectory_factor(group, age, y as f64, ped);
        flat += w[y as usize] * later;
    }
    let d = DISCOUNT.powf(years as f64);
    let term = d * later * curves.trajectory_factor(group, age, years as f64, ped) * terminal_factor;
    total += term;
    flat += d * later * terminal_factor;
    (total, flat, term)
}

/// `_healthy_fpg`: fpg x goalie start share (no availability discount).
pub fn healthy_fpg(pv: &PlayerValue) -> f64 {
    pv.fpg * pv.start_share.unwrap_or(1.0)
}

/// Rank-to-value transform: the player with the i-th highest % owned gets the i-th highest model
/// value (among every modelled player). Ties in % owned share their mean rank value.
pub fn market_values(model: &Map<String, f64>, owned: &Map<String, f64>) -> Map<String, f64> {
    let mut ranked: Vec<f64> = model.values().copied().collect();
    sort_desc_by(&mut ranked, |a, b| cmp_f64(*a, *b));
    let mut out = Map::new();
    if ranked.is_empty() {
        return out;
    }
    let mut cids: Vec<&String> = owned.keys().filter(|c| model.contains_key(*c)).collect();
    cids.sort_by(|a, b| cmp_f64(-owned[*a], -owned[*b]));
    let mut i = 0;
    while i < cids.len() {
        let mut j = i;
        while j + 1 < cids.len() && owned[cids[j + 1]] == owned[cids[i]] {
            j += 1;
        }
        let vals: Vec<f64> = (i..=j).map(|k| ranked[k.min(ranked.len() - 1)]).collect();
        let mean = py_sum(vals.iter().copied()) / vals.len() as f64;
        for c in &cids[i..=j] {
            out.insert((*c).clone(), mean);
        }
        i = j + 1;
    }
    out
}

/// `ctx.keepers_per_team` as an int >= 0, or None for a keep-everyone dynasty.
pub fn keeper_count(ctx: &LeagueContext) -> Option<i64> {
    ctx.keepers_per_team.map(|k| k.max(0))
}

/// The league's lowest replacement FPG this season (min over slots); 0.0 without one.
pub fn min_replacement_fpg(ctx: &LeagueContext, values: &Map<String, PlayerValue>) -> f64 {
    let levels = replacement_for(ctx, values, Horizon::Season);
    levels.values().copied().reduce(|a, b| if b < a { b } else { a }).unwrap_or(0.0)
}

/// Future value of a waiver-level roster spot: `min_replacement_fpg` held flat over the seasons
/// after this one (`sum_{y>=1} w_y + 0.8^H * T`).
pub fn spot_future(ctx: &LeagueContext, values: &Map<String, PlayerValue>, mode: Option<&str>) -> f64 {
    min_replacement_fpg(ctx, values) * future_weight(ctx, mode.unwrap_or(ctx.dynasty_mode.as_str()))
}

/// Total weight of the seasons after this one, `sum_{y>=1} w_y + 0.8^H * T`: the future value of
/// 1 FPG held flat.
fn future_weight(ctx: &LeagueContext, mode: &str) -> f64 {
    let years = ctx.keeper_horizon_years.max(1);
    let (w, terminal) = mode_weights(Some(mode), years);
    py_sum(w[1..].iter().copied()) + DISCOUNT.powf(years as f64) * terminal
}

/// (slot, replacement FPG) at the player's own VORP slot: the cheapest slot he is eligible for,
/// as `vorp` uses. A player who fits no slot gets the league's lowest level (slot "any").
pub fn own_replacement(p: &Player, levels: &Map<String, f64>) -> (String, f64) {
    match best_slot(&p.positions, levels).filter(|_| !positional_off()) {
        Some(slot) => {
            let r = levels[&slot];
            (slot, r)
        }
        None => ("any".to_string(), levels.values().copied().reduce(f64::min).unwrap_or(0.0)),
    }
}

fn ctx_years(ctx: &LeagueContext) -> i64 {
    // `max(1, int(ctx.keeper_horizon_years or 1))`
    if ctx.keeper_horizon_years == 0 {
        1
    } else {
        ctx.keeper_horizon_years.max(1)
    }
}

/// Dynasty value for every PlayerValue. `ages` ({cid: age}) fills in missing birth dates; `mode`
/// overrides `ctx.dynasty_mode`. Keeper leagues value non-keepers on this season only.
pub fn apply_dynasty(
    values: &Map<String, PlayerValue>,
    ctx: &LeagueContext,
    ages: Option<&Map<String, f64>>,
    mode: Option<&str>,
) -> Map<String, DynastyValue> {
    let curves = ProductionCurves::current();
    let years = ctx_years(ctx);
    let mode: String = mode.filter(|m| !m.is_empty()).unwrap_or(ctx.dynasty_mode.as_str()).to_string();
    let (weights, _) = mode_weights(Some(&mode), years);
    let k = keeper_count(ctx);
    let wtxt = weights.iter().map(|x| general(*x, None, false)).collect::<Vec<_>>().join("/");
    let mut out: Map<String, DynastyValue> = Map::with_capacity(values.len());
    for (cid, pv) in values {
        let p = &pv.player;
        let group = position_group(p);
        let fpg = pv.fpg_season;
        let mut age = age_on(p.birth_date, ctx.as_of);
        let mut age_src = "birth date";
        if age.is_none() {
            if let Some(a) = ages.filter(|a| !a.is_empty()).and_then(|a| a.get(cid)) {
                age = Some(*a);
                age_src = "league-reported age";
            }
        }
        let mut reasons: Vec<Reason> = Vec::new();
        let level = match age {
            None => {
                reasons.push(Reason::new("AGE_UNKNOWN", "Age unknown: flat production trajectory").with_value(Some(1.0)));
                1.0
            }
            Some(a) => {
                let level = curves.production_curve(group, Some(a));
                let g_end = curves.growth_ratio(group, Some(a), years as f64);
                reasons.push(
                    Reason::new(
                        "AGE_CURVE",
                        format!(
                            "Age {} ({group}, {age_src}): at {} of peak production, x{} of today's rate by year {years}",
                            fixed(a, 1, false),
                            percent(level, 0, false),
                            fixed(g_end, 2, false)
                        ),
                    )
                    .with_value(Some(level))
                    .with_baseline(Some(1.0)),
                );
                level
            }
        };
        let (ped, ped_text) = pedigree_multiplier(p, age);
        if let Some(t) = ped_text {
            reasons.push(
                Reason::new("PEDIGREE", format!("Pedigree: {t}: future years x{}", fixed(ped, 2, false)))
                    .with_value(Some(ped))
                    .with_baseline(Some(1.0)),
            );
        }
        let healthy = healthy_fpg(pv);
        let (mut base, flat, term) = trajectory(&curves, fpg, group, age, years, healthy, Some(&mode), ped);
        if let Some(conf) = pv.rookie.as_ref().map(|r| r.confidence) {
            if (base - flat).abs() > 1e-9 {
                let f = ROOKIE_UPSIDE_FLOOR + (1.0 - ROOKIE_UPSIDE_FLOOR) * conf.clamp(0.0, 1.0);
                let scaled = flat + (base - flat) * f;
                reasons.push(
                    Reason::new(
                        "ROOKIE_CONFIDENCE",
                        format!(
                            "Rookie model confidence {}: upside {} x{} -> {}",
                            percent(conf, 0, false),
                            fixed(base - flat, 2, true),
                            fixed(f, 2, false),
                            fixed(scaled - flat, 2, true)
                        ),
                    )
                    .with_value(Some(f))
                    .with_baseline(Some(1.0)),
                );
                base = scaled;
            }
        }
        let later = if years > 1 && (healthy - fpg).abs() > 1e-9 {
            format!(", {} healthy in later years", fixed(healthy, 2, false))
        } else {
            String::new()
        };
        let season = weights[0] * fpg;
        let htext = if k.is_none() {
            format!(
                "{years}-year {mode} value {} (weights {wtxt} + {} beyond year {years}) from {} FPG{later}",
                fixed(base, 2, false),
                fixed(term, 2, false),
                fixed(fpg, 2, false)
            )
        } else {
            format!(
                "{years}-year {mode} trajectory {} = this season {} + future {} (weights {wtxt} + {} beyond year \
                 {years}) from {} FPG{later}; the future counts only for a keeper",
                fixed(base, 2, false),
                fixed(season, 2, false),
                fixed(base - season, 2, false),
                fixed(term, 2, false),
                fixed(fpg, 2, false)
            )
        };
        reasons.push(Reason::new("DYNASTY_HORIZON", htext).with_value(Some(base)).with_baseline(Some(fpg)));
        out.insert(
            cid.clone(),
            DynastyValue {
                player: p.clone(),
                value: base,
                age,
                age_mult: level,
                upside: base - flat,
                horizon_years: years,
                model_value: Some(base),
                market_value: None,
                pedigree: ped,
                mode: mode.clone(),
                season_part: season,
                future: base - season,
                keeper_surplus: base - season,
                keeper: true,
                reasons,
            },
        );
    }
    let Some(k) = k else {
        apply_market(&mut out, ctx, Field::Value);
        for dv in out.values_mut() {
            dv.future = dv.value - dv.season_part;
            dv.keeper_surplus = dv.future;
        }
        return out;
    };
    apply_keepers(&mut out, values, ctx, k, &mode);
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Value,
    Future,
    /// `future` while it holds the future over the player's positional spot (keeper leagues)
    FutureOverSpot,
}

impl Field {
    fn get(self, dv: &DynastyValue) -> f64 {
        match self {
            Field::Value => dv.value,
            Field::Future | Field::FutureOverSpot => dv.future,
        }
    }
    fn set(self, dv: &mut DynastyValue, x: f64) {
        match self {
            Field::Value => dv.value = x,
            Field::Future | Field::FutureOverSpot => dv.future = x,
        }
    }
}

/// `_apply_market(out, ctx, field)`: blend `field` with its market counterpart.
fn apply_market(out: &mut Map<String, DynastyValue>, ctx: &LeagueContext, field: Field) {
    let mut owned: Map<String, f64> = Map::new();
    for p in ctx.all_players() {
        if let Some(pct) = p.pct_owned {
            if out.contains_key(&p.cid) {
                owned.insert(p.cid.clone(), pct);
            }
        }
    }
    if owned.len() < MARKET_MIN_PLAYERS {
        return;
    }
    let model: Map<String, f64> = out.iter().map(|(c, dv)| (c.clone(), field.get(dv))).collect();
    let market = market_values(&model, &owned);
    let (what, mdl) = match field {
        Field::Value => ("market value", "model"),
        Field::Future => ("market future value", "model future"),
        Field::FutureOverSpot => ("market future value over replacement", "model future over replacement"),
    };
    for (cid, mv) in &market {
        let dv = out.get_mut(cid).expect("market cid in out");
        let cur = field.get(dv);
        let blended = (1.0 - MARKET_WEIGHT) * cur + MARKET_WEIGHT * mv;
        dv.reasons.push(
            Reason::new(
                "MARKET_PRIOR",
                format!(
                    "Rostered in {}% of leagues -> {what} {}; {} {mdl} {} + {} market = {}",
                    fixed(owned[cid], 0, false),
                    fixed(*mv, 2, false),
                    percent(1.0 - MARKET_WEIGHT, 0, false),
                    fixed(cur, 2, false),
                    percent(MARKET_WEIGHT, 0, false),
                    fixed(blended, 2, false)
                ),
            )
            .with_value(Some(*mv))
            .with_baseline(Some(owned[cid])),
        );
        dv.market_value = Some(*mv);
        field.set(dv, blended);
    }
}

/// English ordinal ("1st", "2nd", "11th", ...).
pub fn ordinal(n: i64) -> String {
    let suffix = if (10..=20).contains(&n.rem_euclid(100)) {
        "th"
    } else {
        match n.rem_euclid(10) {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{n}{suffix}")
}

/// `_apply_keepers`: keeper-league pass (market prior on the future part, surplus over a
/// waiver-level spot at the player's own position, keeper flags per team, free agents against my
/// team). Unlike Python, which used one spot at the league's lowest replacement level for every
/// position and blended the market into the raw future, the spot is the replacement level of the
/// player's own VORP slot and the market is blended into the future over that spot.
fn apply_keepers(out: &mut Map<String, DynastyValue>, values: &Map<String, PlayerValue>, ctx: &LeagueContext, k: i64, mode: &str) {
    let model_future: Map<String, f64> = out.iter().map(|(c, dv)| (c.clone(), dv.future)).collect();
    // A spot's future value is its replacement FPG held flat over the seasons after this one.
    let per = future_weight(ctx, mode);
    let levels = replacement_for(ctx, values, Horizon::Season);
    let spots: Map<String, (String, f64)> = out
        .iter()
        .map(|(c, dv)| {
            let (slot, r) = own_replacement(&dv.player, &levels);
            // Python's text names no slot (it had one spot for everyone)
            let label = if positional_off() { String::new() } else { format!("{slot} ") };
            (c.clone(), (label, r * per))
        })
        .collect();
    // Blend the market into the future over replacement, so equal ownership means equal surplus
    // whatever the position. With one spot for everyone (Python) this equals blending the raw
    // future, which is what the Python text describes.
    let shift = !positional_off();
    if shift {
        for (c, dv) in out.iter_mut() {
            dv.future -= spots[c].1;
        }
    }
    apply_market(out, ctx, if shift { Field::FutureOverSpot } else { Field::Future });
    for (c, dv) in out.iter_mut() {
        let s = spots[c].1;
        if shift {
            dv.future += s;
            dv.market_value = dv.market_value.map(|m| m + s);
        }
        dv.keeper_surplus = (dv.future - s).max(0.0);
    }
    let spot_of = |dv: &DynastyValue| spots[&dv.player.cid].1;
    let surplus_text = |dv: &DynastyValue| {
        let (slot, s) = &spots[&dv.player.cid];
        format!(
            "long-term surplus {} over a waiver-level {slot}spot (future {} - spot {})",
            fixed(dv.keeper_surplus, 2, true),
            fixed(dv.future, 2, false),
            fixed(*s, 2, false)
        )
    };
    let no_surplus = |dv: &DynastyValue| {
        let (slot, s) = &spots[&dv.player.cid];
        Reason::new(
            "NOT_KEEPER",
            format!(
                "No keeper value: future {} does not beat a waiver-level {slot}spot ({}); valued on this season only",
                fixed(dv.future, 2, false),
                fixed(*s, 2, false)
            ),
        )
        .with_value(Some(dv.keeper_surplus))
        .with_baseline(Some(0.0))
    };
    let no_keepers = |s: f64| {
        Reason::new("NOT_KEEPER", "No keepers in this league: valued on this season only")
            .with_value(Some(s))
            .with_baseline(Some(0.0))
    };
    let top = format!("top {k} keeper{}", if k == 1 { "" } else { "s" });
    let mut holder: Map<String, usize> = Map::new();
    let mut by_team: Map<String, Vec<String>> = Map::new();
    for (ti, t) in ctx.teams.iter().enumerate() {
        for p in t.players_iter() {
            if out.contains_key(&p.cid) && !holder.contains_key(&p.cid) {
                holder.insert(p.cid.clone(), ti);
                by_team.entry(t.team_id.clone()).or_default().push(p.cid.clone());
            }
        }
    }
    let empty: Vec<String> = Vec::new();
    for t in &ctx.teams {
        let mut ranked: Vec<&String> = by_team.get(&t.team_id).unwrap_or(&empty).iter().collect();
        ranked.sort_by(|a, b| cmp_f64(-out[*a].keeper_surplus, -out[*b].keeper_surplus));
        let kth = if 0 < k && k as usize <= ranked.len() { out[ranked[k as usize - 1]].keeper_surplus } else { 0.0 };
        for (i, cid) in ranked.iter().enumerate() {
            let dv = out.get_mut(*cid).unwrap();
            dv.keeper = (i as i64) < k && dv.keeper_surplus > 0.0;
            let r = if dv.keeper {
                Reason::new("KEEPER", format!("Keeper #{} of {k} on {}: {}", i + 1, t.name, surplus_text(dv)))
                    .with_value(Some(dv.keeper_surplus))
                    .with_baseline(Some(spot_of(dv)))
            } else if k == 0 {
                no_keepers(dv.keeper_surplus)
            } else if dv.keeper_surplus <= 0.0 {
                no_surplus(dv)
            } else {
                Reason::new(
                    "NOT_KEEPER",
                    format!(
                        "Not among {}'s {top} ({} keeper surplus {}): valued on this season only",
                        t.name,
                        ordinal(k),
                        fixed(kth, 2, false)
                    ),
                )
                .with_value(Some(dv.keeper_surplus))
                .with_baseline(Some(kth))
            };
            dv.reasons.push(r);
        }
    }

    let me: Option<&FantasyTeam> = ctx.my_team().ok();
    let my_name = me.map(|t| t.name.as_str()).unwrap_or("my team");
    let mut mine: Vec<f64> = match me {
        Some(t) => by_team.get(&t.team_id).unwrap_or(&empty).iter().map(|c| out[c].keeper_surplus).collect(),
        None => Vec::new(),
    };
    sort_desc_by(&mut mine, |a, b| cmp_f64(*a, *b));
    let mine_pos: Vec<f64> =
        if k > 0 { mine.iter().copied().filter(|s| *s > 0.0).take(k as usize).collect() } else { Vec::new() };
    let bar = if k > 0 && mine_pos.len() >= k as usize { mine_pos[k as usize - 1] } else { 0.0 };
    for (cid, dv) in out.iter_mut() {
        if holder.contains_key(cid) {
            continue;
        }
        let s = dv.keeper_surplus;
        dv.keeper = k > 0 && s > 0.0 && s > bar;
        let r = if dv.keeper {
            let rank = 1 + mine_pos.iter().filter(|x| **x >= s).count();
            Reason::new(
                "KEEPER",
                format!("Free agent: would be keeper #{rank} of {k} on {my_name}: {}", surplus_text(dv)),
            )
            .with_value(Some(s))
            .with_baseline(Some(spot_of(dv)))
        } else if k == 0 {
            no_keepers(s)
        } else if s <= 0.0 {
            no_surplus(dv)
        } else {
            Reason::new(
                "NOT_KEEPER",
                format!(
                    "Free agent: would not crack {my_name}'s {top} ({} keeper surplus {}): valued on this season only",
                    ordinal(k),
                    fixed(bar, 2, false)
                ),
            )
            .with_value(Some(s))
            .with_baseline(Some(bar))
        };
        dv.reasons.push(r);
    }

    for (cid, dv) in out.iter_mut() {
        let model_surplus = (model_future[cid] - spots[cid].1).max(0.0);
        dv.model_value = Some(dv.season_part + if dv.keeper { model_surplus } else { 0.0 });
        dv.value = dv.season_part + if dv.keeper { dv.keeper_surplus } else { 0.0 };
    }
}

/// Long-term value a roster carries past this season: the sum of the `k` largest
/// `keeper_surplus` among `players` (`k` None: every player's surplus counts).
pub fn keeper_total<'a>(players: impl IntoIterator<Item = &'a Player>, dyn_values: &Map<String, DynastyValue>, k: Option<i64>) -> f64 {
    let mut xs: Vec<f64> = players.into_iter().filter_map(|p| dyn_values.get(&p.cid)).map(|d| d.keeper_surplus).collect();
    sort_desc_by(&mut xs, |a, b| cmp_f64(*a, *b));
    match k {
        None => py_sum(xs),
        Some(k) => py_sum(xs.into_iter().take(k.max(0) as usize)),
    }
}

/// Dynasty values, best first (stable).
pub fn dynasty_rank<'a>(values: impl IntoIterator<Item = &'a DynastyValue>) -> Vec<&'a DynastyValue> {
    let mut items: Vec<&DynastyValue> = values.into_iter().collect();
    sort_desc_by(&mut items, |a, b| cmp_f64(a.value, b.value));
    items
}

/// Builders shared by the valuation unit tests (dynasty, picks, valuate).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use chrono::NaiveDate;
    use fm_core::models::{LeagueContext, Player};
    use fm_core::values::PlayerValue;
    use fm_core::Map;
    use serde_json::{json, Value};

    pub fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, dd).unwrap()
    }

    /// A Player from JSON overrides (`name`/`name_norm` default to the cid, team "BOS").
    pub fn player(v: Value) -> Player {
        let mut base = json!({"ids": {}, "team": "BOS", "positions": ["C"]});
        for (k, x) in v.as_object().unwrap() {
            base[k] = x.clone();
        }
        for k in ["name", "name_norm"] {
            if base.get(k).is_none() {
                base[k] = base["cid"].clone();
            }
        }
        serde_json::from_value(base).unwrap()
    }

    /// `{"split": .., "gp": .., "stats": {...}}`.
    pub fn line(split: &str, gp: i64, stats: Value) -> Value {
        json!({"split": split, "gp": gp, "stats": stats})
    }

    /// PlayerValue(fpg, fpg_season = season or fpg, fpg_week = fpg_season, vorp 0).
    pub fn pv(p: &Player, fpg: f64, season: Option<f64>) -> PlayerValue {
        let s = season.unwrap_or(fpg);
        PlayerValue {
            player: p.clone(),
            fpg,
            fpg_season: s,
            ir_fill: 0.0,
            fpg_error: 0.0,
            fpg_week: s,
            vorp: 0.0,
            vorp_week: 0.0,
            proj_week: None,
            games_next7: None,
            offnight_next7: None,
            start_share: None,
            return_date: None,
            avail_week: 1.0,
            share_source: None,
            share_starts: None,
            share_team_games: None,
            horizon_values: Map::new(),
            rates: Map::new(),
            reasons: vec![],
            rookie: None,
        }
    }

    pub fn values(pvs: Vec<PlayerValue>) -> Map<String, PlayerValue> {
        pvs.into_iter().map(|v| (v.player.cid.clone(), v)).collect()
    }

    /// A LeagueContext from JSON overrides of a minimal points league.
    pub fn ctx(v: Value) -> LeagueContext {
        let mut base = json!({
            "provider": "t", "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {}}, "roster_shape": {},
            "teams": [], "free_agents": [], "matchup_period": null, "as_of": "2026-10-01",
        });
        for (k, x) in v.as_object().unwrap() {
            base[k] = x.clone();
        }
        let mut c: LeagueContext = serde_json::from_value(base).unwrap();
        c.enrich = Arc::new(Default::default());
        c
    }

    /// One team of `slots` = [(slot, player)].
    pub fn team(id: &str, name: &str, me: bool, slots: &[(&str, &Player)]) -> Value {
        let s: Vec<Value> = slots
            .iter()
            .map(|(slot, p)| json!({"slot": slot, "player": p, "starting": !matches!(*slot, "BN" | "IR")}))
            .collect();
        json!({"team_id": id, "name": name, "owner_is_me": me, "slots": s})
    }

    pub fn close(a: f64, b: f64) {
        assert!((a - b).abs() <= 1e-9_f64.max(1e-9 * b.abs()), "{a} != {b}");
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use serde_json::json;

    const AS_OF: (i32, u32, u32) = (2026, 10, 1);

    pub(crate) fn born(age: i32) -> String {
        d(AS_OF.0 - age, AS_OF.1, AS_OF.2).to_string()
    }

    fn sk(cid: &str, pos: &[&str], age: Option<i32>, extra: serde_json::Value) -> Player {
        let mut v = json!({"cid": cid, "positions": pos, "birth_date": age.map(born),
                           "lines": {"season": line("season", 40, json!({"GP": 40}))}});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        player(v)
    }

    fn dctx(players: &[&Player], horizon: i64, mode: &str) -> LeagueContext {
        let slots: Vec<(&str, &Player)> = players.iter().map(|p| ("BN", *p)).collect();
        ctx(json!({"teams": [team("1", "a", true, &slots)], "dynasty": true,
                   "keeper_horizon_years": horizon, "dynasty_mode": mode}))
    }

    #[test]
    fn production_curve_anchors() {
        let cases = [
            ("F", 18.0, 0.762), ("F", 19.0, 0.762), ("F", 20.0, (0.762 + 0.897) / 2.0), ("F", 21.0, 0.897),
            ("F", 24.0, 0.99), ("F", 25.0, 1.00), ("F", 26.0, (1.0 + 0.969) / 2.0), ("F", 29.0, 0.897),
            ("F", 33.0, 0.697), ("F", 35.0, 0.576), ("F", 40.0, 0.459), ("F", 16.0, 0.762),
            ("D", 20.0, (0.848 + 0.946) / 2.0), ("D", 23.0, 1.00), ("D", 29.0, 0.943), ("D", 33.0, 0.831),
            ("D", 35.0, 0.746), ("G", 18.0, 1.00), ("G", 22.0, 1.00), ("G", 24.0, 1.00), ("G", 30.0, 1.00), ("G", 34.0, 0.85),
            ("G", 36.0, 0.70), ("G", 40.0, 0.70),
        ];
        for (g, a, lv) in cases {
            close(production_curve(g, Some(a)), lv);
        }
        assert_eq!(production_curve("F", None), 1.0);
        close(production_curve("X", Some(21.0)), 0.897); // unknown group -> forward curve
        assert_eq!(growth_ratio("F", None, 2.0), 1.0);
    }

    #[test]
    fn curves_rise_to_peak_then_decline() {
        let c = ProductionCurves::current();
        for g in ["F", "D", "G"] {
            let pts = c.get(g);
            let peak: Vec<f64> = pts.iter().filter(|p| p.1 == 1.0).map(|p| p.0).collect();
            let lo = peak.iter().cloned().fold(f64::INFINITY, f64::min);
            let hi = peak.iter().cloned().fold(0.0, f64::max);
            let vals: Vec<(f64, f64)> =
                (64..168).map(|x| x as f64 / 4.0).map(|a| (a, c.production_curve(g, Some(a)))).collect();
            let pre: Vec<f64> = vals.iter().filter(|v| v.0 <= lo).map(|v| v.1).collect();
            let post: Vec<f64> = vals.iter().filter(|v| v.0 >= hi).map(|v| v.1).collect();
            assert!(pre.windows(2).all(|w| w[0] <= w[1] + 1e-12), "{g}");
            assert!(post.windows(2).all(|w| w[0] >= w[1] - 1e-12), "{g}");
            assert!(vals.iter().filter(|v| lo <= v.0 && v.0 <= hi).all(|v| (v.1 - 1.0).abs() < 1e-9));
        }
        assert!(production_curve("D", Some(21.0)) > production_curve("F", Some(21.0)));
        assert!(production_curve("D", Some(33.0)) > production_curve("F", Some(33.0)));
    }

    #[test]
    fn growth_ratio_and_cap() {
        close(growth_ratio("F", Some(19.0), 2.0), 0.897 / 0.762);
        close(trajectory_factor("F", Some(19.0), 2.0, 1.0), 0.897 / 0.762);
        close(trajectory_factor("F", Some(17.0), 8.0, 1.3), 1.3 / 0.762);
        assert!(growth_ratio("F", Some(17.0), 8.0) * 2.0 > MAX_GROWTH);
        close(trajectory_factor("F", Some(17.0), 8.0, 2.0), MAX_GROWTH);
    }

    #[test]
    fn mode_weights_cases() {
        assert_eq!(mode_weights(Some("contend"), 3), (vec![1.0, 0.65, 0.45], 0.8));
        assert_eq!(mode_weights(Some("balanced"), 3), (vec![1.0, 0.8, 0.64], 1.5));
        assert_eq!(mode_weights(Some("rebuild"), 3), (vec![0.7, 0.9, 0.9], 2.0));
        assert_eq!(mode_weights(None, 3), mode_weights(Some("contend"), 3));
        let (w, _) = mode_weights(Some("balanced"), 5);
        for (a, b) in w.iter().zip([1.0, 0.8, 0.64, 0.512, 0.4096]) {
            close(*a, b);
        }
        close(mode_weights(Some("rebuild"), 4).0[3], 0.9 * DISCOUNT);
        assert_eq!(mode_weights(Some("contend"), 1).0, vec![1.0]);
    }

    #[test]
    fn nineteen_year_old_forward_trajectory_grows() {
        let kid = sk("kid", &["C", "F"], Some(19), json!({}));
        let out = &apply_dynasty(&values(vec![pv(&kid, 3.0, None)]), &dctx(&[&kid], 3, "balanced"), None, None)["kid"];
        assert!(out.upside > 0.0);
        assert!(out.value > horizon_value(3.0, "F", Some(26.0), 3, None, Some("balanced"), 1.0));
        assert!((out.age_mult - 0.762).abs() < 1e-3);
    }

    #[test]
    fn balanced_formula_prime_age() {
        let vet = sk("vet", &["C", "F"], Some(25), json!({}));
        let out = &apply_dynasty(&values(vec![pv(&vet, 3.0, None)]), &dctx(&[&vet], 3, "balanced"), None, None)["vet"];
        let g: Vec<f64> = [1.0, 2.0, 3.0].iter().map(|y| growth_ratio("F", out.age, *y)).collect();
        let exp = 3.0 * (1.0 + 0.8 * g[0] + 0.64 * g[1]) + 0.8f64.powi(3) * 3.0 * g[2] * 1.5;
        assert!(g.iter().all(|x| *x < 1.0));
        assert!((out.value - exp).abs() < 0.01);
        assert!(out.upside < 0.0 && out.mode == "balanced");
        let h = out.reasons.iter().find(|r| r.code == "DYNASTY_HORIZON").unwrap();
        assert!((h.value.unwrap() - exp).abs() < 0.01 && h.baseline == Some(3.0));
    }

    #[test]
    fn contend_values_now_rebuild_values_later() {
        let star = sk("star", &["C", "F"], Some(29), json!({}));
        let kid = sk("kid", &["C", "F"], Some(19), json!({"draft_overall": 5, "draft_round": 1, "career_gp": 0}));
        let vals = values(vec![pv(&star, 4.2, None), pv(&kid, 3.0, None)]);
        let contend = apply_dynasty(&vals, &dctx(&[&star, &kid], 3, "contend"), None, None);
        let rebuild = apply_dynasty(&vals, &dctx(&[&star, &kid], 3, "rebuild"), None, None);
        assert!(contend["star"].value > contend["kid"].value);
        assert!(rebuild["kid"].value > rebuild["star"].value);
        let over = apply_dynasty(&vals, &dctx(&[&star, &kid], 3, "contend"), None, Some("rebuild"));
        close(over["kid"].value, rebuild["kid"].value);
    }

    #[test]
    fn pedigree_tiers_and_fade() {
        let mult = |age: Option<f64>, extra: serde_json::Value| {
            let mut v = json!({"cid": "p", "positions": ["C"]});
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            pedigree_multiplier(&player(v), age).0
        };
        let a = |o: i64, r: i64, gp: i64| json!({"draft_overall": o, "draft_round": r, "career_gp": gp});
        close(mult(Some(19.0), a(2, 1, 0)), 1.30);
        close(mult(Some(19.0), a(8, 1, 0)), 1.20);
        close(mult(Some(19.0), a(20, 1, 0)), 1.10);
        close(mult(Some(19.0), a(44, 2, 0)), 1.03);
        assert_eq!(mult(Some(19.0), a(120, 4, 0)), 1.0);
        assert_eq!(mult(Some(19.0), json!({})), 1.0);
        close(mult(Some(23.0), a(1, 1, 0)), 1.30);
        close(mult(Some(24.0), a(1, 1, 0)), 1.15);
        assert_eq!(mult(Some(25.0), a(1, 1, 0)), 1.0);
        assert_eq!(mult(Some(26.0), a(1, 1, 0)), 1.0);
        close(mult(Some(20.0), a(1, 1, 100)), 1.15);
        assert_eq!(mult(Some(20.0), a(1, 1, 250)), 1.0);
        assert_eq!(mult(None, a(1, 1, 0)), 1.0);
    }

    #[test]
    fn goalies_have_no_youth_growth_and_weak_flat_pedigree() {
        // 07 implication 3: a young goalie's future is today's rate, not a rising curve.
        close(growth_ratio("G", Some(20.0), 3.0), 1.0);
        close(growth_ratio("G", Some(24.0), 2.0), 1.0);
        let g = |extra: serde_json::Value| {
            let mut v = json!({"cid": "g", "positions": ["G"]});
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            pedigree_multiplier(&player(v), Some(20.0)).0
        };
        // Rounds 1-2 all get 1 + 0.24 x 0.10; later rounds get nothing.
        close(g(json!({"draft_overall": 1, "draft_round": 1, "career_gp": 0})), 1.024);
        close(g(json!({"draft_overall": 50, "draft_round": 2, "career_gp": 0})), 1.024);
        assert_eq!(g(json!({"draft_overall": 80, "draft_round": 3, "career_gp": 0})), 1.0);
        let kid = sk("kid", &["G"], Some(20), json!({"draft_overall": 10, "draft_round": 1, "career_gp": 0}));
        let vet = sk("vet", &["G"], Some(28), json!({}));
        let vals = values(vec![pv(&kid, 3.0, None), pv(&vet, 3.0, None)]);
        let out = apply_dynasty(&vals, &dctx(&[&kid, &vet], 3, "balanced"), None, None);
        // Balanced, 3 years, flat: 3 x (1 + 0.8 + 0.64) + 0.8^3 x 3 x 1.5 = 9.624, future 6.624.
        close(out["vet"].value, 9.624);
        close(out["vet"].upside, 0.0);
        close(out["kid"].upside, 0.024 * 6.624);
        close(out["kid"].value, 9.624 + 0.024 * 6.624);
    }

    #[test]
    fn defenseman_pedigree_fades_over_400_games() {
        let m = |pos: &str| {
            let p = player(json!({"cid": "p", "positions": [pos], "draft_overall": 1, "draft_round": 1, "career_gp": 300}));
            pedigree_multiplier(&p, Some(20.0)).0
        };
        close(m("D"), 1.0 + 0.30 * (1.0 - 300.0 / 400.0));
        assert_eq!(m("C"), 1.0);
    }

    #[test]
    fn pedigree_only_lifts_future_years() {
        let a = sk("a", &["C", "F"], Some(20), json!({"draft_overall": 1, "draft_round": 1, "career_gp": 0}));
        let b = sk("b", &["C", "F"], Some(20), json!({}));
        let vals = values(vec![pv(&a, 3.0, None), pv(&b, 3.0, None)]);
        let out = apply_dynasty(&vals, &dctx(&[&a, &b], 1, "balanced"), None, None);
        let later = 0.8 * 3.0 * growth_ratio("F", out["b"].age, 1.0) * 1.5;
        close(out["b"].value, 3.0 + later);
        assert!((out["a"].value - (3.0 + 1.3 * later)).abs() < 1e-3);
        let ped = out["a"].reasons.iter().find(|r| r.code == "PEDIGREE").unwrap();
        assert!((ped.value.unwrap() - 1.3).abs() < 1e-9 && ped.text.contains("#1 overall"));
    }

    #[test]
    fn market_values_rank_mapping() {
        let m = |xs: &[(&str, f64)]| -> Map<String, f64> { xs.iter().map(|(k, v)| (k.to_string(), *v)).collect() };
        let model = m(&[("a", 10.0), ("b", 8.0), ("c", 6.0), ("d", 4.0)]);
        let got = market_values(&model, &m(&[("d", 99.0), ("c", 50.0), ("a", 10.0)]));
        assert_eq!(got, m(&[("d", 10.0), ("c", 8.0), ("a", 6.0)]));
        let ties = market_values(&model, &m(&[("a", 50.0), ("b", 50.0), ("c", 1.0)]));
        assert_eq!(ties, m(&[("a", 9.0), ("b", 9.0), ("c", 6.0)]));
        assert!(market_values(&Map::new(), &m(&[("a", 1.0)])).is_empty());
    }

    #[test]
    fn market_blend_needs_fifty_owned_players() {
        let mut ps: Vec<Player> = (0..49)
            .map(|i| sk(&format!("p{i}"), &["C", "F"], Some(27), json!({"pct_owned": (100 - i) as f64})))
            .collect();
        let mut pvs: Vec<PlayerValue> =
            ps.iter().enumerate().map(|(i, p)| pv(p, 1.0 + i as f64 * 0.1, None)).collect();
        let refs: Vec<&Player> = ps.iter().collect();
        let out = apply_dynasty(&values(pvs.clone()), &dctx(&refs, 3, "balanced"), None, None);
        assert!(out.values().all(|d| d.market_value.is_none() && Some(d.value) == d.model_value));
        ps.push(sk("p49", &["C", "F"], Some(27), json!({"pct_owned": 1.0})));
        pvs.push(pv(ps.last().unwrap(), 0.5, None));
        let refs: Vec<&Player> = ps.iter().collect();
        let out = apply_dynasty(&values(pvs), &dctx(&refs, 3, "balanced"), None, None);
        let top_owned = &out["p0"];
        let top_model = out.values().map(|d| d.model_value.unwrap()).fold(f64::MIN, f64::max);
        close(top_owned.market_value.unwrap(), top_model);
        close(top_owned.value, 0.65 * top_owned.model_value.unwrap() + 0.35 * top_model);
        let mp = top_owned.reasons.iter().find(|r| r.code == "MARKET_PRIOR").unwrap();
        assert_eq!(mp.baseline, Some(100.0));
        for dv in out.values() {
            close(dv.season_part + dv.future, dv.value);
            assert!(dv.keeper && dv.keeper_surplus == dv.future);
        }
    }

    #[test]
    fn unknown_age_and_override() {
        let nob = sk("nob", &["D"], None, json!({}));
        let c = dctx(&[&nob], 2, "balanced");
        let vals = values(vec![pv(&nob, 2.0, None)]);
        let dv = &apply_dynasty(&vals, &c, None, None)["nob"];
        assert!(dv.age.is_none() && dv.age_mult == 1.0);
        close(dv.value, 2.0 * (1.0 + 0.8) + 0.8f64.powi(2) * 2.0 * 1.5);
        assert!(dv.reasons.iter().any(|r| r.code == "AGE_UNKNOWN"));
        let ages: Map<String, f64> = [("nob".to_string(), 33.0)].into_iter().collect();
        let over = &apply_dynasty(&vals, &c, Some(&ages), None)["nob"];
        assert_eq!(over.age, Some(33.0));
        close(over.age_mult, 0.831);
        assert!(over.value < dv.value);
    }

    #[test]
    fn horizon_one_year_and_rank() {
        let a = sk("a", &["C", "F"], Some(26), json!({}));
        let b = sk("b", &["C", "F"], Some(26), json!({}));
        let vals = values(vec![pv(&a, 2.0, None), pv(&b, 3.0, None)]);
        let out = apply_dynasty(&vals, &dctx(&[&a, &b], 1, "balanced"), None, None);
        close(out["a"].value, 2.0 + 0.8 * 2.0 * growth_ratio("F", out["a"].age, 1.0) * 1.5);
        let ranked: Vec<&str> = dynasty_rank(out.values()).iter().map(|d| d.player.cid.as_str()).collect();
        assert_eq!(ranked, vec!["b", "a"]);
        close(horizon_value(1.0, "F", None, 3, None, Some("balanced"), 1.0), 1.0 + 0.8 + 0.64 + 0.512 * 1.5);
    }

    #[test]
    fn ordinal_suffixes() {
        let got: Vec<String> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 101, 111].iter().map(|n| ordinal(*n)).collect();
        assert_eq!(got, ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "101st", "111th"]);
    }
}

/// tests/test_keepers_valuation.py (the keeper-league fixture is shared with the picks tests).
#[cfg(test)]
pub(crate) mod keeper_tests {
    use super::test_support::*;
    use super::*;
    use serde_json::{json, Value};

    type Row = (&'static str, i32, f64, Option<f64>, &'static str);
    const MINE: [Row; 5] = [
        ("m1", 26, 3.0, None, "C"),
        ("m2", 27, 2.5, None, "LW"),
        ("m3", 30, 2.0, None, "BN"),
        ("m_ir", 24, 2.2, Some(0.4), "IR"),
        ("m_old", 35, 0.9, None, "BN"),
    ];
    const STACK: [Row; 5] = [
        ("s1", 24, 5.0, None, "C"),
        ("s2", 25, 4.8, None, "LW"),
        ("s3", 23, 4.6, None, "BN"),
        ("kid", 20, 3.0, None, "BN"),
        ("s5", 31, 1.5, None, "BN"),
    ];
    const THIN: [Row; 2] = [("t1", 28, 2.5, None, "C"), ("t2", 34, 0.8, None, "LW")];
    const FAS: [(&str, i32, f64, Option<f64>); 6] = [
        ("fa1", 29, 1.0, None),
        ("fa2", 29, 1.0, None),
        ("fa3", 29, 1.0, None),
        ("fa_star", 21, 3.0, Some(0.5)),
        ("fa_mid", 25, 2.0, Some(0.3)),
        ("fa_old", 35, 0.9, None),
    ];

    fn born(age: i32) -> String {
        d(2026 - age, 10, 1).to_string()
    }

    fn kp(cid: &str, age: i32, prospect: bool, extra: Value) -> Player {
        let lines = if prospect { json!({}) } else { json!({"prior": line("prior", 70, json!({"GP": 70}))}) };
        let mut v = json!({"cid": cid, "positions": ["C", "LW"], "birth_date": born(age), "status": "healthy",
                           "lines": lines, "career_gp": if prospect { 10 } else { 400 }});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        player(v)
    }

    /// (ctx, values) of the Python `league()` fixture.
    pub(crate) fn league(
        keepers: Option<i64>,
        me: &str,
        horizon: i64,
        prospects: &[(&str, i32, f64)],
        fillers: usize,
    ) -> (LeagueContext, Map<String, PlayerValue>) {
        let mut vals: Vec<PlayerValue> = vec![];
        let mut teams = vec![];
        for (tid, rows) in [("mine", &MINE[..]), ("stack", &STACK[..]), ("thin", &THIN[..])] {
            let ps: Vec<(&str, Player)> = rows
                .iter()
                .map(|(cid, age, f, s, slot)| {
                    let p = kp(cid, *age, false, json!({}));
                    vals.push(pv(&p, *f, *s));
                    (*slot, p)
                })
                .collect();
            let slots: Vec<(&str, &Player)> = ps.iter().map(|(s, p)| (*s, p)).collect();
            teams.push(team(tid, &format!("Team {tid}"), tid == me, &slots));
        }
        if fillers > 0 {
            let ps: Vec<Player> =
                (0..fillers).map(|i| kp(&format!("fill{i}"), 27, false, json!({"pct_owned": i as f64}))).collect();
            for (i, p) in ps.iter().enumerate() {
                vals.push(pv(p, 1.0 + 0.02 * i as f64, None));
            }
            let slots: Vec<(&str, &Player)> = ps.iter().map(|p| ("BN", p)).collect();
            teams.push(team("fill", "Team fill", false, &slots));
        }
        let mut fas = vec![];
        for (cid, age, f, s) in FAS {
            let p = kp(cid, age, false, json!({}));
            vals.push(pv(&p, f, s));
            fas.push(p);
        }
        for (cid, age, f) in prospects {
            let p = kp(cid, *age, true, json!({}));
            vals.push(pv(&p, *f, None));
            fas.push(p);
        }
        let picks: serde_json::Map<String, Value> = ["mine", "stack", "thin"]
            .iter()
            .map(|tid| {
                (tid.to_string(), json!({
                    "2027": [{"round": 1, "original_owner": tid}, {"round": 2, "original_owner": tid}],
                    "2028": [{"round": 1, "original_owner": tid}]}))
            })
            .collect();
        let c = ctx(json!({"provider": "fantrax", "roster_shape": {"C": 1, "LW": 1, "BN": 4, "IR": 1},
            "season_start": "2026-10-07", "teams": teams, "free_agents": fas, "dynasty": true,
            "keeper_horizon_years": horizon, "dynasty_mode": "balanced", "keepers_per_team": keepers,
            "draft_picks": picks}));
        (c, values(vals))
    }

    pub(crate) fn std_league(keepers: Option<i64>) -> (LeagueContext, Map<String, PlayerValue>) {
        league(keepers, "mine", 3, &[], 0)
    }

    fn team_players<'a>(c: &'a LeagueContext, tid: &str) -> Vec<&'a Player> {
        c.team(tid).unwrap().players()
    }

    fn kth(c: &LeagueContext, d: &Map<String, DynastyValue>, tid: &str, k: usize) -> f64 {
        let mut xs: Vec<f64> = team_players(c, tid).iter().map(|p| d[&p.cid].keeper_surplus).collect();
        xs.sort_by(|a, b| b.partial_cmp(a).unwrap());
        xs[k - 1]
    }

    fn reason<'a>(dv: &'a DynastyValue, code: &str) -> &'a Reason {
        dv.reasons.iter().find(|r| r.code == code).unwrap_or_else(|| panic!("{code} on {}", dv.player.cid))
    }

    #[test]
    fn keep_everyone_fills_the_keeper_fields_consistently() {
        let (c, vals) = league(None, "mine", 3, &[], 60);
        assert_eq!(keeper_count(&c), None);
        let d = apply_dynasty(&vals, &c, None, None);
        assert!(d.values().any(|dv| dv.market_value.is_some()));
        for (cid, dv) in &d {
            close(dv.season_part, vals[cid].fpg_season);
            close(dv.season_part + dv.future, dv.value);
            assert!(dv.keeper && dv.keeper_surplus == dv.future);
            assert!(!dv.reasons.iter().any(|r| r.code == "KEEPER" || r.code == "NOT_KEEPER"));
        }
        let stack = team_players(&c, "stack");
        close(keeper_total(stack.iter().copied(), &d, None), stack.iter().map(|p| d[&p.cid].future).sum());
    }

    #[test]
    fn each_team_keeps_its_top_k_by_surplus_ir_included() {
        let (c, vals) = std_league(Some(3));
        let d = apply_dynasty(&vals, &c, None, None);
        for (tid, mut want) in
            [("mine", vec!["m1", "m2", "m_ir"]), ("stack", vec!["s1", "s2", "s3"]), ("thin", vec!["t1"])]
        {
            let mut kept: Vec<&str> =
                team_players(&c, tid).iter().filter(|p| d[&p.cid].keeper).map(|p| p.cid.as_str()).collect();
            kept.sort();
            want.sort();
            assert_eq!(kept, want, "{tid}");
        }
        let r = reason(&d["m_ir"], "KEEPER");
        assert!(r.text.contains("of 3 on Team mine") && r.text.contains("waiver-level C spot"));
        close(r.value.unwrap(), d["m_ir"].keeper_surplus);
        assert!(reason(&d["s1"], "KEEPER").text.starts_with("Keeper #1 of 3 on Team stack"));
    }

    #[test]
    fn strong_young_fourth_best_on_a_stacked_team_is_valued_on_this_season_only() {
        let (c, vals) = std_league(Some(3));
        let d = apply_dynasty(&vals, &c, None, None);
        let kid = &d["kid"];
        assert!(kid.keeper_surplus > kth(&c, &d, "mine", 3));
        assert!(kid.keeper_surplus < kth(&c, &d, "stack", 3));
        assert!(!kid.keeper);
        close(kid.value, kid.season_part);
        close(kid.value, 3.0);
        close(kid.model_value.unwrap(), kid.season_part);
        assert!(kid.upside > 0.0);
        let r = reason(kid, "NOT_KEEPER");
        assert!(r.text.contains("Not among Team stack's top 3 keepers (3rd keeper surplus"), "{}", r.text);
        assert!(r.text.contains("this season only"));
        close(r.baseline.unwrap(), kth(&c, &d, "stack", 3));
    }

    #[test]
    fn keeper_value_is_season_plus_surplus() {
        let (c, vals) = std_league(Some(3));
        let d = apply_dynasty(&vals, &c, None, None);
        let ke = apply_dynasty(&vals, &std_league(None).0, None, None);
        for (cid, dv) in &d {
            close(dv.season_part, vals[cid].fpg_season);
            close(dv.future, ke[cid].value - ke[cid].season_part);
            close(dv.value, dv.season_part + if dv.keeper { dv.keeper_surplus } else { 0.0 });
            close(dv.model_value.unwrap(), dv.value);
            assert!(dv.market_value.is_none());
            close(dv.upside, ke[cid].upside);
        }
        let h = reason(&d["s1"], "DYNASTY_HORIZON");
        assert!(h.text.contains("this season") && h.text.contains("future") && h.text.contains("only for a keeper"));
        close(h.value.unwrap(), ke["s1"].value);
    }

    #[test]
    fn surplus_is_future_over_a_waiver_level_spot_floored_at_zero() {
        let (c, vals) = std_league(Some(3));
        let d = apply_dynasty(&vals, &c, None, None);
        let repl = min_replacement_fpg(&c, &vals);
        close(repl, 1.0);
        let (w, terminal) = mode_weights(Some("balanced"), 3);
        let spot = repl * (w[1..].iter().sum::<f64>() + DISCOUNT.powi(3) * terminal);
        close(spot_future(&c, &vals, None), spot);
        for dv in d.values() {
            close(dv.keeper_surplus, (dv.future - spot).max(0.0));
        }
        let t2 = &d["t2"];
        assert!(0.0 < t2.future && t2.future < spot && t2.keeper_surplus == 0.0 && !t2.keeper);
        close(t2.value, t2.season_part);
        assert!(reason(t2, "NOT_KEEPER").text.starts_with("No keeper value"));
    }

    /// The gap's worked example (09 B6): replacement 4.45 at F and 3.64 at D. Ages are unknown, so
    /// the trajectory is flat; balanced over one year weighs the future at 0.8 x 1.5 = 1.2.
    #[test]
    fn surplus_is_over_the_players_own_positional_spot() {
        let mk = |cid: &str, pos: &[&str]| player(json!({"cid": cid, "positions": pos}));
        let (fwd, dman) = (mk("fwd", &["C", "F"]), mk("dman", &["D"]));
        let fa: Vec<Player> = (0..3).map(|i| mk(&format!("faf{i}"), &["C", "F"]))
            .chain((0..3).map(|i| mk(&format!("fad{i}"), &["D"])))
            .collect();
        let mut vals = vec![pv(&fwd, 5.0, None), pv(&dman, 4.5, None)];
        for p in &fa {
            vals.push(pv(p, if p.cid.starts_with("faf") { 4.45 } else { 3.64 }, None));
        }
        let c = ctx(json!({"roster_shape": {"F": 2, "D": 2, "BN": 2},
            "teams": [team("1", "a", true, &[("F", &fwd), ("D", &dman)])], "free_agents": fa,
            "dynasty": true, "keeper_horizon_years": 1, "dynasty_mode": "balanced", "keepers_per_team": 3}));
        let d = apply_dynasty(&values(vals), &c, None, None);
        close(d["fwd"].future, 1.2 * 5.0);
        close(d["fwd"].keeper_surplus, 1.2 * (5.0 - 4.45));
        close(d["dman"].keeper_surplus, 1.2 * (4.5 - 3.64));
        assert!(d["dman"].keeper_surplus > d["fwd"].keeper_surplus);
        let r = reason(&d["fwd"], "KEEPER");
        assert!(r.text.contains("waiver-level F spot"), "{}", r.text);
        close(r.baseline.unwrap(), 1.2 * 4.45);
        // A free-agent forward at replacement has no surplus; under the old league-wide lowest
        // level (D, 3.64) he had 1.2 x 0.81.
        close(d["faf0"].keeper_surplus, 0.0);
    }

    #[test]
    fn free_agents_are_keepers_only_if_they_beat_my_kth_keeper() {
        let (c, vals) = std_league(Some(3));
        let d = apply_dynasty(&vals, &c, None, None);
        let bar = kth(&c, &d, "mine", 3);
        let (star, mid, fa1) = (&d["fa_star"], &d["fa_mid"], &d["fa1"]);
        assert!(star.keeper_surplus > bar && star.keeper);
        close(star.value, 0.5 + star.keeper_surplus);
        assert!(reason(star, "KEEPER").text.contains("Free agent: would be keeper #1 of 3 on Team mine"));
        assert!(0.0 < mid.keeper_surplus && mid.keeper_surplus <= bar && !mid.keeper);
        close(mid.value, mid.season_part);
        let nk = reason(mid, "NOT_KEEPER");
        assert!(nk.text.contains("would not crack Team mine's top 3 keepers"));
        close(nk.baseline.unwrap(), bar);
        assert!(fa1.keeper_surplus == 0.0 && !fa1.keeper);
    }

    #[test]
    fn any_positive_surplus_free_agent_is_a_keeper_when_i_have_fewer_than_k() {
        let (c, vals) = league(Some(3), "thin", 3, &[], 0);
        let d = apply_dynasty(&vals, &c, None, None);
        assert!(d["fa_mid"].keeper && d["fa_star"].keeper);
        assert!(d["fa_mid"].keeper_surplus < d["t1"].keeper_surplus);
        assert!(reason(&d["fa_mid"], "KEEPER").text.starts_with("Free agent: would be keeper #2 of 3 on Team thin"));
        assert!(!d["fa1"].keeper);
        let mine_view = apply_dynasty(&vals, &std_league(Some(3)).0, None, None);
        let kept = |m: &Map<String, DynastyValue>| -> Vec<String> {
            m.iter().filter(|(c, dv)| dv.keeper && !c.starts_with("fa")).map(|(c, _)| c.clone()).collect()
        };
        assert_eq!(kept(&d), kept(&mine_view));
    }

    #[test]
    fn zero_keepers_values_everyone_on_this_season() {
        let (c, vals) = std_league(Some(0));
        assert_eq!(keeper_count(&c), Some(0));
        let d = apply_dynasty(&vals, &c, None, None);
        for dv in d.values() {
            assert!(!dv.keeper);
            close(dv.value, dv.season_part);
            assert!(reason(dv, "NOT_KEEPER").text.starts_with("No keepers in this league"));
        }
        assert_eq!(keeper_total(team_players(&c, "stack").iter().copied(), &d, Some(0)), 0.0);
    }

    #[test]
    fn keeper_total_sums_the_k_largest_surpluses() {
        let (c, vals) = std_league(Some(3));
        let d = apply_dynasty(&vals, &c, None, None);
        let (stack, mine) = (team_players(&c, "stack"), team_players(&c, "mine"));
        let top3: f64 = ["s1", "s2", "s3"].iter().map(|c| d[*c].keeper_surplus).sum();
        close(keeper_total(stack.iter().copied(), &d, Some(3)), top3);
        close(keeper_total(stack.iter().copied(), &d, None), stack.iter().map(|p| d[&p.cid].keeper_surplus).sum());
        let kid = *stack.iter().find(|p| p.cid == "kid").unwrap();
        let gain = keeper_total(mine.iter().copied().chain([kid]), &d, Some(3))
            - keeper_total(mine.iter().copied(), &d, Some(3));
        close(gain, d["kid"].keeper_surplus - kth(&c, &d, "mine", 3));
    }

    #[test]
    fn market_prior_blends_the_future_only() {
        let (c, vals) = league(Some(3), "mine", 3, &[], 60);
        let d = apply_dynasty(&vals, &c, None, None);
        let ke = apply_dynasty(&vals, &league(None, "mine", 3, &[], 60).0, None, None);
        let spot = spot_future(&c, &vals, None);
        let blended: Vec<&String> = d.iter().filter(|(_, dv)| dv.market_value.is_some()).map(|(c, _)| c).collect();
        assert_eq!(blended.len(), 60);
        for cid in &blended {
            let dv = &d[*cid];
            let model_future = ke[*cid].model_value.unwrap() - ke[*cid].season_part;
            close(dv.season_part, vals[*cid].fpg_season);
            close(dv.future, (1.0 - MARKET_WEIGHT) * model_future + MARKET_WEIGHT * dv.market_value.unwrap());
            let ms = if dv.keeper { (model_future - spot).max(0.0) } else { 0.0 };
            close(dv.model_value.unwrap(), dv.season_part + ms);
            assert!(reason(dv, "MARKET_PRIOR").text.contains("market future value"));
        }
        let top_future =
            d.keys().map(|c| ke[c].model_value.unwrap() - ke[c].season_part).fold(f64::MIN, f64::max);
        let top_market = blended.iter().map(|c| d[*c].market_value.unwrap()).fold(f64::MIN, f64::max);
        close(top_market, top_future);
    }
}
