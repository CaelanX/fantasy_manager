//! Rust port of `fantasy_manager/valuation/rookie.py`: the rookie / unproven-player model (NHLe,
//! draft pedigree, baseline votes blended on PTS/GP; Daily Faceoff + news role signals; the
//! expected games share). `RookiePrior` / `RookieEstimate` live in `fm_core::values`.
//! Owner: work package `W3-valuation-schedule` (wave 3).
//!
//! Signals: Python passed role signals around as dicts (`dfo_signals`) or `RoleSignal` objects
//! (`_signal_dict`). Here they are a [`Signal`] struct; anything implementing [`AsSignal`]
//! (`fm_core::snapshot::RoleSignal`, `fm_sources::news_roles::RoleSignal`, `Signal`) can be
//! passed. [`Signal::to_json`] reproduces the dict (key order and all) that Python stored in
//! `RookieEstimate.signals`.
//! `nhle_estimate` returns a struct ([`NhleEstimate`]) instead of a dict; `rookie_value`'s many
//! keyword arguments are the [`RookieInputs`] struct (`preseason` was only reported in Python and
//! is dropped).

use chrono::{DateTime, NaiveDate, Utc};
use fm_core::models::{Player, Reason};
use fm_core::values::{RookieEstimate, RookiePrior};
use fm_core::{Map, PyDateTime};
use fm_pycompat::fmt::{fixed, general, percent};
use fm_pycompat::num::{py_sum, round_to};
use fm_pycompat::order::sort_desc_by;
use serde_json::{json, Value};

pub use fm_core::snapshot::LeagueSeason;

use super::params::age_factor;
pub use super::roles::{role_multiplier, DEPTH_PENALTY, ROLE_BOOST, ROLE_MINOR_BOOST, SCRATCH_MULT, TOP_KINDS};
use crate::scoring::ScoringSystem;

// vote weights, in effective games
pub const W_BASELINE: f64 = 40.0;
pub const W_NHLE: f64 = 40.0;
pub const W_PEDIGREE: f64 = 15.0;
/// GP over the two seasons for full NHLe confidence
pub const NHLE_FULL_GP: f64 = 60.0;
/// a season needs this many GP in table leagues to count
pub const NHLE_MIN_SEASON_GP: i64 = 10;
pub const NHLE_SEASON_WEIGHTS: [f64; 2] = [2.0, 1.0];

/// Draft-tier prior on rookie PTS/GP (APPROXIMATE; forwards; defensemen x PEDIGREE_D_MULT)
pub const PEDIGREE_TIERS: [(i64, f64, &str); 4] =
    [(3, 0.75, "top-3 pick"), (10, 0.60, "top-10 pick"), (32, 0.45, "1st-round pick"), (64, 0.35, "2nd-round pick")];
pub const PEDIGREE_LATER: f64 = 0.30;
pub const PEDIGREE_D_MULT: f64 = 0.7;
pub const PEDIGREE_MAX_AGE: f64 = 23.0;
pub const PEDIGREE_FADE_AGE: f64 = 25.0;
pub const PEDIGREE_PROVEN_GP: f64 = 200.0;

/// points of shrinkage of the observed goal share
pub const G_SHARE_K: f64 = 40.0;

/// `DEFAULT_G_SHARE[group]`
pub fn default_g_share(group: &str) -> f64 {
    if group == "D" {
        0.22
    } else {
        0.40
    }
}
/// `DEFAULT_SH_PCT[group]`
pub fn default_sh_pct(group: &str) -> f64 {
    if group == "D" {
        0.05
    } else {
        0.11
    }
}
/// `DEFAULT_PPP_SHARE[group]`
pub fn default_ppp_share(group: &str) -> f64 {
    if group == "D" {
        0.35
    } else {
        0.28
    }
}

pub const OFFENSIVE: [&str; 11] = ["G", "A", "PTS", "PPG", "PPA", "PPP", "SHG", "SHA", "SHP", "GWG", "SOG"];

// role signals
pub const SIGNAL_MAX_AGE_DAYS: i64 = 21;
pub const SIGNAL_MIN_CONF: f64 = 0.6;
/// LLM labels need more confidence (they come from the ambiguous blurbs)
pub const SIGNAL_MIN_CONF_LLM: f64 = 0.7;
pub const GP_CONFIRMED: f64 = 1.0;
pub const GP_TOP10_ON_TEAM: f64 = 0.9;
pub const GP_ON_TEAM: f64 = 0.75;
pub const GP_NO_TEAM: f64 = 0.3;
pub const GP_TOP_ROLE: f64 = 0.95;
pub const GP_AHL: f64 = 0.05;
pub const GP_JUNIOR: f64 = 0.02;
pub const PROJ_FULL_GP: f64 = 75.0;
/// weight of the projection's GP share in the games share...
pub const PROJ_GP_WEIGHT: f64 = 0.5;
/// ...once he is confirmed in the lineup
pub const PROJ_GP_WEIGHT_CONFIRMED: f64 = 0.25;
pub const GP_FADE_SEASON_GP: f64 = 20.0;
pub const MAX_SANE_PTS_PG: f64 = 2.0;

