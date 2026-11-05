//! Rust port of `fantasy_manager/providers/lines_enrich.py`: fill line / unit / goalie-start
//! fields on a LeagueContext from Daily Faceoff. Owner: work package `W4-enrich` (wave 4).
//!
//! [`enrich_lines`] (best effort: every failure becomes a `ctx.warnings` entry):
//! 1. fetch every team's line-combinations page (12 h cache) and today's starting goalies (3 h)
//! 2. map Daily Faceoff player ids to NHL ids through the crosswalk (source `"dfo"`)
//! 3. set `Player.line` / `pp_unit` / `pk_unit` (+ a DFO injury `status_note`)
//! 4. persist a daily snapshot (`<fm_data_dir>/lines/lines-YYYY-MM-DD.json`), set
//!    `Player.line_change` against the most recent earlier snapshot and `usual_line` /
//!    `usual_pp_unit` from the last 30 days of snapshots
//! 5. goalies: `confirmed_start` / `start_source`
//!
//! Deviations: `_TEAM_META` is `ctx.enrich.line_team_meta` (`updated_at` stored as its ISO
//! string); snapshots are `serde_json::Value`s written with Python's `json.dumps(indent=1,
//! sort_keys=True)` format; the clock (`created`) is a parameter.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use chrono::{NaiveDate, Utc};
use fm_core::models::{LeagueContext, Player};
use fm_core::normalize::{normalize_name, normalize_team};
use fm_core::{Map, PyDateTime};
use fm_http::HttpCache;
use fm_pycompat::pyjson::{dumps, DumpOpts};
use fm_sources::dailyfaceoff::{make_cached_fetch_text, DailyFaceoffClient, GoalieStart, LinePlayer, TeamLines, TEAM_SLUGS};
use fm_sources::matching::crosswalk::{candidates_of, nhl_candidates, Crosswalk, SourceItem};
use fm_sources::matching::matcher::{Candidate, PlayerIndex};
use fm_sources::nhl::{current_season, NhlClient, NhlRosterPlayer};
use regex::Regex;
use serde_json::{json, Value};

use crate::enrich::{ctx_players_mut, Clock, SourceTracker};

pub const SOURCE: &str = "dfo";
pub const LINES_DIR: &str = "lines";
pub const NO_PP: &str = "no PP";
/// snapshots behind Player.usual_line / usual_pp_unit
pub const USUAL_WINDOW_DAYS: i64 = 30;

fn file_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^lines-(\d{4}-\d{2}-\d{2})\.json$").expect("static regex"))
}

// --------------------------------------------------------------------------- snapshots

/// Daily JSON snapshots under `<data_dir>/lines/lines-YYYY-MM-DD.json`.
#[derive(Clone, Debug)]
pub struct LineSnapshotStore {
    pub dir: PathBuf,
}

