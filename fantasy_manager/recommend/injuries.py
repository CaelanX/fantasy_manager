"""Injury alerts: status changes since the last snapshot, IR moves and IR activations.

The IR move itself is free (ESPN counts acquisitions, not IR moves), but the replacement add costs
one move: it is only proposed while the league's move limit has one left (an injury replacement
clears the last move, see ``base.move_scarcity_threshold``), with a MOVE_BUDGET reason; with none
left the IR move is still proposed with a NO_MOVES note. Activating a player from IR never drops a
player I added in the last 7 days unless he is out / IR / suspended (churn guard).

Hold or cut (``hold_or_cut``): for each player of mine who is out / IR / LTIR / suspended, is it better
to keep him through the absence or drop him for the best free agent at a shared slot? Over the rest
of the season (R games), holding is worth his healthy season value per game x the games after his
estimated return (R - missed), cutting is worth the free agent's season value x R. Holding is free
when he sits in an IR slot or an IR slot is open for him (verdict HOLD, nothing to compare); then
the IR-move rec carries a HOLD_VERDICT reason. Otherwise he blocks an active spot and the verdict is
CUT only when the cut beats holding by CUT_EDGE (CUT_EDGE_GUESSED when the return date is assumed,
not read from a note: a guessed absence leans toward keeping a high producer), with the break-even
("cut only if he misses more than ~N games"). Dynasty leagues never cut a protected player or one
worth more than DYNASTY_MIN_RATIO x the replacement long-term. A cut uses an add (MOVE_BUDGET)."""
from __future__ import annotations

import sqlite3
import threading
import time
from pathlib import Path
from typing import Any, Iterable, Mapping

from pydantic import BaseModel

from ..matching.normalize import normalize_team
from ..models import LeagueContext, Player, Reason, Recommendation
from ..valuation.adjust import Horizon, _games_remaining, season_availability
from ..valuation.valuate import PlayerValue
from .base import (CHURN_EXEMPT_STATUSES, IR_SLOT_NAMES, cap_text, capped_positions, churn_text, droppable_players,
                   dynasty_map, moves_budget_reason, moves_left, no_moves_text, player_age, position_room,
                   protection_reason, recently_added, roster_legal_after, shares_slot)
from .strength import IR_FLOOR, apply_ranks, apply_strength
from .waivers import DYNASTY_MIN_RATIO, recommend_waivers

SEVERITY = {"unknown": 0, "healthy": 0, "dtd": 1, "suspended": 2, "out": 2, "ir": 3, "ltir": 4}
IR_ELIGIBLE = ("ir", "ltir", "out")
HOLD_CUT_STATUSES = ("out", "ir", "ltir", "suspended")
# a cut must beat holding by this share of the hold points (more when the return date is a guess)
CUT_EDGE = 0.10
CUT_EDGE_GUESSED = 0.25
HOLD_STRENGTH = 1.0          # a hold verdict is advice not to act: shown, never ranked above real moves


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
    """(best add, the waiver rec it came from or None for the open-spot fallback). Only no-drop
    adds qualify: a pickup that would break a per-position roster maximum (the IR player still
    counts toward it) is skipped. (None, None) when no moves are left this period."""
    if moves_left(ctx) == 0:
        return None, None
    for rec in recommend_waivers(ctx, values, limit=10, horizon=horizon):
        if rec.add and not rec.drop and shares_slot(rec.add[0], injured):
            return rec.add[0], rec
    # the waiver engine demands a gain over a drop; with an open roster spot any positive VORP helps
    team = ctx.my_team
    pool = [fa for fa in ctx.free_agents
            if fa.cid in values and shares_slot(fa, injured) and fa.status in ("healthy", "unknown", "dtd")
            and position_room(team, ctx, fa) > 0]
    pool = [fa for fa in pool if values[fa.cid].vorp_for(horizon) > 0] or pool
    return max(pool, key=lambda fa: values[fa.cid].fpg_for(horizon), default=None), None


