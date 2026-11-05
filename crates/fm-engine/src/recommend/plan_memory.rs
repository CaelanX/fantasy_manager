//! Rust port of `fantasy_manager/recommend/plan_memory.py`: what changed since the last weekly
//! plan the user saw (stability of earlier steps, "Since your last plan", "Did it" checks).
//! Owner: work package `W8-plan-claims-advise`.
//!
//! Snapshots are kept as `serde_json::Value` in the same `<data_dir>/plans/<league>-<start>.json`
//! files Python writes (`json.dumps(snaps, indent=1)`, CRLF line ends on Windows like Python's
//! text mode); `_signature` is byte-identical (`fm_pycompat::pyjson`). The clock is a parameter.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{Duration, NaiveDate};
use fm_core::models::{LeagueContext, Player};
use fm_core::values::PlayerValue;
use fm_core::{Cid, Map, PyDateTime};
use fm_pycompat::fmt::fixed;
use fm_pycompat::pyjson::{dumps, DumpOpts};
use fm_pycompat::time::strftime_date;
use serde_json::{json, Value};

use crate::recommend::base::{open_roster_spots, shares_default_slot, SIZE_EXEMPT_SLOTS};
use crate::recommend::lineup::unavailable;
use crate::recommend::plan::{
    clock, earliest, move_day, slot_label, to_local, utc_seconds, Pair, PlanMove, PlanSince, Timeline, Tx, WeekPlan,
};

/// matchup points a new plan must add over the kept steps to replace them
pub const STABLE_PTS: f64 = 1.5;
/// ... or this share of the kept steps' gain, whichever is larger
pub const STABLE_PCT: f64 = 0.20;
/// snapshots kept per matchup period
pub const SNAPSHOT_LIMIT: usize = 30;
/// periods older than this are deleted
pub const KEEP_DAYS: i64 = 56;
/// % rostered gained this week that makes "he may not last" worth saying
pub const RISER_HINT: f64 = 3.0;

/// (add, drop, to_ir) cids of a plan move, a move in progress or a snapshot step.
pub type MoveKey = (Option<String>, Option<String>, Option<String>);

pub fn move_key_move(m: &PlanMove) -> MoveKey {
    (Some(m.add.cid.clone()), m.drop.as_ref().map(|p| p.cid.clone()), m.to_ir.as_ref().map(|p| p.cid.clone()))
}

fn vcid(v: Option<&Value>) -> Option<String> {
    v.and_then(|x| x.get("cid")).and_then(|c| c.as_str()).map(|s| s.to_string())
}

/// `move_key` of a snapshot step (a dict).
pub fn move_key_value(s: &Value) -> MoveKey {
    (vcid(s.get("add")), vcid(s.get("drop")), vcid(s.get("to_ir")))
}

pub(crate) fn move_key_tx(tl: &Timeline<'_>, t: &Tx<'_>) -> MoveKey {
    (
        Some(tl.p(t.add).cid.clone()),
        t.drop.map(|i| tl.p(i).cid.clone()),
        t.to_ir.map(|i| tl.p(i).cid.clone()),
    )
}

fn key_json(k: &MoveKey) -> String {
    let f = |x: &Option<String>| x.as_ref().map_or(Value::Null, |s| Value::String(s.clone()));
    dumps(&json!([f(&k.0), f(&k.1), f(&k.2)]), &DumpOpts::default())
}

// ------------------------------------------------------------------------------------ storage

fn path(data_dir: &Path, league: &str, start: NaiveDate) -> PathBuf {
    data_dir.join("plans").join(format!("{league}-{}.json", start.format("%Y-%m-%d")))
}

fn period_start(plan: &WeekPlan, ctx: &LeagueContext) -> Option<NaiveDate> {
    plan.start.or(ctx.period_start)
}

fn pref(p: Option<&Player>) -> Value {
    match p {
        None => Value::Null,
        Some(p) => json!({"cid": p.cid, "name": p.name, "positions": p.positions}),
    }
}

fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn my_roster(ctx: &LeagueContext) -> Map<String, Value> {
    let mut out = Map::new();
    if let Ok(t) = ctx.my_team() {
        for s in &t.slots {
            if let Some(p) = &s.player {
                out.insert(p.cid.clone(), json!({"name": p.name, "ir": s.slot == "IR" || s.slot == "IR+"}));
            }
        }
    }
    out
}

/// The snapshot of `plan` saved to the plans file.
pub fn snapshot(plan: &WeekPlan, ctx: &LeagueContext, now: &PyDateTime) -> Value {
    let roster = my_roster(ctx);
    let steps: Vec<Value> = plan
        .moves
        .iter()
        .map(|m| {
            json!({"day": iso(m.day), "effective": iso(m.effective), "add": pref(Some(&m.add)),
                   "drop": pref(m.drop.as_ref()), "to_ir": pref(m.to_ir.as_ref()), "gain_pts": m.gain_pts,
                   "waiver": m.add.waiver_until.map(iso)})
        })
        .collect();
    let sig = signature(&steps, &roster);
    json!({"created": utc_seconds(now), "as_of": iso(ctx.as_of), "moves_left": plan.moves_left, "moves": steps,
           "today": plan.today.iter().map(|a| a.title.clone()).collect::<Vec<_>>(), "roster": roster,
           "signature": sig})
}

fn signature_of(keys: &[MoveKey], roster: &Map<String, Value>) -> String {
    let mut ks: Vec<String> = keys.iter().map(key_json).collect();
    ks.sort();
    let mut pairs: Vec<(String, bool)> = roster.iter().map(|(c, v)| (c.clone(), truthy(v.get("ir")))).collect();
    pairs.sort();
    let v = json!([ks, pairs.iter().map(|(c, b)| json!([c, b])).collect::<Vec<_>>()]);
    dumps(&v, &DumpOpts::default())
}