impl LineSnapshotStore {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        LineSnapshotStore { dir: data_dir.as_ref().join(LINES_DIR) }
    }

    pub fn path(&self, day: NaiveDate) -> PathBuf {
        self.dir.join(format!("lines-{}.json", day.format("%Y-%m-%d")))
    }

    pub fn dates(&self) -> Vec<NaiveDate> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return vec![] };
        let mut out: Vec<NaiveDate> = rd
            .flatten()
            .filter_map(|f| {
                let name = f.file_name().to_string_lossy().to_string();
                let c = file_re().captures(&name)?;
                NaiveDate::parse_from_str(&c[1], "%Y-%m-%d").ok()
            })
            .collect();
        out.sort();
        out
    }

    pub fn load(&self, day: NaiveDate) -> Option<Value> {
        let text = std::fs::read_to_string(self.path(day)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, day: NaiveDate, snapshot: &Value) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        let p = self.path(day);
        let tmp = p.with_extension("json.tmp");
        let opts = DumpOpts { indent: Some(1), ensure_ascii: true, sort_keys: true, separators: None };
        std::fs::write(&tmp, dumps(snapshot, &opts))?;
        std::fs::rename(&tmp, &p)?;
        Ok(p)
    }

    /// Most recent snapshot strictly before `day`.
    pub fn previous(&self, day: NaiveDate) -> Option<(NaiveDate, Value)> {
        for d in self.dates().into_iter().rev() {
            if d < day {
                if let Some(snap) = self.load(d) {
                    return Some((d, snap));
                }
            }
        }
        None
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn str_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn first_max(c: &Map<String, i64>) -> String {
    let best = c.values().copied().max().unwrap_or(0);
    c.iter().find(|(_, v)| **v == best).map(|(k, _)| k.clone()).unwrap_or_default()
}

/// {nhl id: (usual line, usual PP unit or None)}: the most common line and PP unit over the
/// snapshots strictly before `day` within `window_days` (ties: the most recent wins).
pub fn usual_roles(store: &LineSnapshotStore, day: NaiveDate, window_days: i64) -> Map<i64, (String, Option<String>)> {
    type Counter = Map<String, i64>;
    let mut seen: Map<i64, (Counter, Counter)> = Map::new();
    for d in store.dates().into_iter().rev() {
        let age = (day - d).num_days();
        if !(0 < age && age <= window_days) {
            continue;
        }
        let snap = store.load(d).unwrap_or(Value::Null);
        let Some(players) = snap.get("players").and_then(Value::as_object) else { continue };
        for rec in players.values() {
            let Some(nid) = rec.get("nhl_id").and_then(Value::as_i64) else { continue };
            let line = rec.get("line");
            if !truthy(line) || line.and_then(Value::as_str) == Some("g") {
                continue;
            }
            let line = str_of(line);
            let (lines, pps) = seen.entry(nid).or_default();
            *lines.entry(line).or_insert(0) += 1;
            let pp = if truthy(rec.get("pp_unit")) { str_of(rec.get("pp_unit")) } else { String::new() };
            *pps.entry(pp).or_insert(0) += 1;
        }
    }
    seen.into_iter()
        .map(|(nid, (lines, pps))| {
            let pp = first_max(&pps);
            (nid, (first_max(&lines), (!pp.is_empty()).then_some(pp)))
        })
        .collect()
}

fn player_key(lp: &LinePlayer, team: &str) -> String {
    match lp.dfo_id {
        Some(id) => id.to_string(),
        None => format!("{team}:{}", normalize_name(Some(&lp.name))),
    }
}

/// JSON-able snapshot: teams (update time / source), players keyed by DFO id, starts.
pub fn build_snapshot(
    day: NaiveDate,
    lines: &Map<String, TeamLines>,
    starts: &[GoalieStart],
    nhl_ids: &Map<String, i64>,
    now: chrono::DateTime<Utc>,
) -> Value {
    let mut players = serde_json::Map::new();
    let mut teams = serde_json::Map::new();
    let mut sorted: Vec<(&String, &TeamLines)> = lines.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (team, tl) in sorted {
        teams.insert(
            team.clone(),
            json!({"updated_at": tl.updated_at.map(|u| u.isoformat()), "source": tl.source, "source_url": tl.source_url}),
        );
        for lp in &tl.players {
            let key = player_key(lp, team);
            let nid = lp.dfo_id.and_then(|id| nhl_ids.get(&id.to_string()).copied());
            players.insert(
                key,
                json!({
                    "name": lp.name, "team": team, "dfo_id": lp.dfo_id, "nhl_id": nid,
                    "line": lp.line, "group": lp.group, "goalie_depth": lp.goalie_depth,
                    "pp_unit": lp.pp_unit, "pk_unit": lp.pk_unit,
                    "injury_status": lp.injury_status, "gtd": lp.gtd,
                }),
            );
        }
    }
    let created = PyDateTime::from_utc(
        chrono::DateTime::<Utc>::from_timestamp(now.timestamp(), 0).unwrap_or(now),
    )
    .isoformat();
    json!({
        "date": day.format("%Y-%m-%d").to_string(),
        "created": created,
        "teams": Value::Object(teams),
        "players": Value::Object(players),
        "starts": starts.iter().map(|s| serde_json::to_value(s).unwrap_or(Value::Null)).collect::<Vec<_>>(),
    })
}

fn unit(u: Option<&Value>) -> String {
    if truthy(u) {
        str_of(u).to_uppercase()
    } else {
        NO_PP.into()
    }
}

fn line_label(rec: &Value) -> String {
    if rec.get("line").and_then(Value::as_str) == Some("g") && truthy(rec.get("goalie_depth")) {
        return format!("G{}", str_of(rec.get("goalie_depth")));
    }
    if truthy(rec.get("line")) {
        str_of(rec.get("line")).to_uppercase()
    } else {
        String::new()
    }
}

/// {player key: change text} between two snapshots. Only teams present in both snapshots are
/// compared (a team whose page failed on either day reports nothing).
pub fn diff_snapshots(prev: Option<&Value>, cur: &Value) -> Map<String, String> {
    let mut out: Map<String, String> = Map::new();
    let Some(prev) = prev.filter(|p| truthy(Some(p))) else { return out };
    let keys = |v: &Value, k: &str| -> BTreeSet<String> {
        v.get(k).and_then(Value::as_object).map(|o| o.keys().cloned().collect()).unwrap_or_default()
    };
    let pt = keys(prev, "teams");
    let ct = keys(cur, "teams");
    let both: HashSet<&String> = pt.intersection(&ct).collect();
    let empty = serde_json::Map::new();
    let pp_ = prev.get("players").and_then(Value::as_object).unwrap_or(&empty);
    let cp_ = cur.get("players").and_then(Value::as_object).unwrap_or(&empty);
    let all: BTreeSet<&String> = pp_.keys().chain(cp_.keys()).collect();
    let in_both = |v: Option<&Value>| -> bool { v.and_then(Value::as_str).is_some_and(|t| both.iter().any(|b| b.as_str() == t)) };
    for key in all {
        let c = cp_.get(key);
        let p = pp_.get(key);
        let mut changes: Vec<String> = Vec::new();
        if let Some(c) = c.filter(|c| in_both(c.get("team"))) {
            let same_team = p.is_some_and(|p| p.get("team") == c.get("team"));
            if let Some(p) = p {
                if !same_team && in_both(p.get("team")) {
                    changes.push(format!("{} -> {}", str_of(p.get("team")), str_of(c.get("team"))));
                }
            }
            let prev_in = p.is_some_and(|p| truthy(Some(p)) && truthy(p.get("line"))) && same_team;
            let cur_in = truthy(c.get("line"));
            if cur_in && !prev_in {
                changes.push("new to lineup".into());
            } else if prev_in && cur_in {
                let (a, b) = (line_label(p.expect("prev_in")), line_label(c));
                if a != b {
                    changes.push(format!("{a} -> {b}"));
                }
            } else if prev_in && !cur_in {
                if c.get("group").and_then(Value::as_str) == Some("ir") {
                    changes.push(format!("{} -> IR", line_label(p.expect("prev_in"))));
                } else {
                    changes.push("scratched".into());
                }
            }
            let null = Value::Null;
            let before = if same_team { p.and_then(|p| p.get("pp_unit")).unwrap_or(&null) } else { &null };
            let after = c.get("pp_unit").unwrap_or(&null);
            if before != after && (same_team || truthy(Some(after))) {
                changes.push(format!("{} -> {}", unit(Some(before)), unit(Some(after))));
            }
        } else if c.is_none() {
            if let Some(p) = p {
                if truthy(p.get("line")) && in_both(p.get("team")) {
                    changes.push("scratched".into());
                }
            }
        }
        if !changes.is_empty() {
            out.insert(key.clone(), changes.join("; "));
        }
    }
    out
}

// --------------------------------------------------------------------------- helpers

/// `LinesResult`.
#[derive(Clone, Debug, PartialEq)]
pub struct LinesResult {
    pub day: NaiveDate,
    /// team pages parsed
    pub teams: usize,
    /// distinct players on those pages
    pub listed: usize,
    /// of which mapped to NHL ids
    pub matched: usize,
    pub pending: i64,
    /// league players updated
    pub applied: usize,
    /// league player name -> line_change
    pub changes: Map<String, String>,
    pub starts: Vec<GoalieStart>,
    /// league goalies with confirmed_start set
    pub starters_applied: usize,
    pub snapshot_path: Option<String>,
    pub previous_date: Option<NaiveDate>,
    pub warnings: Vec<String>,
}

impl LinesResult {
    pub fn new(day: NaiveDate) -> Self {
        LinesResult {
            day,
            teams: 0,
            listed: 0,
            matched: 0,
            pending: 0,
            applied: 0,
            changes: Map::new(),
            starts: vec![],
            starters_applied: 0,
            snapshot_path: None,
            previous_date: None,
            warnings: vec![],
        }
    }
}

/// lines_enrich's own `_short(e)`: first line of `str(e)` (else the class name), 120 chars.
fn short(e: &anyhow::Error) -> String {
    let s = e.to_string();
    let t = fm_pycompat::text::py_strip(&s);
    if t.is_empty() {
        return crate::enrich::exc_name(e).to_string();
    }
    let first = t.lines().next().unwrap_or("");
    fm_pycompat::text::py_slice_str(first, None, Some(120)).to_string()
}

fn league_candidates(players: &[&Player]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for p in players {
        let Some(nid) = p.nhl_id() else { continue };
        let pos: Vec<&str> = p.positions.iter().map(String::as_str).filter(|x| *x != "F" && *x != "UTIL").collect();
        let pos = if pos.is_empty() { p.positions.iter().map(String::as_str).collect() } else { pos };
        let position = (!pos.is_empty()).then(|| pos.join("/"));
        out.push(Candidate::new(nid, &p.name, normalize_team(p.team.as_deref()).as_deref(), position.as_deref()));
    }
    out
}

fn roster_candidates(cache: &Arc<HttpCache>, teams: &[String], as_of: NaiveDate, today: NaiveDate, warnings: &mut Vec<String>) -> Vec<NhlRosterPlayer> {
    let tracker = SourceTracker::new(cache.clone(), today);
    let client = NhlClient::new(Box::new(move |u, p, _| tracker.fetch_json(u, p)), Some(current_season(as_of)), today);
    let mut out = Vec::new();
    let mut failed = 0;
    let mut last: Option<anyhow::Error> = None;
    for t in teams {
        match client.team_roster(t, None) {
            Ok(r) => out.extend(r),
            Err(e) => {
                failed += 1;
                last = Some(e);
            }
        }
    }
    if let Some(e) = last.filter(|_| failed > 0) {
        warnings.push(format!("NHL rosters for Daily Faceoff matching: {failed} team(s) unavailable ({})", short(&e)));
    }
    out
}

fn resolve(items: &[SourceItem], index: &PlayerIndex, crosswalk: Option<&Crosswalk>) -> anyhow::Result<(Map<String, i64>, i64)> {
    if let Some(xw) = crosswalk {
        let (ids, stats) = xw.resolve_source(SOURCE, items, index, false)?;
        return Ok((ids, stats.pending));
    }
    let mut out: Map<String, i64> = Map::new();
    let mut pending = 0;
    for it in items {
        let m = index.find(&it.name, it.team.as_deref(), it.position.as_deref());
        if m.matched() {
            out.insert(it.source_id.clone(), m.key.as_ref().and_then(|k| k.as_i64()).unwrap_or(0));
        } else if m.confidence.as_str() == "pending" {
            pending += 1;
        }
    }
    Ok((out, pending))
}

fn dfo_note(lp: &LinePlayer) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(s) = lp.injury_status.as_deref().filter(|s| !s.is_empty()) {
        parts.push(s);
    }
    if lp.gtd {
        parts.push("game-time decision");
    }
    (!parts.is_empty()).then(|| format!("DFO: {}", parts.join(", ")))
}