/// Career NHL GP known and < 82, or no prior season (N-1..N-3) with >= 20 GP.
pub fn is_unproven(p: &Player) -> bool {
    fm_core::unproven::is_unproven(p)
}

/// "D" for a pure defenseman, else "F".
pub fn position_group(p: &Player) -> &'static str {
    let has_d = p.positions.iter().any(|x| x == "D");
    let fwd = p.positions.iter().any(|x| matches!(x.as_str(), "C" | "LW" | "RW" | "F"));
    if has_d && !fwd {
        "D"
    } else {
        "F"
    }
}

/// `LeagueSeason.pts_per_game`.
pub fn pts_per_game(row: &LeagueSeason) -> f64 {
    if row.gp != 0 {
        row.pts as f64 / row.gp as f64
    } else {
        0.0
    }
}

// --------------------------------------------------------------------------- NHLe

/// The packaged NHLe table (`providers.nhl.NHLE_FACTORS`).
pub fn nhle_factors() -> &'static Value {
    fm_sources::nhl::nhle_factors()
}

fn league_info<'a>(factors: &'a Value, league: &str) -> Option<&'a Value> {
    factors.get("leagues").and_then(|l| l.get(league)).filter(|v| !v.is_null())
}

fn num(v: Option<&Value>, default: f64) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(default)
}

/// `age_multiplier(league, age, factors, group)`: +per_year for every year under the league
/// class's norm age (at most max_years); NHL rows use the fitted year-over-year age factor. 1.0
/// when unknown. `factors` None = the packaged table; Python's default group is "F".
pub fn age_multiplier(league: &str, age: Option<f64>, factors: Option<&Value>, group: &str) -> f64 {
    let factors = factors.unwrap_or_else(|| nhle_factors());
    let Some(info) = league_info(factors, league) else { return 1.0 };
    let Some(age) = age else { return 1.0 };
    let class = info.get("class").and_then(|c| c.as_str());
    if class == Some("nhl") {
        return age_factor(group, Some(age), None, None);
    }
    let adj = factors.get("age_adjustment");
    let norm = class
        .and_then(|c| factors.get("norm_age").and_then(|n| n.get(c)))
        .and_then(|v| v.as_f64())
        .unwrap_or(21.0);
    let years = num(adj.and_then(|a| a.get("max_years")), 3.0).min(0.0_f64.max(norm - age));
    1.0 + num(adj.and_then(|a| a.get("per_year")), 0.08) * years
}

/// `nhle_estimate(...)` result (a dict in Python: pts_pg, confidence, gp, rows, g, pts).
#[derive(Clone, Debug, PartialEq)]
pub struct NhleEstimate {
    pub pts_pg: f64,
    pub confidence: f64,
    pub gp: i64,
    /// the league-seasons used, as the Python row dicts
    pub rows: Vec<Map<String, Value>>,
    pub g: i64,
    pub pts: i64,
}

