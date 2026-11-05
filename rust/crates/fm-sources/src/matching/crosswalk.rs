//! Rust port of `fantasy_manager/matching/crosswalk.py`: persistent crosswalk from provider player
//! ids (ESPN / Fantrax / Daily Faceoff) to canonical NHL ids, stored in `<data_dir>/crosswalk.db`
//! (table `player_xref`, same schema as Python so either implementation can use the file).
//! Owner: work package `W2-nhl-matching` (wave 2).
//!
//! Differences from Python (API shape only): `resolve` takes `&mut [Player]` and writes
//! `ids["nhl"]`; the candidate pool argument (`list[Candidate] | PlayerIndex`) is [`Pool`]
//! (`&[Candidate]`, `&Vec<Candidate>` and `&PlayerIndex` convert into it);
//! `nhl_candidates(*sources)` takes one `Vec<Candidate>` per source (build them with
//! [`candidates_of`], which accepts NHL season rows / roster players / candidates).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use fm_core::{Map, Player};
use indexmap::IndexMap;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use super::matcher::{match_player, Candidate, Key, PlayerIndex};
use crate::nhl::{NhlGoalieSeason, NhlRosterPlayer, NhlSkaterSeason};

pub const STORED: [&str; 3] = ["exact", "high", "confirmed"];
/// Known id sources. "dfo" (Daily Faceoff) ids come from line / starting-goalie pages.
pub const SOURCES: [&str; 3] = ["espn", "fantrax", "dfo"];

fn now_epoch() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct XrefRow {
    pub source: String,
    pub source_id: String,
    #[serde(default)]
    pub nhl_id: Option<i64>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub team: Option<String>,
    /// exact | high | confirmed | pending | none
    #[serde(default = "none_str")]
    pub confidence: String,
    #[serde(default)]
    pub candidate_name: Option<String>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub updated: f64,
}

fn none_str() -> String {
    "none".to_string()
}

impl XrefRow {
    pub fn blank(source: &str, source_id: &str) -> Self {
        XrefRow {
            source: source.to_string(),
            source_id: source_id.to_string(),
            nhl_id: None,
            name: String::new(),
            team: None,
            confidence: "none".to_string(),
            candidate_name: None,
            score: None,
            updated: 0.0,
        }
    }
}

/// A player record from an external feed (no NHL id) to map to an NHL id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceItem {
    pub source_id: String,
    pub name: String,
    #[serde(default)]
    pub team: Option<String>,
    /// "C", "LW", "D", "G", "C/LW", ... (None: unknown)
    #[serde(default)]
    pub position: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResolveStats {
    /// players that now carry ids["nhl"]
    pub resolved: i64,
    /// of which matched for the first time this run
    pub new: i64,
    pub pending: i64,
    pub unmatched: i64,
}

impl ResolveStats {
    pub fn summary(&self) -> String {
        format!(
            "{} matched to NHL ids ({} new), {} pending review, {} unmatched",
            self.resolved, self.new, self.pending, self.unmatched
        )
    }
}

/// Candidate pool argument: a raw list (indexed on the fly) or a prebuilt [`PlayerIndex`].
pub enum Pool<'a> {
    Cands(&'a [Candidate]),
    Index(&'a PlayerIndex),
}

impl<'a> From<&'a [Candidate]> for Pool<'a> {
    fn from(c: &'a [Candidate]) -> Self {
        Pool::Cands(c)
    }
}
impl<'a> From<&'a Vec<Candidate>> for Pool<'a> {
    fn from(c: &'a Vec<Candidate>) -> Self {
        Pool::Cands(c.as_slice())
    }
}
impl<'a> From<&'a PlayerIndex> for Pool<'a> {
    fn from(c: &'a PlayerIndex) -> Self {
        Pool::Index(c)
    }
}

enum IndexHolder<'a> {
    Owned(PlayerIndex),
    Borrowed(&'a PlayerIndex),
}
impl IndexHolder<'_> {
    fn get(&self) -> &PlayerIndex {
        match self {
            IndexHolder::Owned(i) => i,
            IndexHolder::Borrowed(i) => i,
        }
    }
}
fn holder<'a>(pool: Pool<'a>) -> IndexHolder<'a> {
    match pool {
        Pool::Cands(c) => IndexHolder::Owned(PlayerIndex::new(c.iter().cloned())),
        Pool::Index(i) => IndexHolder::Borrowed(i),
    }
}

