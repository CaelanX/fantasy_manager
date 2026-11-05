//! Rust replacements for Python's module-level enrichment registries ([`EnrichState`]), the
//! provider side data compute code reads off the provider object ([`ProviderExtras`]), and the
//! parity snapshot file ([`Snapshot`], written by `rust/parity/dump_fixtures.py`). COMPLETE.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::{LeagueContext, Player};
use crate::pydt::PyDateTime;
use crate::Map;

// ------------------------------------------------------------------ registry payload types

/// `providers.preseason_enrich.PreseasonLine`: one skater's preseason totals.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreseasonLine {
    pub nhl_id: i64,
    /// NHL season id, e.g. 20262027
    pub season: i64,
    pub gp: i64,
    /// canonical stat keys, `GP` included
    pub stats: Map<String, f64>,
    #[serde(default)]
    pub toi_seconds: f64,
    #[serde(default)]
    pub toi_games: i64,
    /// PPA / PPP present (every game's scoring summary parsed)
    #[serde(default)]
    pub has_strength: bool,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub last_date: Option<NaiveDate>,
    #[serde(default)]
    pub games: Vec<NaiveDate>,
}

impl PreseasonLine {
    /// Minutes per game (games with a TOI value only).
    pub fn toi_per_game(&self) -> Option<f64> {
        if self.toi_games > 0 {
            Some(self.toi_seconds / self.toi_games as f64 / 60.0)
        } else {
            None
        }
    }
}

/// `providers.nhl.LeagueSeason`: one regular-season line in any league.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeagueSeason {
    /// e.g. 20252026
    pub season: i64,
    /// leagueAbbrev: "SHL", "OHL", "AHL", "NHL", "WJC-20", ...
    pub league: String,
    #[serde(default)]
    pub team: Option<String>,
    pub gp: i64,
    #[serde(default)]
    pub g: i64,
    #[serde(default)]
    pub a: i64,
    #[serde(default)]
    pub pts: i64,
    /// age on Oct 1 of the season's start year
    #[serde(default)]
    pub age_at_season: Option<f64>,
}

impl LeagueSeason {
    pub fn start_year(&self) -> i64 {
        self.season.div_euclid(10000)
    }
}

/// `providers.news_roles.RoleSignal`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoleSignal {
    pub player_name: Option<String>,
    #[serde(default)]
    pub cid: Option<String>,
    pub kind: String,
    /// +1 / -1 / 0
    pub direction: i64,
    pub confidence: f64,
    pub quote: String,
    #[serde(default)]
    pub published: Option<PyDateTime>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub item_id: Option<String>,
    /// "rules" | "llm"
    #[serde(default = "default_origin")]
    pub origin: String,
    #[serde(default)]
    pub ambiguous: bool,
}

fn default_origin() -> String {
    "rules".into()
}

impl RoleSignal {
    /// `label()`: "kind+" / "kind-" / "kind?".
    pub fn label(&self) -> String {
        let sign = match self.direction {
            1 => "+",
            -1 => "-",
            _ => "?",
        };
        format!("{}{}", self.kind, sign)
    }

    /// `when()`: published date as %Y-%m-%d or "undated".
    pub fn when(&self) -> String {
        match &self.published {
            Some(p) => p.local.format("%Y-%m-%d").to_string(),
            None => "undated".into(),
        }
    }
}

/// `valuation.regression.GoalieGsax` (data only: the methods `usable` / `sv_pct` /
/// `sv_pct_on` live in `fm_engine::valuation::regression` because they need its constants).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalieGsax {
    pub nhl_id: i64,
    pub seasons: Vec<i64>,
    pub gsax: f64,
    pub shots: f64,
    pub raw_gsax: f64,
    pub raw_shots: f64,
    pub xsv_pct: f64,
    pub per_shot: f64,
    #[serde(default)]
    pub shots_per_game: Option<f64>,
    #[serde(default)]
    pub league_sv_pct: Option<f64>,
}

