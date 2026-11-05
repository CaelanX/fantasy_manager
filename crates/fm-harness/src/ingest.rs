//! Rust port of `fantasy_manager/harness/ingest.py`: backfill the ledger from the JSON archive
//! (v1, v2 and v3 files), idempotently.
//! Owner: work package `W3-harness-deploy-data`.
//!
//! * `projections-<league>-<day>.json` -> `projections`
//! * `recs-<league>-<day>.json` -> `recs` + `rec_players`, then `roll_episodes`.
//!
//! Deviations:
//! * `backtest.archive.{archive_dir, load_snapshot}` are not ported yet (`fm_engine::archive` is
//!   W5-archive-outcomes), so [`archive_dir`] and [`load_snapshot`] live here, written against the
//!   documented archive format. TODO(W5): re-point to `fm_engine::archive` after merge.
//! * `record_run` hashes: `params_hash` is `fm_engine` `params_hash()`, `code_hash` is
//!   `"rust:<crate version>"` (accepted difference, ARCHITECTURE 4.6).
//! * `ingest_archive` returns [`IngestStats`] (`to_json()` gives Python's stats dict); OS/JSON
//!   error texts in `errors` differ from Python's exception strings.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use chrono::NaiveDate;
use fm_core::Map;
use fm_engine::valuation::params as vparams;
use fm_pycompat::pyjson::{dumps, DumpOpts};
use regex::Regex;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::ledger::{now_iso, rec_key, Ledger, Row};

/// seen again within 2 days (one missed archive run) = same episode
pub const EPISODE_GAP_DAYS: i64 = 2;

