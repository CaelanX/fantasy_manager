//! `SnapshotProvider`: a [`LeagueProvider`] over a parity snapshot (`fixtures/<league>/
//! snapshot.json`). `load()` returns the snapshot's (already enriched) context, `extras()` the
//! snapshot's provider extras; remote calls made after load (`activity`, `fantrax_call`,
//! `espn_league_get`, `box_scores`, ...) are answered by a real provider replaying the HTTP tape
//! (`tape.json.gz`), built and loaded lazily on the first such call. Without a tape every remote
//! call fails like an offline cache miss. Used by `pipeline::from_snapshot` (the hidden
//! `--from-snapshot` CLI flag) and the parity tests. Owner: work package `W5-provider-pipeline`.
//!
//! No Python counterpart: the Python dump patched the loader instead (see
//! `parity/dump_fixtures.py`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use chrono::NaiveDate;
use fm_core::config::{format_points, parse_points, Settings};
use fm_core::models::{ActivityItem, LeagueContext, LineupDay};
use fm_core::provider::LeagueProvider;
use fm_core::snapshot::{ProviderExtras, Snapshot};
use fm_core::Map;
use fm_http::{HttpCache, Tape};
use serde_json::Value;

use crate::espn::EspnProvider;
use crate::fantrax::FantraxProvider;
use crate::registry::ESPN_FA_SIZE;

/// The Settings a snapshot's league was loaded with (league ids, my team, Fantrax points /
/// keepers from the dump's env), pointed at `data_dir`, offline.
pub fn snapshot_settings(snap: &Snapshot, data_dir: &Path) -> Settings {
    let ctx = &snap.ctx;
    let env = |k: &str| snap.env.get(k).cloned().flatten();
    let mut s = Settings { fm_data_dir: data_dir.to_path_buf(), fm_offline: true, ..Settings::default() };
    match snap.league.as_str() {
        "espn" => {
            s.espn_league_id = ctx.league_id.parse().ok();
            s.espn_year = Some(ctx.season);
            s.espn_team = ctx.teams.iter().find(|t| t.owner_is_me).map(|t| t.team_id.clone());
        }
        _ => {
            s.fantrax_league_id = Some(ctx.league_id.clone());
            s.fantrax_keepers = env("FANTRAX_KEEPERS").and_then(|v| v.trim().parse().ok());
            let goalie = (!ctx.scoring.goalie_weights.is_empty()).then_some(&ctx.scoring.goalie_weights);
            s.fantrax_points = parse_points(&format_points(&ctx.scoring.weights, goalie)).unwrap_or_default();
        }
    }
    if let Some(m) = env("FANTRAX_MODE").and_then(|m| fm_core::prefs::normalize_mode(&m)) {
        s.fantrax_mode = m;
        s.fields_set.insert("fantrax_mode".into());
    }
    s
}

/// A tape-backed provider for `league` (not loaded).
pub fn tape_provider(league: &str, settings: Settings, tape: Tape, today: NaiveDate) -> Result<Box<dyn LeagueProvider>> {
    match league {
        "espn" => {
            let cache = Arc::new(HttpCache::from_tape(tape));
            Ok(Box::new(EspnProvider::new(settings, cache, ESPN_FA_SIZE).with_today(today)))
        }
        "fantrax" => Ok(Box::new(FantraxProvider::from_tape(settings, tape, today))),
        other => Err(fm_core::ProviderError::Message(format!("unknown league provider {}", crate::registry::py_str_repr(other))).into()),
    }
}

pub struct SnapshotProvider {
    league: String,
    ctx: LeagueContext,
    extras: ProviderExtras,
    tape: Option<Tape>,
    settings: Settings,
    today: NaiveDate,
    remote: OnceLock<Option<Box<dyn LeagueProvider>>>,
}

