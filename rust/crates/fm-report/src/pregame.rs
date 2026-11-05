//! Rust port of `fantasy_manager/pregame.py`: the pre-game afternoon run (`fm harness pregame`):
//! what changed since this morning. Owner: work package `W9-report-pregame`.
//!
//! 1. force-refresh the fast-moving sources (Daily Faceoff goalie page and the ESPN injury feed
//!    bypass the HTTP cache with ttl 0, the line pages of tonight's teams only when older than 6 h);
//! 2. load the league (`fm_providers::pipeline::load_league_full`);
//! 3. recompute week lineup recs, today's optimal lineup, line alerts and injury alerts;
//! 4. diff a compact per-league state against the previous snapshot of the day
//!    (`<fm_data_dir>/pregame/<league>-<YYYY-MM-DD>.json`) and report only what changed, typed;
//! 5. write the new snapshot.
//!
//! Deviations from Python:
//! * the clock is explicit (`now: &PyDateTime`, naive local like Python's `datetime.now()`);
//! * state / snapshot files are `serde_json::Value`s (insertion order kept; files are written with
//!   `indent=1, sort_keys=True` through `fm_pycompat::pyjson`);
//! * injectable collaborators (`loader`, `dfo_client`, `injuries_fetch`, `notifier`, `planner`)
//!   are trait objects / closures in [`PregameOpts`];
//! * `PregameResult.warnings` is a snapshot of the warning list at the moment the result is built,
//!   exactly as pydantic copies the list in Python (warnings raised later, e.g. "week plan failed",
//!   stay out of the result);
//! * `failure_message` takes the message text (Python also accepted exception objects).

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use chrono::{Local, NaiveDate, TimeZone, Utc};
use fm_core::config::Settings;
use fm_core::models::{LeagueContext, Player, Recommendation};
use fm_core::normalize::{normalize_name, normalize_team};
use fm_core::values::{Horizon, PlayerValue};
use fm_core::{Cid, Map, PyDateTime};
use fm_engine::recommend::alerts::{plays_today, recommend_line_alerts, LineAlertOpts};
use fm_engine::recommend::injuries::{recommend_injuries, History, StatusHistory};
use fm_engine::recommend::lineup::{confirmed_start_probability, optimal_lineup, recommend_lineup};
use fm_engine::recommend::lineup_lock;
use fm_engine::recommend::plan::{build_week_plan, today_checklist, PlanOpts, WeekPlan, CHECKLIST_LIMIT};
use fm_engine::recommend::plan_memory::since_line;
use fm_engine::valuation::schedule::ScheduleMemo;
use fm_http::{FetchJson, HttpCache};
use fm_providers::lines_enrich::LineSnapshotStore;
use fm_pycompat::num::{py_sum, round_to};
use fm_pycompat::order::cmp_f64;
use fm_pycompat::pyjson::{dumps, DumpOpts};
use fm_pycompat::text::{py_len, py_rstrip, py_slice, py_strip, split_ws};
use fm_sources::dailyfaceoff::{make_cached_fetch_text, make_refresh_fetch_text, DailyFaceoffClient, GoalieStart};
use fm_sources::injuries::{fetch_injuries, INJURIES_URL};
use serde::Serialize;
use serde_json::{json, Value};

pub const SNAPSHOT_DIR: &str = "pregame";
pub const SNAPSHOT_VERSION: i64 = 1;
pub const SUMMARY_LIMIT: usize = 1500;
pub const MAX_ALERTS_STORED: usize = 20;
/// free-agent goalies whose team plays today, best first
pub const MAX_FA_GOALIES: usize = 60;
pub const STREAMER_LIMIT: usize = 3;
pub const FA_ALERT_LIMIT: usize = 3;
pub const LINES_MAX_AGE: f64 = 6.0 * 3600.0;
pub const INJURED: [&str; 5] = ["dtd", "out", "ir", "ltir", "suspended"];
pub const UNAVAILABLE: [&str; 4] = ["out", "ir", "ltir", "suspended"];
pub const DTD_AVAILABILITY: f64 = 0.75;
pub const NO_CHANGES: &str = "Pre-game: no changes. Lineup set.";
pub const CHECKLIST_HEAD: &str = "Before puck drop: quick lineup, and also:";
/// with plan actions today, "lineup set" would contradict them
pub const NO_CHANGES_PLAN: &str = "Pre-game: no changes since this morning.";
pub const PLAN_REST: &str = "Otherwise the lineup is set";

pub fn league_label(league: &str) -> &str {
    match league {
        "espn" => "ESPN",
        "fantrax" => "Fantrax",
        other => other,
    }
}

// --------------------------------------------------------------------------- types

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    GoalieConfirmed,
    MyPlayerStatusChange,
    LineupChange,
    NewAlert,
    InjuryNew,
}

impl ChangeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeKind::GoalieConfirmed => "goalie_confirmed",
            ChangeKind::MyPlayerStatusChange => "my_player_status_change",
            ChangeKind::LineupChange => "lineup_change",
            ChangeKind::NewAlert => "new_alert",
            ChangeKind::InjuryNew => "injury_new",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Change {
    pub kind: ChangeKind,
    /// one phone line, no trailing period
    pub text: String,
    /// goalie_confirmed: mine / opponent / streamer; new_alert: mine / fa
    pub scope: Option<String>,
    pub player: Option<String>,
    pub team: Option<String>,
    /// 1 = most urgent (summary order)
    pub priority: i64,
    /// compact form for a merged phone line (opponent goalies)
    pub short: Option<String>,
}

impl Change {
    pub fn new(kind: ChangeKind, text: impl Into<String>) -> Change {
        Change { kind, text: text.into(), scope: None, player: None, team: None, priority: 5, short: None }
    }
}

/// `PregameResult.lineup_recs`: `{"week": [Recommendation], "today": {...}}`.
#[derive(Clone, Debug, Default)]
pub struct LineupRecs {
    pub week: Vec<Recommendation>,
    /// today's lineup dict; `Value::Null` stands for Python's `{}`
    pub today: Value,
}