/// `_signature(steps, roster)`.
pub(crate) fn signature(steps: &[Value], roster: &Map<String, Value>) -> String {
    let keys: Vec<MoveKey> = steps.iter().map(move_key_value).collect();
    signature_of(&keys, roster)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn read_list(path: &Path) -> Option<Vec<Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text).ok()? {
        Value::Array(a) => Some(a),
        _ => None,
    }
}

/// Earlier snapshots for this matchup period, oldest first ([] when none or unreadable). Seeds
/// from that period's newest plan archive when no snapshot exists yet.
pub fn load_snapshots(data_dir: &Path, plan: &WeekPlan, ctx: &LeagueContext) -> Vec<Value> {
    let Some(start) = period_start(plan, ctx) else { return vec![] };
    if let Some(snaps) = read_list(&path(data_dir, &plan.league, start)) {
        return snaps;
    }
    from_archive(data_dir, &plan.league, start, plan.end)
}

/// The last 10 characters of a file stem (`f.stem[-10:]`).
fn stem_tail(p: &Path) -> String {
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let chars: Vec<char> = stem.chars().collect();
    chars[chars.len().saturating_sub(10)..].iter().collect()
}

/// Files `<dir>/<prefix>*<suffix>` sorted by name.
fn glob(dir: &Path, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with(prefix) && n.ends_with(suffix) && n.len() >= prefix.len() + suffix.len()
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

fn from_archive(data_dir: &Path, league: &str, start: NaiveDate, end: Option<NaiveDate>) -> Vec<Value> {
    let Ok(dir) = crate::archive::archive_dir(data_dir) else { return vec![] };
    let files = glob(&dir, &format!("plan-{league}-"), ".json");
    for f in files.iter().rev() {
        let Some(day) = fm_pycompat::time::date_fromisoformat(&stem_tail(f)) else { continue };
        if day < start || end.is_some_and(|e| day > e) {
            continue;
        }
        let Some(body) = std::fs::read_to_string(f).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
            continue;
        };
        let p = body.get("plan").filter(|v| truthy(Some(v))).cloned().unwrap_or_else(|| json!({}));
        let empty = vec![];
        let moves = p.get("moves").and_then(|m| m.as_array()).unwrap_or(&empty);
        let steps: Vec<Value> = moves
            .iter()
            .map(|m| {
                json!({"day": m.get("day").cloned().unwrap_or(Value::Null),
                       "effective": m.get("effective").cloned().unwrap_or(Value::Null),
                       "add": strip(m.get("add")), "drop": strip(m.get("drop")), "to_ir": strip(m.get("to_ir")),
                       "gain_pts": m.get("gain_pts").cloned().unwrap_or(Value::Null),
                       "waiver": m.get("add").and_then(|a| a.get("waiver_until")).cloned().unwrap_or(Value::Null)})
            })
            .collect();
        let today: Vec<Value> = p
            .get("today")
            .and_then(|t| t.as_array())
            .map(|a| a.iter().map(|x| x.get("title").cloned().unwrap_or(Value::Null)).collect())
            .unwrap_or_default();
        let sig = signature(&steps, &Map::new());
        return vec![json!({"created": body.get("created").cloned().unwrap_or(Value::Null),
                           "as_of": body.get("as_of").cloned().unwrap_or(Value::Null),
                           "moves_left": p.get("moves_left").cloned().unwrap_or(Value::Null),
                           "moves": steps, "today": today, "roster": Value::Null, "signature": sig})];
    }
    vec![]
}

fn strip(p: Option<&Value>) -> Value {
    match p {
        Some(Value::Object(o)) => {
            let positions = o.get("positions").filter(|v| truthy(Some(v))).cloned().unwrap_or_else(|| json!([]));
            json!({"cid": o.get("cid").cloned().unwrap_or(Value::Null), "name": o.get("name").cloned().unwrap_or(Value::Null),
                   "positions": positions})
        }
        _ => Value::Null,
    }
}

/// Python text-mode writes on Windows end lines with CRLF.
fn text_mode(s: String) -> String {
    if cfg!(windows) {
        s.replace('\n', "\r\n")
    } else {
        s
    }
}

/// Append this plan when its steps or my roster differ from the last snapshot (best effort).
pub fn save_snapshot(data_dir: &Path, plan: &WeekPlan, ctx: &LeagueContext, now: &PyDateTime) {
    let Some(start) = period_start(plan, ctx) else { return };
    if !plan.available {
        return;
    }
    let p = path(data_dir, &plan.league, start);
    let mut snaps = read_list(&p).unwrap_or_default();
    let snap = snapshot(plan, ctx, now);
    if snaps.last().is_some_and(|s| s.get("signature") == snap.get("signature")) {
        return;
    }
    snaps.push(snap);
    if snaps.len() > SNAPSHOT_LIMIT {
        snaps.drain(..snaps.len() - SNAPSHOT_LIMIT);
    }
    let Some(parent) = p.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = p.with_extension("json.tmp");
    let text = text_mode(dumps(&Value::Array(snaps), &DumpOpts { indent: Some(1), ..DumpOpts::default() }));
    if std::fs::write(&tmp, text).is_err() || std::fs::rename(&tmp, &p).is_err() {
        return;
    }
    prune(parent, ctx.as_of);
}

fn prune(folder: &Path, today: NaiveDate) {
    for f in glob(folder, "", ".json") {
        let Some(start) = fm_pycompat::time::date_fromisoformat(&stem_tail(&f)) else { continue };
        if start < today - Duration::days(KEEP_DAYS) {
            let _ = std::fs::remove_file(&f);
        }
    }
}

// ---------------------------------------------------------------------------------- stability

fn done(step: &Value, mine: &HashSet<String>) -> bool {
    let (add, drop, _) = move_key_value(step);
    add.is_some_and(|a| mine.contains(&a)) && drop.map_or(true, |d| !mine.contains(&d))
}