fn fmt_time(dt: Option<&PyDateTime>) -> String {
    let Some(dt) = dt else { return "time n/a".into() };
    let utc = match dt.to_utc() {
        Some(u) => u,
        None => {
            use chrono::TimeZone;
            match chrono::Local.from_local_datetime(&dt.local).earliest() {
                Some(l) => l.with_timezone(&Utc),
                None => return "time n/a".into(),
            }
        }
    };
    utc.format("%b %d %H:%M UTC").to_string()
}

fn py_opt(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("None")
}

/// "DFO Confirmed: Jeremy Swayman starts NYR@BOS (Jack Studley, Sep 29 15:02 UTC)".
pub fn start_source_text(s: &GoalieStart) -> String {
    let who = s.source.as_deref().filter(|x| !x.is_empty()).unwrap_or("Daily Faceoff");
    format!("DFO {}: {} starts {} ({who}, {})", py_opt(&s.strength), s.goalie_name, s.game, fmt_time(s.created_at.as_ref()))
}

// --------------------------------------------------------------------------- main entry

/// Mutates the league players in `ctx`; never fails for a failing source.
///
/// `cache` feeds both Daily Faceoff and (unless `candidates` is given) the NHL roster lookups
/// used as match candidates. `crosswalk` None matches without persisting. `snapshot_store`
/// enables the daily snapshot and `line_change`.
#[allow(clippy::too_many_arguments)]
pub fn enrich_lines(
    ctx: &mut LeagueContext,
    cache: Option<&Arc<HttpCache>>,
    crosswalk: Option<&Crosswalk>,
    as_of: Option<NaiveDate>,
    snapshot_store: Option<&LineSnapshotStore>,
    client: Option<&mut DailyFaceoffClient<'_>>,
    candidates: Option<&PlayerIndex>,
    teams: Option<&[String]>,
    fetch_rosters: bool,
    clock: Clock,
) -> LinesResult {
    enrich_lines_prefetched(ctx, cache, crosswalk, as_of, snapshot_store, client, candidates, teams, fetch_rosters, clock, None)
}