impl LineupRecs {
    /// `lineup_recs.get("today") or None`.
    pub fn today_opt(&self) -> Option<&Value> {
        match &self.today {
            Value::Object(m) if !m.is_empty() => Some(&self.today),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PregameResult {
    pub league: String,
    pub day: NaiveDate,
    pub ran_at: PyDateTime,
    pub changes: Vec<Change>,
    pub lineup_recs: LineupRecs,
    /// Recommendation (kind "alert")
    pub alerts: Vec<Recommendation>,
    /// Recommendation (kind "injury")
    pub injuries: Vec<Recommendation>,
    /// team -> named goalie
    pub goalies_tonight: Value,
    pub summary_text: String,
    /// the week plan's step for today (<= 3)
    pub today_actions: Vec<String>,
    pub nothing_changed: bool,
    /// "daily" / "pregame" (source of the snapshot diffed against) / None
    pub baseline: Option<String>,
    pub baseline_at: Option<String>,
    pub snapshot_path: Option<String>,
    pub refresh: Map<String, Value>,
    /// webhook results when notify=True (None: not sent)
    pub notify: Option<Vec<String>>,
    pub warnings: Vec<String>,
}

impl PregameResult {
    pub fn new(league: &str, day: NaiveDate, ran_at: PyDateTime) -> Self {
        PregameResult {
            league: league.to_string(),
            day,
            ran_at,
            changes: vec![],
            lineup_recs: LineupRecs::default(),
            alerts: vec![],
            injuries: vec![],
            goalies_tonight: json!({}),
            summary_text: String::new(),
            today_actions: vec![],
            nothing_changed: true,
            baseline: None,
            baseline_at: None,
            snapshot_path: None,
            refresh: Map::new(),
            notify: None,
            warnings: vec![],
        }
    }

    /// Bullets for `notify::notify_changes`: today's plan actions first ("Before puck drop
    /// today:"), then what changed since the morning.
    pub fn notify_lines(&self) -> Vec<String> {
        let mut lead: Vec<String> = Vec::new();
        if !self.today_actions.is_empty() {
            lead.push(CHECKLIST_HEAD.to_string());
            lead.extend(self.today_actions.iter().cloned());
        }
        if self.nothing_changed {
            lead.push(self.summary_text.clone());
            return lead;
        }
        let today = self.lineup_recs.today_opt();
        lead.extend(display_changes(&self.changes, today).into_iter().map(|c| c.text));
        let tail = today.and_then(|t| t.get("text")).and_then(Value::as_str).unwrap_or("");
        lead.push(tail.to_string());
        lead
    }

    pub fn title(&self) -> String {
        format!("Pre-game {} - {}", clock(&self.ran_at), league_label(&self.league))
    }

    /// JSON-friendly summary (recommendations as titles).
    pub fn to_dict(&self) -> Value {
        json!({
            "league": self.league,
            "day": self.day.format("%Y-%m-%d").to_string(),
            "ran_at": iso_seconds(&self.ran_at),
            "summary_text": self.summary_text,
            "today_actions": self.today_actions,
            "nothing_changed": self.nothing_changed,
            "changes": self.changes.iter().map(|c| serde_json::to_value(c).unwrap_or(Value::Null)).collect::<Vec<_>>(),
            "lineup": {
                "week": self.lineup_recs.week.iter().map(|r| r.title.clone()).collect::<Vec<_>>(),
                "today": self.lineup_recs.today_opt().cloned().unwrap_or_else(|| json!({})),
            },
            "alerts": self.alerts.iter().map(|r| r.title.clone()).collect::<Vec<_>>(),
            "injuries": self.injuries.iter().map(|r| r.title.clone()).collect::<Vec<_>>(),
            "goalies_tonight": self.goalies_tonight,
            "baseline": self.baseline,
            "baseline_at": self.baseline_at,
            "snapshot": self.snapshot_path,
            "refresh": Value::Object(self.refresh.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            "notify": self.notify,
            "warnings": self.warnings,
        })
    }
}

// --------------------------------------------------------------------------- small helpers

/// `dt.isoformat(timespec="seconds")`.
pub fn iso_seconds(dt: &PyDateTime) -> String {
    let full = PyDateTime { local: dt.local.with_nanosecond_zero(), offset_secs: dt.offset_secs };
    full.isoformat()
}

trait NanoZero {
    fn with_nanosecond_zero(&self) -> Self;
}

impl NanoZero for chrono::NaiveDateTime {
    fn with_nanosecond_zero(&self) -> Self {
        use chrono::Timelike;
        self.with_nanosecond(0).unwrap_or(*self)
    }
}

/// '5:02 PM' (no leading zero, portable across platforms).
pub fn clock(dt: &PyDateTime) -> String {
    use chrono::Timelike;
    let (hour, minute) = (dt.local.hour(), dt.local.minute());
    let h = if hour % 12 == 0 { 12 } else { hour % 12 };
    format!("{h}:{minute:02} {}", if hour < 12 { "AM" } else { "PM" })
}

/// `_short(e, n)`: first line of the message, at most `n` chars; "Error" when blank.
pub fn short_err(msg: &str, n: usize) -> String {
    let t = py_strip(msg);
    if t.is_empty() {
        return "Error".to_string();
    }
    py_slice(t.lines().next().unwrap_or(""), None, Some(n as isize))
}

fn short(msg: &str) -> String {
    short_err(msg, 160)
}

/// Last name for phone lines ('Pyotr Kochetkov' -> 'Kochetkov'; keeps 'van Riemsdyk').
pub fn last(name: Option<&str>) -> String {
    let name = name.unwrap_or("");
    let parts = split_ws(name);
    if parts.len() <= 1 {
        return if name.is_empty() { "?".to_string() } else { name.to_string() };
    }
    let mut tail: Vec<&str> = vec![parts[parts.len() - 1]];
    for w in parts[1..parts.len() - 1].iter().rev() {
        if w.chars().next().is_some_and(|c| c.is_lowercase()) {
            tail.insert(0, w);
        } else {
            break;
        }
    }
    tail.join(" ")
}

pub fn sentence(s: &str) -> String {
    let s = py_strip(s);
    match s.chars().last() {
        None => s.to_string(),
        Some(c) if ".!?".contains(c) => s.to_string(),
        Some(_) => format!("{s}."),
    }
}

pub fn snapshot_path(data_dir: impl AsRef<Path>, league: &str, day: NaiveDate) -> PathBuf {
    data_dir.as_ref().join(SNAPSHOT_DIR).join(format!("{league}-{}.json", day.format("%Y-%m-%d")))
}

pub fn load_snapshot(data_dir: impl AsRef<Path>, league: &str, day: NaiveDate) -> Option<Value> {
    let text = std::fs::read_to_string(snapshot_path(data_dir, league, day)).ok()?;
    let snap: Value = serde_json::from_str(&text).ok()?;
    (snap.is_object() && snap.get("version").and_then(Value::as_i64) == Some(SNAPSHOT_VERSION)).then_some(snap)
}

pub fn save_snapshot(data_dir: impl AsRef<Path>, league: &str, day: NaiveDate, state: &Value) -> Result<PathBuf> {
    let p = snapshot_path(data_dir, league, day);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = p.with_extension("json.tmp");
    let opts = DumpOpts { indent: Some(1), ensure_ascii: true, sort_keys: true, separators: None };
    std::fs::write(&tmp, dumps(state, &opts))?;
    std::fs::rename(&tmp, &p)?;
    Ok(p)
}

// JSON access helpers (Python `dict.get` / truthiness on state dicts)

fn jstr<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn jtruthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `x.get(k) or None` for strings (empty string -> None).
fn jopt<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    jstr(v, k).filter(|s| !s.is_empty())
}

fn jentries<'a>(v: &'a Value, k: &str) -> Vec<(&'a String, &'a Value)> {
    v.get(k).and_then(Value::as_object).map(|o| o.iter().collect()).unwrap_or_default()
}

fn jlist<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// Run a step; a failure (error or panic) becomes a warning and the default (`safe` in `run_pregame`).
fn safe<T>(warnings: &mut Vec<String>, label: &str, f: impl FnOnce() -> Result<T>, default: T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            warnings.push(format!("{label} failed ({})", short(&e.to_string())));
            default
        }
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".to_string());
            warnings.push(format!("{label} failed ({})", short(&msg)));
            default
        }
    }
}

/// Naive local -> aware UTC (`now.astimezone(timezone.utc)`); aware values pass through.
pub fn aware_now(now: &PyDateTime) -> PyDateTime {
    if now.is_aware() {
        return *now;
    }
    match Local.from_local_datetime(&now.local).earliest() {
        Some(l) => PyDateTime::from_utc(l.with_timezone(&Utc)),
        None => PyDateTime::from_utc(Utc.from_utc_datetime(&now.local)),
    }
}

// --------------------------------------------------------------------------- refresh