/// `nhle_estimate(history, factors, group, include_nhl, before_season)`: NHL-equivalent PTS/GP
/// from league history, or None without usable seasons. `before_season` (e.g. 20262027) drops NHL
/// rows of that season and later. Python defaults: group "F", include_nhl true.
pub fn nhle_estimate(
    history: &[LeagueSeason],
    factors: Option<&Value>,
    group: &str,
    include_nhl: bool,
    before_season: Option<i64>,
) -> Option<NhleEstimate> {
    let factors = factors.unwrap_or_else(|| nhle_factors());
    let mut by_season: Map<i64, Vec<(&LeagueSeason, &Value)>> = Map::new();
    for row in history {
        let Some(info) = league_info(factors, &row.league) else { continue };
        if row.gp <= 0 {
            continue;
        }
        if row.league == "NHL" && (!include_nhl || before_season.is_some_and(|b| row.season >= b)) {
            continue;
        }
        by_season.entry(row.season).or_default().push((row, info));
    }
    let mut keys: Vec<i64> = by_season.keys().copied().collect();
    keys.sort_by(|a, b| b.cmp(a));
    let seasons: Vec<i64> = keys
        .into_iter()
        .filter(|s| by_season[s].iter().map(|(r, _)| r.gp).sum::<i64>() >= NHLE_MIN_SEASON_GP)
        .take(NHLE_SEASON_WEIGHTS.len())
        .collect();
    if seasons.is_empty() {
        return None;
    }
    let (mut num_, mut den, mut gp_total, mut rel_num) = (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
    let (mut g_raw, mut pts_raw) = (0_i64, 0_i64);
    let mut rows: Vec<Map<String, Value>> = Vec::new();
    for (sw, season) in NHLE_SEASON_WEIGHTS.iter().zip(seasons.iter()) {
        for (row, info) in &by_season[season] {
            let f = num(info.get("factor"), 0.0);
            let am = age_multiplier(&row.league, row.age_at_season, Some(factors), group);
            let ppg = pts_per_game(row);
            let nhle = ppg * f * am;
            let w = sw * row.gp as f64;
            num_ += w * nhle;
            den += w;
            gp_total += row.gp as f64;
            rel_num += row.gp as f64 * num(info.get("reliability"), 0.7);
            g_raw += row.g;
            pts_raw += row.pts;
            let mut m = Map::new();
            m.insert("season".into(), json!(row.season));
            m.insert("league".into(), json!(row.league));
            m.insert("team".into(), json!(row.team));
            m.insert("gp".into(), json!(row.gp));
            m.insert("g".into(), json!(row.g));
            m.insert("a".into(), json!(row.a));
            m.insert("pts".into(), json!(row.pts));
            m.insert("pts_pg".into(), json!(round_to(ppg, 3)));
            m.insert("factor".into(), json!(f));
            m.insert("age".into(), json!(row.age_at_season));
            m.insert("age_mult".into(), json!(round_to(am, 3)));
            m.insert("nhle_pts_pg".into(), json!(round_to(nhle, 3)));
            m.insert("weight".into(), json!(*sw));
            rows.push(m);
        }
    }
    if den <= 0.0 {
        return None;
    }
    let reliability = if gp_total != 0.0 { rel_num / gp_total } else { 0.0 };
    let conf = 1.0_f64.min(gp_total / NHLE_FULL_GP) * reliability;
    Some(NhleEstimate { pts_pg: num_ / den, confidence: conf, gp: gp_total as i64, rows, g: g_raw, pts: pts_raw })
}

// --------------------------------------------------------------------------- pedigree

/// `pedigree_prior(p, age)`: (PTS/GP prior, vote weight, label) from the draft slot; None when
/// undrafted / unknown or the prior has faded (age >= 25 or 200+ career NHL GP).
pub fn pedigree_prior(p: &Player, age: Option<f64>) -> Option<(f64, f64, String)> {
    let ov = p.draft_overall?;
    let (mut pts, mut label) = (PEDIGREE_LATER, "later-round pick");
    for (cut, val, lab) in PEDIGREE_TIERS {
        if ov <= cut {
            (pts, label) = (val, lab);
            break;
        }
    }
    if position_group(p) == "D" {
        pts *= PEDIGREE_D_MULT;
    }
    let mut f_age = 1.0;
    if let Some(a) = age {
        if a > PEDIGREE_MAX_AGE {
            f_age = 0.0_f64.max((PEDIGREE_FADE_AGE - a) / (PEDIGREE_FADE_AGE - PEDIGREE_MAX_AGE));
        }
    }
    let f_gp = 0.0_f64.max(1.0 - p.career_gp.unwrap_or(0) as f64 / PEDIGREE_PROVEN_GP);
    let w = W_PEDIGREE * f_age * f_gp;
    if w <= 0.0 {
        return None;
    }
    let year = match p.draft_year {
        Some(y) if y != 0 => format!(" {y}"),
        _ => String::new(),
    };
    Some((pts, w, format!("#{ov} overall{year} ({label})")))
}

/// `rookie_prior(player, history, factors, age, include_nhl, before_season)`: NHLe + pedigree
/// evidence. `age` = age on Oct 1 of the season being projected. Python defaults: include_nhl
/// true, before_season None.
pub fn rookie_prior(
    player: &Player,
    history: &[LeagueSeason],
    factors: Option<&Value>,
    age: Option<f64>,
    include_nhl: bool,
    before_season: Option<i64>,
) -> RookiePrior {
    let factors = factors.unwrap_or_else(|| nhle_factors());
    let group = position_group(player);
    let mut prior = RookiePrior { gp_expectation: 1.0, ..Default::default() };
    if let Some(est) = nhle_estimate(history, Some(factors), group, include_nhl, before_season) {
        let pts_pg = est.pts_pg;
        prior.pts_pg_nhle = Some(pts_pg);
        prior.nhle_confidence = est.confidence;
        prior.nhle_gp = est.gp;
        let g_share = (est.g as f64 + default_g_share(group) * G_SHARE_K) / (est.pts as f64 + G_SHARE_K);
        prior.g_share = Some(g_share);
        prior.sog_pg_est = Some(pts_pg * g_share / default_sh_pct(group));
        let mut seasons: Vec<i64> = est.rows.iter().filter_map(|r| r.get("season").and_then(|v| v.as_i64())).collect();
        seasons.sort_by(|a, b| b.cmp(a));
        seasons.dedup();
        let mut parts: Vec<String> = Vec::new();
        for s in seasons {
            let rs: Vec<String> = est
                .rows
                .iter()
                .filter(|r| r.get("season").and_then(|v| v.as_i64()) == Some(s))
                .map(|r| {
                    let am = num(r.get("age_mult"), 1.0);
                    let mut t = format!(
                        "{} {} GP {} PTS x{}",
                        r.get("league").and_then(|v| v.as_str()).unwrap_or(""),
                        r.get("gp").and_then(|v| v.as_i64()).unwrap_or(0),
                        r.get("pts").and_then(|v| v.as_i64()).unwrap_or(0),
                        general(num(r.get("factor"), 0.0), None, false)
                    );
                    if (am - 1.0).abs() > 1e-3 {
                        t += &format!(" x{} age", fixed(am, 2, false));
                    }
                    t
                })
                .collect();
            let tail: String = (s.rem_euclid(10000)).to_string().chars().skip(2).collect();
            parts.push(format!("{}-{} {}", s.div_euclid(10000), tail, rs.join(" + ")));
        }
        prior.reasons.push(
            Reason::new(
                "NHLE",
                format!(
                    "NHL equivalency {} PTS/GP from {} (seasons weighted 2:1 by GP; approximate league factors, {} confidence)",
                    fixed(pts_pg, 2, false),
                    parts.join("; "),
                    percent(est.confidence, 0, false)
                ),
            )
            .with_value(Some(round_to(pts_pg, 4)))
            .with_baseline(Some(round_to(est.confidence, 3))),
        );
        prior.nhle_rows = est.rows;
    }
    if let Some((pts, w, label)) = pedigree_prior(player, age) {
        prior.reasons.push(
            Reason::new(
                "PEDIGREE_PRIOR",
                format!(
                    "Draft pedigree {label}: prior {} PTS/GP (approximate draft-tier rookie rates), vote weight {} games",
                    fixed(pts, 2, false),
                    fixed(w, 0, false)
                ),
            )
            .with_value(Some(round_to(pts, 4)))
            .with_baseline(Some(round_to(w, 2))),
        );
        prior.pedigree_pts_pg = Some(pts);
        prior.pedigree_weight = w;
        prior.pedigree_label = Some(label);
    }
    let w = (if prior.pts_pg_nhle.is_some() { W_NHLE * prior.nhle_confidence } else { 0.0 }) + prior.pedigree_weight;
    prior.confidence = if w > 0.0 { round_to(w / (w + W_BASELINE), 3) } else { 0.0 };
    prior
}

// --------------------------------------------------------------------------- role signals

/// A role signal as the Python dicts carried it: `{kind, direction, confidence, quote, published,
/// source, origin, ambiguous}` (from a `RoleSignal`) or a Daily Faceoff dict (`dfo == true`).
#[derive(Clone, Debug, PartialEq)]
pub struct Signal {
    pub kind: String,
    /// +1 / -1 / 0
    pub direction: i64,
    pub confidence: f64,
    pub quote: String,
    pub published: Option<PyDateTime>,
    /// "" / None print as "news" in the reason texts
    pub source: Option<String>,
    /// "rules" | "llm" | "dfo"
    pub origin: String,
    pub ambiguous: bool,
    /// built by [`dfo_signals`] (dict layout: source, origin, published, confidence, kind,
    /// direction, quote)
    pub dfo: bool,
}

impl Signal {
    /// A bare `{"kind": ..., "direction": ...}` dict (`roles._line_kinds` / `_pp_kinds`).
    pub fn kd(kind: &str, direction: i64) -> Signal {
        Signal {
            kind: kind.to_string(),
            direction,
            confidence: 0.0,
            quote: String::new(),
            published: None,
            source: None,
            origin: "rules".into(),
            ambiguous: false,
            dfo: false,
        }
    }

    fn dfo(kind: &str, direction: i64, quote: String, confidence: f64) -> Signal {
        Signal {
            kind: kind.to_string(),
            direction,
            confidence,
            quote,
            published: None,
            source: Some("dfo".into()),
            origin: "dfo".into(),
            ambiguous: false,
            dfo: true,
        }
    }

    /// `d.get('source') or 'news'`
    pub fn source_or_news(&self) -> &str {
        match self.source.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => "news",
        }
    }

    /// The dict Python stored in `RookieEstimate.signals` (dates as `isoformat()`).
    pub fn to_json(&self) -> Map<String, Value> {
        let published = match &self.published {
            Some(p) => Value::String(p.isoformat()),
            None => Value::Null,
        };
        let mut m = Map::new();
        if self.dfo {
            m.insert("source".into(), json!(self.source.clone().unwrap_or_default()));
            m.insert("origin".into(), json!(self.origin));
            m.insert("published".into(), published);
            m.insert("confidence".into(), json!(self.confidence));
            m.insert("kind".into(), json!(self.kind));
            m.insert("direction".into(), json!(self.direction));
            m.insert("quote".into(), json!(self.quote));
        } else {
            m.insert("kind".into(), json!(self.kind));
            m.insert("direction".into(), json!(self.direction));
            m.insert("confidence".into(), json!(self.confidence));
            m.insert("quote".into(), json!(self.quote));
            m.insert("published".into(), published);
            m.insert("source".into(), json!(self.source.clone().unwrap_or_default()));
            m.insert("origin".into(), json!(self.origin));
            m.insert("ambiguous".into(), json!(self.ambiguous));
        }
        m
    }
}