/// [`enrich_lines`] with the Daily Faceoff pages (and the roster match candidates) already
/// fetched by [`prefetch_lines`] for the same day and teams (`None`: fetch here).
#[allow(clippy::too_many_arguments)]
pub fn enrich_lines_prefetched(
    ctx: &mut LeagueContext,
    cache: Option<&Arc<HttpCache>>,
    crosswalk: Option<&Crosswalk>,
    as_of: Option<NaiveDate>,
    snapshot_store: Option<&LineSnapshotStore>,
    client: Option<&mut DailyFaceoffClient<'_>>,
    candidates: Option<&PlayerIndex>,
    teams: Option<&[String]>,
    fetch_rosters: bool,
    clock: Clock,
    prefetched: Option<LinesFetch>,
) -> LinesResult {
    let day = as_of.unwrap_or(ctx.as_of);
    match client {
        Some(c) => lines_with(ctx, cache, crosswalk, day, snapshot_store, c, candidates, teams, fetch_rosters, clock, prefetched),
        None => {
            let mut own = match cache {
                Some(c) => DailyFaceoffClient::new(Box::new(make_cached_fetch_text(c.as_ref(), None, None))),
                None => DailyFaceoffClient::with_default_fetch(),
            };
            lines_with(ctx, cache, crosswalk, day, snapshot_store, &mut own, candidates, teams, fetch_rosters, clock, prefetched)
        }
    }
}