/// The slice of `DailyFaceoffClient` the refresh uses (Python duck-typed a fake in tests).
pub trait DfoSource {
    fn starting_goalies(&mut self, day: NaiveDate) -> Result<Vec<GoalieStart>>;
    /// `len(client.all_lines(teams))`
    fn all_lines_count(&mut self, teams: &[String]) -> Result<usize>;
    fn warnings(&self) -> Vec<String>;
}

impl DfoSource for DailyFaceoffClient<'_> {
    fn starting_goalies(&mut self, day: NaiveDate) -> Result<Vec<GoalieStart>> {
        DailyFaceoffClient::starting_goalies(self, day)
    }
    fn all_lines_count(&mut self, teams: &[String]) -> Result<usize> {
        Ok(self.all_lines(Some(teams)).len())
    }
    fn warnings(&self) -> Vec<String> {
        self.warnings.clone()
    }
}

/// What `refresh_sources` learned: `info` is Python's dict without its "warnings" key.
#[derive(Clone, Debug, Default)]
pub struct RefreshInfo {
    pub warnings: Vec<String>,
    pub info: Map<String, Value>,
}

/// Force-refresh the fast-moving sources; never fails. Returns (tonight's goalie starts, info).
///
/// The goalie page and the injury feed bypass the cache (the fresh copies are stored, so the
/// league load that follows reads them through its normal TTLs); the line pages of the teams
/// playing today are refetched only when older than `lines_max_age`.
pub fn refresh_sources(
    cache: Option<&HttpCache>,
    day: NaiveDate,
    dfo_client: Option<&mut dyn DfoSource>,
    injuries_fetch: Option<&FetchJson>,
    lines_max_age: f64,
) -> (Vec<GoalieStart>, RefreshInfo) {
    let mut info = RefreshInfo::default();
    let mut own: Option<DailyFaceoffClient> = None;
    let mut starts: Vec<GoalieStart> = Vec::new();
    let client: Option<&mut dyn DfoSource> = match dfo_client {
        Some(c) => Some(c),
        None => {
            if let Some(cache) = cache {
                own = Some(DailyFaceoffClient::new(Box::new(make_refresh_fetch_text(cache, lines_max_age))));
            }
            own.as_mut().map(|c| c as &mut dyn DfoSource)
        }
    };
    if let Some(client) = client {
        match client.starting_goalies(day) {
            Ok(s) => {
                starts = s;
                let confirmed = starts.iter().filter(|s| s.is_confirmed()).count();
                info.info.insert("goalies".into(), json!(format!("{confirmed} confirmed / {} listed", starts.len())));
            }
            Err(e) => info.warnings.push(format!("Daily Faceoff starting goalies refresh failed ({})", short(&e.to_string()))),
        }
        let mut teams: Vec<String> = starts.iter().map(|s| s.team.clone()).collect::<HashSet<_>>().into_iter().collect();
        teams.sort();
        if !teams.is_empty() {
            match client.all_lines_count(&teams) {
                Ok(n) => {
                    info.info.insert(
                        "lines".into(),
                        json!(format!(
                            "{n}/{} teams playing today (refetched if older than {} h)",
                            teams.len(),
                            fm_pycompat::fmt::fixed(lines_max_age / 3600.0, 0, false)
                        )),
                    );
                    info.warnings.extend(client.warnings());
                }
                Err(e) => info.warnings.push(format!("Daily Faceoff lines refresh failed ({})", short(&e.to_string()))),
            }
        }
    }
    let reports = match injuries_fetch {
        Some(f) => Some(fetch_injuries(Some(f), 0.0)),
        None => cache.map(|c| {
            let f = move |url: &str, params: &fm_http::Params, _ttl: f64| c.get_json(url, params, &[], &[], 0.0);
            fetch_injuries(Some(&f), 0.0)
        }),
    };
    match reports {
        Some(Ok(r)) => {
            info.info.insert("injuries".into(), json!(format!("{} reports", r.len())));
        }
        Some(Err(e)) => info.warnings.push(format!(
            "injury feed refresh failed ({}; {})",
            short(&e.to_string()),
            INJURIES_URL.split('/').nth(2).unwrap_or("")
        )),
        None => {}
    }
    (starts, info)
}

/// Tonight's goalie starts after the league load: the (fresh) cached page, else today's lines
/// snapshot written by the load. Never fails.
fn cached_starts(cache: Option<&HttpCache>, day: NaiveDate, data_dir: &Path, dfo_client: Option<&mut dyn DfoSource>) -> Vec<GoalieStart> {
    if let Some(c) = dfo_client {
        match c.starting_goalies(day) {
            Ok(s) => return s,
            Err(e) => tracing::info!("pregame: goalie page unavailable ({e}); using today's lines snapshot"),
        }
    } else if let Some(cache) = cache {
        let client = DailyFaceoffClient::new(Box::new(make_cached_fetch_text(cache, None, None)));
        match client.starting_goalies(day) {
            Ok(s) => return s,
            Err(e) => tracing::info!("pregame: goalie page unavailable ({e}); using today's lines snapshot"),
        }
    }
    let snap = LineSnapshotStore::new(data_dir).load(day).unwrap_or(Value::Null);
    let parsed: Result<Vec<GoalieStart>, _> =
        jlist(&snap, "starts").iter().map(|s| serde_json::from_value::<GoalieStart>(s.clone())).collect();
    parsed.unwrap_or_default()
}

// --------------------------------------------------------------------------- state

/// {team: {"goalie", "strength", "game", "opponent", "home", "depth"}} for tonight: the goalie
/// Daily Faceoff lists for each team (a confirmed report wins over an earlier one). `depth` is
/// his G1 / G2 spot on the team's line page when known.
pub fn goalies_state(starts: &[GoalieStart], lines_snapshot: Option<&Value>) -> Value {
    let mut depth: HashMap<(String, String), i64> = HashMap::new();
    if let Some(players) = lines_snapshot.and_then(|s| s.get("players")).and_then(Value::as_object) {
        for rec in players.values() {
            if jtruthy(rec.get("goalie_depth")) {
                let d = rec.get("goalie_depth").and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)));
                if let Some(d) = d {
                    depth.insert((jstr(rec, "team").unwrap_or("").to_string(), normalize_name(jstr(rec, "name"))), d);
                }
            }
        }
    }
    let mut out: serde_json::Map<String, Value> = serde_json::Map::new();
    for s in starts {
        if let Some(cur) = out.get(&s.team) {
            if jstr(cur, "strength").unwrap_or("").to_lowercase() == "confirmed" && !s.is_confirmed() {
                continue;
            }
        }
        let d = depth.get(&(s.team.clone(), normalize_name(Some(&s.goalie_name)))).copied();
        out.insert(
            s.team.clone(),
            json!({"goalie": s.goalie_name, "strength": s.strength, "game": s.game, "opponent": s.opponent,
                   "home": s.home, "depth": d}),
        );
    }
    Value::Object(out)
}

fn plays_today_safe(ctx: &LeagueContext, p: &Player) -> bool {
    catch_unwind(AssertUnwindSafe(|| plays_today(ctx, p))).unwrap_or(true)
}

fn start_prob(p: &Player, pv: &PlayerValue) -> f64 {
    if let Some(prob) = confirmed_start_probability(p) {
        return prob;
    }
    pv.start_share.unwrap_or(0.5)
}

