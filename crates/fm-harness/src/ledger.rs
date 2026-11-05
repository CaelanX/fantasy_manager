//! Rust port of `fantasy_manager/harness/ledger.py`: the harness ledger, one sqlite file at
//! `<fm_data_dir>/harness.db`, shared byte-compatibly with the Python implementation (same
//! schema, `schema_version=2` migration, WAL, `ON CONFLICT DO UPDATE` upserts, `rec_key`).
//!
//! Owner: work package `W2-harness-ledger-stats`.
//!
//! Notes / deliberate deviations:
//! * Python's `sqlite3` module opens an implicit transaction before INSERT/UPDATE/DELETE/REPLACE
//!   and keeps it until `commit()`. [`Ledger::execute`] reproduces that (a `BEGIN` before a DML
//!   statement while in autocommit mode, `COMMIT` in [`Ledger::commit`]) so batches of writes
//!   stay atomic and fast exactly as in Python.
//! * Statement parameters are positional only (`&[&dyn ToSql]`, see [`Params`]); Python also
//!   allowed a mapping for named parameters, which no caller uses.
//! * JSON text columns written here use the `json.dumps` work-alike ([`json_dumps`], over `fm_pycompat::pyjson`) with
//!   Python's default separators (`", "` / `": "`), `ensure_ascii` and optional `sort_keys`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use chrono::Local;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, ToSql};
use serde_json::{Map as JsonMap, Value};
use sha1::{Digest, Sha1};

pub const DB_NAME: &str = "harness.db";
pub const SCHEMA_VERSION: i64 = 2;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS runs (
    run_id INTEGER PRIMARY KEY AUTOINCREMENT,
    command TEXT NOT NULL, league TEXT, as_of TEXT,
    started_at TEXT NOT NULL, finished_at TEXT, status TEXT,
    params_hash TEXT, code_hash TEXT, summary_json TEXT);
CREATE TABLE IF NOT EXISTS archive_files (
    path TEXT PRIMARY KEY, sha1 TEXT NOT NULL, kind TEXT, league TEXT, as_of TEXT,
    version INTEGER, ingested_at TEXT);
CREATE TABLE IF NOT EXISTS recs (
    league TEXT NOT NULL, as_of TEXT NOT NULL, rec_key TEXT NOT NULL,
    kind TEXT NOT NULL, title TEXT, score REAL, strength REAL,
    predicted_gain REAL, gain_units TEXT, horizon_days INTEGER, counterparty TEXT,
    reasons_json TEXT, archive_version INTEGER, params_hash TEXT, code_hash TEXT,
    PRIMARY KEY (league, as_of, rec_key));
CREATE TABLE IF NOT EXISTS rec_players (
    league TEXT NOT NULL, as_of TEXT NOT NULL, rec_key TEXT NOT NULL,
    side TEXT NOT NULL,                        -- add | drop | subject
    cid TEXT NOT NULL, name TEXT, nhl_id INTEGER,
    PRIMARY KEY (league, as_of, rec_key, side, cid));
CREATE TABLE IF NOT EXISTS rec_episodes (
    episode_id TEXT PRIMARY KEY, league TEXT NOT NULL, rec_key TEXT NOT NULL, kind TEXT NOT NULL,
    title TEXT, first_seen TEXT NOT NULL, last_seen TEXT NOT NULL, n_days INTEGER NOT NULL,
    predicted_gain REAL, gain_units TEXT, horizon_days INTEGER, strength REAL,
    status TEXT NOT NULL DEFAULT 'open',       -- open | followed | partial | proposed | expired
    acted_on TEXT, window_end TEXT, match_json TEXT, updated_at TEXT);
CREATE INDEX IF NOT EXISTS ix_episodes_key ON rec_episodes(league, rec_key, first_seen);
CREATE TABLE IF NOT EXISTS transactions (
    league TEXT NOT NULL, source TEXT, tx_id TEXT NOT NULL, action TEXT NOT NULL, cid TEXT NOT NULL,
    team_id TEXT NOT NULL, ts TEXT NOT NULL, day TEXT NOT NULL, team_name TEXT, player_name TEXT,
    nhl_id INTEGER, group_id TEXT, counterparty_id TEXT, is_me INTEGER NOT NULL DEFAULT 0, pulled_at TEXT,
    PRIMARY KEY (league, tx_id, action, cid, team_id));
