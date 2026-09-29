"""Absolute 0-10 recommendation strength, comparable across kinds and days.

``Recommendation.score`` is an engine score that ``advise()`` rescales by rank within each kind
(the best waiver is always 10). ``strength`` instead maps the rec's explicit
``predicted_gain`` onto fixed per-kind scales (piecewise linear between the anchor points
below, 0 at no gain, capped at 10):

* waiver / FPG gains (season_fpg): 0.3 -> 3, 1.0 -> 6, 2.0 -> 9, 3.0 -> 10
* week points (week_pts, lineup and week-horizon waivers): 1 -> 3, 3 -> 6, 6 -> 9, 8 -> 10
* trades (lineup_fpg ΔMe): 0.5 -> 4, 1.0 -> 6, 2.0 -> 9, 3.0 -> 10; +1 when the dynasty Δ
  (DYNASTY_DELTA reason) is positive, -1 when negative
* flags: |form ratio - 1| 0.35 -> 4, 0.6 -> 7, 1.0 -> 10
* injury: the waiver add's strength, floored at 5 when the rec frees an IR slot; a bare
  status alert maps severity (dtd 2.5, out/suspended 5, ir 7.5, ltir 10)

Waivers and trades multiply the gain by confidence (GP / (GP + k), floor 0.25, averaged over
every player in a trade) before mapping, so preseason moves read lower.
"""
from __future__ import annotations

from typing import Iterable, Sequence

from ..models import Recommendation
from .base import confidence

WAIVER_SCALE: tuple[tuple[float, float], ...] = ((0.0, 0.0), (0.3, 3.0), (1.0, 6.0), (2.0, 9.0), (3.0, 10.0))
WEEK_SCALE: tuple[tuple[float, float], ...] = ((0.0, 0.0), (1.0, 3.0), (3.0, 6.0), (6.0, 9.0), (8.0, 10.0))
TRADE_SCALE: tuple[tuple[float, float], ...] = ((0.0, 0.0), (0.5, 4.0), (1.0, 6.0), (2.0, 9.0), (3.0, 10.0))
FLAG_SCALE: tuple[tuple[float, float], ...] = ((0.0, 0.0), (0.35, 4.0), (0.6, 7.0), (1.0, 10.0))
IR_FLOOR = 5.0
SEVERITY_STRENGTH = {"dtd": 2.5, "suspended": 5.0, "out": 5.0, "ir": 7.5, "ltir": 10.0}
KIND_GROUP = {"injury": "injury", "lineup": "lineup", "waiver": "waiver", "trade": "trade",
              "sell_high": "flags", "buy_low": "flags"}


def interp(x: float, scale: Sequence[tuple[float, float]]) -> float:
    """Piecewise-linear map of ``x`` through ``scale`` anchors; 0 below 0, capped at 10."""
    if x <= scale[0][0]:
        return 0.0
    for (x0, y0), (x1, y1) in zip(scale, scale[1:]):
        if x <= x1:
            return y0 + (y1 - y0) * (x - x0) / (x1 - x0)
    return min(10.0, scale[-1][1])


def _reason(rec: Recommendation, code: str) -> float | None:
    for r in rec.reasons:
        if r.code == code and r.value is not None:
            return float(r.value)
    return None


def rec_confidence(rec: Recommendation) -> float:
    """Waivers: the add's confidence; trades: mean over every player involved."""
    if rec.kind == "waiver":
        return confidence(rec.add[0]) if rec.add else 1.0
    players = list(rec.add) + list(rec.drop)
    return sum(confidence(p) for p in players) / len(players) if players else 1.0


def _gain_strength(gain: float, units: str | None) -> float:
    if units == "week_pts":
        return interp(gain, WEEK_SCALE)
    if units in ("lineup_fpg", "dynasty"):
        return interp(gain, TRADE_SCALE)
    return interp(gain, WAIVER_SCALE)


def strength_for(rec: Recommendation) -> float:
    """Absolute 0-10 strength of one recommendation (see module docstring)."""
    gain = rec.predicted_gain
    if rec.kind in ("sell_high", "buy_low"):
        ratio = _reason(rec, "FORM_RATIO")
        return round(interp(abs(ratio - 1.0), FLAG_SCALE), 2) if ratio is not None else 0.0
    if rec.kind == "trade":
        if gain is None:
            return 0.0
        s = interp(gain * rec_confidence(rec), TRADE_SCALE if rec.gain_units != "week_pts" else WEEK_SCALE)
        dyn = _reason(rec, "DYNASTY_DELTA")
        if dyn is not None and abs(dyn) > 1e-9:
            s += 1.0 if dyn > 0 else -1.0
        return round(min(10.0, max(0.0, s)), 2)
    if rec.kind == "waiver":
        if gain is None:
            return 0.0
        return round(_gain_strength(gain * rec_confidence(rec), rec.gain_units), 2)
    if rec.kind == "injury":
        codes = {r.code for r in rec.reasons}
        s = _gain_strength(gain, rec.gain_units) if gain is not None else 0.0
        if "IR_SLOT" in codes:
            s = max(s, IR_FLOOR)
        elif gain is None and rec.subjects:
            s = SEVERITY_STRENGTH.get(rec.subjects[0].status, 0.0)
        return round(min(10.0, s), 2)
    # lineup
    if gain is None:
        return IR_FLOOR if any(r.code == "STATUS" for r in rec.reasons) else 0.0
    return round(_gain_strength(gain, rec.gain_units), 2)


def apply_strength(recs: Iterable[Recommendation]) -> list[Recommendation]:
    """Set ``strength`` (in place) on every rec that has none; returns the list."""
    out = list(recs)
    for r in out:
        if r.strength is None:
            try:
                r.strength = strength_for(r)
            except Exception:  # never let a display field sink a recommender
                r.strength = None
    return out


def apply_ranks(recs: Iterable[Recommendation]) -> list[Recommendation]:
    """Set ``rank_in_kind`` / ``kind_total`` (in place) from the given order, per kind group
    (sell_high and buy_low share 'flags')."""
    out = list(recs)
    groups: dict[str, list[Recommendation]] = {}
    for r in out:
        groups.setdefault(KIND_GROUP.get(r.kind, r.kind), []).append(r)
    for items in groups.values():
        for i, r in enumerate(items, 1):
            r.rank_in_kind, r.kind_total = i, len(items)
    return out


def gain_text(rec: Recommendation) -> str:
    """'+0.95 FPG', '+3.2 pts/wk', '+0.80 lineup FPG', '+1.10 dyn' or '-'."""
    g = rec.predicted_gain
    if g is None:
        return "-"
    unit = {"week_pts": "pts/wk", "season_fpg": "FPG", "lineup_fpg": "lineup FPG", "dynasty": "dyn"}.get(
        rec.gain_units or "", "")
    if rec.gain_units == "season_fpg" and rec.horizon_days == 7:
        unit = "FPG (wk)"
    if rec.gain_units == "season_fpg" and rec.kind in ("sell_high", "buy_low"):
        unit = "FPG vs L15"
    fmt = f"{g:+.1f}" if rec.gain_units == "week_pts" else f"{g:+.2f}"
    return f"{fmt} {unit}".strip()


def rank_text(rec: Recommendation) -> str:
    return f"{rec.rank_in_kind}/{rec.kind_total}" if rec.rank_in_kind and rec.kind_total else "-"