/// PlayerValues whose season value is TODAY's expected points: per-game value x (plays today)
/// x availability x (goalies) start probability. Feed to `optimal_lineup(.., Season)`.
pub fn today_values(ctx: &LeagueContext, values: &Map<Cid, PlayerValue>) -> Map<Cid, PlayerValue> {
    let mut out: Map<Cid, PlayerValue> = Map::new();
    let Ok(team) = ctx.my_team() else { return out };
    for p in team.players_iter() {
        let Some(pv) = values.get(&p.cid) else { continue };
        let mut v = if plays_today_safe(ctx, p) { pv.fpg } else { 0.0 };
        let st = p.status.as_str();
        if UNAVAILABLE.contains(&st) {
            v = 0.0;
        } else if st == "dtd" {
            v *= DTD_AVAILABILITY;
        }
        if p.is_goalie() {
            v *= start_prob(p, pv);
        }
        let mut c = pv.clone();
        c.fpg_season = v.max(0.0);
        out.insert(p.cid.clone(), c);
    }
    out
}

/// Today's optimal lineup (daily-lineup leagues; None for a weekly lock): the players with a
/// game today it starts / benches, and the moves from the current lineup.
pub fn today_lineup(ctx: &LeagueContext, values: &Map<Cid, PlayerValue>) -> Result<Option<Value>> {
    if lineup_lock::lock(ctx) == "weekly" {
        return Ok(None);
    }
    let team = ctx.my_team()?;
    let tv = today_values(ctx, values);
    let (assign, total) = optimal_lineup(team, &tv, &ctx.roster_shape, Horizon::Season);
    let chosen: HashSet<&str> = assign.values().map(String::as_str).collect();
    let mut by: Map<&str, &Player> = Map::new();
    for p in team.players_iter() {
        by.insert(p.cid.as_str(), p);
    }
    let playing: HashSet<&str> = team.players_iter().filter(|p| plays_today_safe(ctx, p)).map(|p| p.cid.as_str()).collect();
    let current: HashSet<&str> = team.slots.iter().filter(|s| s.starting).filter_map(|s| s.player.as_ref()).map(|p| p.cid.as_str()).collect();
    let sorted = |it: Vec<&str>| -> Vec<String> {
        let mut v: Vec<String> = it.into_iter().map(str::to_string).collect();
        v.sort();
        v
    };
    let start: Vec<String> = sorted(chosen.iter().copied().filter(|c| playing.contains(c) && tv.get(*c).is_some_and(|v| v.fpg_season > 0.0)).collect());
    let start_set: HashSet<&str> = start.iter().map(String::as_str).collect();
    let bench: Vec<String> = sorted(playing.iter().copied().filter(|c| !start_set.contains(c)).collect());
    let to_start: Vec<String> = start.iter().filter(|c| !current.contains(c.as_str())).cloned().collect();
    let to_bench: Vec<String> = sorted(current.iter().copied().filter(|c| playing.contains(c) && !chosen.contains(c)).collect());
    let idle_out: Vec<String> = if to_start.is_empty() {
        vec![]
    } else {
        sorted(current.iter().copied().filter(|c| !playing.contains(c) && !chosen.contains(c)).collect())
    };
    let cur_total = py_sum(current.iter().filter_map(|c| tv.get(*c)).map(|v| v.fpg_season).collect::<Vec<_>>());
    let names = |cs: &[String]| -> Vec<String> { cs.iter().filter_map(|c| by.get(c.as_str()).map(|p| p.name.clone())).collect() };
    let lasts = |cs: &[String]| -> String { names(cs).iter().map(|n| last(Some(n))).collect::<Vec<_>>().join(", ") };
    let text = if !to_start.is_empty() || !to_bench.is_empty() {
        let mut parts: Vec<String> = Vec::new();
        if !to_start.is_empty() {
            parts.push(format!("start {}", lasts(&to_start)));
        }
        if !to_bench.is_empty() {
            parts.push(format!("bench {}", lasts(&to_bench)));
        }
        let mut t = format!("Lineup: {}", parts.join("; "));
        if !idle_out.is_empty() {
            t.push_str(&format!("{} {} (no game)", if !to_bench.is_empty() { "; also out" } else { " for" }, lasts(&idle_out)));
        }
        t
    } else {
        "No lineup changes needed".to_string()
    };
    let set = to_start.is_empty() && to_bench.is_empty();
    Ok(Some(json!({
        "start": start, "bench": bench, "to_start": names(&to_start), "to_bench": names(&to_bench),
        "idle_out": names(&idle_out),
        "optimal_total": round_to(total, 2), "current_total": round_to(cur_total, 2), "text": text,
        "set": set,
    })))
}

fn alert_entry(r: &Recommendation, mine_ids: &HashSet<&str>) -> Value {
    let p = r.subjects.first();
    let mut codes: Vec<&str> = r.reasons.iter().map(|x| x.code.as_str()).collect::<HashSet<_>>().into_iter().collect();
    codes.sort();
    let goalie_start = codes.contains(&"CONFIRMED_START") && !["LINE_CHANGE", "PP_UNIT"].iter().any(|c| codes.contains(c));
    json!({"title": r.title, "cid": p.map(|p| p.cid.clone()), "name": p.map(|p| p.name.clone()),
           "mine": p.is_some_and(|p| mine_ids.contains(p.cid.as_str())), "strength": r.strength, "codes": codes,
           "goalie_start": goalie_start})
}

/// Compact JSON-able state of one league for the day (goalies tonight, my players' statuses /
/// lines, today's optimal lineup, top alerts, free-agent goalies for streamer relevance).
pub fn build_state(
    league: &str,
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    starts: &[GoalieStart],
    alerts: Option<&[Recommendation]>,
    source: &str,
    now: &PyDateTime,
    lines_snapshot: Option<&Value>,
    lineup: Option<Value>,
) -> Result<Value> {
    let team = ctx.my_team()?;
    let mut mine: Map<&str, &Player> = Map::new();
    for p in team.players_iter() {
        mine.insert(p.cid.as_str(), p);
    }
    let mut slot: HashMap<&str, &str> = HashMap::new();
    for s in &team.slots {
        if let Some(p) = &s.player {
            slot.insert(p.cid.as_str(), s.slot.as_str());
        }
    }
    let mut my_players = serde_json::Map::new();
    for (cid, p) in &mine {
        my_players.insert(
            cid.to_string(),
            json!({"name": p.name, "team": normalize_team(p.team.as_deref()), "goalie": p.is_goalie(),
                   "status": p.status.as_str(), "note": p.status_note, "line": p.line, "pp_unit": p.pp_unit,
                   "slot": slot.get(cid), "plays_today": plays_today_safe(ctx, p), "start": p.confirmed_start}),
        );
    }
    let computed;
    let alerts: &[Recommendation] = match alerts {
        Some(a) => a,
        None => {
            computed = catch_unwind(AssertUnwindSafe(|| recommend_line_alerts(ctx, Some(values), &LineAlertOpts::default()))).unwrap_or_default();
            &computed
        }
    };
    let lineup = match lineup {
        Some(l) => Some(l),
        None => today_lineup(ctx, values).ok().flatten(),
    };
    let mut fa_g: Vec<&Player> =
        ctx.free_agents.iter().filter(|p| p.is_goalie() && !mine.contains_key(p.cid.as_str()) && plays_today_safe(ctx, p)).collect();
    fa_g.sort_by(|a, b| {
        let fa = values.get(&a.cid).map_or(0.0, |v| v.fpg);
        let fb = values.get(&b.cid).map_or(0.0, |v| v.fpg);
        cmp_f64(-fa, -fb)
    });
    let mine_ids: HashSet<&str> = mine.keys().copied().collect();
    let lineup_today = match &lineup {
        Some(l) => {
            let mut m = serde_json::Map::new();
            for k in ["start", "bench", "set", "text"] {
                m.insert(k.to_string(), l.get(k).cloned().unwrap_or(Value::Null));
            }
            Value::Object(m)
        }
        None => Value::Null,
    };
    Ok(json!({
        "version": SNAPSHOT_VERSION, "league": league, "date": ctx.as_of.format("%Y-%m-%d").to_string(),
        "source": source, "taken_at": iso_seconds(now),
        "goalies": goalies_state(starts, lines_snapshot),
        "my_players": Value::Object(my_players),
        "lineup_today": lineup_today,
        "alerts": alerts.iter().take(MAX_ALERTS_STORED).map(|r| alert_entry(r, &mine_ids)).collect::<Vec<_>>(),
        "fa_goalies": fa_g.iter().take(MAX_FA_GOALIES)
            .map(|p| json!({"name": p.name, "team": normalize_team(p.team.as_deref())})).collect::<Vec<_>>(),
    }))
}

