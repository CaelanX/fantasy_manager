"""Injury alerts: status changes since the last snapshot, IR moves and IR activations."""
from __future__ import annotations

import sqlite3
import threading
import time
from pathlib import Path
from typing import Iterable, Mapping

from pydantic import BaseModel

from ..models import LeagueContext, Player, Reason, Recommendation
from ..valuation.adjust import Horizon
from ..valuation.valuate import PlayerValue
from .base import droppable_players, shares_slot
from .strength import IR_FLOOR, apply_ranks, apply_strength
from .waivers import recommend_waivers

SEVERITY = {"unknown": 0, "healthy": 0, "dtd": 1, "suspended": 2, "out": 2, "ir": 3, "ltir": 4}
IR_ELIGIBLE = ("ir", "ltir", "out")


def severity(status: str) -> int:
    return SEVERITY.get(status, 0)


class StatusSnapshot(BaseModel):
    cid: str
    status: str
    note: str | None = None
    seen_at: float


class StatusHistory:
    """Append-only status log (a row is written only when a player's status changes)."""

    def __init__(self, data_dir: Path | str):
        self.data_dir = Path(data_dir)
        self.data_dir.mkdir(parents=True, exist_ok=True)
        self._lock = threading.Lock()
        self._db = sqlite3.connect(self.data_dir / "status_history.db", check_same_thread=False)
        self._db.execute("CREATE TABLE IF NOT EXISTS status_history ("
                         " cid TEXT NOT NULL, status TEXT NOT NULL, note TEXT, seen_at REAL NOT NULL)")
        self._db.execute("CREATE INDEX IF NOT EXISTS ix_status_cid ON status_history(cid, seen_at)")
        self._db.commit()

    def last(self) -> dict[str, StatusSnapshot]:
        """Latest snapshot per player."""
        with self._lock:
            rows = self._db.execute(
                "SELECT h.cid, h.status, h.note, h.seen_at FROM status_history h"
                " JOIN (SELECT cid, MAX(seen_at) m FROM status_history GROUP BY cid) x"
                " ON h.cid = x.cid AND h.seen_at = x.m").fetchall()
        return {r[0]: StatusSnapshot(cid=r[0], status=r[1], note=r[2], seen_at=r[3]) for r in rows}

    def by_player(self) -> dict[str, list[StatusSnapshot]]:
        """Every recorded status change per player, oldest first."""
        with self._lock:
            rows = self._db.execute("SELECT cid, status, note, seen_at FROM status_history"
                                    " ORDER BY cid, seen_at").fetchall()
        out: dict[str, list[StatusSnapshot]] = {}
        for r in rows:
            out.setdefault(r[0], []).append(StatusSnapshot(cid=r[0], status=r[1], note=r[2], seen_at=r[3]))
        return out

    def record(self, players: Iterable[Player], when: float | None = None) -> int:
        """Store statuses that differ from the last snapshot. Returns rows written."""
        when = when or time.time()
        prev = self.last()
        rows = []
        for p in {p.cid: p for p in players}.values():
            old = prev.get(p.cid)
            if old is None or old.status != p.status or (old.note or None) != (p.status_note or None):
                rows.append((p.cid, p.status, p.status_note, when))
        if rows:
            with self._lock:
                self._db.executemany("INSERT INTO status_history(cid, status, note, seen_at) VALUES (?,?,?,?)", rows)
                self._db.commit()
        return len(rows)

    def close(self) -> None:
        self._db.close()


def _status_text(p: Player) -> str:
    return p.status + (f" ({p.status_note})" if p.status_note else "")


def _best_add(ctx: LeagueContext, values: Mapping[str, PlayerValue], injured: Player,
              horizon: Horizon) -> Player | None:
    """Best free agent to fill the spot opened by an IR move (reuses the waiver engine)."""
    return _best_add_rec(ctx, values, injured, horizon)[0]


def _best_add_rec(ctx: LeagueContext, values: Mapping[str, PlayerValue], injured: Player,
                  horizon: Horizon) -> tuple[Player | None, Recommendation | None]:
    """(best add, the waiver rec it came from or None for the open-spot fallback)."""
    for rec in recommend_waivers(ctx, values, limit=10, horizon=horizon):
        if rec.add and shares_slot(rec.add[0], injured):
            return rec.add[0], rec
    # the waiver engine demands a gain over a drop; with an open roster spot any positive VORP helps
    pool = [fa for fa in ctx.free_agents
            if fa.cid in values and shares_slot(fa, injured) and fa.status in ("healthy", "unknown", "dtd")]
    pool = [fa for fa in pool if values[fa.cid].vorp_for(horizon) > 0] or pool
    return max(pool, key=lambda fa: values[fa.cid].fpg_for(horizon), default=None), None


def _add_gain(add: Player, values: Mapping[str, PlayerValue], horizon: Horizon
              ) -> tuple[float, str, int | None]:
    """Gain of an add into an open spot (no drop): his projected week or his FPG."""
    av = values[add.cid]
    if horizon == "week" and av.proj_week is not None:
        return float(av.proj_week), "week_pts", 7
    return float(av.fpg_for(horizon)), "season_fpg", 7 if horizon == "week" else None