CREATE INDEX IF NOT EXISTS ix_tx_day ON transactions(league, day);
CREATE TABLE IF NOT EXISTS lineup_days (
    league TEXT NOT NULL, team_id TEXT NOT NULL, day TEXT NOT NULL, cid TEXT NOT NULL,
    slot TEXT, starting INTEGER, provider_pts REAL, is_me INTEGER NOT NULL DEFAULT 0, pulled_at TEXT,
    PRIMARY KEY (league, team_id, day, cid));
CREATE TABLE IF NOT EXISTS projections (
    league TEXT NOT NULL, as_of TEXT NOT NULL, cid TEXT NOT NULL,
    name TEXT, nhl_id INTEGER, team TEXT, positions TEXT, status TEXT, pct_owned REAL,
    fpg REAL, fpg_season REAL, fpg_week REAL, proj_week REAL, vorp REAL, games_next7 INTEGER,
    rates_json TEXT, projected_json TEXT, inputs_json TEXT, archive_version INTEGER,
    PRIMARY KEY (league, as_of, cid));
CREATE INDEX IF NOT EXISTS ix_proj_nhl ON projections(nhl_id, as_of);
CREATE TABLE IF NOT EXISTS realized_daily (
    nhl_id INTEGER NOT NULL, game_date TEXT NOT NULL, gp INTEGER NOT NULL, stats_json TEXT NOT NULL,
    pulled_at TEXT, PRIMARY KEY (nhl_id, game_date));
CREATE TABLE IF NOT EXISTS realized_pulls (
    game_date TEXT PRIMARY KEY, n_players INTEGER NOT NULL, pulled_at TEXT);
CREATE TABLE IF NOT EXISTS decisions (
    decision_id TEXT PRIMARY KEY, league TEXT NOT NULL, day TEXT NOT NULL,
    origin TEXT NOT NULL,                      -- followed | partial | proposed | user_only
    kind TEXT, episode_id TEXT, group_id TEXT, adds_json TEXT, drops_json TEXT, created_at TEXT);
CREATE INDEX IF NOT EXISTS ix_decisions_league ON decisions(league, day);
CREATE TABLE IF NOT EXISTS outcomes (
    outcome_id TEXT PRIMARY KEY, league TEXT, episode_id TEXT, decision_id TEXT,
    window TEXT NOT NULL,                      -- 7d | 28d | ros
    basis TEXT,                                -- first_seen | acted_on
    predicted_gain REAL, realized_gain REAL, gain_units TEXT, hit INTEGER, partial INTEGER,
    graded_at TEXT, detail_json TEXT,
    kind TEXT, origin TEXT,                    -- origin: followed | partial | proposed | ignored | open | user_only
    complete INTEGER NOT NULL DEFAULT 0,       -- 1 = window fully past and realized data for every day
    window_start TEXT, window_end TEXT, predicted_pts REAL);
CREATE TABLE IF NOT EXISTS metric_snapshots (
    snapshot_id TEXT PRIMARY KEY, week TEXT NOT NULL, league TEXT, metric TEXT NOT NULL, pool TEXT,
    value REAL, n INTEGER, ci_lo REAL, ci_hi REAL, trust TEXT, detail_json TEXT, created_at TEXT);
CREATE TABLE IF NOT EXISTS param_versions (
    version TEXT PRIMARY KEY, parent TEXT, params_hash TEXT, status TEXT, created_at TEXT,
    metrics_json TEXT, changelog TEXT);
CREATE TABLE IF NOT EXISTS deployment_daily (
    nhl_id INTEGER NOT NULL, game_date TEXT NOT NULL, team TEXT, game_id INTEGER, opponent TEXT,
    toi REAL, ev_toi REAL, pp_toi REAL, sh_toi REAL, shifts INTEGER,   -- minutes
    team_pp_toi REAL, pp_share REAL, pulled_at TEXT,
    PRIMARY KEY (nhl_id, game_date));
CREATE INDEX IF NOT EXISTS ix_deployment_date ON deployment_daily(game_date);
CREATE TABLE IF NOT EXISTS goalie_starts (
    nhl_id INTEGER NOT NULL, game_date TEXT NOT NULL, team TEXT, game_id INTEGER, opponent TEXT,
    started INTEGER NOT NULL, sa REAL, sv REAL, ga REAL, toi REAL, back_to_back INTEGER, pulled_at TEXT,
    PRIMARY KEY (nhl_id, game_date));