// --------------------------------------------------------------------------- diff

fn is_conf(g: Option<&Value>) -> bool {
    let Some(g) = g else { return false };
    jtruthy(Some(g)) && py_strip(jstr(g, "strength").unwrap_or("")).to_lowercase() == "confirmed"
}

fn is_named(g: Option<&Value>) -> bool {
    let Some(g) = g else { return false };
    jtruthy(Some(g)) && ["confirmed", "likely", "expected"].contains(&py_strip(jstr(g, "strength").unwrap_or("")).to_lowercase().as_str())
}

fn goalie_word(g: &Value) -> String {
    let s = py_strip(jstr(g, "strength").unwrap_or("")).to_lowercase();
    if s == "confirmed" {
        "CONFIRMED".to_string()
    } else if s.is_empty() {
        "listed".to_string()
    } else {
        s
    }
}

pub fn diff_goalies(prev: Option<&Value>, cur: &Value) -> Vec<Change> {
    let null = Value::Null;
    let pg = prev.unwrap_or(&null).get("goalies");
    let mut my_goalie_teams: Map<String, Vec<String>> = Map::new();
    let mut skater_teams: HashSet<String> = HashSet::new();
    for (_, rec) in jentries(cur, "my_players") {
        let Some(team) = jopt(rec, "team") else { continue };
        if jtruthy(rec.get("goalie")) {
            my_goalie_teams.entry(team.to_string()).or_default().push(jstr(rec, "name").unwrap_or("").to_string());
        } else if jtruthy(rec.get("plays_today")) && !UNAVAILABLE.contains(&jstr(rec, "status").unwrap_or("")) {
            skater_teams.insert(team.to_string());
        }
    }
    let fa_list = jlist(cur, "fa_goalies");
    let mut fa: HashSet<String> = HashSet::new();
    let mut fa_rank: HashMap<String, usize> = HashMap::new();
    for (i, g) in fa_list.iter().enumerate() {
        let key = normalize_name(jstr(g, "name"));
        fa.insert(key.clone());
        fa_rank.insert(key, i);
    }
    let mut out: Vec<Change> = Vec::new();
    let mut streamers: Vec<(usize, Change)> = Vec::new();
    let mut goalies = jentries(cur, "goalies");
    goalies.sort_by(|a, b| a.0.cmp(b.0));
    for (team, g) in goalies {
        let team = team.as_str();
        let p = pg.and_then(|m| m.get(team));
        let switched = p.is_some_and(|p| normalize_name(jstr(p, "goalie")) != normalize_name(jstr(g, "goalie")));
        let newly_confirmed = is_conf(Some(g)) && !(p.is_some() && is_conf(p) && !switched);
        if !(newly_confirmed || (switched && is_named(Some(g)))) {
            continue;
        }
        let name = jopt(g, "goalie").unwrap_or("?");
        let opp = jopt(g, "opponent").unwrap_or("?");
        let word = goalie_word(g);
        let was = if switched { format!("; was {}", last(p.and_then(|p| jstr(p, "goalie")))) } else { String::new() };
        if let Some(mine_names) = my_goalie_teams.get(team) {
            if mine_names.iter().any(|n| normalize_name(Some(n)) == normalize_name(Some(name))) {
                let mut c = Change::new(ChangeKind::GoalieConfirmed, format!("{} {word} ({team} vs {opp}{was})", last(Some(name))));
                c.scope = Some("mine".into());
                c.player = Some(name.to_string());
                c.team = Some(team.to_string());
                c.priority = 1;
                out.push(c);
            } else {
                let sits = mine_names.iter().map(|n| last(Some(n))).collect::<Vec<_>>().join(", ");
                let mut c = Change::new(ChangeKind::GoalieConfirmed, format!("{sits} sits: {} {word} for {team} vs {opp}", last(Some(name))));
                c.scope = Some("mine".into());
                c.player = Some(mine_names[0].clone());
                c.team = Some(team.to_string());
                c.priority = 1;
                out.push(c);
            }
            continue;
        }
        if skater_teams.contains(opp) {
            let backup = g.get("depth").and_then(Value::as_i64) == Some(2);
            let short = if is_conf(Some(g)) && !switched {
                Some(format!("{} ({team}{})", last(Some(name)), if backup { ", backup" } else { "" }))
            } else {
                None
            };
            let mut c = Change::new(
                ChangeKind::GoalieConfirmed,
                format!("Your {opp} skaters face {}{}, {word} for {team}{was}", last(Some(name)), if backup { " (backup)" } else { "" }),
            );
            c.scope = Some("opponent".into());
            c.player = Some(name.to_string());
            c.team = Some(team.to_string());
            c.priority = 4;
            c.short = short;
            out.push(c);
            continue;
        }
        let key = normalize_name(Some(name));
        if fa.contains(&key) && is_conf(Some(g)) {
            let mut c = Change::new(
                ChangeKind::GoalieConfirmed,
                format!("Streamer: {} (free agent) CONFIRMED ({team} vs {opp})", last(Some(name))),
            );
            c.scope = Some("streamer".into());
            c.player = Some(name.to_string());
            c.team = Some(team.to_string());
            c.priority = 6;
            streamers.push((fa_rank.get(&key).copied().unwrap_or(99), c));
        }
    }
    streamers.sort_by_key(|(r, _)| *r);
    out.extend(streamers.into_iter().take(STREAMER_LIMIT).map(|(_, c)| c));
    out
}

