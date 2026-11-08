//! Player tools: the /players search, the watchlist (`<FM_DATA_DIR>/watchlist.json`) and the
//! /compare page. The route handlers are in app.rs; this module builds their view models.
//!
//! Watchlist file: `{league: {cid: {"name", "added": ISO datetime, "seen": snapshot}}}` where the
//! snapshot is what the player looked like when last marked seen (owner team id or null, status,
//! return date, line, power-play unit). A change is the difference between that snapshot and the
//! current league data. Reads never fail (missing / corrupt file = empty); writes are atomic.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use chrono::NaiveDate;
use fm_core::models::{FantasyTeam, LeagueContext, Player};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::views::{self, LoadResult};

pub const WATCHLIST_FILE: &str = "watchlist.json";
/// rows shown on /players
pub const RESULTS: usize = 50;
/// players on /compare
pub const COMPARE_MAX: usize = 4;
pub const POSITIONS: [&str; 5] = ["C", "LW", "RW", "D", "G"];
/// ownership filter key -> label
pub const OWNERSHIP: [(&str, &str); 4] = [("all", "All"), ("mine", "Mine"), ("fa", "Free agents"), ("others", "Other teams")];
/// sort keys of /players; "dyn" only in dynasty leagues
pub const SORTS: [&str; 5] = ["vorp", "fpg_season", "fpg_week", "pct", "dyn"];
/// rolling window of the /compare chart, in games
pub const ROLLING_GAMES: usize = 5;

// --------------------------------------------------------------------------- watchlist store

/// What a watched player looked like when last marked seen.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// fantasy team id; None = free agent
    pub owner: Option<String>,
    pub status: String,
    #[serde(default)]
    pub return_date: Option<NaiveDate>,
    #[serde(default)]
    pub line: Option<String>,
    #[serde(default)]
    pub pp: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub added: String,
    pub seen: Snapshot,
}

/// One league's watchlist: cid -> entry.
pub type Watchlist = BTreeMap<String, Entry>;
type Store = BTreeMap<String, Watchlist>;

static LOCK: Mutex<()> = Mutex::new(());

pub fn watchlist_path(data_dir: &Path) -> PathBuf {
    data_dir.join(WATCHLIST_FILE)
}

fn load_store(data_dir: &Path) -> Store {
    std::fs::read_to_string(watchlist_path(data_dir)).ok().and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default()
}

/// A league's watchlist (empty when the file is missing or unreadable).
pub fn load(data_dir: &Path, league: &str) -> Watchlist {
    load_store(data_dir).remove(league).unwrap_or_default()
}

