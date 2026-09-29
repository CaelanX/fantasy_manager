"""The harness ledger: one sqlite file at ``<fm_data_dir>/harness.db``.

The JSON archive (``<fm_data_dir>/archive``) stays the source of truth; everything here can be
rebuilt from it plus the provider / NHL pulls (``fm harness rebuild``). Every write is an
idempotent upsert keyed by natural keys, so re-running a step never duplicates rows.

Tables (M1 fills the first nine, M2 grading fills ``outcomes`` and ``metric_snapshots``;
``param_versions`` is populated by refitting in M3):

* ``runs``              one row per harness command run (what ran, hashes, summary)
* ``recs``              every archived recommendation, per league and day (``rec_key``)
* ``rec_players``       add / drop / subject players of each archived recommendation
* ``rec_episodes``      the same rec on consecutive days rolled into one episode + match status
* ``transactions``      provider activity (adds, drops, trades, IR moves, proposals)
* ``lineup_days``       per-day lineup slots (ESPN box scores, Fantrax roster snapshots)
* ``projections``       archived per-player projections (fm numbers + v2 inputs)
* ``realized_daily``    raw NHL stats per player per game date (scored at grade time)
* ``decisions``         my moves: followed / partial (linked to an episode) or user_only
* ``outcomes``          realized gain per episode / decision and window (``harness.outcomes``)
* ``metric_snapshots``  weekly accuracy / hit-rate snapshots with trust labels (``harness.metrics``)
* ``param_versions``    fitted parameter versions, parents and changelog (M3)

Bookkeeping: ``archive_files`` (sha1 of every ingested archive file, so unchanged files are
skipped) and ``realized_pulls`` (which game dates were pulled, including empty ones).

Deployment (``harness.deployment``, NHL per-game reports; ``DEPLOYMENT_TABLES``, kept by
``fm harness rebuild`` like the other pulled tables because the archive cannot restore them):

* ``deployment_daily``  per skater per game date: TOI / EV / PP / SH minutes, shifts, the team's
                        PP minutes that game and the skater's share of them
* ``goalie_starts``     per goalie per game date: started, SA / SV / GA, TOI minutes and whether
                        the team also played the previous day (``back_to_back``)
* ``deployment_pulls``  which game dates were pulled (including empty ones)
"""
from __future__ import annotations

import hashlib
import json
import sqlite3
import threading
from datetime import datetime
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

DB_NAME = "harness.db"
SCHEMA_VERSION = 2

SCHEMA = """
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
"""

# Columns added after schema v1 (``Ledger._migrate`` adds them to older dbs).
ADDED_COLUMNS: dict[str, dict[str, str]] = {
    "outcomes": {"kind": "TEXT", "origin": "TEXT", "complete": "INTEGER NOT NULL DEFAULT 0",
                 "window_start": "TEXT", "window_end": "TEXT", "predicted_pts": "REAL"},
}

TABLES = ("runs", "recs", "rec_players", "rec_episodes", "transactions", "lineup_days", "projections",
          "realized_daily", "decisions", "outcomes", "metric_snapshots", "param_versions",
          "archive_files", "realized_pulls")
# NHL deployment pulls (harness.deployment). Not in TABLES: `fm harness rebuild` deletes TABLES
# minus a keep-list, and these cannot be restored from the archive.
DEPLOYMENT_TABLES = ("deployment_daily", "goalie_starts", "deployment_pulls")


def rec_key(league: str, kind: str, add: Iterable[str], drop: Iterable[str], counterparty: str | None = None,
            subjects: Iterable[str] = ()) -> str:
    """sha1(league|kind|sorted add cids|sorted drop cids|counterparty or ""), plus
    ``|sorted subject cids`` only when the rec has subjects (IR moves, status alerts: they
    would otherwise all collide on an empty add/drop)."""
    parts = [league, kind, ",".join(sorted(add)), ",".join(sorted(drop)), counterparty or ""]
    subj = sorted(subjects)
    if subj:
        parts.append(",".join(subj))
    return hashlib.sha1("|".join(parts).encode("utf-8")).hexdigest()


def now_iso() -> str:
    return datetime.now().isoformat(timespec="seconds")


