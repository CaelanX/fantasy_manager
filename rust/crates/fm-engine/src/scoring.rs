//! Rust port of `fantasy_manager/scoring/__init__.py` (pluggable scoring systems).
//! Owner: work package `W2-valuation-base` (wave 2).
//!
//! * [`PointsScoring`]     - weighted sum of per-game stats (H2H points leagues).
//! * [`CategoriesScoring`] - sum of weighted z-scores against a reference pool.
//! * [`RotoScoring`]       - categories + team-level roto standings helpers.
//! * [`Scoring`]           - the enum `from_config` returns (Python's `ScoringSystem` instance);
//!   it also implements the [`ScoringSystem`] trait.
//!
//! Deviations from Python (no behavioural differences):
//! * `value(stats, pool)` with a lazy fit is `CategoriesScoring::value_with_pool(&mut self, ..)`
//!   (identity of the pool is its slice pointer + length instead of `id(pool)`); the plain
//!   `value(stats)` takes `&self` (unfitted -> 0.0), so it is `Sync` and cheap in hot loops.
//! * `RotoScoring` wraps a `CategoriesScoring` and derefs to it (Python subclass).
//! * `PointsScoring` precomputes the goalie-merged weight map once (`{**weights, **goalie_weights}`),
//!   keeping dict order, so `value()` is a single pass with Neumaier summation like `sum()`.

use std::borrow::Borrow;
use std::ops::{Deref, DerefMut};

use fm_core::models::{LeagueContext, Player, ScoringConfig, ScoringKind};
use fm_core::Map;
use fm_pycompat::num::py_sum;

/// Categories where a lower number is better. Their weight is always forced negative.
pub const NEGATIVE_BY_DEFAULT: [&str; 3] = ["GAA", "GA", "L"];
pub const RATIO_CATEGORIES: [&str; 2] = ["SVPCT", "GAA"];
/// Stats that mark a stat dict as a goalie line (goalie_weights then apply).
pub const GOALIE_MARKERS: [&str; 4] = ["GS", "SA", "SV", "GA"];

/// Per-game stat dict.
pub type Stats = Map<String, f64>;

/// Anything the `regression.league_weights` helper accepts (`ScoringSystem`, `ScoringConfig`, plain
/// weights mapping). `kind` None means "no kind attribute" (a plain mapping).
pub trait ScoringLike {
    fn scoring_kind(&self) -> Option<&'static str>;
    fn scoring_weights(&self) -> Option<&Map<String, f64>>;
}

pub trait ScoringSystem {
    fn kind(&self) -> &'static str;
    /// Fantasy value per game for a player's rates.
    fn value(&self, per_game_stats: &Stats) -> f64;
}

// ---------------------------------------------------------------------------------- points

#[derive(Clone, Debug)]
pub struct PointsScoring {
    pub weights: Map<String, f64>,
    pub goalie_weights: Map<String, f64>,
    /// `{**weights, **goalie_weights}` (precomputed; empty when there are no goalie weights).
    merged: Map<String, f64>,
}

impl PointsScoring {
    pub fn new(weights: Map<String, f64>, goalie_weights: Option<Map<String, f64>>) -> Self {
        let goalie_weights = goalie_weights.unwrap_or_default();
        let mut merged = weights.clone();
        for (k, v) in &goalie_weights {
            merged.insert(k.clone(), *v);
        }
        PointsScoring { weights, goalie_weights, merged }
    }

    /// Weights for this line: goalie-group overrides apply to goalie-looking lines.
    pub fn weights_for(&self, per_game_stats: &Stats) -> &Map<String, f64> {
        if !self.goalie_weights.is_empty() && GOALIE_MARKERS.iter().any(|m| per_game_stats.contains_key(*m)) {
            &self.merged
        } else {
            &self.weights
        }
    }

    pub fn value(&self, per_game_stats: &Stats) -> f64 {
        let w = self.weights_for(per_game_stats);
        // sum(v * stats[k] for k, v in w.items() if k in stats)
        py_sum(w.iter().filter_map(|(k, v)| per_game_stats.get(k).map(|s| *v * *s)))
    }