/// Anything usable as a role signal (`_signal_dict`).
pub trait AsSignal {
    fn as_signal(&self) -> Signal;
}

impl AsSignal for Signal {
    fn as_signal(&self) -> Signal {
        self.clone()
    }
}

impl AsSignal for fm_core::snapshot::RoleSignal {
    fn as_signal(&self) -> Signal {
        Signal {
            kind: self.kind.clone(),
            direction: self.direction,
            confidence: self.confidence,
            quote: self.quote.clone(),
            published: self.published,
            source: Some(self.source.clone()),
            origin: self.origin.clone(),
            ambiguous: self.ambiguous,
            dfo: false,
        }
    }
}

impl AsSignal for fm_sources::news_roles::RoleSignal {
    fn as_signal(&self) -> Signal {
        Signal {
            kind: self.kind.clone(),
            direction: self.direction,
            confidence: self.confidence,
            quote: self.quote.clone(),
            published: self.published,
            source: Some(self.source.clone()),
            origin: self.origin.clone(),
            ambiguous: self.ambiguous,
            dfo: false,
        }
    }
}

/// `_as_dt(d)`: a naive datetime is read as UTC.
fn as_dt(d: Option<&PyDateTime>) -> Option<DateTime<Utc>> {
    let d = d?;
    if d.is_aware() {
        d.to_utc()
    } else {
        Some(d.local.and_utc())
    }
}