class Ledger:
    """sqlite ledger under ``data_dir`` (created on first use). Thread-safe for simple use."""

    def __init__(self, data_dir: Path | str, filename: str = DB_NAME):
        self.data_dir = Path(data_dir)
        self.data_dir.mkdir(parents=True, exist_ok=True)
        self.path = self.data_dir / filename
        self._lock = threading.RLock()
        self.db = sqlite3.connect(self.path, check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.executescript(SCHEMA)
        self._migrate()
        self.db.execute("INSERT INTO meta(key, value) VALUES ('schema_version', ?) "
                        "ON CONFLICT(key) DO UPDATE SET value=excluded.value", (str(SCHEMA_VERSION),))
        self.db.commit()

    def _migrate(self) -> None:
        """Add columns introduced after a db was created (schema v1 -> v2: outcome columns)."""
        for table, cols in ADDED_COLUMNS.items():
            have = {r[1] for r in self.db.execute(f"PRAGMA table_info({table})").fetchall()}
            for name, decl in cols.items():
                if name not in have:
                    self.db.execute(f"ALTER TABLE {table} ADD COLUMN {name} {decl}")
        self.db.execute("CREATE INDEX IF NOT EXISTS ix_outcomes_league ON outcomes(league, kind, window)")
        self.db.execute("CREATE INDEX IF NOT EXISTS ix_snapshots_week ON metric_snapshots(league, week)")

    # -- plumbing -----------------------------------------------------------
    def close(self) -> None:
        with self._lock:
            self.db.close()

    def __enter__(self) -> "Ledger":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def execute(self, sql: str, params: Sequence[Any] | Mapping[str, Any] = ()) -> sqlite3.Cursor:
        with self._lock:
            return self.db.execute(sql, params)

    def query(self, sql: str, params: Sequence[Any] | Mapping[str, Any] = ()) -> list[dict[str, Any]]:
        with self._lock:
            return [dict(r) for r in self.db.execute(sql, params).fetchall()]

    def commit(self) -> None:
        with self._lock:
            self.db.commit()

    def upsert(self, table: str, rows: Iterable[Mapping[str, Any]], keys: Sequence[str]) -> int:
        """INSERT ... ON CONFLICT(keys) DO UPDATE for every column; returns rows sent."""
        rows = [dict(r) for r in rows]
        if not rows:
            return 0
        cols = list(rows[0])
        updates = [c for c in cols if c not in keys]
        sql = (f"INSERT INTO {table} ({', '.join(cols)}) VALUES ({', '.join('?' for _ in cols)}) "
               f"ON CONFLICT({', '.join(keys)}) DO "
               + (f"UPDATE SET {', '.join(f'{c}=excluded.{c}' for c in updates)}" if updates else "NOTHING"))
        with self._lock:
            self.db.executemany(sql, [tuple(r.get(c) for c in cols) for r in rows])
            self.db.commit()
        return len(rows)

    def count(self, table: str, where: str = "", params: Sequence[Any] = ()) -> int:
        with self._lock:
            return int(self.db.execute(f"SELECT COUNT(*) FROM {table} {where}", params).fetchone()[0])

    def reset(self) -> None:
        """Drop every row of every table (``fm harness rebuild``)."""
        with self._lock:
            for t in TABLES + DEPLOYMENT_TABLES:
                self.db.execute(f"DELETE FROM {t}")
            self.db.commit()

    # -- runs ---------------------------------------------------------------
    def start_run(self, command: str, league: str | None = None, as_of: str | None = None,
                  params_hash: str | None = None, code_hash: str | None = None) -> int:
        with self._lock:
            cur = self.db.execute("INSERT INTO runs(command, league, as_of, started_at, status, params_hash, code_hash)"
                                  " VALUES (?,?,?,?,?,?,?)",
                                  (command, league, as_of, now_iso(), "running", params_hash, code_hash))
            self.db.commit()
            return int(cur.lastrowid)

    def finish_run(self, run_id: int, status: str = "ok", summary: Mapping[str, Any] | None = None) -> None:
        with self._lock:
            self.db.execute("UPDATE runs SET finished_at=?, status=?, summary_json=? WHERE run_id=?",
                            (now_iso(), status, json.dumps(summary or {}, default=str), run_id))
            self.db.commit()