fn ir_title_res() -> &'static [Regex; 2] {
    static RES: OnceLock<[Regex; 2]> = OnceLock::new();
    RES.get_or_init(|| {
        [
            Regex::new(r"\b[Mm]ove (.+?) to IR\b").expect("static regex"),
            Regex::new(r"^(.+?) now [A-Z]+ \(was ").expect("static regex"),
        ]
    })
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// `_int(v)`: `int(v)` or None.
fn to_int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Null => None,
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// `_j(v)`: compact, key-sorted JSON (or None).
fn j(v: Option<&Value>) -> Value {
    match v {
        None | Some(Value::Null) => Value::Null,
        Some(v) => Value::from(dumps(v, &DumpOpts { sort_keys: true, ..DumpOpts::compact() })),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn get_or_null(v: &Value, k: &str) -> Value {
    v.get(k).cloned().unwrap_or(Value::Null)
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    match v.get(k) {
        Some(Value::Array(a)) => a.as_slice(),
        _ => &[],
    }
}

fn s_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

// Archive paths and reader: shared with the writer in fm-engine.
pub use fm_engine::archive::{archive_dir, load_snapshot};

/// (kind, league, day, path) of every archive file, projections before recs per day.
pub fn archive_files(
    data_dir: &Path,
    leagues: Option<&[String]>,
) -> Result<Vec<(String, String, NaiveDate, PathBuf)>> {
    let wanted: Option<std::collections::HashSet<&str>> =
        leagues.filter(|l| !l.is_empty()).map(|l| l.iter().map(|s| s.as_str()).collect());
    let mut out = Vec::new();
    for e in std::fs::read_dir(archive_dir(data_dir)?)? {
        let path = e?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        // ^(projections|recs)-(.+)-(\d{4}-\d{2}-\d{2})$
        let (kind, rest) = if let Some(r) = stem.strip_prefix("projections-") {
            ("projections", r)
        } else if let Some(r) = stem.strip_prefix("recs-") {
            ("recs", r)
        } else {
            continue;
        };
        if rest.len() < 12 || !rest.is_char_boundary(rest.len() - 11) {
            continue;
        }
        let (league, day_s) = rest.split_at(rest.len() - 11);
        let Some(day_s) = day_s.strip_prefix('-') else { continue };
        let bytes = day_s.as_bytes();
        let shape_ok = bytes.len() == 10
            && bytes.iter().enumerate().all(|(i, b)| if i == 4 || i == 7 { *b == b'-' } else { b.is_ascii_digit() });
        if !shape_ok || league.is_empty() {
            continue;
        }
        let Ok(day) = NaiveDate::parse_from_str(day_s, "%Y-%m-%d") else { continue };
        if wanted.as_ref().map(|w| w.contains(league)).unwrap_or(true) {
            out.push((kind.to_string(), league.to_string(), day, path.clone()));
        }
    }
    out.sort_by(|a, b| (a.2, &a.1, (a.0 != "projections") as u8).cmp(&(b.2, &b.1, (b.0 != "projections") as u8)));
    Ok(out)
}

// --------------------------------------------------------------------------- projections

pub fn projection_rows(league: &str, as_of: &str, snap: &Value) -> Vec<Row> {
    let version = to_int(snap.get("version")).filter(|v| *v != 0).unwrap_or(1);
    let mut rows = Vec::new();
    for rec in arr(snap, "players") {
        let fm = rec.get("fm").filter(|v| truthy(Some(v)));
        let inputs = rec.get("inputs").filter(|v| !v.is_null());
        let fm_get = |k: &str| fm.map(|f| get_or_null(f, k)).unwrap_or(Value::Null);
        let positions = arr(rec, "positions").iter().map(|p| p.as_str().unwrap_or("")).collect::<Vec<_>>().join(",");
        let mut r = Row::new();
        r.insert("league".into(), league.into());
        r.insert("as_of".into(), as_of.into());
        r.insert("cid".into(), get_or_null(rec, "cid"));
        r.insert("name".into(), get_or_null(rec, "name"));
        r.insert("nhl_id".into(), to_int(rec.get("nhl_id")).map(Value::from).unwrap_or(Value::Null));
        r.insert("team".into(), get_or_null(rec, "team"));
        r.insert("positions".into(), positions.into());
        r.insert("status".into(), get_or_null(rec, "status"));
        r.insert("pct_owned".into(), get_or_null(rec, "pct_owned"));
        r.insert("fpg".into(), fm_get("fpg"));
        r.insert("fpg_season".into(), fm_get("fpg_season"));
        r.insert("fpg_week".into(), fm_get("fpg_week"));
        r.insert("proj_week".into(), fm_get("proj_week"));
        r.insert("vorp".into(), fm_get("vorp"));
        r.insert("games_next7".into(), inputs.map(|i| get_or_null(i, "games_next7")).unwrap_or(Value::Null));
        r.insert("rates_json".into(), if fm.is_some() { j(fm.and_then(|f| f.get("rates"))) } else { Value::Null });
        r.insert("projected_json".into(), j(rec.get("projected")));
        r.insert("inputs_json".into(), j(inputs));
        r.insert("archive_version".into(), version.into());
        if truthy(r.get("cid")) {
            rows.push(r);
        }
    }
    rows
}

pub fn ingest_projections(ledger: &Ledger, league: &str, as_of: &str, snap: &Value) -> Result<usize> {
    if let Some(means) = snap.get("position_means").filter(|m| m.as_object().map(|o| !o.is_empty()).unwrap_or(false)) {
        // the positional means a league projection is shrunk toward (harness.refit replays them)
        let key = format!("means:{league}:{as_of}");
        let val = dumps(means, &DumpOpts { sort_keys: true, ..Default::default() });
        ledger.execute(
            "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            &[&key, &val],
        )?;
    }
    let (lg, ao) = (league.to_string(), as_of.to_string());
    ledger.execute("DELETE FROM projections WHERE league=? AND as_of=?", &[&lg, &ao])?;
    ledger.upsert("projections", &projection_rows(league, as_of, snap), &["league", "as_of", "cid"])
}

// --------------------------------------------------------------------------- recommendations

/// IR target of a v1 rec (no `subjects` key) from its title + that day's projections.
fn v1_subjects(ledger: &Ledger, league: &str, as_of: &str, rec: &Value) -> Result<Vec<Value>> {
    if !matches!(s_of(rec, "kind"), Some("injury") | Some("waiver")) {
        return Ok(Vec::new());
    }
    let title = if truthy(rec.get("title")) { s_of(rec, "title").unwrap_or("") } else { "" };
    for rx in ir_title_res() {
        let Some(m) = rx.captures(title) else { continue };
        let name = m[1].trim().to_string();
        let (lg, ao) = (league.to_string(), as_of.to_string());
        let hit = ledger.query(
            "SELECT cid, name, nhl_id FROM projections WHERE league=? AND as_of=? AND name=?",
            &[&lg, &ao, &name],
        )?;
        if hit.len() == 1 {
            return Ok(vec![json!({"cid": hit[0]["cid"], "name": hit[0]["name"], "nhl_id": hit[0]["nhl_id"]})]);
        }
        return Ok(vec![json!({"cid": format!("name:{name}"), "name": name, "nhl_id": Value::Null})]);
    }
    Ok(Vec::new())
}

pub fn rec_rows(ledger: &Ledger, league: &str, as_of: &str, snap: &Value) -> Result<(Vec<Row>, Vec<Row>)> {
    let version = to_int(snap.get("version")).filter(|v| *v != 0).unwrap_or(1);
    let mut recs: Map<String, Row> = Map::new();
    let mut players: Map<(String, String, String), Row> = Map::new();
    let with_cid = |list: &[Value]| -> Vec<Value> { list.iter().filter(|p| truthy(p.get("cid"))).cloned().collect() };
    let cids = |list: &[Value]| -> Vec<String> {
        list.iter().map(|p| p["cid"].as_str().map(String::from).unwrap_or_else(|| p["cid"].to_string())).collect()
    };
    for rec in arr(snap, "recommendations") {
        let adds = with_cid(arr(rec, "add"));
        let drops = with_cid(arr(rec, "drop"));
        let subjects_raw: Option<Vec<Value>> = if version >= 2 {
            match rec.get("subjects") {
                Some(Value::Array(a)) => Some(a.clone()),
                _ => None,
            }
        } else {
            None
        };
        let subjects_raw = match subjects_raw {
            Some(s) => s,
            None => v1_subjects(ledger, league, as_of, rec)?,
        };
        let subjects = with_cid(&subjects_raw);
        let kind = s_of(rec, "kind").ok_or_else(|| anyhow!("KeyError: 'kind'"))?;
        let counterparty = s_of(rec, "counterparty");
        let key = rec_key(league, kind, cids(&adds), cids(&drops), counterparty, cids(&subjects));
        if recs.contains_key(&key) {
            continue; // the same rec twice in one day: keep the first (best-ranked)
        }
        let reasons = if truthy(rec.get("reasons")) { rec["reasons"].clone() } else { json!([]) };
        let mut r = Row::new();
        r.insert("league".into(), league.into());
        r.insert("as_of".into(), as_of.into());
        r.insert("rec_key".into(), key.clone().into());
        r.insert("kind".into(), kind.into());
        r.insert("title".into(), get_or_null(rec, "title"));
        r.insert("score".into(), get_or_null(rec, "score"));
        r.insert("strength".into(), get_or_null(rec, "strength"));
        r.insert("predicted_gain".into(), get_or_null(rec, "predicted_gain"));
        r.insert("gain_units".into(), get_or_null(rec, "gain_units"));
        r.insert("horizon_days".into(), get_or_null(rec, "horizon_days"));
        r.insert("counterparty".into(), get_or_null(rec, "counterparty"));
        r.insert("reasons_json".into(), j(Some(&reasons)));
        r.insert("archive_version".into(), version.into());
        r.insert("params_hash".into(), get_or_null(snap, "params_hash"));
        r.insert("code_hash".into(), get_or_null(snap, "code_hash"));
        recs.insert(key.clone(), r);
        for (side, plist) in [("add", &adds), ("drop", &drops), ("subject", &subjects)] {
            for p in plist.iter() {
                let cid = p["cid"].as_str().map(String::from).unwrap_or_else(|| p["cid"].to_string());
                let mut pr = Row::new();
                pr.insert("league".into(), league.into());
                pr.insert("as_of".into(), as_of.into());
                pr.insert("rec_key".into(), key.clone().into());
                pr.insert("side".into(), side.into());
                pr.insert("cid".into(), p["cid"].clone());
                pr.insert("name".into(), get_or_null(p, "name"));
                pr.insert("nhl_id".into(), to_int(p.get("nhl_id")).map(Value::from).unwrap_or(Value::Null));
                players.insert((key.clone(), side.to_string(), cid), pr);
            }
        }
    }
    Ok((recs.into_values().collect(), players.into_values().collect()))
}

pub fn ingest_recs(ledger: &Ledger, league: &str, as_of: &str, snap: &Value) -> Result<usize> {
    let (recs, players) = rec_rows(ledger, league, as_of, snap)?;
    let (lg, ao) = (league.to_string(), as_of.to_string());
    ledger.execute("DELETE FROM recs WHERE league=? AND as_of=?", &[&lg, &ao])?;
    ledger.execute("DELETE FROM rec_players WHERE league=? AND as_of=?", &[&lg, &ao])?;
    ledger.upsert("recs", &recs, &["league", "as_of", "rec_key"])?;
    ledger.upsert("rec_players", &players, &["league", "as_of", "rec_key", "side", "cid"])?;
    ledger.commit()?;
    Ok(recs.len())
}

// --------------------------------------------------------------------------- episodes

/// `sha1(league|key|first_seen)[:20]`
pub fn episode_id(league: &str, key: &str, first_seen: &str) -> String {
    sha1_hex(format!("{league}|{key}|{first_seen}").as_bytes())[..20].to_string()
}

/// Sorted sighting days split into runs where consecutive sightings are <= `gap` apart.
pub fn group_days(days: impl IntoIterator<Item = NaiveDate>, gap: i64) -> Vec<Vec<NaiveDate>> {
    let set: std::collections::BTreeSet<NaiveDate> = days.into_iter().collect();
    let mut out: Vec<Vec<NaiveDate>> = Vec::new();
    for d in set {
        match out.last_mut() {
            Some(run) if (d - *run.last().expect("non-empty run")).num_days() <= gap => run.push(d),
            _ => out.push(vec![d]),
        }
    }
    out
}

/// Recompute episodes from `recs` (keeps each episode's match status); returns count.
pub fn roll_episodes(ledger: &Ledger, leagues: Option<&[String]>) -> Result<usize> {
    let leagues: Vec<String> = match leagues.filter(|l| !l.is_empty()) {
        Some(l) => l.to_vec(),
        None => ledger
            .query("SELECT DISTINCT league FROM recs", &[])?
            .iter()
            .filter_map(|r| r["league"].as_str().map(String::from))
            .collect(),
    };
    let mut total = 0;
    for league in &leagues {
        let rows = ledger.query(
            "SELECT rec_key, as_of, kind, title, predicted_gain, gain_units, horizon_days, strength \
             FROM recs WHERE league=? ORDER BY rec_key, as_of",
            &[league],
        )?;
        let mut by_key: Map<String, Vec<&Row>> = Map::new();
        for r in &rows {
            by_key.entry(r["rec_key"].as_str().unwrap_or("").to_string()).or_default().push(r);
        }
        let mut episodes: Vec<Row> = Vec::new();
        for (key, rs) in &by_key {
            let mut by_day: Map<NaiveDate, &Row> = Map::new();
            for r in rs {
                let d = NaiveDate::parse_from_str(r["as_of"].as_str().unwrap_or(""), "%Y-%m-%d")
                    .map_err(|e| anyhow!("bad as_of in recs: {e}"))?;
                by_day.insert(d, r);
            }
            for run in group_days(by_day.keys().copied(), EPISODE_GAP_DAYS) {
                let (first, last) = (by_day[&run[0]], by_day[run.last().expect("non-empty run")]);
                let first_seen = iso(run[0]);
                let mut e = Row::new();
                e.insert("episode_id".into(), episode_id(league, key, &first_seen).into());
                e.insert("league".into(), league.clone().into());
                e.insert("rec_key".into(), key.clone().into());
                e.insert("kind".into(), first["kind"].clone());
                e.insert("title".into(), last["title"].clone());
                e.insert("first_seen".into(), first_seen.into());
                e.insert("last_seen".into(), iso(*run.last().expect("non-empty run")).into());
                e.insert("n_days".into(), (run.len() as i64).into());
                e.insert("predicted_gain".into(), first["predicted_gain"].clone());
                e.insert("gain_units".into(), first["gain_units"].clone());
                e.insert("horizon_days".into(), first["horizon_days"].clone());
                e.insert("strength".into(), first["strength"].clone());
                e.insert("updated_at".into(), now_iso().into());
                episodes.push(e);
            }
        }
        let keep: std::collections::HashSet<&str> = episodes.iter().filter_map(|e| e["episode_id"].as_str()).collect();
        let stale: Vec<String> = ledger
            .query("SELECT episode_id FROM rec_episodes WHERE league=?", &[league])?
            .iter()
            .filter_map(|r| r["episode_id"].as_str().map(String::from))
            .filter(|id| !keep.contains(id.as_str()))
            .collect();
        for eid in &stale {
            ledger.execute("DELETE FROM rec_episodes WHERE episode_id=?", &[eid])?;
        }
        ledger.upsert("rec_episodes", &episodes, &["episode_id"])?;
        total += episodes.len();
    }
    ledger.commit()?;
    Ok(total)
}

// --------------------------------------------------------------------------- entry points

#[derive(Clone, Debug, Default, PartialEq)]
pub struct IngestStats {
    pub files_seen: i64,
    pub files_ingested: i64,
    pub projections: i64,
    pub recs: i64,
    pub episodes: i64,
    pub errors: Vec<String>,
}

impl IngestStats {
    /// Python's stats dict (key order: files_seen, files_ingested, projections, recs, episodes, errors).
    pub fn to_json(&self) -> Value {
        json!({"files_seen": self.files_seen, "files_ingested": self.files_ingested,
               "projections": self.projections, "recs": self.recs, "episodes": self.episodes,
               "errors": self.errors})
    }
}

/// Ingest every (new or changed) archive file, then roll episodes. Idempotent.
pub fn ingest_archive(ledger: &Ledger, data_dir: &Path, leagues: Option<&[String]>, force: bool) -> Result<IngestStats> {
    let leagues: Option<Vec<String>> = leagues.filter(|l| !l.is_empty()).map(|l| l.to_vec());
    let mut stats = IngestStats::default();
    let known: std::collections::HashMap<String, String> = ledger
        .query("SELECT path, sha1 FROM archive_files", &[])?
        .iter()
        .filter_map(|r| Some((r["path"].as_str()?.to_string(), r["sha1"].as_str()?.to_string())))
        .collect();
    let mut touched: std::collections::BTreeSet<String> = Default::default();
    for (kind, league, day, path) in archive_files(data_dir, leagues.as_deref())? {
        stats.files_seen += 1;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let sha = match std::fs::read(&path) {
            Ok(b) => sha1_hex(&b),
            Err(e) => {
                stats.errors.push(format!("{name}: {e}"));
                continue;
            }
        };
        if !force && known.get(&name) == Some(&sha) {
            continue;
        }
        let snap = match load_snapshot(&path) {
            Ok(s) => s,
            Err(e) => {
                stats.errors.push(format!("{name}: {e}"));
                continue;
            }
        };
        let as_of = match snap.get("as_of") {
            Some(v) if truthy(Some(v)) => v.as_str().map(String::from).unwrap_or_else(|| v.to_string()),
            _ => iso(day),
        };
        if kind == "projections" {
            stats.projections += ingest_projections(ledger, &league, &as_of, &snap)? as i64;
        } else {
            stats.recs += ingest_recs(ledger, &league, &as_of, &snap)? as i64;
            touched.insert(league.clone());
        }
        let version = to_int(snap.get("version")).filter(|v| *v != 0).unwrap_or(1);
        let mut row = Row::new();
        row.insert("path".into(), name.into());
        row.insert("sha1".into(), sha.into());
        row.insert("kind".into(), kind.into());
        row.insert("league".into(), league.into());
        row.insert("as_of".into(), as_of.into());
        row.insert("version".into(), version.into());
        row.insert("ingested_at".into(), now_iso().into());
        ledger.upsert("archive_files", &[row], &["path"])?;
        stats.files_ingested += 1;
    }
    if !touched.is_empty() || force {
        let which: Vec<String> = if touched.is_empty() { leagues.clone().unwrap_or_default() } else { touched.iter().cloned().collect() };
        roll_episodes(ledger, Some(&which))?;
    }
    stats.episodes = match &leagues {
        Some(ls) => {
            let mut n = 0;
            for lg in ls {
                n += ledger.count("rec_episodes", "WHERE league=?", &[lg])?;
            }
            n
        }
        None => ledger.count("rec_episodes", "", &[])?,
    };
    Ok(stats)
}

/// Log one finished harness run with the current params / code hashes.
pub fn record_run(
    ledger: &Ledger,
    command: &str,
    league: Option<&str>,
    as_of: Option<&str>,
    summary: Option<&Value>,
    status: &str,
) -> Result<i64> {
    let params_hash = vparams::params_hash(None);
    let code_hash = format!("rust:{}", env!("CARGO_PKG_VERSION"));
    let rid = ledger.start_run(command, league, as_of.filter(|s| !s.is_empty()), Some(&params_hash), Some(&code_hash))?;
    ledger.finish_run(rid, status, summary)?;
    Ok(rid)
}