CREATE INDEX IF NOT EXISTS ix_goalie_starts_team ON goalie_starts(team, game_date);
CREATE TABLE IF NOT EXISTS deployment_pulls (
    game_date TEXT PRIMARY KEY, n_skaters INTEGER NOT NULL, n_goalies INTEGER NOT NULL, pulled_at TEXT);
CREATE TABLE IF NOT EXISTS matchup_snapshots (
    league TEXT NOT NULL, day TEXT NOT NULL, period INTEGER, period_start TEXT, period_end TEXT,
    opponent_id TEXT, opponent TEXT, days_left INTEGER, playoffs INTEGER,
    my_so_far REAL, their_so_far REAL, my_proj_total REAL, their_proj_total REAL,
    margin REAL, my_sd REAL, their_sd REAL, margin_sd REAL, win_prob REAL,
    skater_sd_k REAL, goalie_sd_k REAL, source TEXT, recorded_at TEXT,
    PRIMARY KEY (league, day));
CREATE TABLE IF NOT EXISTS matchup_results (
    league TEXT NOT NULL, period INTEGER NOT NULL, period_end TEXT NOT NULL, period_start TEXT,
    opponent_id TEXT, my_final REAL, their_final REAL,
    result TEXT,                               -- win | loss | tie
    source TEXT, recorded_at TEXT,
    PRIMARY KEY (league, period, period_end));
";

/// Columns added after schema v1 (`Ledger::migrate` adds them to older dbs).
pub const ADDED_COLUMNS: &[(&str, &[(&str, &str)])] = &[(
    "outcomes",
    &[
        ("kind", "TEXT"),
        ("origin", "TEXT"),
        ("complete", "INTEGER NOT NULL DEFAULT 0"),
        ("window_start", "TEXT"),
        ("window_end", "TEXT"),
        ("predicted_pts", "REAL"),
    ],
)];

pub const TABLES: &[&str] = &[
    "runs",
    "recs",
    "rec_players",
    "rec_episodes",
    "transactions",
    "lineup_days",
    "projections",
    "realized_daily",
    "decisions",
    "outcomes",
    "metric_snapshots",
    "param_versions",
    "archive_files",
    "realized_pulls",
];
/// NHL deployment pulls (harness.deployment). Not in [`TABLES`]: `fm harness rebuild` deletes
/// TABLES minus a keep-list, and these cannot be restored from the archive.
pub const DEPLOYMENT_TABLES: &[&str] = &["deployment_daily", "goalie_starts", "deployment_pulls"];
/// Matchup win-probability snapshots and final scores (harness.matchup_calib).
pub const MATCHUP_TABLES: &[&str] = &["matchup_snapshots", "matchup_results"];