/// Status changes (`injury_new` / `my_player_status_change`) and line / PP moves (`new_alert`,
/// scope mine) of my players.
pub fn diff_players(prev: Option<&Value>, cur: &Value, baseline_status: Option<&Map<String, String>>) -> Vec<Change> {
    let null = Value::Null;
    let pp = prev.unwrap_or(&null).get("my_players");
    let mut out: Vec<Change> = Vec::new();
    for (cid, c) in jentries(cur, "my_players") {
        let p = pp.and_then(|m| m.get(cid.as_str()));
        let old: Option<String> = match p {
            Some(p) => jstr(p, "status").map(str::to_string),
            None => baseline_status.and_then(|b| b.get(cid.as_str())).cloned(),
        };
        let new = jstr(c, "status");
        let name = jstr(c, "name");
        let team = jstr(c, "team");
        if let (Some(old), Some(new_s)) = (old.as_deref(), new) {
            let both_quiet = ["healthy", "unknown"].contains(&old) && ["healthy", "unknown"].contains(&new_s);
            if new_s != old && !both_quiet {
                let note = match jopt(c, "note") {
                    Some(n) => format!(" ({n})"),
                    None => String::new(),
                };
                if INJURED.contains(&new_s) && !INJURED.contains(&old) {
                    let slot_ok = match c.get("slot") {
                        Some(Value::String(s)) => s != "BN" && s != "IR",
                        _ => false,
                    };
                    let tail = if jtruthy(c.get("plays_today")) && slot_ok && UNAVAILABLE.contains(&new_s) { ": bench him" } else { "" };
                    let mut ch = Change::new(ChangeKind::InjuryNew, format!("{} now {}{note}{tail}", last(name), new_s.to_uppercase()));
                    ch.player = name.map(str::to_string);
                    ch.team = team.map(str::to_string);
                    ch.priority = 2;
                    out.push(ch);
                } else if !INJURED.contains(&new_s) {
                    let mut ch = Change::new(ChangeKind::MyPlayerStatusChange, format!("{} cleared: {} -> {new_s}", last(name), old.to_uppercase()));
                    ch.player = name.map(str::to_string);
                    ch.team = team.map(str::to_string);
                    ch.priority = 2;
                    out.push(ch);
                } else {
                    let mut ch = Change::new(
                        ChangeKind::MyPlayerStatusChange,
                        format!("{} now {} (was {}){note}", last(name), new_s.to_uppercase(), old.to_uppercase()),
                    );
                    ch.player = name.map(str::to_string);
                    ch.team = team.map(str::to_string);
                    ch.priority = 2;
                    out.push(ch);
                }
            }
        }
        let Some(p) = p else { continue };
        if jtruthy(c.get("goalie")) {
            continue;
        }
        let (a, b) = (jopt(p, "line"), jopt(c, "line"));
        let mut moved: Vec<String> = Vec::new();
        if let (Some(a), Some(b)) = (a, b) {
            if a != b {
                moved.push(format!("moved to {} (was {})", b.to_uppercase(), a.to_uppercase()));
            }
            if jopt(p, "pp_unit") != jopt(c, "pp_unit") {
                let before = jopt(p, "pp_unit").unwrap_or("no PP").to_uppercase();
                let after = jopt(c, "pp_unit").unwrap_or("no PP").to_uppercase();
                moved.push(format!("{before} -> {after}").replace("NO PP", "no PP"));
            }
        }
        if !moved.is_empty() {
            let mut ch = Change::new(ChangeKind::NewAlert, format!("{} {}", last(name), moved.join(", ")));
            ch.scope = Some("mine".into());
            ch.player = name.map(str::to_string);
            ch.team = team.map(str::to_string);
            ch.priority = 3;
            out.push(ch);
        }
    }
    out
}

pub fn diff_lineup(prev: Option<&Value>, cur: &Value) -> Vec<Change> {
    let null = Value::Null;
    let pl = prev.unwrap_or(&null).get("lineup_today");
    let cl = cur.get("lineup_today");
    let (Some(pl), Some(cl)) = (pl.filter(|v| jtruthy(Some(v))), cl.filter(|v| jtruthy(Some(v)))) else { return vec![] };
    let names: HashMap<&str, Option<&str>> = jentries(cur, "my_players").into_iter().map(|(cid, rec)| (cid.as_str(), jstr(rec, "name"))).collect();
    let strs = |v: &Value, k: &str| -> HashSet<String> { jlist(v, k).iter().filter_map(|x| x.as_str().map(str::to_string)).collect() };
    let (before, after) = (strs(pl, "start"), strs(cl, "start"));
    let playing_before: HashSet<String> = before.union(&strs(pl, "bench")).cloned().collect();
    let mut out: Vec<Change> = Vec::new();
    let mut gained: Vec<&String> = after.difference(&before).collect();
    gained.sort();
    for cid in gained {
        if let Some(name) = names.get(cid.as_str()) {
            let was = if playing_before.contains(cid) { "sat this morning" } else { "not in the morning lineup" };
            let mut c = Change::new(ChangeKind::LineupChange, format!("Start {} ({was})", last(*name)));
            c.player = name.map(str::to_string);
            c.priority = 3;
            out.push(c);
        }
    }
    let mut lost: Vec<&String> = before.difference(&after).collect();
    lost.sort();
    for cid in lost {
        if let Some(name) = names.get(cid.as_str()) {
            let mut c = Change::new(ChangeKind::LineupChange, format!("Sit {} (was starting this morning)", last(*name)));
            c.player = name.map(str::to_string);
            c.priority = 3;
            out.push(c);
        }
    }
    out
}

/// Line alerts whose title is new since `prev` (goalie-start alerts are covered by
/// [`diff_goalies`]; my players already reported by a line diff are skipped).
pub fn diff_alerts(prev: Option<&Value>, cur: &Value, skip_players: &HashSet<String>) -> Vec<Change> {
    let null = Value::Null;
    let seen: HashSet<Option<&str>> = jlist(prev.unwrap_or(&null), "alerts").iter().map(|a| jstr(a, "title")).collect();
    let mut out: Vec<Change> = Vec::new();
    let mut fa = 0usize;
    for a in jlist(cur, "alerts") {
        let name = jstr(a, "name");
        if seen.contains(&jstr(a, "title")) || jtruthy(a.get("goalie_start")) || name.is_some_and(|n| skip_players.contains(n)) {
            continue;
        }
        let mine = jtruthy(a.get("mine"));
        if !mine {
            if fa >= FA_ALERT_LIMIT {
                continue;
            }
            fa += 1;
        }
        let mut c = Change::new(ChangeKind::NewAlert, jstr(a, "title").unwrap_or("").to_string());
        c.scope = Some(if mine { "mine" } else { "fa" }.into());
        c.player = name.map(str::to_string);
        c.priority = if mine { 3 } else { 6 };
        out.push(c);
    }
    out
}

/// Everything that changed between two states of the same league and day, most urgent first.
pub fn diff_states(prev: Option<&Value>, cur: &Value, baseline_status: Option<&Map<String, String>>) -> Vec<Change> {
    let mut changes = diff_goalies(prev, cur);
    let players = diff_players(prev, cur, baseline_status);
    let moved: HashSet<String> = players.iter().filter(|c| c.kind == ChangeKind::NewAlert).filter_map(|c| c.player.clone()).collect();
    changes.extend(players);
    changes.extend(diff_lineup(prev, cur));
    changes.extend(diff_alerts(prev, cur, &moved));
    changes.sort_by_key(|c| c.priority);
    changes
}

// --------------------------------------------------------------------------- summary

/// Changes worth a line on the phone: `lineup_change` entries are dropped when today's lineup
/// needs moves anyway, and the confirmed goalies my skaters face collapse into one line
/// ("Confirmed vs your skaters: Swayman (BOS), Dobes (MTL, backup)").
pub fn display_changes(changes: &[Change], lineup: Option<&Value>) -> Vec<Change> {
    let has_lineup = lineup.is_some_and(|l| jtruthy(Some(l)));
    let unset = has_lineup && !jtruthy(lineup.and_then(|l| l.get("set")));
    let out: Vec<Change> = changes.iter().filter(|c| !(unset && c.kind == ChangeKind::LineupChange)).cloned().collect();
    let opp: Vec<&Change> = out
        .iter()
        .filter(|c| c.kind == ChangeKind::GoalieConfirmed && c.scope.as_deref() == Some("opponent") && c.short.as_deref().is_some_and(|s| !s.is_empty()))
        .collect();
    if opp.len() < 2 {
        return out;
    }
    let mut merged = Change::new(
        ChangeKind::GoalieConfirmed,
        format!("Confirmed vs your skaters: {}", opp.iter().map(|c| c.short.clone().unwrap_or_default()).collect::<Vec<_>>().join(", ")),
    );
    merged.scope = Some("opponent".into());
    merged.priority = opp[0].priority;
    let first = out.iter().position(|c| c == opp[0]).unwrap_or(0);
    let rest: Vec<Change> = out.iter().filter(|c| !opp.contains(c)).cloned().collect();
    let cut = first.min(rest.len());
    let mut res: Vec<Change> = rest[..cut].to_vec();
    res.push(merged);
    res.extend_from_slice(&rest[cut..]);
    res
}

