//! `fantasy_manager/models.py`: provider-neutral domain models. COMPLETE.
//!
//! Field order = Python declaration order (serde serializes in struct order, which keeps
//! `model_dump()`-shaped JSON identical). Python `float | None` -> `Option<f64>`, `int` -> `i64`,
//! `dict` -> `Map` (IndexMap: insertion order), `tuple` -> Rust tuple (JSON array).

use std::sync::Arc;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pydt::PyDateTime;
use crate::snapshot::EnrichState;
use crate::{CoreError, Map};

pub const SKATER_STATS: &[&str] = &[
    "G", "A", "PTS", "PM", "PIM", "PPG", "PPA", "PPP", "SHG", "SHA", "SHP", "GWG", "FOW", "FOL", "SOG", "HIT",
    "BLK", "HAT", "GP", "DEF", "STP", "ENG", "FT",
];
pub const GOALIE_STATS: &[&str] = &["GS", "W", "L", "OTL", "SA", "GA", "SV", "SO", "GAA", "SVPCT"];
/// Stats that are already rates and must not be divided by games played.
pub const RATE_STATS: &[&str] = &["GAA", "SVPCT"];
pub const POSITIONS: &[&str] = &["C", "LW", "RW", "F", "D", "G"];
pub const SLOTS: &[&str] = &["C", "LW", "RW", "F", "D", "G", "UTIL", "BN", "IR"];

pub fn is_canonical_stat(s: &str) -> bool {
    SKATER_STATS.contains(&s) || GOALIE_STATS.contains(&s)
}

/// `models.normalize_name`: the SIMPLE key used for `Player.name_norm` (NFKD, drop combining
/// marks, lowercase, drop `'` and `.`, other non `[a-z0-9 ]` -> space, collapse spaces).
/// NOT the same as `matching.normalize.normalize_name` (see [`crate::normalize`]).
pub fn normalize_name(name: &str) -> String {
    use unicode_normalization::char::is_combining_mark;
    use unicode_normalization::UnicodeNormalization;
    let s: String = name.nfkd().filter(|c| !is_combining_mark(*c)).collect::<String>().to_lowercase();
    let s = s.replace('\'', "").replace('.', "");
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == ' ' {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `Split` literal: which stat line. prior = season N-1, prior2 = N-2, prior3 = N-3.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    Season,
    Prior,
    Prior2,
    Prior3,
    Last7,
    Last15,
    Last30,
    Projected,
}

impl Split {
    pub fn as_str(&self) -> &'static str {
        match self {
            Split::Season => "season",
            Split::Prior => "prior",
            Split::Prior2 => "prior2",
            Split::Prior3 => "prior3",
            Split::Last7 => "last7",
            Split::Last15 => "last15",
            Split::Last30 => "last30",
            Split::Projected => "projected",
        }
    }
}

/// `Status` literal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Healthy,
    Dtd,
    Out,
    Ir,
    Ltir,
    Suspended,
    #[default]
    Unknown,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Healthy => "healthy",
            Status::Dtd => "dtd",
            Status::Out => "out",
            Status::Ir => "ir",
            Status::Ltir => "ltir",
            Status::Suspended => "suspended",
            Status::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "healthy" => Status::Healthy,
            "dtd" => Status::Dtd,
            "out" => Status::Out,
            "ir" => Status::Ir,
            "ltir" => Status::Ltir,
            "suspended" => Status::Suspended,
            "unknown" => Status::Unknown,
            _ => return None,
        })
    }
    /// `status in (...)` helper.
    pub fn is_in(&self, set: &[&str]) -> bool {
        set.contains(&self.as_str())
    }
}

/// `DynastyMode` literal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DynastyMode {
    #[default]
    Contend,
    Balanced,
    Rebuild,
}