fn desc_tx(tl: &Timeline<'_>, t: &Tx<'_>) -> String {
    let mut s = format!("add {}", tl.p(t.add).name);
    if let Some(d) = t.drop {
        s += &format!(", drop {}", tl.p(d).name);
    }
    s
}

fn my_cids(ctx: &LeagueContext) -> HashSet<String> {
    ctx.my_team().map(|t| t.players_iter().map(|p| p.cid.clone()).collect()).unwrap_or_default()
}

/// (moves to plan, {kept step key: why}) - the module docstring's stability rule.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stabilize<'r>(
    ctx: &LeagueContext,
    tl: &Timeline<'_>,
    txs: Vec<Tx<'r>>,
    prev: &Value,
    pairs: &[Pair<'r>],
    max_moves: i64,
    min_pts: f64,
    now: &PyDateTime,
) -> (Vec<Tx<'r>>, Map<MoveKey, String>) {
    let mine = my_cids(ctx);
    let fa: HashMap<&str, &Player> = ctx.free_agents.iter().map(|p| (p.cid.as_str(), p)).collect();
    let active: HashSet<usize> = tl.base.iter().copied().collect();
    let mut rec_for: HashMap<MoveKey, &'r fm_core::models::Recommendation> = HashMap::new();
    for pr in pairs {
        let k = (Some(tl.p(pr.add).cid.clone()), pr.drop.map(|i| tl.p(i).cid.clone()), pr.to_ir.map(|i| tl.p(i).cid.clone()));
        rec_for.insert(k, pr.rec);
    }
    let cur: HashSet<MoveKey> = txs.iter().map(|t| move_key_tx(tl, t)).collect();
    let mut forced: Vec<Tx<'r>> = Vec::new();
    let empty = vec![];
    for step in prev.get("moves").and_then(|m| m.as_array()).unwrap_or(&empty) {
        if forced.len() as i64 >= max_moves || done(step, &mine) {
            continue;
        }
        let (add_id, drop_id, ir_id) = move_key_value(step);
        let Some(add) = add_id.as_deref().and_then(|a| fa.get(a).copied()) else { continue };
        if unavailable(add.status) || ir_id.is_some() {
            continue;
        }
        let Some(add_i) = tl.id(&add.cid) else { continue };
        let drop: Option<usize> = match &drop_id {
            None => None,
            Some(d) => {
                let i = tl.id(d).filter(|i| active.contains(i) || forced.iter().any(|f| f.add == *i));
                match i {
                    Some(i) => Some(i),
                    None => continue,
                }
            }
        };
        let Some(hit) = tl.best_day(&forced, add_i, drop, None, earliest(ctx, add, now)) else { continue };
        if hit.0 < min_pts {
            continue;
        }
        forced.push(Tx {
            day: move_day(ctx, add, hit.1),
            effective: hit.1,
            add: add_i,
            drop,
            to_ir: None,
            rec: rec_for.get(&(add_id.clone(), drop_id.clone(), None)).copied(),
        });
    }
    if forced.is_empty() || forced.iter().all(|f| cur.contains(&move_key_tx(tl, f))) {
        return (txs, Map::new());
    }
    for t in &txs {
        // top up with the new plan's other moves
        if forced.len() as i64 >= max_moves {
            break;
        }
        let adds: HashSet<usize> = forced.iter().map(|f| f.add).collect();
        let gone: HashSet<usize> = forced.iter().flat_map(|f| [f.drop, f.to_ir]).flatten().collect();
        if adds.contains(&t.add) || [t.drop, t.to_ir].into_iter().flatten().any(|x| gone.contains(&x) || adds.contains(&x)) {
            continue;
        }
        let add = tl.p(t.add);
        if let Some(hit) = tl.best_day(&forced, t.add, t.drop, t.to_ir, earliest(ctx, add, now)) {
            if hit.0 >= min_pts {
                forced.push(Tx { day: move_day(ctx, add, hit.1), effective: hit.1, add: t.add, drop: t.drop, to_ir: t.to_ir, rec: t.rec });
            }
        }
    }
    let base = tl.total(&[]);
    let (new_gain, old_gain) = (tl.total(&txs) - base, tl.total(&forced) - base);
    let bar = STABLE_PTS.max(STABLE_PCT * old_gain.max(0.0));
    if new_gain - old_gain >= bar {
        return (txs, Map::new());
    }
    let mut kept: Map<MoveKey, String> = Map::new();
    for f in &forced {
        let k = move_key_tx(tl, f);
        if !cur.contains(&k) {
            kept.insert(
                k,
                format!(
                    "Kept {} from your earlier plan: changing it would add only {} pts (under the {}-pt bar for changing a plan)",
                    desc_tx(tl, f),
                    fixed(new_gain - old_gain, 1, true),
                    fixed(bar, 1, false)
                ),
            );
        }
    }
    forced.sort_by(|a, b| (a.day, a.effective).cmp(&(b.day, b.effective)));
    (forced, kept)
}

// ------------------------------------------------------------------------ since your last plan

fn name_of(v: Option<&Value>) -> Option<String> {
    v.filter(|x| truthy(Some(x))).and_then(|x| x.get("name")).and_then(|n| n.as_str()).map(|s| s.to_string())
}

fn desc(step: &Value) -> String {
    let add = name_of(step.get("add")).unwrap_or_else(|| "?".to_string());
    match (name_of(step.get("drop")), name_of(step.get("to_ir"))) {
        (Some(d), _) if !d.is_empty() => format!("{add} for {d}"),
        (_, Some(i)) if !i.is_empty() => format!("{add} ({i} to IR)"),
        _ => add,
    }
}

fn wd_iso(iso: Option<&Value>) -> String {
    iso.and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .and_then(fm_pycompat::time::date_fromisoformat)
        .map(|d| strftime_date(d, "%a"))
        .unwrap_or_default()
}