/// Change one league's list under the lock and write the file atomically (temp file + rename).
fn update(data_dir: &Path, league: &str, f: impl FnOnce(&mut Watchlist)) -> anyhow::Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load_store(data_dir);
    let list = store.entry(league.to_string()).or_default();
    f(list);
    if list.is_empty() {
        store.remove(league);
    }
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let path = watchlist_path(data_dir);
    let tmp = data_dir.join(format!(".watchlist-{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&store)? + "\n").with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn watch(data_dir: &Path, league: &str, cid: &str, entry: Entry) -> anyhow::Result<()> {
    update(data_dir, league, |l| {
        l.insert(cid.to_string(), entry);
    })
}

pub fn unwatch(data_dir: &Path, league: &str, cid: &str) -> anyhow::Result<()> {
    update(data_dir, league, |l| {
        l.remove(cid);
    })
}

/// Mark every watched player seen as he is now (players missing from `current` keep their snapshot).
pub fn mark_seen(data_dir: &Path, league: &str, current: &HashMap<String, Snapshot>) -> anyhow::Result<()> {
    update(data_dir, league, |l| {
        for (cid, e) in l.iter_mut() {
            if let Some(s) = current.get(cid) {
                e.seen = s.clone();
            }
        }
    })
}

// --------------------------------------------------------------------------- snapshots and changes

/// cid -> the fantasy team that rosters him.
fn owners(ctx: &LeagueContext) -> HashMap<&str, &FantasyTeam> {
    let mut out = HashMap::new();
    for t in &ctx.teams {
        for p in t.players_iter() {
            out.entry(p.cid.as_str()).or_insert(t);
        }
    }
    out
}

fn snapshot_of(res: &LoadResult, owners: &HashMap<&str, &FantasyTeam>, p: &Player) -> Snapshot {
    Snapshot {
        owner: owners.get(p.cid.as_str()).map(|t| t.team_id.clone()),
        status: p.status.as_str().to_string(),
        return_date: res.values.get(&p.cid).and_then(|v| v.return_date),
        line: p.line.clone().filter(|s| !s.is_empty()),
        pp: p.pp_unit.clone().filter(|s| !s.is_empty()),
    }
}

/// The player as a watchlist entry right now (None: not in this league's player pool).
pub fn current_entry(res: &LoadResult, cid: &str, added: &str) -> Option<Entry> {
    let p = res.ctx.all_players().into_iter().find(|p| p.cid == cid)?;
    Some(Entry { name: p.name.clone(), added: added.to_string(), seen: snapshot_of(res, &owners(&res.ctx), p) })
}

/// Current snapshots of every watched player that is in the league data.
pub fn current_snapshots(res: &LoadResult, list: &Watchlist) -> HashMap<String, Snapshot> {
    let own = owners(&res.ctx);
    res.ctx.all_players().into_iter().filter(|p| list.contains_key(&p.cid)).map(|p| (p.cid.clone(), snapshot_of(res, &own, p))).collect()
}

fn status_label(s: &str) -> String {
    match s {
        "healthy" => "Healthy",
        "dtd" => "Day-to-day",
        "out" => "Out",
        "ir" => "IR",
        "ltir" => "LTIR",
        "suspended" => "Suspended",
        "unknown" => "Unknown",
        other => other,
    }
    .to_string()
}

fn line_label(line: &str) -> Option<&'static str> {
    Some(match line.to_lowercase().as_str() {
        "f1" => "the first line",
        "f2" => "the second line",
        "f3" => "the third line",
        "f4" => "the fourth line",
        "d1" => "the top pair",
        "d2" => "the second pair",
        "d3" => "the third pair",
        _ => return None,
    })
}

/// "Nov 12"
pub fn short_date(d: NaiveDate) -> String {
    d.format("%b %-d").to_string()
}

/// One change since the snapshot: `text` stands alone ("Became a free agent"), `phrase` follows
/// the player's name ("became a free agent").
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Change {
    pub text: String,
    pub phrase: String,
}

fn change(text: String, phrase: String) -> Change {
    Change { text, phrase }
}

/// What changed between `seen` and `now`, in plain words. Line and power-play moves count only
/// when the player is now on a unit (a missing line usually means the line source had no data).
pub fn changes(seen: &Snapshot, now: &Snapshot, ctx: &LeagueContext) -> Vec<Change> {
    let mut out = Vec::new();
    if seen.owner != now.owner {
        match now.owner.as_deref() {
            None => out.push(change("Became a free agent".into(), "became a free agent".into())),
            Some(id) => {
                let team = ctx.team(id);
                let name = match team {
                    Some(t) if t.owner_is_me => "your team".to_string(),
                    Some(t) => t.name.clone(),
                    None => format!("team {id}"),
                };
                out.push(change(format!("Now on {name}"), format!("is now on {name}")));
            }
        }
    }
    if seen.status != now.status {
        let (a, b) = (status_label(&seen.status), status_label(&now.status));
        out.push(change(format!("{a} \u{2192} {b}"), format!("went from {a} to {b}")));
    }
    if let Some(d) = now.return_date.filter(|d| seen.return_date != Some(*d)) {
        let when = short_date(d);
        if seen.return_date.is_some() {
            out.push(change(format!("Return date moved to {when}"), format!("return date moved to {when}")));
        } else {
            out.push(change(format!("Return date: {when}"), format!("has a return date: {when}")));
        }
    }
    if let Some(pp) = now.pp.as_deref().filter(|u| seen.pp.as_deref() != Some(*u)) {
        let unit = pp.to_uppercase();
        out.push(change(format!("Moved to {unit}"), format!("moved to {unit}")));
    }
    if let Some(l) = now.line.as_deref().filter(|l| seen.line.as_deref() != Some(*l)).and_then(line_label) {
        let text = format!("Moved to {l}");
        out.push(change(text.clone(), format!("moved to {l}")));
    }
    out
}

