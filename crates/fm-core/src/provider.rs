//! `providers/base.py`: the league provider interface, widened to everything compute code calls
//! on a provider object in Python (duck-typed `getattr(provider, ...)` / `provider._get_client()`
//! / espn_api `League` access). Implemented by `fm_providers::{espn, fantrax}` (live) and
//! `fm_providers::snapshot::SnapshotProvider` (parity: extras from the snapshot, remote calls
//! replayed from the HTTP tape). COMPLETE as a contract; implementers may override defaults.
//!
//! Compute code takes `Option<&dyn LeagueProvider>` wherever Python took `provider: Any = None`.

use chrono::NaiveDate;
use serde_json::Value;

use crate::models::{ActivityItem, LeagueContext, LineupDay};
use crate::snapshot::{EspnLeagueMeta, LeagueRules, ProviderExtras};
use crate::{Map, ProviderError};

/// A league's scoring-period calendar as the Fantrax official API reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct OfficialCalendar {
    /// (period number, first day, last day), both days inclusive, in period order.
    pub periods: Vec<(i64, NaiveDate, NaiveDate)>,
    /// First playoff period, when the league uses playoffs.
    pub playoff_start: Option<i64>,
}

/// One scoring period's head-to-head game of the owner's team, from the Fantrax official API.
#[derive(Clone, Debug, PartialEq)]
pub struct OfficialMatchup {
    pub opponent_id: String,
    pub my_points: f64,
    pub their_points: f64,
    /// Whether the period is a playoff one, from the league's first playoff period.
    pub playoffs: bool,
}

pub trait LeagueProvider: Send + Sync {
    /// "espn" | "fantrax"
    fn name(&self) -> &str;

    /// Fetch league settings, rosters and free agents (fills `extras()` as a side effect, like
    /// the Python provider's attributes).
    fn load(&mut self) -> anyhow::Result<LeagueContext>;

    /// Side data gathered by `load()`.
    fn extras(&self) -> &ProviderExtras;

    // ---- convenience accessors over extras (Python getattr idioms) ----
    fn rules(&self) -> Option<&LeagueRules> {
        self.extras().rules.as_ref()
    }
    fn ages(&self) -> &Map<String, f64> {
        &self.extras().ages
    }
    fn espn_league(&self) -> Option<&EspnLeagueMeta> {
        self.extras().espn_league.as_ref()
    }
    fn warnings(&self) -> &[String] {
        &self.extras().warnings
    }

    // ---- remote calls made after load() (each provider implements its own) ----

    /// League activity feed (adds / drops / trades) since `since`.
    fn activity(&self, _since: Option<NaiveDate>) -> anyhow::Result<Vec<ActivityItem>> {
        Ok(vec![])
    }

    /// Lineup slots of every team on `day` (Fantrax) - harness ingest.
    fn lineup_snapshot(&self, _day: Option<NaiveDate>) -> anyhow::Result<Vec<LineupDay>> {
        Ok(vec![])
    }

    /// ESPN box-score lineups for one scoring period - harness ingest.
    fn box_scores(&self, _scoring_period: i64, _day: Option<NaiveDate>) -> anyhow::Result<Vec<LineupDay>> {
        Ok(vec![])
    }

    /// ESPN scoring period id for a date.
    fn scoring_period_for(&self, _day: NaiveDate, _today: Option<NaiveDate>) -> Option<i64> {
        None
    }

    /// ESPN trending players (`fm trending`): rows as JSON objects.
    fn trending(&self, _limit: usize, _fallers: bool) -> anyhow::Result<Vec<Map<String, Value>>> {
        Ok(vec![])
    }

    /// Fantrax `FxpaClient.call(*calls, fresh=)`: one `data` object per (method, params) message.
    /// Python: `provider._get_client().call(("getStandings", {"view": "SCHEDULE"}))`.
    fn fantrax_call(&self, _calls: &[(String, Map<String, Value>)], _fresh: bool) -> anyhow::Result<Vec<Value>> {
        Err(ProviderError::Unavailable("not a Fantrax provider".into()).into())
    }

    /// Scoring periods and playoff start from the Fantrax official API. Unavailable (the default) for
    /// every provider without that API attached, and when its read failed; callers then use the private
    /// `fantrax_call` data.
    fn official_calendar(&self) -> anyhow::Result<OfficialCalendar> {
        Err(ProviderError::Unavailable("no official Fantrax API".into()).into())
    }

    /// The pairing and scores of `my_team_id` in scoring period `period` (a finished or the current one)
    /// from the Fantrax official API: the opponent from `getLeagueInfo` matchups, the points from
    /// `getMatchupScores`. `Ok(None)` (the default) means there is nothing official to use, silently:
    /// no official API attached, an offline cache miss, or no pairing of two real teams (playoff periods
    /// hold seed placeholders). `Err` is a failed read or inconsistent data, holding the one warning to
    /// show; either way the caller falls back to the private `fantrax_call` data.
    fn official_matchup(&self, _my_team_id: &str, _period: i64) -> Result<Option<OfficialMatchup>, String> {
        Ok(None)
    }

    /// ESPN `league.espn_request.league_get(params=...)` (e.g. view=mMatchupScore): raw JSON.
    fn espn_league_get(&self, _params: &[(String, String)]) -> anyhow::Result<Value> {
        Err(ProviderError::Unavailable("not an ESPN provider".into()).into())
    }

    /// ESPN `league.scoreboard(matchup_period)` reduced to the fields compute code reads:
    /// `[{"home_team": id, "away_team": id, "home_score": f, "away_score": f}, ...]`.
    fn espn_scoreboard(&self, _matchup_period: i64) -> anyhow::Result<Vec<Map<String, Value>>> {
        Err(ProviderError::Unavailable("not an ESPN provider".into()).into())
    }

    /// `provider.raw_settings()` (Fantrax rules / info dump for `fm settings`).
    fn raw_settings(&self) -> anyhow::Result<Value> {
        Ok(Value::Null)
    }

    /// `provider.describe_settings()` lines for `fm settings`.
    fn describe_settings(&self) -> Vec<String> {
        vec![]
    }

    /// `provider.fit_points()` (Fantrax `fm settings --fit-points`).
    fn fit_points(&self) -> anyhow::Result<Value> {
        Err(ProviderError::Unavailable("fit_points is Fantrax only".into()).into())
    }

    /// `provider.ping()` (Fantrax session check).
    fn ping(&self) -> anyhow::Result<Value> {
        Err(ProviderError::Unavailable("ping is Fantrax only".into()).into())
    }
}