/// Phone-sized summary (<= `limit` chars).
pub fn summarize(changes: &[Change], lineup: Option<&Value>, ran_at: &PyDateTime, limit: usize) -> String {
    let lim = limit as isize;
    let tail: Option<String> = lineup.filter(|l| jtruthy(Some(l))).and_then(|l| jstr(l, "text")).map(str::to_string);
    let mut shown: Vec<Change> = changes.to_vec();
    if !changes.is_empty() {
        shown = display_changes(changes, lineup);
        if shown.is_empty() {
            if let Some(t) = tail.as_deref().filter(|t| !t.is_empty()) {
                // only start / sit changes: the lineup tail says it all
                return py_slice(&sentence(&format!("Pre-game {}: {t}", clock(ran_at))), None, Some(lim));
            }
        }
    }
    if shown.is_empty() {
        let set = lineup.is_none() || jtruthy(lineup.and_then(|l| l.get("set")));
        if set {
            return NO_CHANGES.to_string();
        }
        return py_slice(&sentence(&format!("Pre-game: no changes. {}", tail.clone().unwrap_or_else(|| "None".into()))), None, Some(lim));
    }
    let head = format!("Pre-game {}:", clock(ran_at));
    let tail_s = match tail.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => format!(" {}", sentence(t)),
        None => String::new(),
    };
    let mut body: Vec<String> = Vec::new();
    for (i, c) in shown.iter().enumerate() {
        let more = shown.len() - i - 1;
        let piece = format!(" {}", sentence(&c.text));
        let reserve = py_len(&tail_s) + if more > 0 { py_len(&format!(" (+{more} more)")) } else { 0 };
        let used: usize = py_len(&head) + body.iter().map(|b| py_len(b)).sum::<usize>();
        if used + py_len(&piece) + reserve > limit {
            body.push(format!(" (+{} more)", shown.len() - i));
            break;
        }
        body.push(piece);
    }
    let text = format!("{head}{}{tail_s}", body.concat());
    if py_len(&text) <= limit {
        text
    } else {
        format!("{}...", py_rstrip(&py_slice(&text, None, Some(lim - 3))))
    }
}

// --------------------------------------------------------------------------- failures

/// Phone line for a league that did not load (login expired, provider down, ...).
pub fn failure_message(league: &str, error: &str) -> String {
    let low = error.to_lowercase();
    if league == "fantrax" && ["not logged in", "login", "cookie", "session"].iter().any(|k| low.contains(k)) {
        return "Fantrax login expired: refresh FANTRAX_COOKIE (or run `fm auth fantrax --login`)".to_string();
    }
    if league == "espn" && ["denied access", "espn_s2", "swid"].iter().any(|k| low.contains(k)) {
        return "ESPN login expired: refresh ESPN_S2 / ESPN_SWID in .env".to_string();
    }
    let first = if py_strip(error).is_empty() { "Error".to_string() } else { py_slice(py_strip(error).lines().next().unwrap_or(""), None, Some(160)) };
    format!("{} not loaded: {first}", league_label(league))
}

pub fn has_webhooks(settings: &Settings) -> bool {
    let set = |s: &Option<fm_core::config::Secret>| s.as_ref().is_some_and(|x| !x.expose().is_empty());
    set(&settings.discord_webhook_url) || set(&settings.slack_webhook_url)
}

// --------------------------------------------------------------------------- main entry

/// `(ctx, values, warnings)` for one league (`Loader` in Python).
pub type Loaded = (LeagueContext, Map<Cid, PlayerValue>, Vec<String>);
pub type Loader<'a> = dyn Fn(&str, &Settings, Option<&Arc<HttpCache>>) -> Result<Loaded> + 'a;
pub type Notifier<'a> = dyn Fn(&Settings, &str, &[String]) -> Vec<String> + 'a;
pub type Planner<'a> = dyn Fn(&LeagueContext, &Map<Cid, PlayerValue>, &PyDateTime) -> Option<WeekPlan> + 'a;

/// `default_loader`: `fm_providers::pipeline::load_league_full`.
pub fn default_loader(league: &str, settings: &Settings, cache: Option<&Arc<HttpCache>>, now: &PyDateTime) -> Result<Loaded> {
    let cache: Arc<HttpCache> = match cache {
        Some(c) => c.clone(),
        None => fm_providers::pipeline::open_cache(settings)?,
    };
    let utc = aware_now(now).to_utc().unwrap_or_else(Utc::now);
    let clk = fm_providers::enrich::Clock::at(utc);
    let full = fm_providers::pipeline::load_league_full(league, settings, &cache, Some(clk))?;
    Ok((full.ctx, full.values, full.warnings))
}

/// The weekly action plan for today's checklist (no provider: no win probabilities needed;
/// `data_dir`: compared with the last plan shown, plan memory).
pub fn week_plan(ctx: &LeagueContext, values: &Map<Cid, PlayerValue>, now: &PyDateTime, data_dir: &Path) -> WeekPlan {
    let aware = aware_now(now);
    let opts = PlanOpts { data_dir: Some(data_dir), ..PlanOpts::default() };
    build_week_plan(ctx, values, &opts, &aware, &ScheduleMemo::new(Some(data_dir.to_path_buf())))
}

/// Latest injury status per player (`status_history.db`), read only.
fn history_snapshots(data_dir: &Path) -> Result<Map<String, fm_engine::recommend::injuries::StatusSnapshot>> {
    StatusHistory::new(data_dir)?.last()
}

pub struct PregameOpts<'a> {
    pub as_of: Option<NaiveDate>,
    pub notify: bool,
    pub force_refresh: bool,
    pub quiet_if_unchanged: bool,
    pub loader: Option<&'a Loader<'a>>,
    pub dfo_client: Option<&'a mut dyn DfoSource>,
    pub injuries_fetch: Option<&'a FetchJson<'a>>,
    pub notifier: Option<&'a Notifier<'a>>,
    pub write_snapshot: bool,
    pub planner: Option<&'a Planner<'a>>,
    /// webhook transport for the default notifier (None: real HTTP)
    pub sender: Option<&'a dyn crate::notify::Sender>,
}

impl Default for PregameOpts<'_> {
    fn default() -> Self {
        PregameOpts {
            as_of: None,
            notify: false,
            force_refresh: true,
            quiet_if_unchanged: false,
            loader: None,
            dfo_client: None,
            injuries_fetch: None,
            notifier: None,
            write_snapshot: true,
            planner: None,
            sender: None,
        }
    }
}