    /// Per-stat contribution to fantasy points per game.
    pub fn breakdown(&self, per_game_stats: &Stats) -> Map<String, f64> {
        let w = self.weights_for(per_game_stats);
        let mut out = Map::new();
        for (k, v) in w {
            if let Some(s) = per_game_stats.get(k) {
                out.insert(k.clone(), *v * *s);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------- helpers

/// `_mean_std`: (mean, population std), (0, 0) when empty.
fn mean_std(xs: &[f64]) -> (f64, f64) {
    if xs.is_empty() {
        return (0.0, 0.0);
    }
    let n = xs.len() as f64;
    let mu = py_sum(xs.iter().copied()) / n;
    let var = py_sum(xs.iter().map(|x| (x - mu).powf(2.0))) / n;
    (mu, var.sqrt())
}

fn saves(pg: &Stats) -> Option<f64> {
    if let Some(sv) = pg.get("SV") {
        return Some(*sv);
    }
    if let (Some(sa), Some(ga)) = (pg.get("SA"), pg.get("GA")) {
        return Some(*sa - *ga);
    }
    None
}

/// Truthy `pg.get("SA")` (present and non-zero).
fn sa_truthy(pg: &Stats) -> Option<f64> {
    pg.get("SA").copied().filter(|s| *s != 0.0)
}

fn svpct(pg: &Stats) -> Option<f64> {
    if let Some(v) = pg.get("SVPCT") {
        let v = *v;
        return Some(if v > 1.0 { v / 100.0 } else { v });
    }
    let sa = sa_truthy(pg);
    let sv = saves(pg);
    if let (Some(sa), Some(sv)) = (sa, sv) {
        return Some(sv / sa);
    }
    None
}

/// Volume proxy for GAA: starts per game played (GS/GP); 1.0 when GS is unknown.
fn starts(pg: &Stats) -> f64 {
    match pg.get("GS") {
        Some(gs) if *gs > 0.0 => *gs,
        _ => 1.0,
    }
}

fn gaa(pg: &Stats) -> Option<f64> {
    if let Some(g) = pg.get("GAA") {
        return Some(*g);
    }
    if let Some(ga) = pg.get("GA") {
        return Some(*ga / starts(pg));
    }
    None
}

fn is_goalie_row(pg: &Stats) -> bool {
    ["SVPCT", "SV", "SA", "GAA", "GA", "GS"].iter().any(|k| pg.contains_key(*k))
}

// ---------------------------------------------------------------------------------- categories

#[derive(Clone, Debug)]
pub struct CategoriesScoring {
    pub categories: Vec<String>,
    pub directions: Map<String, f64>,
    params: Option<Map<String, (f64, f64)>>,
    pool_key: Option<(usize, usize)>,
    mean_sa: Option<f64>,
}

impl CategoriesScoring {
    pub fn new(categories: Vec<String>, directions: Option<Map<String, f64>>) -> Self {
        CategoriesScoring {
            categories,
            directions: directions.unwrap_or_default(),
            params: None,
            pool_key: None,
            mean_sa: None,
        }
    }

    pub fn kind(&self) -> &'static str {
        "categories"
    }

    // -- configuration
    pub fn weight(&self, cat: &str) -> f64 {
        let w = self.directions.get(cat).copied().unwrap_or(1.0);
        if NEGATIVE_BY_DEFAULT.contains(&cat) {
            return if w != 0.0 { -w.abs() } else { -1.0 };
        }
        w
    }

    /// +1 when a higher team total is better, -1 when lower is better.
    pub fn better(&self, cat: &str) -> i32 {
        if self.weight(cat) < 0.0 {
            -1
        } else {
            1
        }
    }

    pub fn fitted(&self) -> bool {
        self.params.is_some()
    }

    /// Fitted (reference, sigma) per category (reference = mean, or pool ratio).
    pub fn params(&self) -> Map<String, (f64, f64)> {
        self.params.clone().unwrap_or_default()
    }

    // -- fitting
    /// Precompute per-category reference stats from per-game stat dicts.
    pub fn fit<I>(&mut self, pool: I) -> &mut Self
    where
        I: IntoIterator,
        I::Item: Borrow<Stats>,
    {
        let pool: Vec<I::Item> = pool.into_iter().filter(|p| !p.borrow().is_empty()).collect();
        let pool: Vec<&Stats> = pool.iter().map(|p| p.borrow()).collect();
        self.fit_refs(&pool);
        self
    }

    fn fit_refs(&mut self, pool: &[&Stats]) {
        let goalies: Vec<&Stats> = pool.iter().copied().filter(|p| is_goalie_row(p)).collect();
        let mut params: Map<String, (f64, f64)> = Map::new();
        for c in &self.categories {
            if c == "SVPCT" {
                let mut rows: Vec<(f64, f64)> = Vec::new();
                for p in &goalies {
                    let sv = saves(p);
                    let sa = sa_truthy(p);
                    if let (Some(sv), Some(sa)) = (sv, sa) {
                        rows.push((sv, sa));
                    }
                }
                let (rf, sd);
                if !rows.is_empty() {
                    rf = py_sum(rows.iter().map(|r| r.0)) / py_sum(rows.iter().map(|r| r.1));
                    let xs: Vec<f64> = rows.iter().map(|(sv, sa)| sv - rf * sa).collect();
                    sd = mean_std(&xs).1;
                    self.mean_sa = Some(py_sum(rows.iter().map(|r| r.1)) / rows.len() as f64);
                } else {
                    // only SV% known: unweighted fallback
                    let xs: Vec<f64> = goalies.iter().filter_map(|p| svpct(p)).collect();
                    let (m, s) = mean_std(&xs);
                    rf = m;
                    sd = s;
                    self.mean_sa = None;
                }
                params.insert(c.clone(), (rf, sd));
            } else if c == "GAA" {
                let rows: Vec<(f64, f64)> = goalies.iter().filter_map(|p| gaa(p).map(|g| (g, starts(p)))).collect();
                if !rows.is_empty() {
                    let rf = py_sum(rows.iter().map(|(g, v)| g * v)) / py_sum(rows.iter().map(|r| r.1));
                    let xs: Vec<f64> = rows.iter().map(|(g, v)| (g - rf) * v).collect();
                    let sd = mean_std(&xs).1;
                    params.insert(c.clone(), (rf, sd));
                } else {
                    params.insert(c.clone(), (0.0, 0.0));
                }
            } else {
                let xs: Vec<f64> = pool.iter().filter_map(|p| p.get(c.as_str()).copied()).collect();
                params.insert(c.clone(), mean_std(&xs));
            }
        }
        self.params = Some(params);
    }

    /// Python `_ensure_fit(pool)`: fit lazily when a (different) non-empty pool is passed.
    fn ensure_fit(&mut self, pool: Option<&[Stats]>) -> bool {
        if let Some(pool) = pool {
            if !pool.is_empty() {
                let key = (pool.as_ptr() as usize, pool.len());
                if self.params.is_none() || self.pool_key != Some(key) {
                    let refs: Vec<&Stats> = pool.iter().filter(|p| !p.is_empty()).collect();
                    self.fit_refs(&refs);
                    self.pool_key = Some(key);
                }
            }
        }
        self.params.is_some()
    }

    // -- valuation
    /// Raw-orientation z per category (higher stat -> higher z); missing stats omitted. Empty when
    /// unfitted.
    pub fn z_scores(&self, per_game_stats: &Stats) -> Map<String, f64> {
        let Some(params) = &self.params else {
            return Map::new();
        };
        let mut out = Map::new();
        for c in &self.categories {
            let (rf, sd) = params.get(c).copied().unwrap_or((0.0, 0.0));
            if sd <= 0.0 {
                continue;
            }
            if c == "SVPCT" {
                let sv = saves(per_game_stats);
                let sa = sa_truthy(per_game_stats);
                if let (Some(sv), Some(sa)) = (sv, sa) {
                    out.insert(c.clone(), (sv - rf * sa) / sd);
                } else if let Some(pct) = svpct(per_game_stats) {
                    let msa = match self.mean_sa {
                        Some(m) if m != 0.0 => m,
                        _ => 1.0,
                    };
                    out.insert(c.clone(), (pct - rf) * msa / sd);
                }
            } else if c == "GAA" {
                if let Some(g) = gaa(per_game_stats) {
                    out.insert(c.clone(), (g - rf) * starts(per_game_stats) / sd);
                }
            } else if let Some(x) = per_game_stats.get(c.as_str()) {
                out.insert(c.clone(), (*x - rf) / sd);
            }
        }
        out
    }

    /// `z_scores(stats, pool)` with lazy fitting.
    pub fn z_scores_with_pool(&mut self, per_game_stats: &Stats, pool: Option<&[Stats]>) -> Map<String, f64> {
        if !self.ensure_fit(pool) {
            return Map::new();
        }
        self.z_scores(per_game_stats)
    }

    /// Per-category weighted z contribution (w_c * z_c).
    pub fn breakdown(&self, per_game_stats: &Stats) -> Map<String, f64> {
        self.z_scores(per_game_stats).into_iter().map(|(c, z)| {
            let w = self.weight(&c);
            (c, w * z)
        }).collect()
    }

    pub fn value(&self, per_game_stats: &Stats) -> f64 {
        py_sum(self.breakdown(per_game_stats).values().copied())
    }

    /// `value(stats, pool)`: fits lazily when a (different) pool slice is passed.
    pub fn value_with_pool(&mut self, per_game_stats: &Stats, pool: Option<&[Stats]>) -> f64 {
        if !self.ensure_fit(pool) {
            return 0.0;
        }
        self.value(per_game_stats)
    }

    // -- team level
    /// Team per-game totals: counting stats summed; SV% = sum SV / sum SA; GAA = starts-weighted mean.
    pub fn team_totals<I>(&self, per_game_rows: I) -> Map<String, f64>
    where
        I: IntoIterator,
        I::Item: Borrow<Stats>,
    {
        let rows: Vec<I::Item> = per_game_rows.into_iter().filter(|r| !r.borrow().is_empty()).collect();
        let mut out = Map::new();
        for c in &self.categories {
            if c == "SVPCT" {
                let (mut sv, mut sa) = (0.0f64, 0.0f64);
                for r in &rows {
                    let r = r.borrow();
                    if let (Some(s), Some(a)) = (saves(r), sa_truthy(r)) {
                        sv += s;
                        sa += a;
                    }
                }
                if sa != 0.0 {
                    out.insert(c.clone(), sv / sa);
                }
            } else if c == "GAA" {
                let (mut num, mut den) = (0.0f64, 0.0f64);
                for r in &rows {
                    let r = r.borrow();
                    if let Some(g) = gaa(r) {
                        let v = starts(r);
                        num += g * v;
                        den += v;
                    }
                }
                if den != 0.0 {
                    out.insert(c.clone(), num / den);
                }
            } else {
                out.insert(c.clone(), py_sum(rows.iter().map(|r| r.borrow().get(c.as_str()).copied().unwrap_or(0.0))));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------- roto

#[derive(Clone, Debug)]
pub struct RotoScoring {
    pub cats: CategoriesScoring,
}

impl Deref for RotoScoring {
    type Target = CategoriesScoring;
    fn deref(&self) -> &CategoriesScoring {
        &self.cats
    }
}
impl DerefMut for RotoScoring {
    fn deref_mut(&mut self) -> &mut CategoriesScoring {
        &mut self.cats
    }
}

impl RotoScoring {
    pub fn new(categories: Vec<String>, directions: Option<Map<String, f64>>) -> Self {
        RotoScoring { cats: CategoriesScoring::new(categories, directions) }
    }

    pub fn kind(&self) -> &'static str {
        "roto"
    }

    /// Per-category standings points vs the OTHER teams: 1 + teams beaten + 0.5 * ties.
    pub fn roto_points<I>(&self, my_totals: &Map<String, f64>, league_totals: I) -> Map<String, f64>
    where
        I: IntoIterator,
        I::Item: Borrow<Map<String, f64>>,
    {
        let others: Vec<I::Item> = league_totals.into_iter().collect();
        self.roto_points_slice(my_totals, &others)
    }

    fn roto_points_slice<T: Borrow<Map<String, f64>>>(&self, my_totals: &Map<String, f64>, others: &[T]) -> Map<String, f64> {
        let mut pts = Map::new();
        for c in &self.cats.categories {
            let Some(&mine) = my_totals.get(c) else { continue };
            let sgn = self.cats.better(c) as f64;
            let mut p = 1.0f64;
            for t in others {
                let Some(&tc) = t.borrow().get(c) else { continue };
                let d = sgn * (mine - tc);
                p += if d > 1e-12 {
                    1.0
                } else if d.abs() <= 1e-12 {
                    0.5
                } else {
                    0.0
                };
            }
            pts.insert(c.clone(), p);
        }
        pts
    }

    /// How far my weak categories sit below mid-pack (0 = none below the median).
    pub fn category_balance_penalty<I>(&self, my_totals: &Map<String, f64>, league_totals: I) -> f64
    where
        I: IntoIterator,
        I::Item: Borrow<Map<String, f64>>,
    {
        let others: Vec<I::Item> = league_totals.into_iter().collect();
        let n = (others.len() + 1) as f64;
        let pts = self.roto_points_slice(my_totals, &others);
        if pts.is_empty() {
            return 0.0;
        }
        let mid = (n + 1.0) / 2.0;
        py_sum(pts.values().map(|p| (mid - p).max(0.0) / n)) / pts.len() as f64
    }
}

// ---------------------------------------------------------------------------------- enum + glue

/// A `ScoringSystem` instance (what `from_config` returns).
#[derive(Clone, Debug)]
pub enum Scoring {
    Points(PointsScoring),
    Categories(CategoriesScoring),
    Roto(RotoScoring),
}

impl Scoring {
    pub fn kind(&self) -> &'static str {
        match self {
            Scoring::Points(_) => "points",
            Scoring::Categories(_) => "categories",
            Scoring::Roto(_) => "roto",
        }
    }

    pub fn value(&self, per_game_stats: &Stats) -> f64 {
        match self {
            Scoring::Points(s) => s.value(per_game_stats),
            Scoring::Categories(s) => s.value(per_game_stats),
            Scoring::Roto(s) => s.value(per_game_stats),
        }
    }

    pub fn as_points(&self) -> Option<&PointsScoring> {
        match self {
            Scoring::Points(s) => Some(s),
            _ => None,
        }
    }

    /// `isinstance(scoring, CategoriesScoring)` (roto included) and the instance.
    pub fn as_categories(&self) -> Option<&CategoriesScoring> {
        match self {
            Scoring::Categories(s) => Some(s),
            Scoring::Roto(s) => Some(&s.cats),
            Scoring::Points(_) => None,
        }
    }

    pub fn as_categories_mut(&mut self) -> Option<&mut CategoriesScoring> {
        match self {
            Scoring::Categories(s) => Some(s),
            Scoring::Roto(s) => Some(&mut s.cats),
            Scoring::Points(_) => None,
        }
    }

    pub fn as_roto(&self) -> Option<&RotoScoring> {
        match self {
            Scoring::Roto(s) => Some(s),
            _ => None,
        }
    }
}

impl ScoringSystem for PointsScoring {
    fn kind(&self) -> &'static str {
        "points"
    }
    fn value(&self, s: &Stats) -> f64 {
        PointsScoring::value(self, s)
    }
}
impl ScoringSystem for CategoriesScoring {
    fn kind(&self) -> &'static str {
        "categories"
    }
    fn value(&self, s: &Stats) -> f64 {
        CategoriesScoring::value(self, s)
    }
}
impl ScoringSystem for RotoScoring {
    fn kind(&self) -> &'static str {
        "roto"
    }
    fn value(&self, s: &Stats) -> f64 {
        self.cats.value(s)
    }
}
impl ScoringSystem for Scoring {
    fn kind(&self) -> &'static str {
        Scoring::kind(self)
    }
    fn value(&self, s: &Stats) -> f64 {
        Scoring::value(self, s)
    }
}

impl ScoringLike for PointsScoring {
    fn scoring_kind(&self) -> Option<&'static str> {
        Some("points")
    }
    fn scoring_weights(&self) -> Option<&Map<String, f64>> {
        Some(&self.weights)
    }
}
impl ScoringLike for CategoriesScoring {
    fn scoring_kind(&self) -> Option<&'static str> {
        Some("categories")
    }
    fn scoring_weights(&self) -> Option<&Map<String, f64>> {
        None
    }
}
impl ScoringLike for RotoScoring {
    fn scoring_kind(&self) -> Option<&'static str> {
        Some("roto")
    }
    fn scoring_weights(&self) -> Option<&Map<String, f64>> {
        None
    }
}
impl ScoringLike for Scoring {
    fn scoring_kind(&self) -> Option<&'static str> {
        Some(self.kind())
    }
    fn scoring_weights(&self) -> Option<&Map<String, f64>> {
        match self {
            Scoring::Points(p) => Some(&p.weights),
            _ => None,
        }
    }
}
impl ScoringLike for ScoringConfig {
    fn scoring_kind(&self) -> Option<&'static str> {
        Some(match self.kind {
            ScoringKind::Points => "points",
            ScoringKind::Categories => "categories",
            ScoringKind::Roto => "roto",
        })
    }
    fn scoring_weights(&self) -> Option<&Map<String, f64>> {
        Some(&self.weights)
    }
}
impl ScoringLike for Map<String, f64> {
    fn scoring_kind(&self) -> Option<&'static str> {
        None
    }
    fn scoring_weights(&self) -> Option<&Map<String, f64>> {
        Some(self)
    }
}

