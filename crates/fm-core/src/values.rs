//! Value types shared by valuation, recommend, analysis, CLI and web. COMPLETE.
//!
//! * `valuation.valuate.PlayerValue`      -> [`PlayerValue`]
//! * `valuation.dynasty.DynastyValue`     -> [`DynastyValue`]
//! * `valuation.picks.PickAsset`          -> [`PickAsset`]
//! * `valuation.rookie.RookiePrior/RookieEstimate` -> [`RookiePrior`], [`RookieEstimate`]
//!
//! Python's `dynasty_values: Mapping[str, Any]` is either `DynastyValue`s or plain floats
//! (`recommend.base.dynasty_floats`); in Rust it is always `Map<Cid, DynastyValue>` and float
//! maps are separate `Map<Cid, f64>`.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::{Player, Reason};
use crate::Map;

/// `valuation.adjust.Horizon`: "week" | "season".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Horizon {
    Week,
    Season,
}

impl Horizon {
    pub fn as_str(&self) -> &'static str {
        match self {
            Horizon::Week => "week",
            Horizon::Season => "season",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayerValue {
    pub player: Player,
    /// healthy per-game value (blended rates)
    pub fpg: f64,
    /// fpg * season availability
    pub fpg_season: f64,
    /// week per-game value (see valuate module docs)
    pub fpg_week: f64,
    /// season horizon
    pub vorp: f64,
    #[serde(default)]
    pub vorp_week: f64,
    /// projected points over the 7-day window
    #[serde(default)]
    pub proj_week: Option<f64>,
    #[serde(default)]
    pub games_next7: Option<i64>,
    #[serde(default)]
    pub offnight_next7: Option<i64>,
    /// goalies only
    #[serde(default)]
    pub start_share: Option<f64>,
    #[serde(default)]
    pub return_date: Option<NaiveDate>,
    #[serde(default = "one")]
    pub avail_week: f64,
    /// Excluded from Python dumps (archive refit inputs only); present in the parity fixture.
    #[serde(default, skip_serializing)]
    pub share_source: Option<String>,
    #[serde(default, skip_serializing)]
    pub share_starts: Option<f64>,
    #[serde(default, skip_serializing)]
    pub share_team_games: Option<f64>,
    #[serde(default)]
    pub horizon_values: Map<String, f64>,
    /// Per-game rates. Python builds this from set unions, so its KEY ORDER is hash-random:
    /// compare as a map, never by order.
    #[serde(default)]
    pub rates: Map<String, f64>,
    #[serde(default)]
    pub reasons: Vec<Reason>,
    /// Unproven skaters valued by valuation.rookie (display / dynasty only; excluded from dumps).
    #[serde(default, skip_serializing)]
    pub rookie: Option<RookieEstimate>,
}

fn one() -> f64 {
    1.0
}

impl PlayerValue {
    pub fn fpg_for(&self, horizon: Horizon) -> f64 {
        match horizon {
            Horizon::Week => self.fpg_week,
            Horizon::Season => self.fpg_season,
        }
    }

    pub fn vorp_for(&self, horizon: Horizon) -> f64 {
        match horizon {
            Horizon::Week => self.vorp_week,
            Horizon::Season => self.vorp,
        }
    }

    /// Value used by the lineup solver: projected week points when a schedule is known.
    pub fn lineup_value(&self, horizon: Horizon) -> f64 {
        match horizon {
            Horizon::Week => self.proj_week.unwrap_or(self.fpg_week),
            Horizon::Season => self.fpg_season,
        }
    }

    pub fn reason(&self, code: &str) -> Option<&Reason> {
        self.reasons.iter().find(|r| r.code == code)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DynastyValue {
    pub player: Player,
    pub value: f64,
    pub age: Option<f64>,
    /// production-curve level today (1.0 = positional peak)
    pub age_mult: f64,
    /// growth / pedigree premium over a flat trajectory (signed)
    pub upside: f64,
    pub horizon_years: i64,
    /// before the market prior
    #[serde(default)]
    pub model_value: Option<f64>,
    /// keeper leagues: the market's view of `future` only
    #[serde(default)]
    pub market_value: Option<f64>,
    #[serde(default = "one")]
    pub pedigree: f64,
    #[serde(default = "default_mode")]
    pub mode: String,
    /// this season's term (w_0 x fpg_season)
    #[serde(default)]
    pub season_part: f64,
    /// seasons after this one (years >= 1 + terminal), market-blended
    #[serde(default)]
    pub future: f64,
    /// max(0, future - a waiver-level roster spot's future value)
    #[serde(default)]
    pub keeper_surplus: f64,
    /// among his holder's top keepers_per_team by keeper_surplus (free agents: would enter MY top)
    #[serde(default = "default_true")]
    pub keeper: bool,
    #[serde(default)]
    pub reasons: Vec<Reason>,
}

fn default_mode() -> String {
    // valuation.dynasty.DEFAULT_MODE
    "contend".to_string()
}

fn default_true() -> bool {
    true
}

/// One draft pick and its value (`valuation.picks.PickAsset`, a dataclass).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PickAsset {
    pub year: i64,
    pub round: i64,
    /// team id that holds the pick now
    pub holder: String,
    /// team id whose draft slot it is
    pub original: String,
    pub original_name: String,
    pub years_ahead: i64,
    /// within the round, 1 = first
    pub expected_slot: f64,
    /// expected overall pick number
    pub overall: f64,
    /// dynasty value units (the expected draftee, comparable to players)
    pub value: f64,
    /// holder inferred (the provider did not report this team's picks)
    #[serde(default)]
    pub inferred: bool,
    /// value over the roster spot the draftee will take (>= 0)
    #[serde(default)]
    pub net: f64,
}

impl PickAsset {
    pub fn key(&self) -> String {
        format!("pick:{}:{}:{}", self.year, self.round, self.original)
    }

    pub fn label(&self) -> String {
        format!("{} R{} ({})", self.year, self.round, self.original_name)
    }

    pub fn short(&self, holder_id: Option<&str>) -> String {
        let own = holder_id.is_some_and(|h| self.original == h);
        let base = format!("{} R{}", self.year, self.round);
        if own {
            base
        } else {
            format!("{base} (from {})", self.original_name)
        }
    }
}

/// Evidence about an unproven player that does not come from his NHL stat lines.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RookiePrior {
    #[serde(default)]
    pub pts_pg_nhle: Option<f64>,
    #[serde(default)]
    pub nhle_confidence: f64,
    #[serde(default)]
    pub nhle_gp: i64,
    #[serde(default)]
    pub nhle_rows: Vec<Map<String, Value>>,
    #[serde(default)]
    pub g_share: Option<f64>,
    #[serde(default)]
    pub sog_pg_est: Option<f64>,
    #[serde(default)]
    pub pedigree_pts_pg: Option<f64>,
    #[serde(default)]
    pub pedigree_weight: f64,
    #[serde(default)]
    pub pedigree_label: Option<String>,
    #[serde(default = "one")]
    pub gp_expectation: f64,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub reasons: Vec<Reason>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RookieEstimate {
    /// per game, before preseason
    #[serde(default)]
    pub rates: Map<String, f64>,
    /// season games share (0..1)
    #[serde(default = "one")]
    pub gp_expectation: f64,
    /// near-term games share (chance to dress this week)
    #[serde(default = "one")]
    pub week_share: f64,
    #[serde(default)]
    pub confidence: f64,
    /// posterior PTS/GP (before the role multiplier)
    #[serde(default)]
    pub pts_pg: Option<f64>,
    #[serde(default = "one")]
    pub role_mult: f64,
    #[serde(default)]
    pub baseline_pts_pg: Option<f64>,
    /// [{name, pts_pg, weight}]
    #[serde(default)]
    pub votes: Vec<Map<String, Value>>,
    /// role signals used (DFO + news)
    #[serde(default)]
    pub signals: Vec<Map<String, Value>>,
    #[serde(default)]
    pub prior: Option<RookiePrior>,
    /// league-season rows for display
    #[serde(default)]
    pub history: Vec<Map<String, Value>>,
    #[serde(default)]
    pub reasons: Vec<Reason>,
}

impl Default for RookieEstimate {
    fn default() -> Self {
        RookieEstimate {
            rates: Map::new(),
            gp_expectation: 1.0,
            week_share: 1.0,
            confidence: 0.0,
            pts_pg: None,
            role_mult: 1.0,
            baseline_pts_pg: None,
            votes: vec![],
            signals: vec![],
            prior: None,
            history: vec![],
            reasons: vec![],
        }
    }
}
