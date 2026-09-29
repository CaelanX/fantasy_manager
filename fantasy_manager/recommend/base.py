"""Shared helpers for recommendation engines."""
from __future__ import annotations

from typing import Any, Mapping

from ..models import FantasyTeam, LeagueContext, Player
from ..valuation.blend import shrink_k
from ..valuation.replacement import DEFAULT_SLOTS, eligible_slots

MIN_CONFIDENCE = 0.25


def confidence(player: Player) -> float:
    """GP/(GP+k) from current-season games played, floored at MIN_CONFIDENCE."""
    gp = player.gp("season")
    k = shrink_k(player.is_goalie)
    return max(MIN_CONFIDENCE, gp / (gp + k)) if gp > 0 else MIN_CONFIDENCE


def droppable_players(team: FantasyTeam) -> list[Player]:
    """Rostered players outside IR slots."""
    return [s.player for s in team.slots if s.player is not None and s.slot != "IR"]


def shares_slot(a: Player, b: Player, slots=DEFAULT_SLOTS) -> bool:
    return bool(set(eligible_slots(a.positions, slots)) & set(eligible_slots(b.positions, slots)))


IR_SLOT_NAMES = ("IR", "IR+")
IR_MOVE_STATUSES = ("ltir", "ir")      # most severe first


def free_ir_slots(team: FantasyTeam, roster_shape: Mapping[str, int]) -> int:
    """IR slots in the league's roster shape not yet occupied on `team`."""
    cap = sum(int(roster_shape.get(s, 0)) for s in IR_SLOT_NAMES)
    used = sum(1 for s in team.slots if s.player is not None and s.slot in IR_SLOT_NAMES)
    return max(0, cap - used)


def ir_movable(team: FantasyTeam, values: Mapping[str, Any] | None = None) -> list[Player]:
    """Players in active/bench slots whose status (IR/LTIR) lets them move to an IR slot,
    most severe first, then most valuable first."""
    def fpg(p: Player) -> float:
        pv = (values or {}).get(p.cid)
        return float(getattr(pv, "fpg", 0.0) or 0.0)

    out = [s.player for s in team.slots
           if s.player is not None and s.slot not in IR_SLOT_NAMES and s.player.status in IR_MOVE_STATUSES]
    return sorted(out, key=lambda p: (IR_MOVE_STATUSES.index(p.status), -fpg(p)))


def dynasty_map(ctx: LeagueContext, values: Mapping[str, Any],
                dynasty_values: Mapping[str, Any] | None = None) -> dict[str, float] | None:
    """{cid: dynasty value} in dynasty leagues (computed from `values` when not supplied),
    else None."""
    if not ctx.dynasty:
        return None
    if dynasty_values is None:
        try:
            from ..valuation.dynasty import apply_dynasty
            dynasty_values = apply_dynasty(values, ctx)
        except Exception:
            return None
    out: dict[str, float] = {}
    for cid, dv in (dynasty_values or {}).items():
        v = dv if isinstance(dv, (int, float)) else getattr(dv, "value", None)
        if v is not None:
            out[cid] = float(v)
    return out or None


def open_roster_spots(team: FantasyTeam, roster_shape: Mapping[str, int]) -> int:
    """Empty non-IR roster spots (starting + bench/reserve) on `team`."""
    cap = sum(int(n) for s, n in roster_shape.items() if s not in IR_SLOT_NAMES)
    used = sum(1 for s in team.slots if s.player is not None and s.slot not in IR_SLOT_NAMES)
    return max(0, cap - used)


def has_data(pv: Any) -> bool:
    """False when valuation found no stats at all (NO_DATA): the value is unknown, not zero
    (typically a prospect), so dynasty drop logic must not treat it as the weakest asset."""
    return pv is not None and not any(getattr(r, "code", None) == "NO_DATA" for r in getattr(pv, "reasons", []))


# -- dynasty prospect protection ----------------------------------------------------

PROTECT_MAX_AGE = 23.0
PROTECT_MAX_PICK = 32
PROTECT_MIN_OWNED = 80.0
PROTECT_UNPROVEN_GP = 82          # career NHL GP below this = still unproven
PROTECT_UNPROVEN_OWNED = 60.0     # ... and rostered at least this widely
PROTECT_OVERRIDE = 1.5      # dynasty(add) must be at least this multiple of dynasty(drop)


def player_age(p: Player, ctx: LeagueContext, dynasty_values: Mapping[str, Any] | None = None) -> float | None:
    """Age from the dynasty value object (which includes league-reported ages), else the
    birth date."""
    dv = (dynasty_values or {}).get(p.cid)
    age = getattr(dv, "age", None)
    if isinstance(age, (int, float)):
        return float(age)
    if p.birth_date is not None:
        return (ctx.as_of - p.birth_date).days / 365.25
    return None


def protection_reason(p: Player, age: float | None) -> str | None:
    """Why `p` is a protected young dynasty asset, or None. Protected players are never
    proposed as drops unless the incoming value is at least PROTECT_OVERRIDE times theirs.

    Protected = age <= 23 AND (a top-32 pick, OR rostered in >= 80% of leagues, OR unproven
    (< 82 career NHL GP) and rostered in >= 60%). Veterans are never protected: the normal
    dynasty-value comparison (DYNASTY_MIN_RATIO) governs them. (A rule protecting anyone
    >= 80% rostered covered most of a competitive dynasty roster, so waivers never fired.)"""
    if age is None or age > PROTECT_MAX_AGE:
        return None
    if p.draft_overall is not None and p.draft_overall <= PROTECT_MAX_PICK:
        return f"{p.name} is {age:.0f} and a #{p.draft_overall} overall pick"
    owned = p.pct_owned
    if owned is not None and owned >= PROTECT_MIN_OWNED:
        return f"{p.name} is {age:.0f} and rostered in {owned:.0f}% of leagues"
    if owned is not None and owned >= PROTECT_UNPROVEN_OWNED and p.career_gp is not None             and p.career_gp < PROTECT_UNPROVEN_GP:
        return f"{p.name} is {age:.0f}, {p.career_gp} NHL GP and rostered in {owned:.0f}% of leagues"
    return None