/// `dfo_signals(p)`: role signals from the Daily Faceoff lineup fields on the Player.
pub fn dfo_signals(p: &Player) -> Vec<Signal> {
    let mut out = Vec::new();
    let line = p.line.as_deref().unwrap_or("").to_lowercase();
    let pp = p.pp_unit.as_deref().unwrap_or("").to_lowercase();
    if !line.is_empty() {
        out.push(Signal::dfo("nhl_roster", 1, format!("Daily Faceoff lineup: {}", line.to_uppercase()), 0.8));
    }
    match line.as_str() {
        "f1" => out.push(Signal::dfo("top_line", 1, "Daily Faceoff: first line (F1)".into(), 0.9)),
        "d1" => out.push(Signal::dfo("first_line_pairing", 1, "Daily Faceoff: top pair (D1)".into(), 0.9)),
        "f2" | "d2" => out.push(Signal::dfo("extended_role", 1, format!("Daily Faceoff: {}", line.to_uppercase()), 0.7)),
        "f4" | "d3" | "d4" => out.push(Signal::dfo("top_line", -1, format!("Daily Faceoff: {}", line.to_uppercase()), 0.7)),
        _ => {}
    }
    match pp.as_str() {
        "pp1" => out.push(Signal::dfo("pp1", 1, "Daily Faceoff: first power-play unit (PP1)".into(), 0.9)),
        "pp2" => out.push(Signal::dfo("pp2", 1, "Daily Faceoff: second power-play unit (PP2)".into(), 0.8)),
        _ => {}
    }
    if p.line_change.as_deref().unwrap_or("").to_lowercase() == "scratched" {
        out.push(Signal::dfo("scratched", -1, "Daily Faceoff: dropped out of the lineup".into(), 0.7));
    }
    out
}

/// `usable_news(signals, as_of, max_age_days)`: news signals recent enough (<= `max_age_days`
/// before `as_of`) and confident enough (>= 0.6, >= 0.7 for LLM labels; not ambiguous, not
/// direction 0) to move a valuation; newest first. Python default max age: `SIGNAL_MAX_AGE_DAYS`.
pub fn usable_news<S: AsSignal>(signals: &[S], as_of: Option<NaiveDate>, max_age_days: i64) -> Vec<Signal> {
    let rf: Option<DateTime<Utc>> = as_of.map(|d| d.and_hms_opt(0, 0, 0).expect("midnight").and_utc());
    let mut out: Vec<Signal> = Vec::new();
    for s in signals {
        let d = s.as_signal();
        let floor = if d.origin == "llm" { SIGNAL_MIN_CONF_LLM } else { SIGNAL_MIN_CONF };
        if d.ambiguous || d.confidence < floor || d.direction == 0 {
            continue;
        }
        if let (Some(r), Some(p)) = (rf, as_dt(d.published.as_ref())) {
            let micros = (r - p).num_microseconds().unwrap_or(i64::MAX);
            let age = micros as f64 / 1e6 / 86400.0;
            if age > (max_age_days + 1) as f64 || age < -2.0 {
                continue;
            }
        }
        out.push(d);
    }
    let epoch = DateTime::<Utc>::MIN_UTC;
    sort_desc_by(&mut out, |a, b| {
        let ka = as_dt(a.published.as_ref()).unwrap_or(epoch);
        let kb = as_dt(b.published.as_ref()).unwrap_or(epoch);
        ka.cmp(&kb)
    });
    out
}