def _add_gain(add: Player, values: Mapping[str, PlayerValue], horizon: Horizon
              ) -> tuple[float, str, int | None]:
    """Gain of an add into an open spot (no drop): his projected week or his FPG."""
    av = values[add.cid]
    if horizon == "week" and av.proj_week is not None:
        return float(av.proj_week), "week_pts", 7
    return float(av.fpg_for(horizon)), "season_fpg", 7 if horizon == "week" else None


class Absence(BaseModel):
    """How long `p` is out over the rest of the season and what he is worth when back."""
    remaining: float             # his team's games left
    missed: float
    healthy_fpg: float           # season value per game once healthy (start share / games share kept)
    back: str                    # "back ~Nov 21" / "return unknown"
    source: str
    guessed: bool                # the length is assumed (no timetable in the note)

    @property
    def hold_pts(self) -> float:
        return self.healthy_fpg * max(0.0, self.remaining - self.missed)


def absence(ctx: LeagueContext, p: Player, pv: PlayerValue) -> Absence:
    team_dates = ctx.schedule.get(normalize_team(p.team) or "", [])
    a_season, est = season_availability(p.status, p.status_note, ctx.as_of, team_dates, ctx.season_start)
    healthy = pv.fpg_season / a_season if a_season > 0 else pv.fpg
    if est is None:      # nothing to read: the flat multiplier, i.e. a guess
        remaining = _games_remaining(ctx.as_of, team_dates, ctx.season_start)
        return Absence(remaining=remaining, missed=remaining * (1.0 - a_season), healthy_fpg=healthy,
                       back="return unknown", source=f"no timetable, flat {p.status} value x{a_season:.2f}",
                       guessed=True)
    ret = est.return_date
    back = f"back ~{ret:%b} {ret.day}" if ret is not None else "out for the season"
    return Absence(remaining=est.games_remaining, missed=est.games_missed, healthy_fpg=healthy, back=back,
                   source=est.source, guessed="assumed" in est.source)


def _replacement(ctx: LeagueContext, values: Mapping[str, PlayerValue], p: Player) -> Player | None:
    """Best free agent (season value) on an NHL team who can take `p`'s roster spot."""
    team = ctx.my_team
    pool = [fa for fa in ctx.free_agents
            if fa.cid in values and normalize_team(fa.team) and shares_slot(fa, p)
            and fa.status in ("healthy", "unknown", "dtd")
            and roster_legal_after(team, ctx, add=[fa], drop=[p])[0]]
    return max(pool, key=lambda fa: values[fa.cid].fpg_season, default=None)


def _hold_rec(p: Player, ab: Absence, title: str, reasons: list[Reason]) -> Recommendation:
    return Recommendation(kind="injury", score=0.0, title=title, subjects=[], strength=HOLD_STRENGTH,
                          reasons=[Reason(code="RETURN_ESTIMATE",
                                          text=f"{p.name}: {ab.back}, ~{ab.missed:.0f} of {ab.remaining:.0f} games "
                                               f"missed ({ab.source}" + (", a guess)" if ab.guessed else ")"),
                                          value=round(ab.missed, 1), baseline=round(ab.remaining, 1))] + reasons)