/// The enrichment registries Python keeps as module globals. Lookups mirror the Python
/// accessors (`preseason_line`, `history_for`, `signals_for`, `all_signals`, `goalie_gsax`,
/// `onice_norms`, `team_meta_for`).
#[derive(Clone, Debug, Default)]
pub struct EnrichState {
    /// (NHL season id, NHL player id) -> preseason line (`preseason_enrich._REGISTRY`)
    pub preseason: HashMap<(i64, i64), PreseasonLine>,
    /// NHL id -> league-season history (`rookie_enrich._HISTORY`)
    pub rookie_history: HashMap<i64, Vec<LeagueSeason>>,
    /// cid -> role signals, insertion ordered (`rookie_enrich._SIGNALS`)
    pub rookie_signals: Map<String, Vec<RoleSignal>>,
    /// (season start year, NHL id) -> regressed GSAx (`xg_enrich._GSAX`)
    pub gsax: HashMap<(i64, i64), GoalieGsax>,
    /// season start year -> 5v5 on-ice shooting % norms (`xg_enrich._NORMS`)
    pub xg_norms: HashMap<i64, Map<String, f64>>,
    /// date -> team -> {"source", "source_url", "updated_at"} (`lines_enrich._TEAM_META`)
    pub line_team_meta: HashMap<NaiveDate, Map<String, Map<String, Value>>>,
}

impl EnrichState {
    pub fn is_empty(&self) -> bool {
        self.preseason.is_empty()
            && self.rookie_history.is_empty()
            && self.rookie_signals.is_empty()
            && self.gsax.is_empty()
            && self.xg_norms.is_empty()
            && self.line_team_meta.is_empty()
    }
    /// `preseason_enrich.preseason_line(p, season)`.
    pub fn preseason_line(&self, p: &Player, season: i64) -> Option<&PreseasonLine> {
        self.preseason.get(&(season, p.nhl_id()?))
    }
    /// `rookie_enrich.history_for(p)`.
    pub fn history_for(&self, p: &Player) -> &[LeagueSeason] {
        p.nhl_id().and_then(|id| self.rookie_history.get(&id)).map(|v| v.as_slice()).unwrap_or(&[])
    }
    /// `rookie_enrich.signals_for(p)`.
    pub fn signals_for(&self, p: &Player) -> &[RoleSignal] {
        self.rookie_signals.get(&p.cid).map(|v| v.as_slice()).unwrap_or(&[])
    }
    /// `xg_enrich.goalie_gsax(p, season_start_year)`.
    pub fn goalie_gsax(&self, p: &Player, season_start_year: i64) -> Option<&GoalieGsax> {
        self.gsax.get(&(season_start_year, p.nhl_id()?))
    }
    /// `xg_enrich.onice_norms(season_start_year)` ({} if none).
    pub fn onice_norms(&self, season_start_year: i64) -> Map<String, f64> {
        self.xg_norms.get(&season_start_year).cloned().unwrap_or_default()
    }
    /// `lines_enrich.team_meta_for(day)`.
    pub fn team_meta_for(&self, day: NaiveDate) -> Option<&Map<String, Map<String, Value>>> {
        self.line_team_meta.get(&day)
    }
}

// ------------------------------------------------------------------ provider side data

/// `providers.fantrax.TradeBlockInfo`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TradeBlockInfo {
    pub team_id: String,
    pub team_name: String,
    #[serde(default)]
    pub note: String,
    /// player cids
    #[serde(default)]
    pub players_offered: Vec<String>,
    #[serde(default)]
    pub players_wanted: Vec<String>,
    #[serde(default)]
    pub positions_offered: Vec<String>,
    #[serde(default)]
    pub positions_wanted: Vec<String>,
    #[serde(default)]
    pub stats_offered: Vec<String>,
    #[serde(default)]
    pub stats_wanted: Vec<String>,
}

/// `providers.fantrax.TransactionInfo`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransactionInfo {
    pub tx_id: String,
    pub team_id: Option<String>,
    pub when: Option<String>,
    pub kind: String,
    pub player_cid: Option<String>,
    pub player_name: Option<String>,
}

/// `providers.fantrax.PendingTradeInfo`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingTradeInfo {
    pub trade_id: String,
    pub proposed_by: Option<String>,
    #[serde(default)]
    pub moves: Vec<Map<String, Value>>,
    #[serde(default)]
    pub proposed_at: Option<String>,
}