/// `_fmt_signal(d)`: `"quote" (source, YYYY-MM-DD)`.
pub fn fmt_signal(d: &Signal) -> String {
    let when = match &d.published {
        Some(p) => format!(", {}", p.local.format("%Y-%m-%d")),
        None => String::new(),
    };
    format!("\"{}\" ({}{})", d.quote, d.source_or_news(), when)
}

fn sign_char(direction: i64) -> &'static str {
    if direction > 0 {
        "+"
    } else {
        "-"
    }
}

// --------------------------------------------------------------------------- the blend

fn pts(rates: &Map<String, f64>) -> f64 {
    if let Some(p) = rates.get("PTS") {
        return *p;
    }
    rates.get("G").copied().unwrap_or(0.0) + rates.get("A").copied().unwrap_or(0.0)
}

/// `comparable_baseline(rates)`: the baseline carries points (PTS, or both G and A) at a plausible
/// NHL rate (< 2 PTS/GP).
pub fn comparable_baseline(rates: &Map<String, f64>) -> bool {
    if rates.is_empty() || !(rates.contains_key("PTS") || (rates.contains_key("G") && rates.contains_key("A"))) {
        return false;
    }
    let p = pts(rates);
    0.0 < p && p < MAX_SANE_PTS_PG
}

/// `_from_prior(pts, prior, group, mean)`: per-game rates built from a PTS/GP estimate.
fn from_prior(pts_v: f64, prior: Option<&RookiePrior>, group: &str, mean: &Map<String, f64>) -> Map<String, f64> {
    let g_share = prior.and_then(|p| p.g_share).unwrap_or_else(|| default_g_share(group));
    let mut out: Map<String, f64> =
        mean.iter().filter(|(k, _)| !OFFENSIVE.contains(&k.as_str())).map(|(k, v)| (k.clone(), *v)).collect();
    let g = pts_v * g_share;
    let mean_pts = if mean.is_empty() { 0.0 } else { pts(mean) };
    let ppp_share = match mean.get("PPP") {
        Some(ppp) if mean_pts > 0.0 => ppp / mean_pts,
        _ => default_ppp_share(group),
    };
    let ppp = pts_v * ppp_share;
    for (k, v) in [
        ("PTS", pts_v),
        ("G", g),
        ("A", pts_v - g),
        ("PPP", ppp),
        ("PPG", ppp * g_share),
        ("PPA", ppp * (1.0 - g_share)),
        ("SOG", g / default_sh_pct(group)),
    ] {
        out.insert(k.to_string(), v);
    }
    if !mean.is_empty() && mean_pts > 0.0 {
        for k in ["SHG", "SHA", "SHP", "GWG"] {
            if let Some(m) = mean.get(k) {
                out.insert(k.to_string(), m * pts_v / mean_pts);
            }
        }
    }
    out
}

fn scale(rates: &Map<String, f64>, s: f64) -> Map<String, f64> {
    rates.iter().map(|(k, v)| (k.clone(), if OFFENSIVE.contains(&k.as_str()) { v * s } else { *v })).collect()
}

/// `rookie_value`'s keyword arguments.
#[derive(Clone, Copy, Default)]
pub struct RookieInputs<'a> {
    /// the baseline rates (`valuate.baseline_rates`; None / empty without one)
    pub provider_projection: Option<&'a Map<String, f64>>,
    /// the Player carrying the Daily Faceoff fields, or None
    pub deployment: Option<&'a Player>,
    /// NHL history GP inside the baseline (adds to its vote weight)
    pub baseline_gp: f64,
    /// the league projection's GP (games-share evidence)
    pub projection_gp: Option<i64>,
    /// positional mean rates (rates built from the priors without a baseline)
    pub mean: Option<&'a Map<String, f64>>,
    pub season_gp: i64,
    pub as_of: Option<NaiveDate>,
    /// prints FPG in the ROOKIE_BLEND reason
    pub scoring: Option<&'a dyn ScoringSystem>,
}

