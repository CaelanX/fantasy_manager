//! Rust port of `fantasy_manager/harness/realized.py`: nightly pulls of what actually happened
//! (NHL stats, lineups, league transactions) into the ledger.
//! Owner: work package `W3-harness-deploy-data`.
//!
//! Deviations:
//! * `pull_lineups` picks the ESPN path by `provider.name() == "espn"` (Python used
//!   `hasattr(provider, "box_scores")`); any other provider uses `lineup_snapshot`.
//! * `pull_transactions` has no `hasattr(provider, "activity")` guard: the trait always has it.
//! * `yesterday` takes `today` explicitly.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{Duration, NaiveDate};
use fm_backtest::data::fetch_window;
use fm_core::models::{ActivityItem, LeagueContext, LineupDay};
use fm_core::provider::LeagueProvider;
use fm_pycompat::pyjson::{dumps, DumpOpts};
use fm_sources::nhl::{current_season, NhlClient};
use serde_json::Value;

use crate::episode_match::set_my_team_id;
use crate::ledger::{now_iso, Ledger, Row};

fn my_team(ctx: Option<&LeagueContext>) -> Option<String> {
    ctx?.my_team().ok().map(|t| t.team_id.clone())
}

fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn opt_s(x: &Option<String>) -> Value {
    x.as_ref().map(|s| Value::from(s.clone())).unwrap_or(Value::Null)
}

/// Store every player's raw stats for games on `day`; returns players stored (0 = no games).
pub fn pull_realized(ledger: &Ledger, nhl_client: &NhlClient, day: NaiveDate, season: Option<i64>) -> Result<usize> {
    let season = season.filter(|s| *s != 0).unwrap_or_else(|| current_season(day));
    let lines = fetch_window(nhl_client, season, Some(day), Some(day), true)?;
    let now = now_iso();
    let opts = DumpOpts { sort_keys: true, ..Default::default() };
    let rows: Vec<Row> = lines
        .iter()
        .filter(|(_, wl)| wl.gp > 0)
        .map(|(pid, wl)| {
            let stats: Value = serde_json::to_value(&wl.stats).unwrap_or(Value::Null);
            let mut r = Row::new();
            r.insert("nhl_id".into(), (*pid).into());
            r.insert("game_date".into(), iso(day).into());
            r.insert("gp".into(), wl.gp.into());
            r.insert("stats_json".into(), dumps(&stats, &opts).into());
            r.insert("pulled_at".into(), now.clone().into());
            r
        })
        .collect();
    ledger.upsert("realized_daily", &rows, &["nhl_id", "game_date"])?;
    let mut pull = Row::new();
    pull.insert("game_date".into(), iso(day).into());
    pull.insert("n_players".into(), (rows.len() as i64).into());
    pull.insert("pulled_at".into(), now.into());
    ledger.upsert("realized_pulls", &[pull], &["game_date"])?;
    Ok(rows.len())
}

pub fn lineup_rows(league: &str, items: &[LineupDay], my_team: Option<&str>) -> Vec<Row> {
    let now = now_iso();
    items
        .iter()
        .map(|i| {
            let mut r = Row::new();
            r.insert("league".into(), league.into());
            r.insert("team_id".into(), i.team_id.clone().into());
            r.insert("day".into(), iso(i.date).into());
            r.insert("cid".into(), i.cid.clone().into());
            r.insert("slot".into(), i.slot.clone().into());
            r.insert("starting".into(), (i.starting as i64).into());
            r.insert("provider_pts".into(), i.provider_pts.map(Value::from).unwrap_or(Value::Null));
            r.insert("is_me".into(), ((my_team == Some(i.team_id.as_str())) as i64).into());
            r.insert("pulled_at".into(), now.clone().into());
            r
        })
        .collect()
}

/// Lineups of every team for `day`: ESPN box scores of that scoring period, Fantrax a snapshot
/// of the loaded rosters labelled `day`. Returns rows stored.
pub fn pull_lineups(
    ledger: &Ledger,
    provider: &dyn LeagueProvider,
    league: &str,
    day: NaiveDate,
    ctx: Option<&LeagueContext>,
    today: Option<NaiveDate>,
) -> Result<usize> {
    let items: Vec<LineupDay> = if provider.name() == "espn" {
        match provider.scoring_period_for(day, today) {
            Some(sp) => provider.box_scores(sp, Some(day))?,
            None => Vec::new(),
        }
    } else {
        provider.lineup_snapshot(Some(day))?
    };
    let team = my_team(ctx);
    ledger.upsert("lineup_days", &lineup_rows(league, &items, team.as_deref()), &["league", "team_id", "day", "cid"])
}

/// `ts.isoformat(timespec="seconds")`: no fractional seconds, offset kept.
fn iso_seconds(ts: &fm_core::PyDateTime) -> String {
    let s = ts.isoformat();
    match s.find('.') {
        Some(i) => {
            let rest = &s[i + 1..];
            let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            format!("{}{}", &s[..i], &rest[digits..])
        }
        None => s,
    }
}