/// `_when(created)`: "Mon 2:43 PM" in the display zone.
fn when(created: Option<&Value>) -> String {
    let Some(t) = created.and_then(|v| v.as_str()).filter(|s| !s.is_empty()).and_then(PyDateTime::parse) else {
        return String::new();
    };
    let loc = to_local(&t);
    format!("{} {}", loc.format("%a"), clock(&t))
}

fn value_of(values: &Map<Cid, PlayerValue>, dyn_: Option<&Map<Cid, f64>>, p: &Player) -> f64 {
    match dyn_ {
        Some(d) => d.get(&p.cid).copied().unwrap_or(0.0),
        None => values.get(&p.cid).map_or(0.0, |pv| pv.fpg_season),
    }
}

/// The "Since your last plan" block (module docstring), or None without an earlier plan.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compare(
    plan: &mut WeekPlan,
    ctx: &LeagueContext,
    snaps: &[Value],
    kept: &Map<MoveKey, String>,
    tl: &Timeline<'_>,
    txs: &[Tx<'_>],
    values: &Map<Cid, PlayerValue>,
    dyn_: Option<&Map<Cid, f64>>,
    now: &PyDateTime,
) -> Option<PlanSince> {
    let cur_keys_list: Vec<MoveKey> = plan.moves.iter().map(move_key_move).collect();
    let cur_roster = my_roster(ctx);
    let empty_roster: Map<String, Value> = Map::new();
    let same = |s: &Value| {
        let r = if s.get("roster").is_some_and(|r| !r.is_null()) { &cur_roster } else { &empty_roster };
        s.get("signature").and_then(|x| x.as_str()) == Some(signature_of(&cur_keys_list, r).as_str())
    };
    let Some(base) = snaps.iter().rev().find(|s| !same(s)) else {
        // the plan is unchanged; say so when stability is what kept it
        if !kept.is_empty() && !snaps.is_empty() {
            return Some(PlanSince {
                at: when(snaps[snaps.len() - 1].get("created")),
                kept: kept.values().cloned().collect(),
                ..PlanSince::default()
            });
        }
        return None;
    };
    let mut since = PlanSince { at: when(base.get("created")), ..PlanSince::default() };
    let mine: HashSet<String> = cur_roster.keys().cloned().collect();
    let fa: HashMap<&str, &Player> = ctx.free_agents.iter().map(|p| (p.cid.as_str(), p)).collect();
    let me = ctx.my_team_index();
    let mut owner: HashMap<&str, &str> = HashMap::new();
    for (ti, t) in ctx.teams.iter().enumerate() {
        if Some(ti) == me {
            continue;
        }
        for p in t.players_iter() {
            owner.insert(p.cid.as_str(), t.name.as_str());
        }
    }
    let prev_roster = base.get("roster").and_then(|r| r.as_object());
    let cur_keys: HashSet<MoveKey> = cur_keys_list.iter().cloned().collect();
    let empty = vec![];
    let prev_steps: &Vec<Value> = base.get("moves").and_then(|m| m.as_array()).unwrap_or(&empty);
    let prev_keys: HashSet<MoveKey> = prev_steps.iter().map(move_key_value).collect();

    // done steps and roster changes
    let mut planned_adds: HashSet<String> = HashSet::new();
    for s in prev_steps {
        if done(s, &mine) {
            let add = name_of(s.get("add")).unwrap_or_else(|| "None".to_string());
            let mut line = format!("Done: added {add}");
            if let Some(d) = name_of(s.get("drop")).filter(|d| !d.is_empty()) {
                line += &format!(", dropped {d}");
            }
            if truthy(s.get("day")) {
                line += &format!(" ({})", wd_iso(s.get("day")));
            }
            since.done.push(line);
            let k = move_key_value(s);
            if let Some(a) = k.0 {
                planned_adds.insert(a);
            }
            if let Some(d) = k.1 {
                planned_adds.insert(d);
            }
        }
    }
    let mut ir_freed: HashSet<String> = HashSet::new();
    if let Some(prev_roster) = prev_roster {
        let ir = |v: &Value| truthy(v.get("ir"));
        let out_ir: Vec<&String> =
            cur_roster.iter().filter(|(c, v)| !ir(v) && prev_roster.get(c.as_str()).is_some_and(|p| ir(p))).map(|(c, _)| c).collect();
        let in_ir: Vec<&String> = cur_roster
            .iter()
            .filter(|(c, v)| ir(v) && prev_roster.get(c.as_str()).is_some_and(|p| !ir(p)))
            .map(|(c, _)| c)
            .collect();
        ir_freed = out_ir.iter().map(|c| c.to_string()).collect();
        let cname = |c: &String| cur_roster.get(c).and_then(|v| v.get("name")).and_then(|n| n.as_str()).unwrap_or("").to_string();
        if !out_ir.is_empty() || !in_ir.is_empty() {
            let names: Vec<String> = out_ir.iter().chain(in_ir.iter()).map(|c| cname(c)).collect();
            let today: Vec<&str> =
                base.get("today").and_then(|t| t.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
            let planned = today.iter().any(|t| names.iter().any(|n| t.contains(n.as_str())));
            let mut parts: Vec<String> = Vec::new();
            if !out_ir.is_empty() {
                parts.push(format!("activated {}", out_ir.iter().map(|c| cname(c)).collect::<Vec<_>>().join(", ")));
            }
            if !in_ir.is_empty() {
                parts.push(format!("moved {} to IR", in_ir.iter().map(|c| cname(c)).collect::<Vec<_>>().join(", ")));
            }
            let text = format!("{}{}", if planned { "Done: IR swap, " } else { "IR change: " }, parts.join(" and "));
            if planned {
                since.done.push(text);
            } else {
                since.roster.push(text);
            }
        }
        let added: Vec<String> = cur_roster
            .iter()
            .filter(|(c, _)| !prev_roster.contains_key(c.as_str()) && !planned_adds.contains(*c))
            .map(|(c, _)| cname(c))
            .collect();
        let dropped: Vec<String> = prev_roster
            .iter()
            .filter(|(c, _)| !cur_roster.contains_key(c.as_str()) && !planned_adds.contains(*c))
            .map(|(_, v)| v.get("name").and_then(|n| n.as_str()).unwrap_or("None").to_string())
            .collect();
        if !added.is_empty() || !dropped.is_empty() {
            let mut parts = Vec::new();
            if !added.is_empty() {
                parts.push(format!("added {}", added.join(", ")));
            }
            if !dropped.is_empty() {
                parts.push(format!("dropped {}", dropped.join(", ")));
            }
            since.roster.push(format!("Roster change outside the plan: {}", parts.join("; ")));
        }
    }

    // new steps
    let new_moves: Vec<usize> = (0..plan.moves.len())
        .filter(|&i| !prev_keys.contains(&move_key_move(&plan.moves[i])) && plan.moves[i].status != "kept")
        .collect();
    for &i in &new_moves {
        let m = &mut plan.moves[i];
        m.status = "new".to_string();
        let drop = m.drop.as_ref().map_or(String::new(), |d| format!(", drop {}", d.name));
        since.new.push(format!(
            "New: add {}{drop} ({}, +{} pts)",
            m.add.name,
            strftime_date(m.day, "%a"),
            fixed(m.gain_pts, 1, false)
        ));
    }
    since.kept.extend(kept.values().cloned());

    // removed steps, each with its reason
    let pending: Vec<&Value> = prev_steps.iter().filter(|s| !done(s, &mine) && !cur_keys.contains(&move_key_value(s))).collect();
    let mut used: HashSet<String> = HashSet::new();
    let active: HashMap<&str, usize> = tl.base.iter().map(|&i| (tl.p(i).cid.as_str(), i)).collect();
    let binding = plan.moves_left.is_some_and(|l| l > 0 && plan.moves.len() as i64 >= l);
    let reset = plan.end.map(|e| e + Duration::days(1));
    let not_done = prev_steps.iter().filter(|x| !done(x, &mine)).count() as i64;
    for s in pending {
        let (add_id, drop_id, _) = move_key_value(s);
        let name = s.get("add").and_then(|a| a.get("name")).and_then(|n| n.as_str()).unwrap_or("?").to_string();
        let old = s.get("gain_pts").and_then(|g| g.as_f64()).unwrap_or(0.0);
        let add: Option<&Player> = add_id.as_deref().and_then(|a| fa.get(a).copied());
        let head = format!("{} (+{}) dropped from the plan: ", desc(s), fixed(old, 1, false));
        let why: String = match add {
            None => match add_id.as_deref().and_then(|a| owner.get(a)) {
                Some(o) => format!("{name} was picked up by {o}"),
                None => format!("{name} is no longer available"),
            },
            Some(add) if unavailable(add.status) => {
                let note = add.status_note.as_deref().filter(|n| !n.is_empty()).map_or(String::new(), |n| format!(" ({n})"));
                format!("{name} is now {}{note}", add.status.as_str())
            }
            Some(add) if add.waiver_until.is_some_and(|w| w >= ctx.as_of) && !truthy(s.get("waiver")) => format!(
                "{name} went on waivers (clears {}), so he can't count when planned",
                strftime_date(add.waiver_until.unwrap_or(ctx.as_of), "%a")
            ),
            Some(_) if drop_id.as_ref().is_some_and(|d| !mine.contains(d)) => {
                format!("you no longer have {}", s.get("drop").and_then(|d| d.get("name")).and_then(|n| n.as_str()).unwrap_or("?"))
            }
            Some(_) if plan.moves_left.is_some_and(|l| l < not_done) && new_moves.is_empty() => {
                let l = plan.moves_left.unwrap_or(0);
                format!("only {l} move{} left", if l != 1 { "s" } else { "" })
            }
            Some(add) => {
                let drop: Option<usize> = drop_id.as_deref().and_then(|d| active.get(d).copied());
                let mut now_gain: Option<f64> = None;
                if drop_id.is_none() || drop.is_some() {
                    let hit = tl.id(&add.cid).and_then(|ai| tl.best_day(txs, ai, drop, None, earliest(ctx, add, now)));
                    now_gain = Some(hit.map_or(0.0, |h| h.0));
                }
                let mut repl = new_moves
                    .iter()
                    .copied()
                    .find(|&i| !used.contains(&plan.moves[i].add.cid) && shares_default_slot(&plan.moves[i].add, add));
                if repl.is_none() {
                    let mut by_gain = new_moves.clone();
                    by_gain.sort_by(|a, b| fm_pycompat::order::cmp_f64(-plan.moves[*a].gain_pts, -plan.moves[*b].gain_pts));
                    repl = by_gain.into_iter().find(|&i| !used.contains(&plan.moves[i].add.cid));
                }
                if let Some(ri) = repl {
                    let r = &plan.moves[ri];
                    used.insert(r.add.cid.clone());
                    let mut why = format!(
                        "replaced by {}{} (+{})",
                        r.add.name,
                        r.drop.as_ref().map_or(String::new(), |d| format!(" for {}", d.name)),
                        fixed(r.gain_pts, 1, false)
                    );
                    if let Some(d) = r.drop.as_ref().filter(|d| ir_freed.contains(&d.cid)) {
                        why += &format!("; {} became droppable after your IR swap", d.name);
                    }
                    if binding {
                        let best = plan
                            .moves
                            .iter()
                            .map(|m| format!("{} (+{})", m.add.name, fixed(m.gain_pts, 1, false)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let n = plan.moves_left.unwrap_or(0);
                        why += &format!(
                            "; with {n} move{} left, the best {} {best}",
                            if n != 1 { "s" } else { "" },
                            if n == 1 { "one is".to_string() } else { format!("{n} are") }
                        );
                    }
                    why
                } else if let Some(g) = now_gain {
                    format!("it's now worth {} pts, under the bar for a move", fixed(g, 1, true))
                } else {
                    "the plan found better uses for your moves".to_string()
                }
            }
        };
        since.changed.push(head + &why);
        // a better long-term asset than his drop: worth a move when they reset
        let drop_p: Option<&Player> = drop_id.as_deref().and_then(|d| active.get(d)).map(|&i| tl.p(i));
        if let (Some(add), Some(drop_p)) = (add, drop_p) {
            if !unavailable(add.status) && value_of(values, dyn_, add) > value_of(values, dyn_, drop_p) {
                let (unit, nd) = if dyn_.is_some() { ("dynasty value", 1) } else { ("FPG rest of season", 2) };
                let (a, d) = (value_of(values, dyn_, add), value_of(values, dyn_, drop_p));
                let when_txt = match reset {
                    Some(r) => format!("{} {} when moves reset", strftime_date(r, "%a %b"), chrono::Datelike::day(&r)),
                    None => "next week".to_string(),
                };
                let mut line = format!(
                    "Long-term option: {} is a better long-term {} than {} ({} vs {} {unit}, +{}); consider him {when_txt} if he's still available",
                    add.name,
                    slot_label(add),
                    drop_p.name,
                    fixed(a, nd, false),
                    fixed(d, nd, false),
                    fixed(a - d, nd, false)
                );
                let chg = add.pct_owned_change;
                match chg {
                    Some(c) if c >= RISER_HINT => {
                        let po = add.pct_owned.filter(|x| *x != 0.0).unwrap_or(0.0);
                        line += &format!(
                            " \u{b7} rising: rostered {}% ({} this week), he may not last",
                            fixed(po, 0, false),
                            fixed(c, 1, true)
                        );
                    }
                    _ => {
                        if let Some(po) = add.pct_owned {
                            line += &format!(" \u{b7} rostered {}%", fixed(po, 0, false));
                            if let Some(c) = chg {
                                line += &format!(" ({} this week)", fixed(c, 1, true));
                            }
                        }
                    }
                }
                since.long_term.push(line);
            }
        }
    }
    if since.empty() {
        None
    } else {
        Some(since)
    }
}

// --------------------------------------------------------------------- "Did it": verify a move

pub fn league_label(league: &str) -> &str {
    match league {
        "espn" => "ESPN",
        "fantrax" => "Fantrax",
        other => other,
    }
}

/// Plan files of `league`, newest matchup period first.
fn plan_files(data_dir: &Path, league: &str) -> Vec<PathBuf> {
    let mut files = glob(&data_dir.join("plans"), &format!("{league}-"), ".json");
    files.sort_by_key(|f| std::cmp::Reverse(stem_tail(f)));
    files
}

/// The newest snapshot of `league` (newest matchup period first), or None.
pub fn latest_snapshot(data_dir: &Path, league: &str) -> Option<Value> {
    for f in plan_files(data_dir, league) {
        let Some(text) = std::fs::read_to_string(&f).ok() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if let Value::Array(a) = v {
            if let Some(last) = a.last() {
                return Some(last.clone());
            }
        }
    }
    None
}

pub fn find_snapshot(data_dir: &Path, league: &str, created: &str) -> Option<Value> {
    for f in plan_files(data_dir, league) {
        let Some(text) = std::fs::read_to_string(&f).ok() else { continue };
        let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(s) = a.iter().rev().find(|s| s.get("created").and_then(|c| c.as_str()) == Some(created)) {
            return Some(s.clone());
        }
    }
    None
}

/// (changes, open roster spots) of my roster now vs a snapshot's roster.
pub fn roster_diff(prev: Option<&Value>, ctx: &LeagueContext) -> (Vec<String>, i64) {
    let Ok(team) = ctx.my_team() else { return (vec![], 0) };
    let mut now: Map<&str, (&str, bool)> = Map::new();
    for s in &team.slots {
        if let Some(p) = &s.player {
            now.insert(p.cid.as_str(), (p.name.as_str(), s.slot == "IR" || s.slot == "IR+"));
        }
    }
    let mut out: Vec<String> = Vec::new();
    if let Some(prev) = prev.and_then(|p| p.as_object()) {
        for (c, (name, ir)) in &now {
            match prev.get(*c) {
                None => out.push(format!("Added {name}{}", if *ir { " (IR)" } else { "" })),
                Some(v) => {
                    let was = truthy(v.get("ir"));
                    if *ir && !was {
                        out.push(format!("{name} \u{2192} IR"));
                    } else if !*ir && was {
                        out.push(format!("{name} off IR"));
                    }
                }
            }
        }
        for (c, v) in prev {
            if !now.contains_key(c.as_str()) {
                out.push(format!("Dropped {}", v.get("name").and_then(|n| n.as_str()).unwrap_or("?")));
            }
        }
    }
    let mut spots = open_roster_spots(team, &ctx.roster_shape);
    if let Some(cap) = ctx.max_roster_size.filter(|c| *c != 0) {
        let used = team.slots.iter().filter(|s| s.player.is_some() && !SIZE_EXEMPT_SLOTS.contains(&s.slot.as_str())).count() as i64;
        spots = spots.min((cap - used).max(0));
    }
    (out, spots)
}

/// (banner text, is_warning, retry once more) after "Did it".
pub fn moved_banner(
    data_dir: &Path,
    league: &str,
    since: Option<&str>,
    ctx: &LeagueContext,
    label: &str,
    retry: bool,
) -> (String, bool, bool) {
    let snap = since.filter(|s| !s.is_empty()).and_then(|s| find_snapshot(data_dir, league, s));
    let (changes, spots) = roster_diff(snap.as_ref().and_then(|s| s.get("roster")), ctx);
    let spot_text = if spots != 0 { format!("{spots} open roster spot{}", if spots != 1 { "s" } else { "" }) } else { String::new() };
    if !changes.is_empty() {
        let mut parts = changes;
        if !spot_text.is_empty() {
            parts.push(spot_text);
        }
        return (format!("\u{2713} Seen: {}. The plan is updated.", parts.join(" \u{b7} ")), false, false);
    }
    let name = league_label(league);
    let what = if label.is_empty() { String::new() } else { format!(" ({label})") };
    if retry {
        return (
            format!("Still not showing{what}: {name} hasn't reported it yet. Check the move went through, then press Refresh data."),
            true,
            false,
        );
    }
    (format!("Not showing yet{what}: {name} can take a minute; we'll keep checking."), true, true)
}

/// One line for the pre-game post and the morning digest.
pub fn since_line(plan: &WeekPlan) -> Option<String> {
    let s = plan.since.as_ref()?;
    if s.empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if !s.done.is_empty() {
        parts.push(format!("{} done", s.done.len()));
    }
    if !s.changed.is_empty() {
        let sep = " dropped from the plan: ";
        let items: Vec<String> = s
            .changed
            .iter()
            .take(2)
            .map(|c| {
                let first = c.split(sep).next().unwrap_or("");
                let last = c.rsplit(sep).next().unwrap_or("");
                format!("{first} -> {last}")
            })
            .collect();
        parts.push(format!("changed: {}", items.join("; ")));
    }
    if !s.new.is_empty() {
        parts.push(format!("{} new", s.new.len()));
    }
    if !s.kept.is_empty() {
        parts.push(format!("{} kept", s.kept.len()));
    }
    if parts.is_empty() {
        return None;
    }
    let at = if s.at.is_empty() { String::new() } else { format!(" ({})", s.at) };
    Some(format!("Since your last plan{at}: {}", parts.join(", ")))
}

#[cfg(test)]
mod tests {
    //! Port of tests/test_plan_memory.py (intent).
    use super::*;
    use crate::recommend::plan::tests::{ctx_for, days7, mk, mon, plan_with, valuate};
    use crate::recommend::plan::plan_summary;
    use fm_core::models::{FantasyTeam, RosterSlot};

    fn sched() -> Vec<(&'static str, Vec<NaiveDate>)> {
        let d = days7();
        vec![("MON", vec![d[0]]), ("TW", d[1..3].to_vec()), ("FS", d[4..7].to_vec()), ("TT", d[1..4].to_vec()), ("NONE", vec![])]
    }

    fn plan_mem(ctx: &LeagueContext, dir: &Path) -> WeekPlan {
        plan_with(ctx, &valuate(ctx), "even", Some(dir))
    }

    fn h(cid: &str, team: &str, g: f64) -> Player {
        mk(cid, team, g, "healthy")
    }

    fn moves(p: &WeekPlan) -> Vec<(String, Option<String>)> {
        p.moves.iter().map(|m| (m.add.cid.clone(), m.drop.as_ref().map(|x| x.cid.clone()))).collect()
    }

    #[test]
    fn executed_step_is_done_and_the_rest_carries_on() {
        let dir = tempfile::tempdir().unwrap();
        let first = plan_mem(
            &ctx_for(&[("C", h("my_c", "MON", 2.0)), ("BN", h("weak", "NONE", 0.1))], vec![h("x", "TW", 2.0), h("y", "FS", 2.0)], &sched(), 2, "test"),
            dir.path(),
        );
        assert_eq!(moves(&first), vec![("x".into(), Some("weak".into())), ("y".into(), Some("x".into()))]);
        assert!(first.since.is_none());
        let ctx = ctx_for(&[("C", h("my_c", "MON", 2.0)), ("BN", h("x", "TW", 2.0))], vec![h("y", "FS", 2.0)], &sched(), 1, "test");
        let p = plan_mem(&ctx, dir.path());
        let since = p.since.clone().expect("since");
        assert_eq!(since.done, vec!["Done: added x, dropped weak (Tue)".to_string()]);
        assert_eq!(moves(&p), vec![("y".into(), Some("x".into()))]);
        assert_ne!(p.moves[0].status, "new");
        assert!(since.changed.is_empty());
        let again = plan_mem(&ctx, dir.path());
        assert_eq!(again.since.expect("since").done, since.done);
    }

    #[test]
    fn removed_step_gets_the_reason_from_the_data() {
        let dir = tempfile::tempdir().unwrap();
        let mine = [("C", h("my_c", "MON", 2.0)), ("BN", h("weak", "NONE", 0.1))];
        plan_mem(&ctx_for(&mine, vec![h("x", "TW", 2.0), h("y", "FS", 2.0)], &sched(), 2, "test"), dir.path());
        let mut ctx = ctx_for(&mine, vec![h("x", "TW", 2.0)], &sched(), 2, "test");
        ctx.teams.push(FantasyTeam {
            team_id: "2".into(),
            name: "rival".into(),
            owner_is_me: false,
            slots: vec![RosterSlot { slot: "BN".into(), player: Some(h("y", "FS", 2.0)), starting: false }],
            record: None,
        });
        let p = plan_mem(&ctx, dir.path());
        let since = p.since.expect("since");
        assert!(since.changed.iter().any(|c| c.starts_with("y for x (+") && c.ends_with("y was picked up by rival")), "{:?}", since.changed);
    }

    #[test]
    fn stability_keeps_an_earlier_step_unless_clearly_beaten() {
        let dir = tempfile::tempdir().unwrap();
        let mine = [("C", h("my_c", "MON", 2.0)), ("BN", h("weak", "NONE", 0.1))];
        let first = plan_mem(&ctx_for(&mine, vec![h("a", "TT", 2.0)], &sched(), 1, "test"), dir.path());
        assert_eq!(first.moves.iter().map(|m| m.add.cid.as_str()).collect::<Vec<_>>(), vec!["a"]);
        let p = plan_mem(&ctx_for(&mine, vec![h("a", "TT", 2.0), h("b", "TT", 2.2)], &sched(), 1, "test"), dir.path());
        assert_eq!(p.moves.iter().map(|m| m.add.cid.as_str()).collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(p.moves[0].status, "kept");
        let since = p.since.expect("since");
        assert!(!since.kept.is_empty() && since.kept[0].contains("under the 1.5-pt bar"), "{:?}", since.kept);
        let q = plan_mem(&ctx_for(&mine, vec![h("a", "TT", 2.0), h("c", "TT", 5.0)], &sched(), 1, "test"), dir.path());
        assert_eq!(q.moves.iter().map(|m| m.add.cid.as_str()).collect::<Vec<_>>(), vec!["c"]);
        assert_eq!(q.moves[0].status, "new");
        let since = q.since.expect("since");
        assert!(
            since.changed.iter().any(|x| x.contains("replaced by c for weak (+") && x.contains("with 1 move left, the best one is c")),
            "{:?}",
            since.changed
        );
    }

    #[test]
    fn long_term_option_for_a_dropped_keeper() {
        let dir = tempfile::tempdir().unwrap();
        let mine = [("C", h("my_c", "MON", 2.0)), ("BN", h("weak", "NONE", 0.1))];
        plan_mem(&ctx_for(&mine, vec![h("keeper", "FS", 3.0)], &sched(), 1, "test"), dir.path());
        let p = plan_mem(&ctx_for(&mine, vec![h("keeper", "FS", 3.0), h("s", "TT", 5.0)], &sched(), 1, "test"), dir.path());
        assert_eq!(p.moves.iter().map(|m| m.add.cid.as_str()).collect::<Vec<_>>(), vec!["s"]);
        let lt: Vec<&String> =
            p.watch.iter().filter(|w| w.starts_with("Long-term option: keeper is a better long-term C than weak")).collect();
        assert!(!lt.is_empty() && lt[0].contains("Mon Oct 12 when moves reset"), "{:?}", p.watch);
        assert!(since_line(&p).unwrap().starts_with("Since your last plan"));
        assert!(plan_summary(&p, mon()).iter().any(|x| x.starts_with("Since your last plan")));
    }

    #[test]
    fn open_spot_after_an_ir_move_yields_a_no_drop_add() {
        let dir = tempfile::tempdir().unwrap();
        let mine = [("C", h("my_c", "MON", 2.0)), ("IR", mk("hurt", "NONE", 2.0, "ir"))];
        let p = plan_mem(&ctx_for(&mine, vec![h("x", "TW", 2.0)], &sched(), 2, "test"), dir.path());
        assert_eq!(p.moves.len(), 1);
        assert_eq!((p.moves[0].add.cid.as_str(), p.moves[0].drop.is_none(), p.moves[0].to_ir.is_none()), ("x", true, true));
        assert!(p.moves[0].headline.contains("open roster spot, no drop"));
    }

    #[test]
    fn moved_banner_seen_and_not_seen() {
        let dir = tempfile::tempdir().unwrap();
        let before = [("C", h("my_c", "MON", 2.0)), ("BN", mk("hurt", "NONE", 2.0, "ir"))];
        plan_mem(&ctx_for(&before, vec![h("x", "TW", 2.0)], &sched(), 2, "test"), dir.path());
        let since = latest_snapshot(dir.path(), "test").unwrap()["created"].as_str().unwrap().to_string();
        let (text, warn, again) =
            moved_banner(dir.path(), "test", Some(&since), &ctx_for(&before, vec![], &sched(), 2, "test"), "hurt to IR", false);
        assert!(text.starts_with("Not showing yet (hurt to IR): test can take a minute") && warn && again, "{text}");
        let (text, warn, again) = moved_banner(dir.path(), "test", Some(&since), &ctx_for(&before, vec![], &sched(), 2, "test"), "", true);
        assert!(text.starts_with("Still not showing") && warn && !again);
        let after = [("C", h("my_c", "MON", 2.0)), ("IR", mk("hurt", "NONE", 2.0, "ir"))];
        let (text, warn, again) = moved_banner(dir.path(), "test", Some(&since), &ctx_for(&after, vec![], &sched(), 2, "test"), "", false);
        assert!(text.starts_with("\u{2713} Seen: hurt \u{2192} IR \u{b7} 1 open roster spot") && !warn && !again, "{text}");
    }

    #[test]
    fn snapshots_are_appended_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&[("C", h("my_c", "MON", 2.0)), ("BN", h("weak", "NONE", 0.1))], vec![h("x", "TW", 2.0)], &sched(), 2, "test");
        for _ in 0..3 {
            plan_mem(&ctx, dir.path());
        }
        let files = glob(&dir.path().join("plans"), "", ".json");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_name().unwrap().to_string_lossy(), format!("test-{}.json", mon().format("%Y-%m-%d")));
        let list = read_list(&files[0]).unwrap();
        assert_eq!(list.len(), 1);
        // the stored signature is what _signature computes from the stored steps and roster
        let s = &list[0];
        let steps = s["moves"].as_array().unwrap().clone();
        let roster: Map<String, Value> = s["roster"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(s["signature"].as_str().unwrap(), signature(&steps, &roster));
    }

    #[test]
    fn signature_matches_python_bytes() {
        let steps = vec![json!({"add": {"cid": "x"}, "drop": {"cid": "weak"}, "to_ir": null})];
        let mut roster: Map<String, Value> = Map::new();
        roster.insert("weak".into(), json!({"name": "weak", "ir": false}));
        roster.insert("a".into(), json!({"name": "a", "ir": true}));
        assert_eq!(signature(&steps, &roster), r#"[["[\"x\", \"weak\", null]"], [["a", true], ["weak", false]]]"#);
    }
}