/// One league's pre-game run (see the module docs). Fails only when the league cannot be loaded;
/// every later step degrades to a warning.
///
/// `notify`: post the changes (or the no-change line) with `notify::notify_changes`; with
/// `quiet_if_unchanged` nothing is sent when nothing changed.
pub fn run_pregame(league: &str, settings: &Settings, cache: Option<&Arc<HttpCache>>, now: &PyDateTime, mut opts: PregameOpts<'_>) -> Result<PregameResult> {
    let day = opts.as_of.unwrap_or_else(|| now.date());
    let data_dir: PathBuf = settings.fm_data_dir.clone();
    let mut warnings: Vec<String> = Vec::new();
    let mut refresh = RefreshInfo::default();
    let mut dfo = opts.dfo_client.take();
    let mut starts: Option<Vec<GoalieStart>> = None;
    if opts.force_refresh {
        let (s, info) = refresh_sources(cache.map(|c| c.as_ref()), day, dfo.as_mut().map(|c| &mut **c as &mut dyn DfoSource), opts.injuries_fetch, LINES_MAX_AGE);
        warnings.extend(info.warnings.iter().cloned());
        refresh = info;
        starts = if s.is_empty() { None } else { Some(s) };
    }
    let (mut ctx, values, load_warnings) = match opts.loader {
        Some(l) => l(league, settings, cache)?,
        None => default_loader(league, settings, cache, now)?,
    };
    warnings.extend(load_warnings);
    if let Some(a) = opts.as_of {
        if ctx.as_of != a {
            ctx.as_of = a;
        }
    }
    let starts: Vec<GoalieStart> = match starts {
        Some(s) => s,
        None => cached_starts(cache.map(|c| c.as_ref()), day, &data_dir, dfo.as_mut().map(|c| &mut **c as &mut dyn DfoSource)),
    };
    let aware = aware_now(now);
    let memo = ScheduleMemo::new(Some(data_dir.clone()));

    let week = safe(&mut warnings, "week lineup", || Ok(recommend_lineup(&ctx, &values, Horizon::Week, &aware, &memo)?), vec![]);
    let lineup = safe(&mut warnings, "today's lineup", || today_lineup(&ctx, &values), None);
    let alerts = safe(&mut warnings, "line alerts", || Ok(recommend_line_alerts(&ctx, Some(&values), &LineAlertOpts::default())), vec![]);
    let mine_ids: Vec<String> = safe(&mut warnings, "roster", || Ok(ctx.my_team()?.players_iter().map(|p| p.cid.clone()).collect()), vec![]);
    let history = safe(&mut warnings, "injury status history", || history_snapshots(&data_dir), Map::new());
    let baseline_status: Map<String, String> =
        mine_ids.iter().filter_map(|c| history.get(c).map(|h| (c.clone(), h.status.clone()))).collect();
    let injuries = safe(
        &mut warnings,
        "injury alerts",
        || recommend_injuries(&ctx, &values, History::Snapshots(&history), Horizon::Week, None, &aware, &memo),
        vec![],
    );
    let lines_snap = LineSnapshotStore::new(&data_dir).load(day);
    let mut state = build_state(league, &ctx, &values, &starts, Some(&alerts), "pregame", now, lines_snap.as_ref(), lineup.clone())?;
    state["date"] = json!(day.format("%Y-%m-%d").to_string());
    let prev = load_snapshot(&data_dir, league, day);
    let changes = diff_states(prev.as_ref(), &state, if prev.is_none() { Some(&baseline_status) } else { None });
    let mut res = PregameResult::new(league, day, *now);
    res.nothing_changed = changes.is_empty();
    res.changes = changes.clone();
    res.lineup_recs = LineupRecs { week, today: lineup.clone().unwrap_or(Value::Null) };
    res.alerts = alerts;
    res.injuries = injuries;
    res.goalies_tonight = state["goalies"].clone();
    res.baseline = prev.as_ref().and_then(|p| jstr(p, "source")).map(str::to_string);
    res.baseline_at = prev.as_ref().and_then(|p| jstr(p, "taken_at")).map(str::to_string);
    res.refresh = refresh.info;
    // pydantic copies the list: warnings raised after this point do not reach the result
    res.warnings = warnings.clone();

    let plan: Option<WeekPlan> = safe(
        &mut warnings,
        "week plan",
        || Ok(match opts.planner {
            Some(p) => p(&ctx, &values, now),
            None => Some(week_plan(&ctx, &values, now, &data_dir)),
        }),
        None,
    );
    let mut since: Option<String> = None;
    if let Some(plan) = &plan {
        res.today_actions = today_checklist(plan, day, CHECKLIST_LIMIT);
        since = since_line(plan);
    }
    // plan steps that change the roster or lineup (a goalie check alone leaves "lineup set" true)
    let moves_today: Vec<&String> = res.today_actions.iter().filter(|a| !a.starts_with("Check goalie")).collect();
    let mut lineup = lineup;
    if !moves_today.is_empty() && lineup.as_ref().is_some_and(|l| jtruthy(l.get("set"))) {
        if let Some(l) = lineup.as_mut() {
            l["text"] = json!(PLAN_REST);
        }
        res.lineup_recs.today = lineup.clone().unwrap_or(Value::Null);
    }
    res.summary_text = summarize(&changes, lineup.as_ref(), now, SUMMARY_LIMIT);
    if !res.today_actions.is_empty() {
        if !moves_today.is_empty() && res.summary_text == NO_CHANGES {
            res.summary_text = NO_CHANGES_PLAN.to_string();
        }
        let head = sentence(&format!("{CHECKLIST_HEAD} {}", res.today_actions.join("; ")));
        res.summary_text = py_slice(&format!("{head} {}", res.summary_text), None, Some(SUMMARY_LIMIT as isize));
    }
    if let Some(s) = since {
        res.summary_text = py_slice(&format!("{} {}", res.summary_text, sentence(&s)), None, Some(SUMMARY_LIMIT as isize));
    }
    if opts.write_snapshot {
        match save_snapshot(&data_dir, league, day, &state) {
            Ok(p) => res.snapshot_path = Some(p.display().to_string()),
            Err(e) => warnings.push(format!("pre-game snapshot not written ({})", short(&e.to_string()))),
        }
    }
    // An unset lineup at 5 PM is the most useful thing to send: post even when nothing changed.
    let today_unset = res.lineup_recs.today_opt().is_some_and(|t| !t.get("set").map_or(true, |s| jtruthy(Some(s))));
    // an add the plan schedules today is an action too, even when nothing changed since morning
    let pending_today = today_unset || res.today_actions.iter().any(|a| a.starts_with("Add ") || a.starts_with("Claim "));
    if opts.notify && !(opts.quiet_if_unchanged && res.nothing_changed && !pending_today) {
        let title = res.title();
        let lines = res.notify_lines();
        res.notify = Some(match opts.notifier {
            Some(n) => n(settings, &title, &lines),
            None => crate::notify::notify_changes(settings, &title, &lines, opts.sender),
        });
    }
    Ok(res)
}

/// The morning baseline: called by `fm harness daily` after each league load (goalie starts
/// from today's Daily Faceoff lines snapshot, which the load just wrote).
pub fn write_daily_snapshot(league: &str, settings: &Settings, ctx: &LeagueContext, values: &Map<Cid, PlayerValue>, now: &PyDateTime) -> Result<PathBuf> {
    let data_dir = &settings.fm_data_dir;
    let snap = LineSnapshotStore::new(data_dir).load(ctx.as_of).unwrap_or_else(|| json!({}));
    let starts: Vec<GoalieStart> =
        jlist(&snap, "starts").iter().filter_map(|s| serde_json::from_value::<GoalieStart>(s.clone()).ok()).collect();
    let state = build_state(league, ctx, values, &starts, None, "daily", now, Some(&snap), None)?;
    save_snapshot(data_dir, league, ctx.as_of, &state)
}