pub const DEFAULT_SPLITS: [&str; 3] = ["season", "projected", "prior"];

/// Per-game rows for a reference pool: each player's first split with >= min_gp games.
pub fn pool_from_players<'a, I>(players: I, splits: &[&str], min_gp: i64) -> Vec<Stats>
where
    I: IntoIterator<Item = &'a Player>,
{
    let mut rows = Vec::new();
    for p in players {
        for s in splits {
            if let Some(ln) = p.lines.get(*s) {
                if ln.gp >= min_gp {
                    rows.push(ln.per_game());
                    break;
                }
            }
        }
    }
    rows
}

/// Fit a relative scoring system on the league's rostered players (no-op for points).
pub fn fit_to_context(scoring: &mut Scoring, ctx: &LeagueContext) {
    if let Some(cs) = scoring.as_categories_mut() {
        let pool = pool_from_players(ctx.teams.iter().flat_map(|t| t.players_iter()), &DEFAULT_SPLITS, 1);
        cs.fit(pool.iter());
    }
}

pub fn from_config(cfg: &ScoringConfig) -> Scoring {
    match cfg.kind {
        ScoringKind::Points => {
            let gw = if cfg.goalie_weights.is_empty() { None } else { Some(cfg.goalie_weights.clone()) };
            Scoring::Points(PointsScoring::new(cfg.weights.clone(), gw))
        }
        ScoringKind::Categories => {
            Scoring::Categories(CategoriesScoring::new(cfg.categories.clone(), Some(cfg.weights.clone())))
        }
        ScoringKind::Roto => Scoring::Roto(RotoScoring::new(cfg.categories.clone(), Some(cfg.weights.clone()))),
    }
}