def hold_or_cut(ctx: LeagueContext, values: Mapping[str, PlayerValue], players: Iterable[Player],
                in_ir: Iterable[Player] = (), dynasty_values: Mapping[str, Any] | None = None
                ) -> list[Recommendation]:
    """Hold-or-cut verdicts (module docstring) for `players` (injured, blocking an active spot) and
    `in_ir` (injured, already in an IR slot: holding is free)."""
    recs: list[Recommendation] = []
    for p in in_ir:
        pv = values.get(p.cid)
        if pv is None:
            continue
        ab = absence(ctx, p, pv)
        recs.append(_hold_rec(p, ab, f"Hold {p.name} in IR ({ab.back}): it costs no roster spot",
                              [Reason(code="HOLD_VERDICT", text=f"{p.name} sits in an IR slot, so keeping him "
                                      f"costs nothing; ~{ab.hold_pts:.0f} pts after his return",
                                      value=round(ab.hold_pts, 1))]))
    dyn = dynasty_map(ctx, values, dynasty_values)
    for p in players:
        pv = values.get(p.cid)
        fa = _replacement(ctx, values, p)
        if pv is None or fa is None:
            continue
        ab = absence(ctx, p, pv)
        r = values[fa.cid].fpg_season
        hold, cut = ab.hold_pts, r * ab.remaining
        edge = CUT_EDGE_GUESSED if ab.guessed else CUT_EDGE
        # games missed at which cutting starts to win by `edge`: h (R - n)(1 + edge) = r R
        h = ab.healthy_fpg
        breakeven = ab.remaining - r * ab.remaining / (h * (1.0 + edge)) if h > 0 else 0.0
        reasons = [
            Reason(code="HOLD_PTS", text=f"Hold: {h:.2f} FPG x {max(0.0, ab.remaining - ab.missed):.0f} games after "
                   f"his return = {hold:.0f} pts (his roster spot is dead until then: no IR slot free)",
                   value=round(hold, 1)),
            Reason(code="CUT_PTS", text=f"Cut: {fa.name} ({'/'.join(x for x in fa.positions if x != 'F')}, "
                   f"{fa.team}) {r:.2f} FPG x {ab.remaining:.0f} games = {cut:.0f} pts", value=round(cut, 1)),
            Reason(code="BREAK_EVEN",
                   text=(f"Cut only if he misses more than ~{breakeven:.0f} games" if 0 < breakeven < ab.remaining
                         else f"{fa.name} out-produces him even healthy: cut" if breakeven <= 0
                         else f"Holding wins however long he is out"), value=round(breakeven, 1),
                   baseline=round(ab.missed, 1)),
        ]
        protect = None
        if dyn is not None:
            d_p, d_fa = dyn.get(p.cid, 0.0), dyn.get(fa.cid, 0.0)
            why = protection_reason(p, player_age(p, ctx, dynasty_values))
            if why:
                protect = f"protected dynasty asset ({why})"
            elif d_p > DYNASTY_MIN_RATIO * d_fa:
                protect = f"long-term value {d_p:.1f} vs {fa.name} {d_fa:.1f}"
            reasons.append(Reason(code="DYNASTY_COST", text=f"Dynasty value {p.name} {d_p:.1f} vs {fa.name} "
                                  f"{d_fa:.1f}" + (f": cutting gives up {d_p - d_fa:.1f}" if d_p > d_fa else ""),
                                  value=d_fa - d_p, baseline=d_p))
        if cut <= hold * (1.0 + edge) or protect is not None:
            if protect is not None and cut > hold * (1.0 + edge):
                why_hold = f"keep him: {protect}"
            elif cut > hold:
                why_hold = (f"close call, cutting for {fa.name} gains only {cut - hold:.0f} pts "
                            f"(under the {edge:.0%} margin{' for a guessed return' if ab.guessed else ''})")
            else:
                why_hold = f"{hold - cut:+.0f} pts vs cutting for {fa.name}"
            title = f"Hold {p.name} through his injury ({ab.back}): {why_hold}"
            recs.append(_hold_rec(p, ab, title, reasons))
            continue
        if (budget := moves_budget_reason(ctx)) is not None:
            reasons.append(budget)
        if moves_left(ctx) == 0:
            reasons.append(Reason(code="NO_MOVES", text=no_moves_text(ctx) + ": make the cut when moves reset",
                                  value=0.0))
        gain = (cut - hold) / ab.remaining if ab.remaining > 0 else 0.0
        recs.append(Recommendation(
            kind="injury", score=gain + 0.5, title=f"Cut {p.name}, add {fa.name}: {cut - hold:+.0f} pts rest of season",
            add=[fa], drop=[p], predicted_gain=gain, gain_units="season_fpg",
            reasons=[Reason(code="RETURN_ESTIMATE",
                            text=f"{p.name}: {ab.back}, ~{ab.missed:.0f} of {ab.remaining:.0f} games missed "
                                 f"({ab.source}" + (", a guess)" if ab.guessed else ")"),
                            value=round(ab.missed, 1), baseline=round(ab.remaining, 1))] + reasons))
    return recs


