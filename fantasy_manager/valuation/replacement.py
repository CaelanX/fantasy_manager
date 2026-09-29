"""Replacement level from the free-agent pool and value over replacement (VORP)."""
from __future__ import annotations

from typing import Iterable, Mapping

from ..models import Player

DEFAULT_SLOTS = ("C", "LW", "RW", "D", "G")
FORWARDS = ("C", "LW", "RW")
TOP_N = 3


def eligible_slots(positions: Iterable[str], slots: Iterable[str] = DEFAULT_SLOTS) -> list[str]:
    """Slots a player with `positions` can fill. A generic 'F' counts for C/LW/RW."""
    pos = set(positions)
    if "F" in pos and not pos & set(FORWARDS):
        pos |= set(FORWARDS)
    is_skater = bool(pos - {"G"})
    out = []
    for s in slots:
        if s == "F":
            ok = bool(pos & {"C", "LW", "RW", "F"})
        elif s == "UTIL":
            ok = is_skater
        else:
            ok = s in pos
        if ok:
            out.append(s)
    return out


def replacement_levels(free_agents_values: Mapping[str, float], players: Mapping[str, Player],
                       slots: Iterable[str] = DEFAULT_SLOTS) -> dict[str, float]:
    """Mean of the top-3 free-agent values per slot; missing entries count as 0 (monotone)."""
    slots = list(slots)
    by_slot: dict[str, list[float]] = {s: [] for s in slots}
    for cid, val in free_agents_values.items():
        p = players.get(cid)
        if p is None:
            continue
        for s in eligible_slots(p.positions, slots):
            by_slot[s].append(val)
    out = {}
    for s, vals in by_slot.items():
        top = sorted(vals + [0.0] * TOP_N, reverse=True)[:TOP_N]
        out[s] = sum(top) / TOP_N
    return out


def vorp(value: float, positions: Iterable[str], repl: Mapping[str, float]) -> float:
    """Value minus the smallest replacement level among the player's eligible slots."""
    elig = eligible_slots(positions, repl.keys())
    if not elig:
        return value
    return value - min(repl[s] for s in elig)


def best_slot(positions: Iterable[str], repl: Mapping[str, float]) -> str | None:
    elig = eligible_slots(positions, repl.keys())
    return min(elig, key=lambda s: repl[s]) if elig else None


ROSTERED_FALLBACK_PCT = 0.10


def percentile(vals: Iterable[float], q: float) -> float:
    """Linear-interpolated q-quantile (0..1) of vals; 0.0 when empty."""
    xs = sorted(vals)
    if not xs:
        return 0.0
    pos = q * (len(xs) - 1)
    lo = int(pos)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (pos - lo)


def replacement_with_fallback(free_agents_values: Mapping[str, float], rostered_values: Mapping[str, float],
                              players: Mapping[str, Player], slots: Iterable[str] = DEFAULT_SLOTS,
                              pct: float = ROSTERED_FALLBACK_PCT) -> tuple[dict[str, float], set[str]]:
    """(levels, fallback slots). Top-3 free-agent mean per slot; a slot with no eligible free
    agent at all (e.g. the provider returned none) falls back to the ``pct`` quantile of the
    rostered players eligible there instead of 0."""
    slots = list(slots)
    levels = replacement_levels(free_agents_values, players, slots)
    covered: set[str] = set()
    for cid in free_agents_values:
        p = players.get(cid)
        if p is not None:
            covered.update(eligible_slots(p.positions, slots))
    fallback: set[str] = set()
    for s in slots:
        if s in covered:
            continue
        vals = [v for cid, v in rostered_values.items()
                if (p := players.get(cid)) is not None and s in eligible_slots(p.positions, [s])]
        if vals:
            levels[s] = percentile(vals, pct)
            fallback.add(s)
    return levels, fallback