fn action_str(i: &ActivityItem) -> String {
    serde_json::to_value(i.action).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

pub fn transaction_rows(
    league: &str,
    items: &[ActivityItem],
    my_team: Option<&str>,
    nhl_ids: Option<&HashMap<String, i64>>,
) -> Vec<Row> {
    let now = now_iso();
    let mut out = Vec::new();
    for i in items {
        let Some(cid) = i.cid.as_ref().filter(|c| !c.is_empty()) else { continue };
        let action = action_str(i);
        let mine = match my_team {
            Some(t) => {
                i.team_id.as_deref() == Some(t) || (action == "PROPOSED" && i.counterparty_id.as_deref() == Some(t))
            }
            None => false,
        };
        let nhl_id = i.nhl_id.or_else(|| nhl_ids.and_then(|m| m.get(cid).copied()));
        let mut r = Row::new();
        r.insert("league".into(), league.into());
        r.insert("source".into(), i.source.clone().into());
        r.insert("tx_id".into(), i.tx_id.clone().into());
        r.insert("action".into(), action.into());
        r.insert("cid".into(), cid.clone().into());
        r.insert("team_id".into(), i.team_id.clone().unwrap_or_default().into());
        r.insert("ts".into(), iso_seconds(&i.ts).into());
        r.insert("day".into(), iso(i.ts.date()).into());
        r.insert("team_name".into(), opt_s(&i.team_name));
        r.insert("player_name".into(), opt_s(&i.player_name));
        r.insert("nhl_id".into(), nhl_id.map(Value::from).unwrap_or(Value::Null));
        r.insert("group_id".into(), opt_s(&i.group_id));
        r.insert("counterparty_id".into(), opt_s(&i.counterparty_id));
        r.insert("is_me".into(), (mine as i64).into());
        r.insert("pulled_at".into(), now.clone().into());
        out.push(r);
    }
    out
}

/// Provider activity since `since` into `transactions`; returns rows stored.
pub fn pull_transactions(
    ledger: &Ledger,
    provider: &dyn LeagueProvider,
    league: &str,
    since: Option<NaiveDate>,
    ctx: Option<&LeagueContext>,
) -> Result<usize> {
    let items = provider.activity(since)?;
    let team = my_team(ctx);
    if let Some(t) = &team {
        set_my_team_id(ledger, league, t)?;
    }
    let nhl_ids: HashMap<String, i64> = match ctx {
        Some(c) => c.all_players().iter().filter_map(|p| p.nhl_id().map(|n| (p.cid.clone(), n))).collect(),
        None => HashMap::new(),
    };
    let mut rows = transaction_rows(league, &items, team.as_deref(), Some(&nhl_ids));
    // a pending proposal has no reliable timestamp: keep the time it was first seen
    let mut seen: HashMap<(String, String, String), (Value, Value)> = HashMap::new();
    for r in ledger.query(
        "SELECT tx_id, cid, team_id, ts, day FROM transactions WHERE league=? AND action='PROPOSED'",
        &[&league],
    )? {
        let s = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        seen.insert((s("tx_id"), s("cid"), s("team_id")), (r["ts"].clone(), r["day"].clone()));
    }
    for r in rows.iter_mut() {
        if r["action"].as_str() != Some("PROPOSED") {
            continue;
        }
        let s = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let key = (s("tx_id"), s("cid"), s("team_id"));
        if let Some((ts, day)) = seen.get(&key) {
            r.insert("ts".into(), ts.clone());
            r.insert("day".into(), day.clone());
        }
    }
    ledger.upsert("transactions", &rows, &["league", "tx_id", "action", "cid", "team_id"])
}

pub fn yesterday(today: NaiveDate) -> NaiveDate {
    today - Duration::days(1)
}

/// Furthest back a daily run catches up on missed NHL days.
pub const BACKFILL_MAX_DAYS: i64 = 60;

/// Days from `from` through `through` that `table` (`realized_pulls` or `deployment_pulls`) has
/// not marked pulled yet, oldest first.
pub fn unpulled_days(ledger: &Ledger, table: &str, from: NaiveDate, through: NaiveDate) -> Result<Vec<NaiveDate>> {
    let (f, t) = (iso(from), iso(through));
    let done: std::collections::HashSet<String> = ledger
        .query(&format!("SELECT game_date FROM {table} WHERE game_date >= ? AND game_date <= ?"), &[&f, &t])?
        .iter()
        .filter_map(|r| r.get("game_date").and_then(|v| v.as_str()).map(String::from))
        .collect();
    let mut out = Vec::new();
    let mut d = from;
    while d <= through {
        if !done.contains(&iso(d)) {
            out.push(d);
        }
        d += Duration::days(1);
    }
    Ok(out)
}

/// Where catching up starts: the first archived projection or recommendation day (nothing before
/// it can be graded), at most `BACKFILL_MAX_DAYS` before `through`; None with an empty ledger.
pub fn backfill_start(ledger: &Ledger, through: NaiveDate) -> Result<Option<NaiveDate>> {
    let rows = ledger.query("SELECT MIN(d) AS d FROM (SELECT MIN(as_of) AS d FROM projections UNION ALL SELECT MIN(as_of) FROM recs)", &[])?;
    let first = rows
        .first()
        .and_then(|r| r.get("d"))
        .and_then(|v| v.as_str())
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    Ok(first.map(|f| f.max(through - Duration::days(BACKFILL_MAX_DAYS))))
}