/// `'espn:123'` -> `("espn", "123")`; an unprefixed id gets source `"unknown"`.
pub fn split_cid(cid: &str) -> (String, String) {
    match cid.split_once(':') {
        Some((s, i)) => (s.to_string(), i.to_string()),
        None => ("unknown".to_string(), cid.to_string()),
    }
}

/// `'espn:123=8478402'` -> `("espn", "123", 8478402)`.
pub fn parse_confirm(spec: &str) -> Result<(String, String, i64), String> {
    let bad = || format!("bad --confirm value {spec:?}; expected SOURCE:ID=NHLID, e.g. espn:123=8478402");
    let (left, right) = spec.split_once('=').ok_or_else(bad)?;
    let (src, sid) = split_cid(left.trim());
    if src == "unknown" || sid.is_empty() {
        return Err(bad());
    }
    let n = right.trim().parse::<i64>().map_err(|_| bad())?;
    Ok((src, sid, n))
}

/// `_match_position(p)`: positions without the generic "F" (unless that is all), joined by "/".
pub fn match_position(p: &Player) -> Option<String> {
    let pos: Vec<&str> = p.positions.iter().map(String::as_str).filter(|x| *x != "F").collect();
    let pos = if pos.is_empty() { p.positions.iter().map(String::as_str).collect() } else { pos };
    if pos.is_empty() {
        None
    } else {
        Some(pos.join("/"))
    }
}

/// Anything `nhl_candidates` accepts: yields `(player id, name, team, position)`.
pub trait NhlCandidateSource {
    fn nhl_candidate_parts(&self) -> (Option<i64>, String, Option<String>, Option<String>);
}

impl NhlCandidateSource for Candidate {
    fn nhl_candidate_parts(&self) -> (Option<i64>, String, Option<String>, Option<String>) {
        (self.key.as_i64(), self.name.clone(), self.team.clone(), self.position.clone())
    }
}
impl NhlCandidateSource for NhlSkaterSeason {
    fn nhl_candidate_parts(&self) -> (Option<i64>, String, Option<String>, Option<String>) {
        (Some(self.player_id), self.name.clone(), self.team.clone(), self.position.clone())
    }
}
impl NhlCandidateSource for NhlGoalieSeason {
    fn nhl_candidate_parts(&self) -> (Option<i64>, String, Option<String>, Option<String>) {
        (Some(self.player_id), self.name.clone(), self.team.clone(), Some(self.position.clone()))
    }
}
impl NhlCandidateSource for NhlRosterPlayer {
    fn nhl_candidate_parts(&self) -> (Option<i64>, String, Option<String>, Option<String>) {
        (Some(self.player_id), self.name(), Some(self.team.clone()), self.position.clone())
    }
}

/// Convert one source's items to candidates (Python passes the items to `nhl_candidates` directly).
pub fn candidates_of<T: NhlCandidateSource>(items: &[T]) -> Vec<Candidate> {
    items
        .iter()
        .filter_map(|it| {
            let (pid, name, team, position) = it.nhl_candidate_parts();
            Some(Candidate {
                key: Key::Int(pid?),
                name,
                team: team.filter(|s| !s.is_empty()),
                position: position.filter(|s| !s.is_empty()),
            })
        })
        .collect()
}

/// `nhl_candidates(*sources)`: candidates keyed by NHL player id. Later sources override earlier
/// ones for the same id (team / position fall back to the earlier value when missing), so pass
/// season rows first and current rosters last.
pub fn nhl_candidates(sources: &[Vec<Candidate>]) -> Vec<Candidate> {
    let mut by_id: IndexMap<i64, Candidate> = IndexMap::new();
    for items in sources {
        for it in items {
            let Some(pid) = it.key.as_i64() else { continue };
            if it.name.is_empty() {
                continue;
            }
            let prev = by_id.get(&pid);
            let team = it.team.clone().filter(|s| !s.is_empty()).or_else(|| prev.and_then(|p| p.team.clone()));
            let position = it.position.clone().filter(|s| !s.is_empty()).or_else(|| prev.and_then(|p| p.position.clone()));
            by_id.insert(pid, Candidate { key: Key::Int(pid), name: it.name.clone(), team, position });
        }
    }
    by_id.into_values().collect()
}

const COLS: &str = "source, source_id, nhl_id, name, team, confidence, candidate_name, score, updated";