/// `providers.fantrax.LeagueRules`: what the league's Rules page says.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LeagueRules {
    /// section -> label -> value
    #[serde(default)]
    pub settings: Map<String, Map<String, String>>,
    /// group -> {abbr: points}
    #[serde(default)]
    pub scoring: Map<String, Map<String, f64>>,
    /// abbr -> category name
    #[serde(default)]
    pub scoring_names: Map<String, String>,
    /// pos -> min/max active, max
    #[serde(default)]
    pub positions: Map<String, Map<String, Option<i64>>>,
}

impl LeagueRules {
    /// `rules.get(label)`: first section value whose label matches case-insensitively.
    pub fn get(&self, label: &str) -> Option<&str> {
        let want = label.to_lowercase();
        self.settings.values().flat_map(|s| s.iter()).find(|(k, _)| k.to_lowercase() == want).map(|(_, v)| v.as_str())
    }

    /// `rules.get_int(label)`: leading integer of the value (`re.match(r"\s*(-?\d+)")`).
    pub fn get_int(&self, label: &str) -> Option<i64> {
        let v = self.get(label)?.trim_start();
        let (neg, rest) = match v.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, v),
        };
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        let n: i64 = digits.parse().ok()?;
        Some(if neg { -n } else { n })
    }
}

/// The espn_api `League` attributes compute code reads (`league.current_week`,
/// `league.settings.matchup_periods`, ...). `matchup_periods`: matchup period -> scoring periods.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EspnLeagueMeta {
    #[serde(default)]
    pub current_week: Option<i64>,
    #[serde(default, rename = "currentMatchupPeriod")]
    pub current_matchup_period: Option<i64>,
    #[serde(default, rename = "finalScoringPeriod")]
    pub final_scoring_period: Option<i64>,
    #[serde(default, rename = "scoringPeriodId")]
    pub scoring_period_id: Option<i64>,
    #[serde(default)]
    pub settings: EspnLeagueSettings,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EspnLeagueSettings {
    #[serde(default)]
    pub matchup_periods: Map<String, Vec<i64>>,
    #[serde(default)]
    pub reg_season_count: Option<i64>,
    #[serde(default)]
    pub playoff_team_count: Option<i64>,
    /// epoch milliseconds (espn_api passes ESPN's value through)
    #[serde(default)]
    pub trade_deadline: Option<i64>,
    #[serde(default)]
    pub team_count: Option<i64>,
    #[serde(default)]
    pub playoff_matchup_period_length: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Everything compute code reads off a provider object besides `load()`'s LeagueContext
/// (`getattr(provider, "rules" | "ages" | "standings" | "trade_blocks" | ...)`). Providers fill it
/// during `load()`; the snapshot carries it. Fields a provider does not have stay empty/None.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderExtras {
    /// "EspnProvider" | "FantraxProvider"
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Fantrax: cid -> age (fills players without a birth date in apply_dynasty)
    #[serde(default)]
    pub ages: Map<String, f64>,
    /// Fantrax: team id -> {rank, win, loss, tie, points_for, points_against, waiver_order}
    #[serde(default)]
    pub standings: Map<String, Map<String, Value>>,
    #[serde(default)]
    pub trade_blocks: Vec<TradeBlockInfo>,
    #[serde(default)]
    pub pending_trades: Vec<PendingTradeInfo>,
    #[serde(default)]
    pub transactions: Vec<TransactionInfo>,
    #[serde(default)]
    pub scoring_source: Option<String>,
    /// NaN in Python when unchecked (the snapshot writes null)
    #[serde(default)]
    pub weights_mae: Option<f64>,
    #[serde(default)]
    pub weights_checked: Option<i64>,
    #[serde(default)]
    pub config_mae: Option<f64>,
    #[serde(default)]
    pub config_checked: Option<i64>,
    /// Fantrax: my picks by year
    #[serde(default)]
    pub draft_picks: Map<i64, Vec<crate::models::DraftPickRef>>,
    /// Fantrax: every team's picks: team id -> year -> picks
    #[serde(default)]
    pub team_draft_picks: Map<String, Map<i64, Vec<crate::models::DraftPickRef>>>,
    /// ESPN: settings.acquisitionSettings (claims planner)
    #[serde(default)]
    pub acquisition_settings: Map<String, Value>,
    /// ESPN: my team's waiverRank (1 = first claim)
    #[serde(default)]
    pub waiver_rank: Option<i64>,
    /// Fantrax: cid -> (FPts, FP/G) as Fantrax reports them
    #[serde(default)]
    pub fantasy_points: Map<String, (Option<f64>, Option<f64>)>,
    #[serde(default)]
    pub rules: Option<LeagueRules>,
    /// "daily" | "weekly" (Fantrax property)
    #[serde(default)]
    pub lineup_lock: Option<String>,
    #[serde(default)]
    pub rules_dynasty: Option<bool>,
    /// ESPN only
    #[serde(default)]
    pub espn_league: Option<EspnLeagueMeta>,
    /// errors the dump hit reading properties (diagnostic only)
    #[serde(default)]
    pub errors: Vec<String>,
}