def recommend_injuries(ctx: LeagueContext, values: Mapping[str, PlayerValue],
                       history: Mapping[str, StatusSnapshot] | StatusHistory | None,
                       horizon: Horizon = "week") -> list[Recommendation]:
    """Worsened statuses since the last snapshot, IR moves (+ best add) and IR activations.

    ``history`` is a StatusHistory (its latest snapshot is read, not written) or a mapping
    cid -> StatusSnapshot. Call ``StatusHistory.record`` afterwards to advance the baseline.
    """
    prev = history.last() if isinstance(history, StatusHistory) else dict(history or {})
    team = ctx.my_team
    recs: list[Recommendation] = []

    def fpg(p: Player) -> float:
        pv = values.get(p.cid)
        return pv.fpg if pv else 0.0

    # 1. newly worsened statuses (my roster)
    for s in team.slots:
        p = s.player
        if p is None:
            continue
        old = prev.get(p.cid)
        if old is None or severity(p.status) <= severity(old.status):
            continue
        recs.append(Recommendation(
            kind="injury", score=fpg(p) * (1.0 + severity(p.status)),
            title=f"{p.name} now {p.status.upper()} (was {old.status})", drop=[], subjects=[p],
            reasons=[Reason(code="STATUS_CHANGE", text=f"{p.name}: {old.status} -> {_status_text(p)}",
                            value=float(severity(p.status)), baseline=float(severity(old.status)))]))

    # 2. move to IR when a slot is free
    ir_cap = int(ctx.roster_shape.get("IR", 0))
    ir_used = sum(1 for s in team.slots if s.slot == "IR" and s.player is not None)
    free_ir = max(0, ir_cap - ir_used)
    candidates = sorted((s.player for s in team.slots
                         if s.player is not None and s.slot != "IR" and s.player.status in IR_ELIGIBLE),
                        key=lambda p: (-severity(p.status), -fpg(p)))
    for p in candidates[:free_ir]:
        moved = ctx.model_copy(deep=True)
        mt = moved.my_team
        for slot in mt.slots:
            if slot.player is not None and slot.player.cid == p.cid:
                slot.slot, slot.starting = "IR", False
        add, wrec = _best_add_rec(moved, values, p, horizon)
        reasons = [Reason(code="STATUS", text=f"{p.name} is {_status_text(p)}"),
                   Reason(code="IR_SLOT", text=f"{free_ir} of {ir_cap} IR slot(s) free", value=float(free_ir))]
        if p.status == "out":
            reasons.append(Reason(code="IR_RULES", text="Status is OUT, not IR: check your league allows it in IR"))
        title = f"Move {p.name} to IR"
        if add is not None:
            av = values[add.cid]
            title += f" and add {add.name}"
            reasons.append(Reason(code="WAIVER_ADD",
                                  text=f"{add.name} ({'/'.join(x for x in add.positions if x != 'F')}, "
                                       f"{add.team or 'FA'}): {av.fpg_for(horizon):.2f} FPG, no drop needed",
                                  value=av.fpg_for(horizon)))
        if wrec is not None:
            gain, units, days = wrec.predicted_gain, wrec.gain_units, wrec.horizon_days
        elif add is not None:
            gain, units, days = _add_gain(add, values, horizon)
        else:
            gain, units, days = None, None, None
        rec = Recommendation(kind="injury", score=(values[add.cid].fpg_for(horizon) if add else 0.0) + 0.5,
                             title=title, add=[add] if add else [], drop=[], reasons=reasons, subjects=[p],
                             predicted_gain=gain, gain_units=units, horizon_days=days)
        if wrec is not None and wrec.strength is not None:
            rec.strength = round(max(IR_FLOOR, wrec.strength), 2)   # inherit the waiver add's strength
        recs.append(rec)

    # 3. activate healthy players from IR
    droppable = [d for d in droppable_players(team) if d.cid in values]
    for s in team.slots:
        p = s.player
        if p is None or s.slot != "IR" or p.status not in ("healthy",):
            continue
        reasons = [Reason(code="STATUS", text=f"{p.name} is healthy but in an IR slot")]
        drop = None
        full = len([x for x in team.slots if x.player is not None and x.slot != "IR"]) >= \
            sum(n for k, n in ctx.roster_shape.items() if k != "IR")
        if full and droppable:
            drop = min(droppable, key=lambda d: values[d.cid].fpg_for("season"))
            reasons.append(Reason(code="DROP", text=f"Roster is full: weakest is {drop.name} "
                                                    f"({values[drop.cid].fpg_for('season'):.2f} FPG)",
                                  value=values[drop.cid].fpg_for("season")))
        pv = values.get(p.cid)
        gain = None
        if pv is not None:
            gain = pv.fpg_season - (values[drop.cid].fpg_season if drop is not None else 0.0)
        recs.append(Recommendation(kind="injury", score=fpg(p) + 1.0, title=f"Activate {p.name} from IR",
                                   add=[p], drop=[drop] if drop else [], reasons=reasons,
                                   predicted_gain=gain, gain_units="season_fpg" if gain is not None else None))
    recs.sort(key=lambda r: r.score, reverse=True)
    return apply_ranks(apply_strength(recs))