impl DynastyMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DynastyMode::Contend => "contend",
            DynastyMode::Balanced => "balanced",
            DynastyMode::Rebuild => "rebuild",
        }
    }
    pub fn parse(s: &str) -> Option<DynastyMode> {
        match s.trim().to_lowercase().as_str() {
            "contend" => Some(DynastyMode::Contend),
            "balanced" => Some(DynastyMode::Balanced),
            "rebuild" => Some(DynastyMode::Rebuild),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatLine {
    pub split: Split,
    pub gp: i64,
    pub stats: Map<String, f64>,
}

impl StatLine {
    /// Per-game rates; rate stats (GAA, SV%) pass through; empty if gp <= 0; "GP" dropped.
    pub fn per_game(&self) -> Map<String, f64> {
        let mut out = Map::new();
        if self.gp <= 0 {
            return out;
        }
        for (k, v) in &self.stats {
            if k == "GP" {
                continue;
            }
            let x = if RATE_STATS.contains(&k.as_str()) { *v } else { *v / self.gp as f64 };
            out.insert(k.clone(), x);
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Player {
    pub cid: String,
    pub name: String,
    pub name_norm: String,
    pub ids: Map<String, String>,
    pub team: Option<String>,
    pub positions: Vec<String>,
    /// The platform's single default position (ESPN defaultPositionId); None when the provider
    /// has no such notion.
    #[serde(default)]
    pub primary_position: Option<String>,
    #[serde(default)]
    pub birth_date: Option<NaiveDate>,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub status_note: Option<String>,
    /// On waivers: the date the claim clears; None for a true free agent.
    #[serde(default)]
    pub waiver_until: Option<NaiveDate>,
    /// Keyed by split name ("season", "prior", ...).
    #[serde(default)]
    pub lines: Map<String, StatLine>,
    #[serde(default)]
    pub pct_owned: Option<f64>,
    #[serde(default)]
    pub draft_overall: Option<i64>,
    #[serde(default)]
    pub draft_round: Option<i64>,
    #[serde(default)]
    pub draft_year: Option<i64>,
    #[serde(default)]
    pub career_gp: Option<i64>,
    #[serde(default)]
    pub pct_owned_change: Option<f64>,
    #[serde(default)]
    pub pct_started: Option<f64>,
    #[serde(default)]
    pub adp: Option<f64>,
    #[serde(default)]
    pub adp_change: Option<f64>,
    #[serde(default)]
    pub toi_per_game: Option<f64>,
    #[serde(default)]
    pub pp_toi_per_game: Option<f64>,
    #[serde(default)]
    pub pp_share: Option<f64>,
    #[serde(default)]
    pub toi_trend: Option<f64>,
    #[serde(default)]
    pub pp_share_trend: Option<f64>,
    /// f1..f4, d1..d3, g
    #[serde(default)]
    pub line: Option<String>,
    /// pp1, pp2 or None
    #[serde(default)]
    pub pp_unit: Option<String>,
    #[serde(default)]
    pub pk_unit: Option<String>,
    #[serde(default)]
    pub line_change: Option<String>,
    #[serde(default)]
    pub usual_line: Option<String>,
    #[serde(default)]
    pub usual_pp_unit: Option<String>,
    #[serde(default)]
    pub confirmed_start: Option<bool>,
    #[serde(default)]
    pub start_source: Option<String>,
    #[serde(default)]
    pub ixg_per_game: Option<f64>,
    #[serde(default)]
    pub goals_minus_ixg: Option<f64>,
    #[serde(default)]
    pub onice_sh_pct: Option<f64>,
    #[serde(default)]
    pub onice_xg_pct: Option<f64>,
    /// "season" / "prior" (which season the MoneyPuck fields describe); None when unknown.
    #[serde(default)]
    pub xg_split: Option<String>,
}

impl Player {
    pub fn is_goalie(&self) -> bool {
        self.positions.iter().any(|p| p == "G")
    }

    /// Canonical NHL id once resolved by the crosswalk (`ids["nhl"]`), None when missing / not
    /// an int.
    pub fn nhl_id(&self) -> Option<i64> {
        self.ids.get("nhl").and_then(|s| s.trim().parse::<i64>().ok())
    }

    /// `p.gp(split)`: games played on that line, 0 without it.
    pub fn gp(&self, split: &str) -> i64 {
        self.lines.get(split).map(|l| l.gp).unwrap_or(0)
    }

    pub fn line_for(&self, split: &str) -> Option<&StatLine> {
        self.lines.get(split)
    }

    pub fn has_position(&self, pos: &str) -> bool {
        self.positions.iter().any(|p| p == pos)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RosterSlot {
    pub slot: String,
    pub player: Option<Player>,
    pub starting: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FantasyTeam {
    pub team_id: String,
    pub name: String,
    pub owner_is_me: bool,
    pub slots: Vec<RosterSlot>,
    #[serde(default)]
    pub record: Option<(i64, i64, i64)>,
}

impl FantasyTeam {
    /// `team.players`: rostered players in slot order.
    pub fn players(&self) -> Vec<&Player> {
        self.slots.iter().filter_map(|s| s.player.as_ref()).collect()
    }
    pub fn players_iter(&self) -> impl Iterator<Item = &Player> {
        self.slots.iter().filter_map(|s| s.player.as_ref())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScoringKind {
    Points,
    Categories,
    Roto,
}

impl ScoringKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ScoringKind::Points => "points",
            ScoringKind::Categories => "categories",
            ScoringKind::Roto => "roto",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoringConfig {
    pub kind: ScoringKind,
    #[serde(default)]
    pub weights: Map<String, f64>,
    #[serde(default)]
    pub categories: Vec<String>,
    /// Points leagues with a separate goalie scoring group: overrides applied to goalie lines.
    #[serde(default)]
    pub goalie_weights: Map<String, f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceSeverity {
    #[default]
    Ok,
    Warn,
    Fail,
}

/// Structured freshness / failure state of one data source (filled by providers.enrich).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub name: String,
    #[serde(default = "default_true")]
    pub ok: bool,
    #[serde(default)]
    pub severity: SourceSeverity,
    #[serde(default)]
    pub fetched_at: Option<PyDateTime>,
    #[serde(default)]
    pub age_seconds: Option<f64>,
    #[serde(default)]
    pub ttl_seconds: Option<f64>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub requests: Option<i64>,
    #[serde(default)]
    pub feeds_valuation: bool,
    #[serde(default)]
    pub stale: Option<bool>,
}

fn default_true() -> bool {
    true
}

fn default_keeper_horizon() -> i64 {
    3
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineupLock {
    Daily,
    Weekly,
}

/// One draft pick a team holds: `{"round": 1, "original_owner": "<team id>"}` (Fantrax).
/// Extra keys are kept in `extra` (Python passes the dict through).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DraftPickRef {
    pub round: i64,
    #[serde(default)]
    pub original_owner: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeagueContext {
    pub provider: String,
    pub league_id: String,
    pub season: i64,
    pub name: String,
    pub scoring: ScoringConfig,
    pub roster_shape: Map<String, i64>,
    pub teams: Vec<FantasyTeam>,
    /// C/LW/RW/F/D/G -> max players at that position on the whole roster (IR included).
    #[serde(default)]
    pub position_limits: Map<String, i64>,
    #[serde(default)]
    pub max_roster_size: Option<i64>,
    /// [] = checked, none set; None = the provider could not tell.
    #[serde(default)]
    pub stat_limits: Option<Vec<String>>,
    pub free_agents: Vec<Player>,
    pub matchup_period: Option<i64>,
    #[serde(default)]
    pub dynasty: bool,
    #[serde(default = "default_keeper_horizon")]
    pub keeper_horizon_years: i64,
    /// Players each team may keep into next season; None = every player is kept (true dynasty).
    #[serde(default)]
    pub keepers_per_team: Option<i64>,
    #[serde(default)]
    pub dynasty_mode: DynastyMode,
    /// "prefs" / "env" / "default" / "option"; None when a loader did not resolve it.
    #[serde(default)]
    pub dynasty_mode_source: Option<String>,
    pub as_of: NaiveDate,
    /// team -> regular-season game dates (NHL schedule)
    #[serde(default)]
    pub schedule: Map<String, Vec<NaiveDate>>,
    /// league-wide games per date
    #[serde(default)]
    pub games_per_day: Map<NaiveDate, i64>,
    /// team -> game date -> opponent label ("BOS" at home, "@BOS" away)
    #[serde(default)]
    pub opponents: Map<String, Map<NaiveDate, String>>,
    /// team -> game date -> puck drop (UTC)
    #[serde(default)]
    pub game_starts: Map<String, Map<NaiveDate, PyDateTime>>,
    #[serde(default)]
    pub season_start: Option<NaiveDate>,
    #[serde(default)]
    pub lineup_lock: Option<LineupLock>,
    /// goalie cid -> (starts, team games) this season (harness ledger)
    #[serde(default)]
    pub goalie_actual_starts: Map<String, (i64, i64)>,
    /// NHL team -> (P(#1 starts the second night of a back-to-back), back-to-backs seen)
    #[serde(default)]
    pub b2b_second_night: Map<String, (f64, i64)>,
    /// NHL team -> {NHL id: name} of the goalies on the club's NHL roster
    #[serde(default)]
    pub nhl_goalies: Map<String, Map<i64, String>>,
    /// cid -> deployment summary (harness.deployment.summarize_rows). Python excludes it from
    /// `model_dump`; the parity snapshot carries it, so it deserializes but never serializes.
    #[serde(default, skip_serializing)]
    pub deployment_details: Map<String, Map<String, Value>>,
    #[serde(default)]
    pub moves_limit_per_period: Option<i64>,
    #[serde(default)]
    pub moves_used_this_period: Option<i64>,
    #[serde(default)]
    pub moves_period_label: Option<String>,
    #[serde(default)]
    pub moves_limit_season: Option<i64>,
    #[serde(default)]
    pub moves_used_season: Option<i64>,
    #[serde(default)]
    pub period_start: Option<NaiveDate>,
    #[serde(default)]
    pub period_end: Option<NaiveDate>,
    /// cid -> date I added him, last 14 days (churn guard)
    #[serde(default)]
    pub recent_adds: Map<String, NaiveDate>,
    #[serde(default)]
    pub faab_remaining: Option<f64>,
    /// team_id -> {draft year: [{"round", "original_owner"}]}
    #[serde(default)]
    pub draft_picks: Map<String, Map<i64, Vec<DraftPickRef>>>,
    #[serde(default)]
    pub source_notes: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub sources: Vec<SourceStatus>,

    /// RUST ONLY: the enrichment registries Python keeps in module globals
    /// (`preseason_enrich._REGISTRY`, `rookie_enrich._HISTORY/_SIGNALS`, `xg_enrich._GSAX/_NORMS`,
    /// `lines_enrich._TEAM_META`). Filled by `fm_providers::enrich` (live) or the snapshot loader
    /// (parity); read by valuation / recommend. Shared (`Arc`) so context clones stay cheap.
    #[serde(skip)]
    pub enrich: Arc<EnrichState>,
}

impl LeagueContext {
    /// `ctx.my_team` (Python raises LookupError when no team is flagged as mine).
    pub fn my_team(&self) -> Result<&FantasyTeam, CoreError> {
        self.teams
            .iter()
            .find(|t| t.owner_is_me)
            .ok_or_else(|| CoreError::Lookup("no team flagged as mine".into()))
    }

    pub fn my_team_index(&self) -> Option<usize> {
        self.teams.iter().position(|t| t.owner_is_me)
    }

    pub fn team(&self, team_id: &str) -> Option<&FantasyTeam> {
        self.teams.iter().find(|t| t.team_id == team_id)
    }

    /// `ctx.all_players()`: rostered players (team order, slot order) then free agents, first
    /// occurrence of each cid wins.
    pub fn all_players(&self) -> Vec<&Player> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for t in &self.teams {
            for p in t.players_iter() {
                if seen.insert(p.cid.as_str()) {
                    out.push(p);
                }
            }
        }
        for p in &self.free_agents {
            if seen.insert(p.cid.as_str()) {
                out.push(p);
            }
        }
        out
    }

    /// cid -> owning team id for rostered players (first team wins), a frequent Python idiom.
    pub fn owner_of(&self) -> Map<String, String> {
        let mut out = Map::new();
        for t in &self.teams {
            for p in t.players_iter() {
                out.entry(p.cid.clone()).or_insert_with(|| t.team_id.clone());
            }
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reason {
    pub code: String,
    pub text: String,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub baseline: Option<f64>,
}

impl Reason {
    pub fn new(code: impl Into<String>, text: impl Into<String>) -> Self {
        Reason { code: code.into(), text: text.into(), value: None, baseline: None }
    }
    pub fn with_value(mut self, v: Option<f64>) -> Self {
        self.value = v;
        self
    }
    pub fn with_baseline(mut self, b: Option<f64>) -> Self {
        self.baseline = b;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecKind {
    Lineup,
    Waiver,
    Trade,
    SellHigh,
    BuyLow,
    Injury,
    Alert,
}

impl RecKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecKind::Lineup => "lineup",
            RecKind::Waiver => "waiver",
            RecKind::Trade => "trade",
            RecKind::SellHigh => "sell_high",
            RecKind::BuyLow => "buy_low",
            RecKind::Injury => "injury",
            RecKind::Alert => "alert",
        }
    }
}

/// Units of `Recommendation.predicted_gain` (see models.py).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GainUnits {
    WeekPts,
    DayPts,
    MatchupPts,
    SeasonFpg,
    LineupFpg,
    Dynasty,
}

impl GainUnits {
    pub fn as_str(&self) -> &'static str {
        match self {
            GainUnits::WeekPts => "week_pts",
            GainUnits::DayPts => "day_pts",
            GainUnits::MatchupPts => "matchup_pts",
            GainUnits::SeasonFpg => "season_fpg",
            GainUnits::LineupFpg => "lineup_fpg",
            GainUnits::Dynasty => "dynasty",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    pub kind: RecKind,
    pub score: f64,
    pub title: String,
    #[serde(default)]
    pub add: Vec<Player>,
    #[serde(default)]
    pub drop: Vec<Player>,
    #[serde(default)]
    pub counterparty: Option<String>,
    #[serde(default)]
    pub reasons: Vec<Reason>,
    #[serde(default)]
    pub narrative: Option<String>,
    /// The deciding reason in one plain sentence (recommend.headline).
    #[serde(default)]
    pub headline: Option<String>,
    #[serde(default)]
    pub predicted_gain: Option<f64>,
    #[serde(default)]
    pub gain_units: Option<GainUnits>,
    #[serde(default)]
    pub horizon_days: Option<i64>,
    /// Players the rec acts on without adding or dropping them (IR moves, status alerts).
    #[serde(default)]
    pub subjects: Vec<Player>,
    /// Absolute 0-10 strength (recommend.strength).
    #[serde(default)]
    pub strength: Option<f64>,
    #[serde(default)]
    pub rank_in_kind: Option<i64>,
    #[serde(default)]
    pub kind_total: Option<i64>,
}

impl Recommendation {
    pub fn new(kind: RecKind, score: f64, title: impl Into<String>) -> Self {
        Recommendation {
            kind,
            score,
            title: title.into(),
            add: vec![],
            drop: vec![],
            counterparty: None,
            reasons: vec![],
            narrative: None,
            headline: None,
            predicted_gain: None,
            gain_units: None,
            horizon_days: None,
            subjects: vec![],
            strength: None,
            rank_in_kind: None,
            kind_total: None,
        }
    }

    pub fn reason(&self, code: &str) -> Option<&Reason> {
        self.reasons.iter().find(|r| r.code == code)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActivityAction {
    Add,
    Drop,
    TradeIn,
    TradeOut,
    Ir,
    Activate,
    Proposed,
}

/// One player move from a provider's league activity feed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActivityItem {
    pub source: String,
    pub tx_id: String,
    pub ts: PyDateTime,
    pub team_id: Option<String>,
    #[serde(default)]
    pub team_name: Option<String>,
    pub action: ActivityAction,
    pub cid: Option<String>,
    #[serde(default)]
    pub player_name: Option<String>,
    #[serde(default)]
    pub nhl_id: Option<i64>,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub counterparty_id: Option<String>,
}

/// One rostered player's lineup slot on one scoring day.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LineupDay {
    pub team_id: String,
    pub date: NaiveDate,
    pub cid: String,
    pub slot: String,
    pub starting: bool,
    #[serde(default)]
    pub provider_pts: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_normalize() {
        assert_eq!(normalize_name("Tim Stützle"), "tim stutzle");
        assert_eq!(normalize_name("Ryan O'Reilly"), "ryan oreilly");
        assert_eq!(normalize_name("J.T. Miller"), "jt miller");
        assert_eq!(normalize_name("  Jean-Gabriel  Pageau "), "jean gabriel pageau");
    }
}
