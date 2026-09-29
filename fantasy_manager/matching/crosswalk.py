"""Persistent crosswalk from provider player ids (ESPN / Fantrax) to canonical NHL ids.

Confident matches (``exact`` / ``high`` from :func:`match_player`) are stored and reused;
ambiguous ones are recorded as ``pending`` for ``fm sync --review`` and can be settled with
``fm sync --confirm espn:123=8478402``. ``Player.cid`` is never changed: the NHL id is
written to ``player.ids["nhl"]`` (exposed as ``Player.nhl_id``).
"""
from __future__ import annotations

import sqlite3
import threading
import time
from pathlib import Path
from typing import Any, Iterable

from pydantic import BaseModel

from ..models import Player
from .matcher import Candidate, PlayerIndex, match_player

STORED = ("exact", "high", "confirmed")


class XrefRow(BaseModel):
    source: str
    source_id: str
    nhl_id: int | None = None
    name: str = ""
    team: str | None = None
    confidence: str = "none"          # exact | high | confirmed | pending | none
    candidate_name: str | None = None
    score: float | None = None
    updated: float = 0.0


class ResolveStats(BaseModel):
    resolved: int = 0     # players that now carry ids["nhl"]
    new: int = 0          # of which matched for the first time this run
    pending: int = 0
    unmatched: int = 0

    def summary(self) -> str:
        return (f"{self.resolved} matched to NHL ids ({self.new} new), "
                f"{self.pending} pending review, {self.unmatched} unmatched")


def split_cid(cid: str) -> tuple[str, str]:
    """'espn:123' -> ('espn', '123'); an unprefixed id gets source 'unknown'."""
    if ":" in cid:
        src, sid = cid.split(":", 1)
        return src, sid
    return "unknown", cid


def parse_confirm(spec: str) -> tuple[str, str, int]:
    """'espn:123=8478402' -> ('espn', '123', 8478402)."""
    try:
        left, right = spec.split("=", 1)
        src, sid = split_cid(left.strip())
        if src == "unknown" or not sid:
            raise ValueError
        return src, sid, int(right.strip())
    except ValueError as e:
        raise ValueError(f"bad --confirm value {spec!r}; expected SOURCE:ID=NHLID, e.g. espn:123=8478402") from e


def _match_position(p: Player) -> str | None:
    pos = [x for x in p.positions if x != "F"] or p.positions
    return "/".join(pos) if pos else None


def nhl_candidates(*sources: Iterable[Any]) -> list[Candidate]:
    """Candidates keyed by NHL player id from NHL season rows / roster players.

    Later sources override earlier ones for the same id, so pass season rows first and
    current rosters last (rosters carry the up-to-date team and include prospects).
    """
    by_id: dict[int, Candidate] = {}
    for items in sources:
        for it in items or []:
            pid = it.key if isinstance(it, Candidate) else getattr(it, "player_id", None)
            name = getattr(it, "name", None)
            if pid is None or not name:
                continue
            prev = by_id.get(int(pid))
            by_id[int(pid)] = Candidate(key=int(pid), name=name,
                                        team=getattr(it, "team", None) or (prev.team if prev else None),
                                        position=getattr(it, "position", None) or (prev.position if prev else None))
    return list(by_id.values())


class Crosswalk:
    def __init__(self, data_dir: Path | str):
        self.data_dir = Path(data_dir)
        self.data_dir.mkdir(parents=True, exist_ok=True)
        self.path = self.data_dir / "crosswalk.db"
        self._lock = threading.Lock()
        self._db = sqlite3.connect(self.path, check_same_thread=False)
        self._db.execute(
            "CREATE TABLE IF NOT EXISTS player_xref ("
            " source TEXT NOT NULL, source_id TEXT NOT NULL, nhl_id INTEGER, name TEXT, team TEXT,"
            " confidence TEXT NOT NULL, candidate_name TEXT, score REAL, updated REAL NOT NULL,"
            " PRIMARY KEY (source, source_id))")
        self._db.commit()

    # -- storage -------------------------------------------------------------
    _COLS = "source, source_id, nhl_id, name, team, confidence, candidate_name, score, updated"

    def _row(self, r: tuple) -> XrefRow:
        return XrefRow(**dict(zip(self._COLS.split(", "), r)))

    def get(self, source: str, source_id: str) -> XrefRow | None:
        with self._lock:
            r = self._db.execute(f"SELECT {self._COLS} FROM player_xref WHERE source=? AND source_id=?",
                                 (source, str(source_id))).fetchone()
        return self._row(r) if r else None

    def _put(self, row: XrefRow) -> None:
        with self._lock:
            self._db.execute(
                f"INSERT OR REPLACE INTO player_xref({self._COLS}) VALUES (?,?,?,?,?,?,?,?,?)",
                (row.source, row.source_id, row.nhl_id, row.name, row.team, row.confidence,
                 row.candidate_name, row.score, row.updated or time.time()))
            self._db.commit()

    def all(self) -> list[XrefRow]:
        with self._lock:
            rows = self._db.execute(f"SELECT {self._COLS} FROM player_xref ORDER BY source, name").fetchall()
        return [self._row(r) for r in rows]

    def pending(self, include_unmatched: bool = False) -> list[XrefRow]:
        wanted = ("pending", "none") if include_unmatched else ("pending",)
        return [r for r in self.all() if r.confidence in wanted]

    def confirm(self, source: str, source_id: str, nhl_id: int) -> XrefRow:
        """Pin a mapping by hand; confirmed rows are never re-matched."""
        row = self.get(source, source_id) or XrefRow(source=source, source_id=str(source_id))
        row.nhl_id = int(nhl_id)
        row.confidence = "confirmed"
        row.updated = time.time()
        self._put(row)
        return row

    def close(self) -> None:
        self._db.close()

    # -- resolution ------------------------------------------------------------
    def resolve(self, players: list[Player], nhl_candidates: list[Candidate] | PlayerIndex) -> ResolveStats:
        """Attach ``ids["nhl"]`` to each player (stored mapping first, else fuzzy match)."""
        index = nhl_candidates if isinstance(nhl_candidates, PlayerIndex) else PlayerIndex(nhl_candidates)
        stats = ResolveStats()
        now = time.time()
        for p in players:
            source, sid = split_cid(p.cid)
            if p.nhl_id is not None:  # provider already knows the NHL id
                stats.resolved += 1
                if (self.get(source, sid) or XrefRow(source="", source_id="")).confidence not in STORED:
                    self._put(XrefRow(source=source, source_id=sid, nhl_id=p.nhl_id, name=p.name, team=p.team,
                                      confidence="exact", updated=now))
                continue
            stored = self.get(source, sid)
            if stored and stored.confidence in STORED and stored.nhl_id is not None:
                p.ids["nhl"] = str(stored.nhl_id)
                stats.resolved += 1
                continue
            if not len(index):
                continue
            m = match_player(p.name, index, team=p.team, position=_match_position(p))
            row = XrefRow(source=source, source_id=sid, name=p.name, team=p.team, confidence=m.confidence,
                          candidate_name=m.name, score=m.score or None, updated=now,
                          nhl_id=int(m.key) if m.key is not None else None)
            self._put(row)
            if m.matched:
                p.ids["nhl"] = str(int(m.key))
                stats.resolved += 1
                stats.new += 1
            elif m.confidence == "pending":
                stats.pending += 1
            else:
                stats.unmatched += 1
        return stats