pub struct Crosswalk {
    pub data_dir: PathBuf,
    pub path: PathBuf,
    db: Mutex<Option<Connection>>,
}

impl Crosswalk {
    pub fn new(data_dir: impl AsRef<Path>) -> anyhow::Result<Crosswalk> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)?;
        let path = data_dir.join("crosswalk.db");
        let db = Connection::open(&path)?;
        db.execute(
            "CREATE TABLE IF NOT EXISTS player_xref (\
             source TEXT NOT NULL, source_id TEXT NOT NULL, nhl_id INTEGER, name TEXT, team TEXT,\
             confidence TEXT NOT NULL, candidate_name TEXT, score REAL, updated REAL NOT NULL,\
             PRIMARY KEY (source, source_id))",
            [],
        )?;
        Ok(Crosswalk { data_dir, path, db: Mutex::new(Some(db)) })
    }

    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        let guard = self.db.lock().map_err(|_| anyhow::anyhow!("crosswalk lock poisoned"))?;
        let conn = guard.as_ref().ok_or_else(|| anyhow::anyhow!("Cannot operate on a closed database."))?;
        Ok(f(conn)?)
    }

    fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<XrefRow> {
        Ok(XrefRow {
            source: r.get(0)?,
            source_id: r.get(1)?,
            nhl_id: r.get(2)?,
            name: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            team: r.get(4)?,
            confidence: r.get(5)?,
            candidate_name: r.get(6)?,
            score: r.get(7)?,
            updated: r.get(8)?,
        })
    }

    pub fn get(&self, source: &str, source_id: &str) -> anyhow::Result<Option<XrefRow>> {
        self.with_db(|c| {
            let mut st = c.prepare(&format!("SELECT {COLS} FROM player_xref WHERE source=?1 AND source_id=?2"))?;
            let mut rows = st.query_map(params![source, source_id], Self::row)?;
            rows.next().transpose()
        })
    }

    fn put(&self, row: &XrefRow) -> anyhow::Result<()> {
        let updated = if row.updated != 0.0 { row.updated } else { now_epoch() };
        self.with_db(|c| {
            c.execute(
                &format!("INSERT OR REPLACE INTO player_xref({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)"),
                params![
                    row.source,
                    row.source_id,
                    row.nhl_id,
                    row.name,
                    row.team,
                    row.confidence,
                    row.candidate_name,
                    row.score,
                    updated
                ],
            )?;
            Ok(())
        })
    }

    pub fn all(&self) -> anyhow::Result<Vec<XrefRow>> {
        self.with_db(|c| {
            let mut st = c.prepare(&format!("SELECT {COLS} FROM player_xref ORDER BY source, name"))?;
            let rows = st.query_map([], Self::row)?;
            rows.collect()
        })
    }

    pub fn pending(&self, include_unmatched: bool) -> anyhow::Result<Vec<XrefRow>> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|r| r.confidence == "pending" || (include_unmatched && r.confidence == "none"))
            .collect())
    }

    /// Pin a mapping by hand; confirmed rows are never re-matched.
    pub fn confirm(&self, source: &str, source_id: &str, nhl_id: i64) -> anyhow::Result<XrefRow> {
        let mut row = self.get(source, source_id)?.unwrap_or_else(|| XrefRow::blank(source, source_id));
        row.nhl_id = Some(nhl_id);
        row.confidence = "confirmed".to_string();
        row.updated = now_epoch();
        self.put(&row)?;
        Ok(row)
    }

    pub fn close(&self) {
        if let Ok(mut g) = self.db.lock() {
            *g = None;
        }
    }

    /// Map external-feed records (e.g. `source="dfo"`) to NHL ids -> (`{source_id: nhl_id}`, stats).
    /// Stored confident / confirmed rows win; otherwise the fuzzy matcher runs with the record's
    /// team and position. Pending matches are stored for `fm sync --review`; "none" rows are only
    /// stored with `store_unmatched`.
    pub fn resolve_source<'a>(
        &self,
        source: &str,
        items: &[SourceItem],
        nhl_candidates: impl Into<Pool<'a>>,
        store_unmatched: bool,
    ) -> anyhow::Result<(Map<String, i64>, ResolveStats)> {
        let h = holder(nhl_candidates.into());
        let _txn = Txn::begin(self);
        let index = h.get();
        let mut stats = ResolveStats::default();
        let mut out: Map<String, i64> = Map::new();
        let now = now_epoch();
        for it in items {
            let sid = it.source_id.clone();
            if out.contains_key(&sid) {
                continue;
            }
            let stored = self.get(source, &sid)?;
            if let Some(s) = &stored {
                if STORED.contains(&s.confidence.as_str()) {
                    if let Some(n) = s.nhl_id {
                        out.insert(sid, n);
                        stats.resolved += 1;
                        continue;
                    }
                }
            }
            if index.is_empty() {
                continue;
            }
            let m = match_player(&it.name, index, it.team.as_deref(), it.position.as_deref());
            if m.matched() {
                out.insert(sid.clone(), m.key.as_ref().and_then(Key::as_i64).unwrap_or(0));
                stats.resolved += 1;
                stats.new += 1;
            } else if m.confidence.as_str() == "pending" {
                stats.pending += 1;
            } else {
                stats.unmatched += 1;
                if !store_unmatched {
                    continue;
                }
            }
            self.put(&XrefRow {
                source: source.to_string(),
                source_id: sid,
                name: it.name.clone(),
                team: it.team.clone(),
                confidence: m.confidence.as_str().to_string(),
                candidate_name: m.name.clone(),
                score: if m.score != 0.0 { Some(m.score) } else { None },
                updated: now,
                nhl_id: m.key.as_ref().and_then(Key::as_i64),
            })?;
        }
        Ok((out, stats))
    }

    /// Attach `ids["nhl"]` to each player (stored mapping first, else fuzzy match).
    pub fn resolve<'a>(&self, players: &mut [Player], nhl_candidates: impl Into<Pool<'a>>) -> anyhow::Result<ResolveStats> {
        let h = holder(nhl_candidates.into());
        let _txn = Txn::begin(self);
        let index = h.get();
        let mut stats = ResolveStats::default();
        let now = now_epoch();
        for p in players.iter_mut() {
            let (source, sid) = split_cid(&p.cid);
            if let Some(nid) = p.nhl_id() {
                // provider already knows the NHL id
                stats.resolved += 1;
                let prior = self.get(&source, &sid)?;
                if !prior.map(|r| STORED.contains(&r.confidence.as_str())).unwrap_or(false) {
                    self.put(&XrefRow {
                        source,
                        source_id: sid,
                        nhl_id: Some(nid),
                        name: p.name.clone(),
                        team: p.team.clone(),
                        confidence: "exact".to_string(),
                        candidate_name: None,
                        score: None,
                        updated: now,
                    })?;
                }
                continue;
            }
            let stored = self.get(&source, &sid)?;
            if let Some(s) = &stored {
                if STORED.contains(&s.confidence.as_str()) {
                    if let Some(n) = s.nhl_id {
                        p.ids.insert("nhl".to_string(), n.to_string());
                        stats.resolved += 1;
                        continue;
                    }
                }
            }
            if index.is_empty() {
                continue;
            }
            let m = match_player(&p.name, index, p.team.as_deref(), match_position(p).as_deref());
            let mk = m.key.as_ref().and_then(Key::as_i64);
            self.put(&XrefRow {
                source,
                source_id: sid,
                name: p.name.clone(),
                team: p.team.clone(),
                confidence: m.confidence.as_str().to_string(),
                candidate_name: m.name.clone(),
                score: if m.score != 0.0 { Some(m.score) } else { None },
                updated: now,
                nhl_id: mk,
            })?;
            if m.matched() {
                p.ids.insert("nhl".to_string(), mk.unwrap_or(0).to_string());
                stats.resolved += 1;
                stats.new += 1;
            } else if m.confidence.as_str() == "pending" {
                stats.pending += 1;
            } else {
                stats.unmatched += 1;
            }
        }
        Ok(stats)
    }
}

/// One SQLite transaction around a whole resolve run (Python commits after every row; the rows
/// written are identical, one fsync instead of one per player). Committed on drop, also on error.
struct Txn<'a>(&'a Crosswalk);

impl<'a> Txn<'a> {
    fn begin(xw: &'a Crosswalk) -> Self {
        let _ = xw.with_db(|c| c.execute_batch("BEGIN"));
        Txn(xw)
    }
}

impl Drop for Txn<'_> {
    fn drop(&mut self) {
        let _ = self.0.with_db(|c| c.execute_batch("COMMIT"));
    }
}