/// The league-independent first half of `enrich_lines`: every team page and the starting
/// goalies page for `day`, then (with a cache) the NHL rosters used as match candidates.
#[derive(Debug)]
pub struct LinesFetch {
    pub day: NaiveDate,
    lines: Map<String, TeamLines>,
    warnings: Vec<String>,
    starts: Result<Vec<GoalieStart>, String>,
    /// roster candidates + their warning (fetched only with a cache)
    rosters: Option<(Vec<NhlRosterPlayer>, Vec<String>)>,
}

fn team_list_of(teams: Option<&[String]>) -> Vec<String> {
    match teams {
        Some(t) => t.iter().map(|t| normalize_team(Some(t)).unwrap_or_else(|| t.clone())).collect(),
        None => TEAM_SLUGS.iter().map(|(t, _)| normalize_team(Some(t)).unwrap_or_else(|| t.to_string())).collect(),
    }
}

fn fetch_pages(client: &mut DailyFaceoffClient<'_>, team_list: &[String], day: NaiveDate) -> (Map<String, TeamLines>, Vec<String>, Result<Vec<GoalieStart>, String>) {
    let lines = client.all_lines(Some(team_list));
    let warnings = std::mem::take(&mut client.warnings);
    let starts = client.starting_goalies(day).map_err(|e| format!("Daily Faceoff starting goalies unavailable ({})", short(&e)));
    (lines, warnings, starts)
}

fn roster_teams(lines: &Map<String, TeamLines>, starts: &[GoalieStart]) -> Vec<String> {
    let mut tset: BTreeSet<String> = lines.keys().cloned().collect();
    tset.extend(starts.iter().map(|s| s.team.clone()));
    tset.into_iter().collect()
}

/// Fetch (and parse) what `enrich_lines(ctx, cache, as_of=day, teams=None)` reads from the
/// network, through the cache, without a context (runs on a worker thread).
pub fn prefetch_lines(cache: &Arc<HttpCache>, day: NaiveDate, today: NaiveDate) -> LinesFetch {
    let mut client = DailyFaceoffClient::new(Box::new(make_cached_fetch_text(cache.as_ref(), None, None)));
    let (lines, warnings, starts) = fetch_pages(&mut client, &team_list_of(None), day);
    let empty = Vec::new();
    let tlist = roster_teams(&lines, starts.as_ref().unwrap_or(&empty));
    let mut rw = Vec::new();
    let ros = roster_candidates(cache, &tlist, day, today, &mut rw);
    LinesFetch { day, lines, warnings, starts, rosters: Some((ros, rw)) }
}