impl SnapshotProvider {
    /// Over a loaded snapshot; `tape` answers the remote calls, `data_dir` is where the inner
    /// provider keeps its state (Fantrax file cache is not used with a tape).
    pub fn new(snap: &Snapshot, tape: Option<Tape>, data_dir: &Path) -> SnapshotProvider {
        SnapshotProvider {
            league: snap.league.clone(),
            ctx: snap.ctx.clone(),
            extras: snap.provider.clone(),
            tape,
            settings: snapshot_settings(snap, data_dir),
            today: snap.today,
            remote: OnceLock::new(),
        }
    }

    /// The tape-backed provider (built and loaded on first use); None without a tape.
    fn remote(&self) -> Option<&dyn LeagueProvider> {
        self.remote
            .get_or_init(|| {
                let tape = self.tape.clone()?;
                let mut p = match tape_provider(&self.league, self.settings.clone(), tape, self.today) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!("snapshot provider: {e}");
                        return None;
                    }
                };
                if let Err(e) = p.load() {
                    tracing::warn!("snapshot provider: replaying {} load failed: {e}", self.league);
                }
                Some(p)
            })
            .as_deref()
    }

    fn need_remote(&self) -> Result<&dyn LeagueProvider> {
        self.remote().ok_or_else(|| fm_http::CacheMiss(format!("offline and not cached: no tape for {}", self.league)).into())
    }
}

impl LeagueProvider for SnapshotProvider {
    fn name(&self) -> &str {
        &self.league
    }

    fn load(&mut self) -> Result<LeagueContext> {
        Ok(self.ctx.clone())
    }

    fn extras(&self) -> &ProviderExtras {
        &self.extras
    }

    fn activity(&self, since: Option<NaiveDate>) -> Result<Vec<ActivityItem>> {
        self.need_remote()?.activity(since)
    }

    fn lineup_snapshot(&self, day: Option<NaiveDate>) -> Result<Vec<LineupDay>> {
        if self.league == "fantrax" {
            // Fantrax builds it from the loaded rosters: the snapshot's context is that state
            return Ok(crate::fantrax::lineup_days_from_teams(&self.ctx.teams, day.unwrap_or(self.ctx.as_of)));
        }
        self.need_remote()?.lineup_snapshot(day)
    }

    fn box_scores(&self, scoring_period: i64, day: Option<NaiveDate>) -> Result<Vec<LineupDay>> {
        self.need_remote()?.box_scores(scoring_period, day)
    }

    fn scoring_period_for(&self, day: NaiveDate, today: Option<NaiveDate>) -> Option<i64> {
        self.remote()?.scoring_period_for(day, today)
    }

    fn trending(&self, limit: usize, fallers: bool) -> Result<Vec<Map<String, Value>>> {
        self.need_remote()?.trending(limit, fallers)
    }

    fn fantrax_call(&self, calls: &[(String, Map<String, Value>)], fresh: bool) -> Result<Vec<Value>> {
        self.need_remote()?.fantrax_call(calls, fresh)
    }

    fn espn_league_get(&self, params: &[(String, String)]) -> Result<Value> {
        self.need_remote()?.espn_league_get(params)
    }

    fn espn_scoreboard(&self, matchup_period: i64) -> Result<Vec<Map<String, Value>>> {
        self.need_remote()?.espn_scoreboard(matchup_period)
    }

    fn raw_settings(&self) -> Result<Value> {
        self.need_remote()?.raw_settings()
    }

    fn describe_settings(&self) -> Vec<String> {
        self.remote().map(|r| r.describe_settings()).unwrap_or_default()
    }

    fn fit_points(&self) -> Result<Value> {
        self.need_remote()?.fit_points()
    }

    fn ping(&self) -> Result<Value> {
        self.need_remote()?.ping()
    }
}

/// `<dir>/snapshot.json` for a fixture directory, or the path itself for a file.
pub fn snapshot_file(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("snapshot.json")
    } else {
        path.to_path_buf()
    }
}

/// The tape next to a snapshot (`tape.json.gz`), if present.
pub fn tape_next_to(snapshot_file: &Path) -> Option<PathBuf> {
    let p = snapshot_file.parent()?.join("tape.json.gz");
    p.is_file().then_some(p)
}