/// Positional statement parameters.
pub type Params<'a> = &'a [&'a dyn ToSql];

/// One result row as a Python dict (column order of the SELECT).
pub type Row = JsonMap<String, Value>;

/// `sha1(league|kind|sorted add cids|sorted drop cids|counterparty or ""[|sorted subject cids])`;
/// the subject part only when the rec has subjects (IR moves, status alerts: they would
/// otherwise all collide on an empty add/drop).
pub fn rec_key<A, D, S>(
    league: &str,
    kind: &str,
    add: A,
    drop: D,
    counterparty: Option<&str>,
    subjects: S,
) -> String
where
    A: IntoIterator,
    A::Item: AsRef<str>,
    D: IntoIterator,
    D::Item: AsRef<str>,
    S: IntoIterator,
    S::Item: AsRef<str>,
{
    fn sorted<I: IntoIterator>(it: I) -> Vec<String>
    where
        I::Item: AsRef<str>,
    {
        let mut v: Vec<String> = it.into_iter().map(|s| s.as_ref().to_string()).collect();
        v.sort();
        v
    }
    let joined = |it: Vec<String>| it.join(",");
    let mut parts = vec![
        league.to_string(),
        kind.to_string(),
        joined(sorted(add)),
        joined(sorted(drop)),
        counterparty.unwrap_or("").to_string(),
    ];
    let subj = sorted(subjects);
    if !subj.is_empty() {
        parts.push(joined(subj));
    }
    let mut h = Sha1::new();
    h.update(parts.join("|").as_bytes());
    hex::encode(h.finalize())
}

/// `datetime.now().isoformat(timespec="seconds")`.
pub fn now_iso() -> String {
    Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// The sqlite ledger under `data_dir` (created on first use). Thread-safe (a mutex around the
/// connection, like Python's RLock).
pub struct Ledger {
    pub data_dir: PathBuf,
    pub path: PathBuf,
    db: Mutex<Connection>,
}

impl Ledger {
    /// `Ledger(data_dir)` with the default file name.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Ledger> {
        Ledger::open_named(data_dir, DB_NAME)
    }

    /// `Ledger(data_dir, filename)`.
    pub fn open_named(data_dir: impl AsRef<Path>, filename: &str) -> Result<Ledger> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("creating {}", data_dir.display()))?;
        let path = data_dir.join(filename);
        let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        // PRAGMA journal_mode returns a row.
        conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))?;
        conn.execute_batch(SCHEMA)?;
        let ledger = Ledger { data_dir, path, db: Mutex::new(conn) };
        ledger.migrate()?;
        {
            let conn = ledger.conn();
            begin_if_needed(&conn)?;
            conn.execute(
                "INSERT INTO meta(key, value) VALUES ('schema_version', ?) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [SCHEMA_VERSION.to_string()],
            )?;
        }
        ledger.commit()?;
        Ok(ledger)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Add columns introduced after a db was created (schema v1 -> v2: outcome columns).
    fn migrate(&self) -> Result<()> {
        let conn = self.conn();
        for (table, cols) in ADDED_COLUMNS {
            let mut have: Vec<String> = Vec::new();
            {
                let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
                for r in rows {
                    have.push(r?);
                }
            }
            for (name, decl) in cols.iter() {
                if !have.iter().any(|h| h == name) {
                    conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {decl}"), [])?;
                }
            }
        }
        conn.execute("CREATE INDEX IF NOT EXISTS ix_outcomes_league ON outcomes(league, kind, window)", [])?;
        conn.execute("CREATE INDEX IF NOT EXISTS ix_snapshots_week ON metric_snapshots(league, week)", [])?;
        Ok(())
    }

    // -- plumbing -----------------------------------------------------------

    /// Run `f` with the locked connection (for multi-statement work in other harness modules).
    /// Do not call other `Ledger` methods from inside `f` (the lock is not reentrant).
    pub fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        let conn = self.conn();
        f(&conn)
    }

    /// `db.execute(sql, params)`; returns the number of rows changed (0 for statements that
    /// return rows). Starts Python's implicit transaction before a DML statement.
    pub fn execute(&self, sql: &str, params: Params<'_>) -> Result<usize> {
        let conn = self.conn();
        if is_dml(sql) {
            begin_if_needed(&conn)?;
        }
        match conn.execute(sql, params) {
            Ok(n) => Ok(n),
            Err(rusqlite::Error::ExecuteReturnedResults) => {
                let mut stmt = conn.prepare(sql)?;
                let mut rows = stmt.query(params)?;
                while rows.next()?.is_some() {}
                Ok(0)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `cursor.lastrowid` of the most recent INSERT on this connection.
    pub fn last_insert_rowid(&self) -> i64 {
        self.conn().last_insert_rowid()
    }

    /// `[dict(r) for r in db.execute(sql, params).fetchall()]`.
    pub fn query(&self, sql: &str, params: Params<'_>) -> Result<Vec<Row>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(sql)?;
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = stmt.query(params)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let mut m = Row::new();
            for (i, n) in names.iter().enumerate() {
                m.insert(n.clone(), value_ref_to_json(r.get_ref(i)?));
            }
            out.push(m);
        }
        Ok(out)
    }

    pub fn commit(&self) -> Result<()> {
        let conn = self.conn();
        if !conn.is_autocommit() {
            conn.execute_batch("COMMIT")?;
        }
        Ok(())
    }

    /// `INSERT ... ON CONFLICT(keys) DO UPDATE` for every column (`DO NOTHING` when every column
    /// is a key); returns rows sent. Columns come from the first row; missing values are NULL.
    pub fn upsert(&self, table: &str, rows: &[Row], keys: &[&str]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let cols: Vec<&String> = rows[0].keys().collect();
        let updates: Vec<&&String> = cols.iter().filter(|c| !keys.contains(&c.as_str())).collect();
        let col_list = cols.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ");
        let marks = vec!["?"; cols.len()].join(", ");
        let tail = if updates.is_empty() {
            "NOTHING".to_string()
        } else {
            format!(
                "UPDATE SET {}",
                updates.iter().map(|c| format!("{c}=excluded.{c}")).collect::<Vec<_>>().join(", ")
            )
        };
        let sql = format!(
            "INSERT INTO {table} ({col_list}) VALUES ({marks}) ON CONFLICT({}) DO {tail}",
            keys.join(", ")
        );
        let conn = self.conn();
        begin_if_needed(&conn)?;
        {
            let mut stmt = conn.prepare(&sql)?;
            for r in rows {
                let vals: Vec<SqlValue> =
                    cols.iter().map(|c| json_to_sql(r.get(c.as_str()).unwrap_or(&Value::Null))).collect();
                stmt.execute(rusqlite::params_from_iter(vals.iter()))?;
            }
        }
        conn.execute_batch("COMMIT")?;
        Ok(rows.len())
    }

    /// `SELECT COUNT(*) FROM table {where}`.
    pub fn count(&self, table: &str, where_: &str, params: Params<'_>) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table} {where_}"), params, |r| r.get(0))?)
    }

    /// Delete every row of every table (`fm harness rebuild`).
    pub fn reset(&self) -> Result<()> {
        {
            let conn = self.conn();
            begin_if_needed(&conn)?;
            for t in TABLES.iter().chain(DEPLOYMENT_TABLES).chain(MATCHUP_TABLES) {
                conn.execute(&format!("DELETE FROM {t}"), [])?;
            }
        }
        self.commit()
    }

    // -- runs ---------------------------------------------------------------

    pub fn start_run(
        &self,
        command: &str,
        league: Option<&str>,
        as_of: Option<&str>,
        params_hash: Option<&str>,
        code_hash: Option<&str>,
    ) -> Result<i64> {
        let id;
        {
            let conn = self.conn();
            begin_if_needed(&conn)?;
            conn.execute(
                "INSERT INTO runs(command, league, as_of, started_at, status, params_hash, code_hash) \
                 VALUES (?,?,?,?,?,?,?)",
                rusqlite::params![command, league, as_of, now_iso(), "running", params_hash, code_hash],
            )?;
            id = conn.last_insert_rowid();
        }
        self.commit()?;
        Ok(id)
    }

    pub fn finish_run(&self, run_id: i64, status: &str, summary: Option<&Value>) -> Result<()> {
        let empty = Value::Object(Row::new());
        let summary_json = json_dumps(summary.unwrap_or(&empty), false);
        {
            let conn = self.conn();
            begin_if_needed(&conn)?;
            conn.execute(
                "UPDATE runs SET finished_at=?, status=?, summary_json=? WHERE run_id=?",
                rusqlite::params![now_iso(), status, summary_json, run_id],
            )?;
        }
        self.commit()
    }
}

fn is_dml(sql: &str) -> bool {
    let head = sql.trim_start().get(..7).unwrap_or(sql.trim_start()).to_ascii_uppercase();
    ["INSERT", "UPDATE", "DELETE", "REPLACE"].iter().any(|k| head.starts_with(k))
}

fn begin_if_needed(conn: &Connection) -> rusqlite::Result<()> {
    if conn.is_autocommit() {
        conn.execute_batch("BEGIN")?;
    }
    Ok(())
}

/// A sqlite value as the Python object `sqlite3` would hand back, as JSON.
pub fn value_ref_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        // NaN / inf cannot live in a serde_json Value (documented port deviation: null).
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(hex::encode(b)),
    }
}

/// A JSON value as a sqlite parameter (bool -> 0/1; containers -> their JSON text).
pub fn json_to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                SqlValue::Real(f)
            } else {
                SqlValue::Text(n.to_string())
            }
        }
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(json_dumps(other, false)),
    }
}

/// `json.dumps(value)` / `json.dumps(value, sort_keys=True)`: Python's default separators,
/// `ensure_ascii`, floats via `repr`. Shared with `episode_match`.
pub fn json_dumps(v: &Value, sort_keys: bool) -> String {
    fm_pycompat::pyjson::dumps(v, &fm_pycompat::pyjson::DumpOpts { sort_keys, ..Default::default() })
}