#[allow(clippy::too_many_arguments)]
fn lines_with(
    ctx: &mut LeagueContext,
    cache: Option<&Arc<HttpCache>>,
    crosswalk: Option<&Crosswalk>,
    day: NaiveDate,
    snapshot_store: Option<&LineSnapshotStore>,
    client: &mut DailyFaceoffClient<'_>,
    candidates: Option<&PlayerIndex>,
    teams: Option<&[String]>,
    fetch_rosters: bool,
    clock: Clock,
    prefetched: Option<LinesFetch>,
) -> LinesResult {
    let mut res = LinesResult::new(day);
    let store = snapshot_store;

    // 1. fetch ----------------------------------------------------------------------
    // (prefetched pages are only valid for the default team list and the same day)
    let prefetched = prefetched.filter(|p| teams.is_none() && p.day == day);
    let (lines, mut fetch_warnings, fetched_starts, mut pre_rosters) = match prefetched {
        Some(p) => (p.lines, p.warnings, p.starts, p.rosters),
        None => {
            let (l, w, s) = fetch_pages(client, &team_list_of(teams), day);
            (l, w, s, None)
        }
    };
    res.warnings.append(&mut fetch_warnings);
    let mut starts: Vec<GoalieStart> = Vec::new();
    match fetched_starts {
        Ok(s) => starts = s,
        Err(msg) => res.warnings.push(msg),
    }
    res.starts = starts.clone();
    res.teams = lines.len();

    // 2. crosswalk --------------------------------------------------------------------
    let mut items: Vec<SourceItem> = Vec::new();
    for (team, tl) in &lines {
        for lp in &tl.players {
            if let Some(id) = lp.dfo_id {
                items.push(SourceItem { source_id: id.to_string(), name: lp.name.clone(), team: Some(team.clone()), position: lp.position.clone() });
            }
        }
    }
    for s in &starts {
        if let Some(id) = s.dfo_id {
            items.push(SourceItem { source_id: id.to_string(), name: s.goalie_name.clone(), team: Some(s.team.clone()), position: Some("G".into()) });
        }
    }
    let line_ids: HashSet<String> =
        lines.values().flat_map(|tl| tl.players.iter().filter_map(|lp| lp.dfo_id.map(|i| i.to_string()))).collect();
    res.listed = line_ids.len();
    let mut dfo_to_nhl: Map<String, i64> = Map::new();
    if !items.is_empty() {
        let mut resolve_all = || -> anyhow::Result<(Map<String, i64>, i64)> {
            let built;
            let index: &PlayerIndex = match candidates {
                Some(i) => i,
                None => {
                    let players = ctx.all_players();
                    let mut cands_src: Vec<Vec<Candidate>> = vec![league_candidates(&players)];
                    if fetch_rosters {
                        if let Some(c) = cache {
                            let ros = match pre_rosters.take() {
                                Some((ros, mut w)) => {
                                    res.warnings.append(&mut w);
                                    ros
                                }
                                None => roster_candidates(c, &roster_teams(&lines, &starts), day, clock.today, &mut res.warnings),
                            };
                            cands_src.push(candidates_of(&ros));
                        }
                    }
                    built = PlayerIndex::new(nhl_candidates(&cands_src));
                    &built
                }
            };
            resolve(&items, index, crosswalk)
        };
        let r = resolve_all();
        match r {
            Ok((m, pending)) => {
                dfo_to_nhl = m;
                res.pending = pending;
            }
            Err(e) => res.warnings.push(format!("Daily Faceoff player matching failed ({})", short(&e))),
        }
    }
    res.matched = line_ids.iter().filter(|k| dfo_to_nhl.contains_key(*k)).count();

    if !lines.is_empty() {
        let meta: Map<String, Map<String, Value>> = lines
            .iter()
            .map(|(t, tl)| {
                let mut m: Map<String, Value> = Map::new();
                m.insert("source".into(), json!(tl.source));
                m.insert("source_url".into(), json!(tl.source_url));
                m.insert("updated_at".into(), json!(tl.updated_at.map(|u| u.isoformat())));
                (t.clone(), m)
            })
            .collect();
        Arc::make_mut(&mut ctx.enrich).line_team_meta.insert(day, meta);
    }

    // 3. snapshot + diff -------------------------------------------------------------------
    let mut changes: Map<String, String> = Map::new();
    if !lines.is_empty() {
        let mut snap = build_snapshot(day, &lines, &starts, &dfo_to_nhl, clock.utc());
        if let Some(store) = store {
            let r = (|| -> anyhow::Result<PathBuf> {
                if let Some((d, prev)) = store.previous(day) {
                    res.previous_date = Some(d);
                    changes = diff_snapshots(Some(&prev), &snap);
                }
                snap["changes"] = serde_json::to_value(&changes)?;
                store.save(day, &snap)
            })();
            match r {
                Ok(p) => res.snapshot_path = Some(p.to_string_lossy().to_string()),
                Err(e) => res.warnings.push(format!("line snapshot failed ({})", short(&e))),
            }
        }
    }

    // 4. apply to league players --------------------------------------------------------------
    let usual = store.map(|s| usual_roles(s, day, USUAL_WINDOW_DAYS));
    let prev_snap = match (store, res.previous_date) {
        (Some(s), Some(d)) if changes.iter().any(|(k, c)| c == "scratched" && !dfo_to_nhl.contains_key(k)) => s.load(d),
        _ => None,
    };
    let mut res_changes: Map<String, String> = Map::new();
    let mut applied = 0;
    {
        let mut players = ctx_players_mut(ctx);
        let mut by_nhl: HashMap<i64, Vec<usize>> = HashMap::new();
        for (i, p) in players.iter().enumerate() {
            if let Some(n) = p.nhl_id() {
                by_nhl.entry(n).or_default().push(i);
            }
        }
        if let Some(usual) = &usual {
            for (nid, (line, pp)) in usual {
                for &i in by_nhl.get(nid).map(Vec::as_slice).unwrap_or(&[]) {
                    players[i].usual_line = Some(line.clone());
                    players[i].usual_pp_unit = pp.clone();
                }
            }
        }
        for (team, tl) in &lines {
            for lp in &tl.players {
                let nid = lp.dfo_id.and_then(|id| dfo_to_nhl.get(&id.to_string()).copied());
                let Some(nid) = nid else { continue };
                for &i in by_nhl.get(&nid).map(Vec::as_slice).unwrap_or(&[]) {
                    let p = &mut players[i];
                    p.line = lp.line.clone();
                    p.pp_unit = lp.pp_unit.clone();
                    p.pk_unit = lp.pk_unit.clone();
                    let ch = changes.get(&player_key(lp, team)).cloned();
                    p.line_change = ch.clone();
                    if let Some(ch) = ch.filter(|c| !c.is_empty()) {
                        res_changes.insert(p.name.clone(), ch);
                    }
                    if let Some(note) = dfo_note(lp) {
                        if p.status_note.as_deref().is_none_or(str::is_empty) {
                            p.status_note = Some(note);
                        }
                    }
                    applied += 1;
                }
            }
        }
        // players who vanished from their team's page since the last snapshot
        for (key, ch) in &changes {
            if ch != "scratched" || key.is_empty() || !key.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let mut nid = dfo_to_nhl.get(key).copied();
            if nid.is_none() {
                nid = prev_snap
                    .as_ref()
                    .and_then(|s| s.get("players"))
                    .and_then(|p| p.get(key))
                    .and_then(|r| r.get("nhl_id"))
                    .and_then(Value::as_i64);
            }
            let Some(nid) = nid else { continue };
            for &i in by_nhl.get(&nid).map(Vec::as_slice).unwrap_or(&[]) {
                if players[i].line_change.is_none() {
                    players[i].line_change = Some(ch.clone());
                    res_changes.insert(players[i].name.clone(), ch.clone());
                }
            }
        }
    }
    res.applied = applied;
    res.changes = res_changes;

    // 5. goalie starts ------------------------------------------------------------------------
    let mut starters: Map<String, &GoalieStart> = Map::new();
    for s in &starts {
        if s.is_start() {
            let replace = match starters.get(&s.team) {
                None => true,
                Some(prior) => s.is_confirmed() && !prior.is_confirmed(),
            };
            if replace {
                starters.insert(s.team.clone(), s);
            }
        }
    }
    let mut starters_applied = 0;
    for p in ctx_players_mut(ctx) {
        if !p.is_goalie() {
            continue;
        }
        let Some(team) = normalize_team(p.team.as_deref()) else { continue };
        let Some(s) = starters.get(&team) else { continue };
        let sid = s.dfo_id.and_then(|id| dfo_to_nhl.get(&id.to_string()).copied());
        let is_him = match sid {
            Some(sid) => p.nhl_id() == Some(sid),
            None => normalize_name(Some(&p.name)) == normalize_name(Some(&s.goalie_name)),
        };
        p.confirmed_start = Some(is_him);
        p.start_source = Some(start_source_text(s));
        starters_applied += 1;
    }
    res.starters_applied = starters_applied;

    // 6. notes --------------------------------------------------------------------------------
    if !lines.is_empty() || !starts.is_empty() {
        let n_start = starts.iter().filter(|s| s.is_start()).count();
        let n_conf = starts.iter().filter(|s| s.is_confirmed()).count();
        let n_games = starts.iter().map(|s| s.game.as_str()).collect::<HashSet<_>>().len();
        let prev_txt = res.previous_date.map(|d| format!(" vs {d}")).unwrap_or_default();
        ctx.source_notes.push(format!(
            "Daily Faceoff: lines for {} teams ({}/{} players matched to NHL ids, {} league players updated, {} line changes{prev_txt}); goalies {day}: {n_conf} confirmed / {n_start} named starters in {n_games} games",
            res.teams,
            res.matched,
            res.listed,
            res.applied,
            res.changes.len()
        ));
        if res.pending > 0 {
            ctx.source_notes.push(format!("Daily Faceoff: {} player matches pending (fm sync --review)", res.pending));
        }
    }
    ctx.warnings.extend(res.warnings.iter().cloned());
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(teams: &[&str], players: Value) -> Value {
        let t: serde_json::Map<String, Value> = teams.iter().map(|t| (t.to_string(), json!({}))).collect();
        json!({"teams": t, "players": players})
    }

    #[test]
    fn diff_snapshot_changes() {
        let prev = snap(
            &["EDM", "BOS"],
            json!({
                "1": {"team": "EDM", "line": "f2", "pp_unit": "pp1"},
                "2": {"team": "EDM", "line": "f3", "pp_unit": null},
                "3": {"team": "EDM", "line": "g", "goalie_depth": 1},
                "4": {"team": "BOS", "line": "d1"},
                "5": {"team": "EDM", "line": "f4"},
            }),
        );
        let cur = snap(
            &["EDM", "BOS"],
            json!({
                "1": {"team": "EDM", "line": "f1", "pp_unit": "pp2"},
                "2": {"team": "EDM", "line": null, "group": "ir", "pp_unit": null},
                "3": {"team": "EDM", "line": "g", "goalie_depth": 2},
                "4": {"team": "EDM", "line": "d2"},
                "6": {"team": "BOS", "line": "f1", "pp_unit": "pp1"},
            }),
        );
        let d = diff_snapshots(Some(&prev), &cur);
        assert_eq!(d["1"], "F2 -> F1; PP1 -> PP2");
        assert_eq!(d["2"], "F3 -> IR");
        assert_eq!(d["3"], "G1 -> G2");
        assert_eq!(d["4"], "BOS -> EDM; new to lineup");
        assert_eq!(d["5"], "scratched");
        assert_eq!(d["6"], "new to lineup; no PP -> PP1");
        assert!(diff_snapshots(None, &cur).is_empty());
    }

    #[test]
    fn usual_roles_most_common_newest_wins_ties() {
        let dir = tempfile::tempdir().unwrap();
        let store = LineSnapshotStore::new(dir.path());
        let day = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let save = |d: u32, line: &str, pp: Option<&str>| {
            let s = snap(&["EDM"], json!({"1": {"nhl_id": 7, "team": "EDM", "line": line, "pp_unit": pp}}));
            store.save(NaiveDate::from_ymd_opt(2026, 11, d).unwrap(), &s).unwrap();
        };
        save(1, "f2", None);
        save(2, "f1", Some("pp1"));
        save(3, "f2", Some("pp1"));
        save(4, "f4", None); // same day: not "before"
        let u = usual_roles(&store, day, USUAL_WINDOW_DAYS);
        assert_eq!(u[&7], ("f2".to_string(), Some("pp1".to_string())));
        assert_eq!(store.previous(day).unwrap().0, NaiveDate::from_ymd_opt(2026, 11, 3).unwrap());
        let text = std::fs::read_to_string(store.path(day)).unwrap();
        assert!(text.starts_with("{\n \"players\": {"));
    }

    #[test]
    fn start_source_text_format() {
        let s: GoalieStart = serde_json::from_value(json!({
            "game": "NYR@BOS", "game_time": null, "team": "BOS", "opponent": "NYR", "home": true,
            "goalie_name": "Jeremy Swayman", "dfo_id": 1, "strength": "Confirmed", "source": "Jack Studley",
            "created_at": "2026-09-29T15:02:00Z"}))
        .unwrap();
        assert_eq!(start_source_text(&s), "DFO Confirmed: Jeremy Swayman starts NYR@BOS (Jack Studley, Sep 29 15:02 UTC)");
    }
}