/// The watchlist section of /players: one item per watched player, changed ones first.
pub fn watch_rows(res: &LoadResult, list: &Watchlist) -> Vec<Value> {
    let own = owners(&res.ctx);
    let by_cid: HashMap<&str, &Player> = res.ctx.all_players().into_iter().map(|p| (p.cid.as_str(), p)).collect();
    let mut rows: Vec<(bool, String, Value)> = list
        .iter()
        .map(|(cid, e)| {
            let Some(p) = by_cid.get(cid.as_str()) else {
                let v = json!({"cid": cid, "name": e.name, "found": false, "changes": [], "added": e.added});
                return (false, e.name.clone(), v);
            };
            let now = snapshot_of(res, &own, p);
            let ch = changes(&e.seen, &now, &res.ctx);
            let team = own.get(cid.as_str());
            let v = json!({
                "cid": cid, "name": p.name, "found": true, "added": e.added, "pos": views::pos_str(p),
                "team": p.team.clone().unwrap_or_else(|| "FA".into()), "status": p.status.as_str(), "note": p.status_note,
                "owner": team.map(|t| t.name.clone()), "mine": team.is_some_and(|t| t.owner_is_me),
                "return_date": now.return_date.map(short_date), "badges": views::unit_badges(p), "line_change": p.line_change,
                "changes": ch,
            });
            (!ch.is_empty(), p.name.clone(), v)
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    rows.into_iter().map(|r| r.2).collect()
}

/// Unseen watchlist changes for the "This week" strip: `[{cid, name, phrase}]`, one per change.
pub fn watch_changes(res: &LoadResult, data_dir: &Path, league: &str) -> Vec<Value> {
    let list = load(data_dir, league);
    if list.is_empty() {
        return Vec::new();
    }
    let now = current_snapshots(res, &list);
    let mut out = Vec::new();
    for (cid, e) in &list {
        let Some(s) = now.get(cid) else { continue };
        for c in changes(&e.seen, s, &res.ctx) {
            out.push(json!({"cid": cid, "name": e.name, "phrase": c.phrase}));
        }
    }
    out
}

// --------------------------------------------------------------------------- /players

/// The /players query, validated.
#[derive(Clone, Debug, PartialEq)]
pub struct Filters {
    pub q: Option<String>,
    pub pos: Option<String>,
    pub own: String,
    pub sort: String,
}

impl Filters {
    pub fn parse(q: Option<&str>, pos: Option<&str>, own: Option<&str>, sort: Option<&str>, dynasty: bool) -> Filters {
        let q = q.map(|s| s.trim().chars().take(60).collect::<String>()).filter(|s| !s.is_empty());
        let pos = pos.filter(|p| POSITIONS.contains(p)).map(str::to_string);
        let own = own.filter(|o| OWNERSHIP.iter().any(|(k, _)| k == o)).unwrap_or("all").to_string();
        let sort = sort.filter(|s| SORTS.contains(s) && (*s != "dyn" || dynasty)).unwrap_or("vorp").to_string();
        Filters { q, pos, own, sort }
    }

    pub fn active(&self) -> bool {
        self.q.is_some() || self.pos.is_some() || self.own != "all" || self.sort != "vorp"
    }
}

fn pos_ok(p: &Player, pos: Option<&str>) -> bool {
    pos.is_none_or(|pos| p.positions.iter().any(|x| x == pos))
}

fn sort_value(res: &LoadResult, p: &Player, key: &str) -> Option<f64> {
    let pv = res.values.get(&p.cid);
    match key {
        "fpg_season" => pv.map(|v| v.fpg_season),
        "fpg_week" => pv.map(|v| v.fpg_week),
        "pct" => p.pct_owned,
        "dyn" => res.dynasty.as_ref().and_then(|d| d.get(&p.cid)).map(|d| d.value),
        _ => pv.map(|v| v.vorp),
    }
}

/// Every player matching the filters, best first by the sort key (missing values last, then
/// VORP, then name).
pub fn search<'a>(res: &'a LoadResult, f: &Filters) -> Vec<&'a Player> {
    let ctx = &res.ctx;
    let own = owners(ctx);
    let mine = views::my_team(ctx).map(|t| t.team_id.as_str());
    let needle = f.q.as_deref().map(|q| fm_core::normalize::normalize_name(Some(q))).unwrap_or_default();
    let mut out: Vec<&Player> = ctx
        .all_players()
        .into_iter()
        .filter(|p| pos_ok(p, f.pos.as_deref()))
        .filter(|p| {
            let team = own.get(p.cid.as_str()).map(|t| t.team_id.as_str());
            match f.own.as_str() {
                "mine" => team.is_some() && team == mine,
                "fa" => team.is_none(),
                "others" => team.is_some() && team != mine,
                _ => true,
            }
        })
        .filter(|p| needle.is_empty() || fm_core::normalize::normalize_name(Some(&p.name)).contains(&needle))
        .collect();
    let key = |p: &Player| (sort_value(res, p, &f.sort), sort_value(res, p, "vorp"));
    out.sort_by(|a, b| {
        let ((ka, va), (kb, vb)) = (key(a), key(b));
        let desc = |x: Option<f64>, y: Option<f64>| match (x, y) {
            (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        desc(ka, kb).then_with(|| desc(va, vb)).then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// The /players view model: filters, the top results and the watchlist.
pub fn players_page(res: &LoadResult, data_dir: &Path, league: &str, f: &Filters) -> Value {
    let ctx = &res.ctx;
    let own = owners(ctx);
    let list = load(data_dir, league);
    let found = search(res, f);
    let results: Vec<Value> = found
        .iter()
        .take(RESULTS)
        .map(|p| {
            let team = own.get(p.cid.as_str());
            let mut row = views::player_row(res, p, None, team.map(|t| t.name.as_str()));
            if let Value::Object(m) = &mut row {
                m.insert("mine".into(), json!(team.is_some_and(|t| t.owner_is_me)));
                m.insert("watched".into(), json!(list.contains_key(&p.cid)));
            }
            row
        })
        .collect();
    let all = ctx.all_players();
    let present: Vec<&str> = POSITIONS.iter().copied().filter(|x| all.iter().any(|p| p.positions.iter().any(|q| q == x))).collect();
    let dynasty = res.dynasty.is_some();
    let watch = watch_rows(res, &list);
    let unseen = watch.iter().filter(|w| w["changes"].as_array().is_some_and(|c| !c.is_empty())).count();
    json!({
        "filter": {"q": f.q, "pos": f.pos, "own": f.own, "sort": f.sort, "active": f.active()},
        "positions": present,
        "ownership": OWNERSHIP.iter().map(|(k, l)| json!([k, l])).collect::<Vec<_>>(),
        "sorts": SORTS.iter().filter(|s| **s != "dyn" || dynasty).collect::<Vec<_>>(),
        "results": results,
        "total": found.len(),
        "has_dyn": dynasty,
        "has_pct": found.iter().take(RESULTS).any(|p| p.pct_owned.is_some()),
        "watch": watch,
        "watch_unseen": unseen,
    })
}

// --------------------------------------------------------------------------- /compare

/// `/compare?league=..&p=..` for these players.
pub fn compare_href(league: &str, cids: &[String]) -> String {
    let mut q: Vec<(String, String)> = vec![("league".into(), league.into())];
    q.extend(cids.iter().map(|c| ("p".to_string(), c.clone())));
    format!("/compare?{}", crate::app::urlencode(&q))
}

/// The requested players that exist, without repeats, at most [`COMPARE_MAX`].
pub fn valid_cids(res: &LoadResult, cids: &[String]) -> Vec<String> {
    let all: std::collections::HashSet<&str> = res.ctx.all_players().into_iter().map(|p| p.cid.as_str()).collect();
    let mut out: Vec<String> = Vec::new();
    for c in cids {
        if all.contains(c.as_str()) && !out.contains(c) && out.len() < COMPARE_MAX {
            out.push(c.clone());
        }
    }
    out
}

/// The add-a-player search on /compare: Ok(new list) when the name picks one player (a single
/// match, or an exact name), else Err(candidates) (empty: nothing matched).
pub fn resolve_add(res: &LoadResult, league: &str, cids: &[String], q: &str) -> Result<Vec<String>, Vec<Value>> {
    let f = Filters { q: Some(q.to_string()), pos: None, own: "all".into(), sort: "vorp".into() };
    let found: Vec<&Player> = search(res, &f).into_iter().filter(|p| !cids.contains(&p.cid)).collect();
    let needle = fm_core::normalize::normalize_name(Some(q));
    let exact: Vec<&&Player> = found.iter().filter(|p| fm_core::normalize::normalize_name(Some(&p.name)) == needle).collect();
    let pick = match (found.len(), exact.len()) {
        (1, _) => Some(found[0]),
        (_, 1) => Some(*exact[0]),
        _ => None,
    };
    if let Some(p) = pick.filter(|_| cids.len() < COMPARE_MAX) {
        let mut out = cids.to_vec();
        out.push(p.cid.clone());
        return Ok(out);
    }
    let own = owners(&res.ctx);
    Err(found
        .iter()
        .take(8)
        .map(|p| {
            let mut next = cids.to_vec();
            next.push(p.cid.clone());
            json!({"name": p.name, "pos": views::pos_str(p), "team": p.team.clone().unwrap_or_else(|| "FA".into()),
                   "owner": own.get(p.cid.as_str()).map(|t| t.name.clone()), "href": compare_href(league, &next)})
        })
        .collect())
}

/// Fantasy points per game of a stat split under the league's points weights.
fn split_fpg(p: &Player, split: &str, weights: Option<&fm_core::Map<String, f64>>) -> Option<f64> {
    let w = weights.filter(|w| !w.is_empty())?;
    let line = p.lines.get(split).filter(|l| l.gp > 0)?;
    let pg = line.per_game();
    Some(w.iter().map(|(k, w)| w * pg.get(k).copied().unwrap_or(0.0)).sum())
}

/// One metric row of the comparison: a value per player and which ones are best.
fn metric(label: &str, fmt: &str, vals: Vec<Option<f64>>, lower_better: bool) -> Value {
    let known: Vec<f64> = vals.iter().flatten().copied().collect();
    let best = if known.len() < 2 || known.iter().all(|v| *v == known[0]) {
        None
    } else if lower_better {
        known.iter().copied().reduce(f64::min)
    } else {
        known.iter().copied().reduce(f64::max)
    };
    let cells: Vec<Value> = vals.iter().map(|v| json!({"v": v, "best": best.is_some() && *v == best})).collect();
    json!({"label": label, "fmt": fmt, "cells": cells})
}

/// Rolling `ROLLING_GAMES`-game average fantasy points, oldest game first (the average of the
/// games so far while fewer than the window).
pub fn rolling_avg(fpts: &[f64]) -> Vec<f64> {
    (0..fpts.len())
        .map(|i| {
            let w = &fpts[(i + 1).saturating_sub(ROLLING_GAMES)..=i];
            w.iter().sum::<f64>() / w.len() as f64
        })
        .collect()
}

/// The /compare chart: one coloured line per player (`ch-s1`..`ch-s4`, the column order) of his
/// rolling average fantasy points, by game number. None without fantasy points (categories
/// leagues) or with fewer than 2 games for everyone.
pub fn rolling_chart(players: &[(&Player, &[Value])]) -> Option<Value> {
    let series: Vec<(usize, &Player, Vec<f64>)> = players
        .iter()
        .enumerate()
        .map(|(i, (p, log))| (i, *p, rolling_avg(&log.iter().rev().filter_map(|g| g["fpts"].as_f64()).collect::<Vec<_>>())))
        .filter(|(_, _, v)| v.len() >= 2)
        .collect();
    let n = series.iter().map(|s| s.2.len()).max()?;
    let x: Vec<String> = (1..=n).map(|i| format!("Game {i}")).collect();
    let lines: Vec<crate::charts::Line> = series
        .iter()
        .map(|(i, p, v)| {
            let mut vals: Vec<Option<f64>> = v.iter().map(|x| Some(*x)).collect();
            vals.resize(n, None);
            crate::charts::Line::new(&p.cid, &p.name, vals, "main", crate::charts::DASHES[*i % crate::charts::DASHES.len()])
                .with_class(&format!("ch-s{}", i + 1))
        })
        .collect();
    let fmt = |v: f64| fm_pycompat::fmt::fixed(v, 1, false);
    let opts = crate::charts::ChartOpts { y_fmt: &fmt, zero_line: true, height: 160, ..Default::default() };
    let svg = crate::charts::line_chart(
        "cmp-rolling",
        &x,
        &lines,
        &format!("Fantasy points per game, rolling {ROLLING_GAMES}-game average"),
        "One line per player, by game number this season.",
        &opts,
    );
    Some(json!({"svg": svg, "legend": crate::charts::legend(&lines, None)}))
}

/// The /compare view model for already validated `cids`.
pub fn compare_page(res: &LoadResult, league: &str, cids: &[String]) -> Value {
    let ctx = &res.ctx;
    let own = owners(ctx);
    let by_cid: HashMap<&str, &Player> = ctx.all_players().into_iter().map(|p| (p.cid.as_str(), p)).collect();
    let players: Vec<&Player> = cids.iter().filter_map(|c| by_cid.get(c.as_str()).copied()).collect();
    let points = ctx.scoring.kind.as_str() == "points";
    let weights = points.then_some(&ctx.scoring.weights);
    let (start, _, _) = views::week_start(ctx);
    let logs: Vec<Vec<Value>> = players.iter().map(|p| views::game_log(res, p)).collect();
    let rows: Vec<Value> = players.iter().map(|p| views::player_row(res, p, None, None)).collect();
    let cols: Vec<Value> = players
        .iter()
        .zip(&logs)
        .map(|(p, log)| {
            let team = own.get(p.cid.as_str());
            let rest: Vec<String> = cids.iter().filter(|c| **c != p.cid).cloned().collect();
            json!({
                "cid": p.cid, "name": p.name, "pos": views::pos_str(p), "team": p.team.clone().unwrap_or_else(|| "FA".into()),
                "status": p.status.as_str(), "note": p.status_note,
                "owner": team.map(|t| t.name.clone()), "mine": team.is_some_and(|t| t.owner_is_me),
                "return_date": res.values.get(&p.cid).and_then(|v| v.return_date).map(short_date),
                "badges": views::unit_badges(p), "line_change": p.line_change,
                "remove_href": compare_href(league, &rest), "log": log,
            })
        })
        .collect();
    let col = |k: &str| -> Vec<Option<f64>> { rows.iter().map(|r| r[k].as_f64()).collect() };
    let vl = if points { "FPG" } else { "Val/G" };
    let mut metrics = vec![
        metric(&format!("{vl} healthy"), "n1", col("fpg"), false),
        metric(&format!("{vl} season"), "n1", col("fpg_season"), false),
        metric(&format!("{vl} this week"), "n1", col("fpg_week"), false),
        metric("VORP", "n1s", col("vorp"), false),
    ];
    if res.dynasty.is_some() {
        metrics.push(metric("Dynasty value", "n1", col("dyn"), false));
    }
    metrics.push(metric("Games next 7", "int", col("games_n"), false));
    let g14: Vec<Option<f64>> = players.iter().map(|p| Some(views::team_games(ctx, p.team.as_deref(), start, 14).len() as f64)).collect();
    metrics.push(metric("Games next 14", "int", g14, false));
    if points {
        let actual = |split: &str| -> Vec<Option<f64>> { players.iter().map(|p| split_fpg(p, split, weights)).collect() };
        metrics.push(metric("Actual FPG, season", "n1", actual("season"), false));
        metrics.push(metric("Actual FPG, last 30 days", "n1", actual("last30"), false));
    }
    if players.iter().any(|p| p.pct_owned.is_some()) {
        metrics.push(metric("% rostered", "pct", players.iter().map(|p| p.pct_owned).collect(), false));
    }
    let mut rate_keys: Vec<&str> = Vec::new();
    for k in views::SKATER_RATES.iter().chain(views::GOALIE_RATES.iter()) {
        if rows.iter().any(|r| r["rates"][*k].is_number()) && !rate_keys.contains(k) {
            rate_keys.push(k);
        }
    }
    let rates: Vec<Value> = rate_keys
        .iter()
        .map(|k| {
            let label = match *k {
                "SVPCT" => "SV%".to_string(),
                "GAA" => "GAA".to_string(),
                k => format!("{k}/GP"),
            };
            let vals = rows.iter().map(|r| r["rates"][*k].as_f64()).collect();
            let mut m = metric(&label, &format!("rate:{k}"), vals, *k == "GAA");
            m["stat"] = json!(k);
            m
        })
        .collect();
    let pairs: Vec<(&Player, &[Value])> = players.iter().copied().zip(logs.iter().map(|l| l.as_slice())).collect();
    json!({
        "cols": cols,
        "metrics": metrics,
        "rates": rates,
        "chart": rolling_chart(&pairs),
        "points": points,
        "full": players.len() >= COMPARE_MAX,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> LeagueContext {
        serde_json::from_value(json!({
            "provider": "espn", "league_id": "1", "name": "L", "season": 2027,
            "scoring": {"kind": "points", "weights": {"G": 3.0}}, "roster_shape": {"C": 1},
            "teams": [{"team_id": "1", "name": "Mine", "owner_is_me": true, "slots": []},
                      {"team_id": "2", "name": "Rivals", "owner_is_me": false, "slots": []}],
            "free_agents": [], "as_of": "2026-11-04"
        }))
        .unwrap()
    }

    fn snap(owner: Option<&str>, status: &str) -> Snapshot {
        Snapshot { owner: owner.map(str::to_string), status: status.into(), ..Snapshot::default() }
    }

    fn texts(c: Vec<Change>) -> Vec<String> {
        c.into_iter().map(|c| c.text).collect()
    }

    #[test]
    fn plain_change_wording() {
        let c = ctx();
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        assert!(changes(&snap(Some("2"), "healthy"), &snap(Some("2"), "healthy"), &c).is_empty());
        assert_eq!(texts(changes(&snap(Some("2"), "healthy"), &snap(None, "healthy"), &c)), ["Became a free agent"]);
        assert_eq!(texts(changes(&snap(None, "healthy"), &snap(Some("2"), "healthy"), &c)), ["Now on Rivals"]);
        assert_eq!(texts(changes(&snap(None, "healthy"), &snap(Some("1"), "healthy"), &c)), ["Now on your team"]);
        let st = changes(&snap(None, "out"), &snap(None, "healthy"), &c);
        assert_eq!(st[0].text, "Out \u{2192} Healthy");
        assert_eq!(st[0].phrase, "went from Out to Healthy");
        let (mut a, mut b) = (snap(None, "out"), snap(None, "out"));
        a.return_date = Some(d(11, 2));
        b.return_date = Some(d(11, 12));
        let r = changes(&a, &b, &c);
        assert_eq!((r[0].text.as_str(), r[0].phrase.as_str()), ("Return date moved to Nov 12", "return date moved to Nov 12"));
        a.return_date = None;
        assert_eq!(texts(changes(&a, &b, &c)), ["Return date: Nov 12"]);
        let (mut a, mut b) = (snap(None, "healthy"), snap(None, "healthy"));
        a.pp = Some("pp2".into());
        a.line = Some("f2".into());
        b.pp = Some("pp1".into());
        b.line = Some("f1".into());
        assert_eq!(texts(changes(&a, &b, &c)), ["Moved to PP1", "Moved to the first line"]);
        // losing the unit data is not reported (usually a line-source outage)
        assert!(changes(&b, &snap(None, "healthy"), &c).is_empty());
    }

    #[test]
    fn store_round_trip_and_atomic_file() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert!(load(d, "espn").is_empty());
        let e = Entry { name: "Patrick Kane".into(), added: "2026-11-04T10:00:00".into(), seen: snap(Some("2"), "out") };
        watch(d, "espn", "espn:1", e.clone()).unwrap();
        watch(d, "fantrax", "ftx:9", e.clone()).unwrap();
        assert_eq!(load(d, "espn").get("espn:1"), Some(&e));
        let mut now = HashMap::new();
        now.insert("espn:1".to_string(), snap(None, "healthy"));
        mark_seen(d, "espn", &now).unwrap();
        assert_eq!(load(d, "espn")["espn:1"].seen, snap(None, "healthy"));
        assert_eq!(load(d, "fantrax")["ftx:9"].seen, snap(Some("2"), "out"), "other leagues untouched");
        unwatch(d, "espn", "espn:1").unwrap();
        assert!(load(d, "espn").is_empty());
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(watchlist_path(d)).unwrap()).unwrap();
        assert!(raw.get("espn").is_none() && raw["fantrax"]["ftx:9"]["seen"]["owner"] == "2");
        let leftovers: Vec<_> = std::fs::read_dir(d).unwrap().filter(|e| e.as_ref().unwrap().file_name() != WATCHLIST_FILE).collect();
        assert!(leftovers.is_empty(), "no temp files left behind");
        std::fs::write(watchlist_path(d), "not json").unwrap();
        assert!(load(d, "fantrax").is_empty(), "a corrupt file reads as empty");
    }

    #[test]
    fn rolling_average_and_best_marks() {
        assert_eq!(rolling_avg(&[1.0, 3.0]), [1.0, 2.0]);
        let r = rolling_avg(&[5.0, 0.0, 0.0, 0.0, 0.0, 10.0]);
        assert_eq!(r[4], 1.0);
        assert_eq!(r[5], 2.0, "the window drops the oldest game");
        let m = metric("x", "n1", vec![Some(1.0), None, Some(3.0)], false);
        assert_eq!(m["cells"][2]["best"], true);
        assert_eq!(m["cells"][0]["best"], false);
        let gaa = metric("GAA", "rate:GAA", vec![Some(2.1), Some(3.0)], true);
        assert_eq!(gaa["cells"][0]["best"], true);
        let tie = metric("x", "n1", vec![Some(1.0), Some(1.0)], false);
        assert_eq!(tie["cells"][0]["best"], false, "no winner on a tie");
    }
}