def recommend_injuries(ctx: LeagueContext, values: Mapping[str, PlayerValue],
                       history: Mapping[str, StatusSnapshot] | StatusHistory | None,
                       horizon: Horizon = "week",
                       dynasty_values: Mapping[str, Any] | None = None) -> list[Recommendation]:
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
        capped = capped_positions(mt, moved, p)
        if add is None and capped:
            reasons.append(Reason(code="POSITION_CAP",
                                  text=f"No {'/'.join(capped)} pickup without a drop: "
                                       + ", ".join(cap_text(moved, k) for k in capped)
                                       + " reached (players in IR still count)"))
        title = f"Move {p.name} to IR"
        if add is not None:
            av = values[add.cid]
            title += f" and add {add.name}"
            reasons.append(Reason(code="WAIVER_ADD",
                                  text=f"{add.name} ({'/'.join(x for x in add.positions if x != 'F')}, "
                                       f"{add.team or 'FA'}): {av.fpg_for(horizon):.2f} FPG, no drop needed",
                                  value=av.fpg_for(horizon)))
        if add is not None and (budget := moves_budget_reason(moved)) is not None:
            reasons.append(budget)
        elif add is None and moves_left(moved) == 0:
            reasons.append(Reason(code="NO_MOVES", text=no_moves_text(moved) + " (the IR move itself is free)",
                                  value=0.0))
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
        if (pv := values.get(p.cid)) is not None:
            ab = absence(ctx, p, pv)
            rec.reasons.append(Reason(code="HOLD_VERDICT",
                                      text=f"Hold, don't cut: with an IR slot free keeping {p.name} costs no roster "
                                           f"spot ({ab.back}, ~{ab.hold_pts:.0f} pts after his return)",
                                      value=round(ab.hold_pts, 1)))
        recs.append(rec)

    # 2b. hold or cut: injured players with no free IR slot, and those already in one
    moving = {p.cid for p in candidates[:free_ir]}
    blocking = [s.player for s in team.slots if s.player is not None and s.slot not in IR_SLOT_NAMES
                and s.player.status in HOLD_CUT_STATUSES and s.player.cid not in moving]
    stashed = [s.player for s in team.slots if s.player is not None and s.slot in IR_SLOT_NAMES
               and s.player.status in HOLD_CUT_STATUSES]
    # an IR slot held by a healthy / day-to-day player can be swapped: holding is free there too
    swappable = [s.player for s in team.slots if s.player is not None and s.slot in IR_SLOT_NAMES
                 and s.player.status in ("healthy", "dtd")]
    blocking.sort(key=lambda p: -(values[p.cid].fpg if p.cid in values else 0.0))
    for p, back in zip(blocking, swappable):
        if (pv := values.get(p.cid)) is None:
            continue
        ab = absence(ctx, p, pv)
        recs.append(_hold_rec(p, ab, f"Hold {p.name} ({ab.back}): activate {back.name} ({back.status}) and move "
                                     f"{p.name} into his IR slot",
                              [Reason(code="HOLD_VERDICT", text=f"Swapping IR slots makes keeping {p.name} free; "
                                      f"~{ab.hold_pts:.0f} pts after his return", value=round(ab.hold_pts, 1))]))
    recs.extend(hold_or_cut(ctx, values, blocking[len(swappable):], stashed, dynasty_values))

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
        # churn guard: never drop a player I just added (unless he is hurt) to activate someone
        pool = [d for d in droppable if not recently_added(ctx, d) or d.status in CHURN_EXEMPT_STATUSES]
        if full and len(pool) < len(droppable):
            kept = [d for d in droppable if d not in pool]
            reasons.append(Reason(code="CHURN_GUARD",
                                  text="Not dropping " + ", ".join(f"{d.name} ({churn_text(ctx, d)})" for d in kept)
                                       + ": just added"))
        droppable = pool
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