// ------------------------------------------------------------------ the snapshot file

/// `snapshot.json` "extras" (registries in JSON form).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotRegistries {
    #[serde(default)]
    pub preseason_lines: Vec<PreseasonLine>,
    #[serde(default)]
    pub rookie_history: Map<String, Vec<LeagueSeason>>,
    #[serde(default)]
    pub rookie_signals: Map<String, Vec<RoleSignal>>,
    /// GoalieGsax fields + "season_start_year"
    #[serde(default)]
    pub gsax: Vec<Value>,
    #[serde(default)]
    pub xg_norms: Map<String, Map<String, f64>>,
    #[serde(default)]
    pub line_team_meta: Map<String, Map<String, Map<String, Value>>>,
}

impl SnapshotRegistries {
    pub fn to_state(&self) -> anyhow::Result<EnrichState> {
        let mut st = EnrichState::default();
        for l in &self.preseason_lines {
            st.preseason.insert((l.season, l.nhl_id), l.clone());
        }
        for (k, v) in &self.rookie_history {
            st.rookie_history.insert(k.parse()?, v.clone());
        }
        st.rookie_signals = self.rookie_signals.clone();
        for g in &self.gsax {
            let year = g.get("season_start_year").and_then(Value::as_i64).context("gsax season_start_year")?;
            let est: GoalieGsax = serde_json::from_value(g.clone())?;
            st.gsax.insert((year, est.nhl_id), est);
        }
        for (k, v) in &self.xg_norms {
            st.xg_norms.insert(k.parse()?, v.clone());
        }
        for (k, v) in &self.line_team_meta {
            st.line_team_meta.insert(NaiveDate::parse_from_str(k, "%Y-%m-%d")?, v.clone());
        }
        Ok(st)
    }
}

/// `snapshot.json` "params": the merged valuation params the run used.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotParams {
    /// `valuation.params.load_params()` (packaged fitted_params.json + harness override)
    pub merged: Value,
    #[serde(default)]
    pub active: Value,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub hash: Option<String>,
}

/// `rust/parity/fixtures/<league>/snapshot.json`.
#[derive(Clone, Debug, Deserialize)]
pub struct Snapshot {
    pub schema: i64,
    pub league: String,
    /// the frozen clock (UTC) every Python stage saw
    pub now: PyDateTime,
    /// local date at `now` (Python `date.today()`)
    pub today: NaiveDate,
    /// local wall clock at `now` (Python naive `datetime.now()`)
    #[serde(default)]
    pub local_now: Option<PyDateTime>,
    #[serde(default)]
    pub local_utc_offset_secs: Option<i32>,
    pub ctx: LeagueContext,
    pub extras: SnapshotRegistries,
    pub params: SnapshotParams,
    #[serde(default)]
    pub env: Map<String, Option<String>>,
    pub provider: ProviderExtras,
    /// `cli._trade_wants(lc, provider)`: team id -> positions (sorted list), null when none
    #[serde(default)]
    pub trade_wants: Option<Map<String, Vec<String>>>,
    /// `cli._trade_offered(provider)`: team id -> cids (sorted list), null when none
    #[serde(default)]
    pub trade_offered: Option<Map<String, Vec<String>>>,
}

impl Snapshot {
    /// Load a snapshot and attach the registries to `ctx.enrich`.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Snapshot> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut snap: Snapshot =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        snap.ctx.enrich = Arc::new(snap.extras.to_state()?);
        Ok(snap)
    }
}