/// `rookie_value(player, prior, ...)`: blend the votes into per-game rates and a games share
/// (module docstring of the Python file).
pub fn rookie_value<S: AsSignal>(
    player: &Player,
    prior: Option<&RookiePrior>,
    news_signals: &[S],
    inp: RookieInputs<'_>,
) -> RookieEstimate {
    let group = position_group(player);
    let base: Map<String, f64> = inp.provider_projection.cloned().unwrap_or_default();
    let empty = Map::new();
    let mean = inp.mean.unwrap_or(&empty);
    let mut est = RookieEstimate { prior: prior.cloned(), ..Default::default() };
    let mut prior = prior;
    let mut reasons: Vec<Reason> = prior.map(|p| p.reasons.clone()).unwrap_or_default();
    let base_pts = if base.is_empty() { 0.0 } else { pts(&base) };
    if !base.is_empty() && !comparable_baseline(&base) {
        if prior.is_some_and(|p| p.pts_pg_nhle.is_some() || p.pedigree_pts_pg.is_some()) {
            reasons = vec![Reason::new(
                "ROOKIE_BLEND",
                "Rookie priors not blended: the baseline carries no comparable PTS/GP (needs PTS, or G and A, below 2.0)",
            )];
        }
        prior = None;
    }
    // (name, pts_pg, weight)
    let mut votes: Vec<(&str, f64, f64)> = Vec::new();
    if !base.is_empty() && base_pts > 0.0 {
        votes.push(("baseline", base_pts, W_BASELINE + 0.0_f64.max(inp.baseline_gp)));
    }
    if let Some(p) = prior {
        if let Some(n) = p.pts_pg_nhle {
            if p.nhle_confidence > 0.0 {
                votes.push(("NHLe", n, W_NHLE * p.nhle_confidence));
            }
        }
        if let Some(pp) = p.pedigree_pts_pg {
            if p.pedigree_weight > 0.0 {
                votes.push(("pedigree", pp, p.pedigree_weight));
            }
        }
    }
    est.votes = votes
        .iter()
        .map(|(n, p, w)| {
            let mut m = Map::new();
            m.insert("name".to_string(), json!(n));
            m.insert("pts_pg".to_string(), json!(round_to(*p, 4)));
            m.insert("weight".to_string(), json!(round_to(*w, 2)));
            m
        })
        .collect();
    est.baseline_pts_pg = if base_pts > 0.0 { Some(base_pts) } else { None };
    let total_w = py_sum(votes.iter().map(|v| v.2));
    let mut rates: Map<String, f64>;
    let post: Option<f64>;
    if votes.is_empty() {
        rates = base.clone();
        post = if base_pts > 0.0 { Some(base_pts) } else { None };
    } else if votes.len() == 1 && votes[0].0 == "baseline" {
        rates = base.clone();
        post = Some(base_pts);
    } else {
        let p = py_sum(votes.iter().map(|v| v.2 * v.1)) / total_w;
        rates = if !base.is_empty() && base_pts > 0.0 { scale(&base, p / base_pts) } else { from_prior(p, prior, group, mean) };
        let vtxt: Vec<String> = votes
            .iter()
            .map(|(n, pp, w)| format!("{n} {} (w {})", fixed(*pp, 2, false), fixed(*w, 0, false)))
            .collect();
        let old = if base_pts > 0.0 { fixed(base_pts, 2, false) } else { "none".to_string() };
        let fpg = match inp.scoring {
            Some(s) if !base.is_empty() => {
                format!("; {} -> {} FPG", fixed(s.value(&base), 2, false), fixed(s.value(&rates), 2, false))
            }
            Some(s) => format!("; {} FPG", fixed(s.value(&rates), 2, false)),
            None => String::new(),
        };
        reasons.push(
            Reason::new(
                "ROOKIE_BLEND",
                format!("Rookie blend: PTS/GP {old} -> {} from {}{fpg}", fixed(p, 2, false), vtxt.join(" + ")),
            )
            .with_value(Some(round_to(p, 4)))
            .with_baseline(if base_pts > 0.0 { Some(round_to(base_pts, 4)) } else { None }),
        );
        post = Some(p);
    }
    est.pts_pg = post;
    est.confidence = if total_w > 0.0 { round_to(total_w / (total_w + W_BASELINE), 3) } else { 0.0 };

    // ---- role signals: Daily Faceoff + news
    let dfo: Vec<Signal> = inp.deployment.map(dfo_signals).unwrap_or_default();
    let news = usable_news(news_signals, inp.as_of, SIGNAL_MAX_AGE_DAYS);
    let used: Vec<Signal> = dfo.iter().chain(news.iter()).cloned().collect();
    est.signals = used.iter().map(|s| s.to_json()).collect();
    let pos_top = used.iter().any(|d| TOP_KINDS.contains(&d.kind.as_str()) && d.direction > 0);
    let (mult, why) = role_multiplier(&used);
    for d in &dfo {
        reasons.push(
            Reason::new("ROLE_DFO", format!("{} ({}, {})", d.quote, d.kind.replace('_', " "), sign_char(d.direction)))
                .with_value(Some(d.direction as f64))
                .with_baseline(Some(d.confidence)),
        );
    }
    for d in &news {
        reasons.push(
            Reason::new(
                "ROLE_NEWS",
                format!("{} {}: {}", d.kind.replace('_', " "), sign_char(d.direction), fmt_signal(d)),
            )
            .with_value(Some(d.direction as f64))
            .with_baseline(Some(d.confidence)),
        );
    }
    if mult != 1.0 && !rates.is_empty() {
        rates = scale(&rates, mult);
        let w0 = &why[0];
        reasons.push(
            Reason::new(
                if mult > 1.0 { "ROLE_BOOST" } else { "ROLE_DEPTH" },
                format!(
                    "Role x{} on scoring rates ({} from {})",
                    fixed(mult, 2, false),
                    w0.kind.replace('_', " "),
                    w0.source_or_news()
                ),
            )
            .with_value(Some(mult))
            .with_baseline(Some(1.0)),
        );
    }
    est.role_mult = mult;

    // ---- expected games share
    let season_gp = inp.season_gp;
    let confirmed = season_gp > 0 || used.iter().any(|d| d.kind == "nhl_roster" && d.direction > 0);
    let (mut gp_exp, why_txt): (f64, String) = if confirmed {
        (
            GP_CONFIRMED,
            if season_gp <= 0 { "confirmed in the NHL lineup".to_string() } else { format!("{season_gp} GP this season") },
        )
    } else if let Some(team) = player.team.as_deref().filter(|t| !t.is_empty()) {
        let top10 = player.draft_overall.is_some_and(|o| o <= 10);
        (
            if top10 { GP_TOP10_ON_TEAM } else { GP_ON_TEAM },
            format!("on {team}{}, roster spot not confirmed", if top10 { " as a top-10 pick" } else { "" }),
        )
    } else {
        (GP_NO_TEAM, "no NHL team".to_string())
    };
    let mut parts: Vec<String> = vec![format!("{why_txt} {}", percent(gp_exp, 0, false))];
    if let Some(pg) = inp.projection_gp.filter(|g| *g != 0) {
        let share = 1.0_f64.min(pg as f64 / PROJ_FULL_GP);
        let wp = if confirmed { PROJ_GP_WEIGHT_CONFIRMED } else { PROJ_GP_WEIGHT };
        gp_exp = (1.0 - wp) * gp_exp + wp * share;
        parts.push(format!(
            "{} weight on the projection's {pg} GP ({})",
            percent(wp, 0, false),
            percent(share, 0, false)
        ));
    }
    let mut week = 1.0;
    let latest = news
        .iter()
        .chain(dfo.iter())
        .find(|d| matches!(d.kind.as_str(), "nhl_roster" | "ahl_demotion" | "junior_return"));
    let demoted = latest.is_some_and(|l| matches!(l.kind.as_str(), "ahl_demotion" | "junior_return") && l.direction < 0);
    if demoted {
        let l = latest.expect("demoted implies latest");
        gp_exp = if l.kind == "junior_return" { GP_JUNIOR } else { GP_AHL };
        week = gp_exp;
        parts.push(format!("latest news {} -> {}", fmt_signal(l), percent(gp_exp, 0, false)));
    } else {
        if pos_top && gp_exp < GP_TOP_ROLE {
            gp_exp = GP_TOP_ROLE;
            parts.push(format!("top role -> {}", percent(gp_exp, 0, false)));
        }
        if used.iter().find(|d| d.kind == "scratched").is_some_and(|d| d.direction < 0) {
            gp_exp *= SCRATCH_MULT;
            week *= SCRATCH_MULT;
            parts.push(format!("scratched x{}", general(SCRATCH_MULT, None, false)));
        }
        if season_gp > 0 && gp_exp < 1.0 {
            gp_exp += (1.0 - gp_exp) * 1.0_f64.min(season_gp as f64 / GP_FADE_SEASON_GP);
        }
        if !confirmed {
            week = gp_exp;
        }
    }
    est.gp_expectation = round_to(0.0_f64.max(1.0_f64.min(gp_exp)), 4);
    est.week_share = round_to(0.0_f64.max(1.0_f64.min(week)), 4);
    reasons.push(
        Reason::new(
            "GP_EXPECTATION",
            format!("Expected games share {}: {}", percent(est.gp_expectation, 0, false), parts.join("; ")),
        )
        .with_value(Some(est.gp_expectation))
        .with_baseline(Some(1.0)),
    );
    est.rates = rates;
    est.reasons = reasons;
    est
}

/// `has_rookie_evidence(p, history, signals)`: pedigree loaded (draft slot or career GP), league
/// history, a Daily Faceoff lineup spot or a news role signal.
pub fn has_rookie_evidence<S>(p: &Player, history: &[LeagueSeason], signals: &[S]) -> bool {
    !history.is_empty()
        || !signals.is_empty()
        || p.draft_overall.is_some()
        || p.career_gp.is_some()
        || p.line.as_deref().is_some_and(|s| !s.is_empty())
        || p.pp_unit.as_deref().is_some_and(|s| !s.is_empty())
}
