"""Valuation constants fitted by ``fm backtest fit``, shipped with the package.

The packaged ``fitted_params.json`` next to this module is a copy of
``<FM_DATA_DIR>/backtest/fitted_params.json`` (ESPN scoring, 10+ seasons of NHL history). It is
promoted by hand: ``fm backtest fit`` writes to the data dir and prints the copy command, it
never overwrites this file. Every accessor falls back to the hard-coded values below (the
2026-09-28 fit) when the file is missing, unreadable or lacks a key, so the app never depends on
the file being present.

Accessors:

* ``k_baseline()``     - {F, D, G}: games of shrinkage of the 3-season (5/4/3 GP-weighted)
                         history toward the positional mean (``preseason.k_multi``).
* ``k_inseason()``     - {skater, goalie}: games of shrinkage of season-to-date toward the
                         preseason baseline.
* ``recency_weights()`` - season / last30 / last15 / last7 base weights.
* ``age_yoy()``        - {group: {age: factor}}: expected FPG ratio from the season played at
                         ``age`` (on Oct 1) to the next one; only for ``age_groups()`` (F, D -
                         the goalie curve rests on too few players at the extremes).
* ``dynasty_age_curves()`` - {F, D}: ((age, level), ...) production curves, peak = 1.0.
"""
from __future__ import annotations

import json
import math
from functools import lru_cache
from pathlib import Path
from typing import Any, Mapping

PARAMS_FILE = Path(__file__).with_name("fitted_params.json")

# ---- hard-coded fallbacks (fit of 2026-09-28, ESPN scoring, target seasons 2011-12..2025-26)
FALLBACK_K_BASELINE: dict[str, float] = {"F": 8.0, "D": 14.0, "G": 120.0}
FALLBACK_K_INSEASON: dict[str, float] = {"skater": 25.0, "goalie": 40.0}
FALLBACK_RECENCY: dict[str, float] = {"season": 0.85, "last30": 0.15, "last15": 0.0, "last7": 0.0}
FALLBACK_AGE_GROUPS: tuple[str, ...] = ("F", "D")
FALLBACK_AGE_YOY: dict[str, dict[int, float]] = {
    "F": {18: 1.111, 19: 1.101, 20: 1.070, 21: 1.054, 22: 1.037, 23: 1.019, 24: 1.002, 25: 0.988,
          26: 0.981, 27: 0.970, 28: 0.954, 29: 0.939, 30: 0.941, 31: 0.936, 32: 0.940, 33: 0.910,
          34: 0.908, 35: 0.896, 36: 0.889, 37: 0.881, 38: 0.858, 39: 0.856, 40: 1.0},
    "D": {18: 1.032, 19: 1.057, 20: 1.056, 21: 1.039, 22: 1.017, 23: 0.997, 24: 0.995, 25: 0.994,
          26: 0.993, 27: 0.983, 28: 0.980, 29: 0.977, 30: 0.979, 31: 0.963, 32: 0.958, 33: 0.943,
          34: 0.952, 35: 0.917, 36: 0.913, 37: 0.893, 38: 0.875, 39: 1.0, 40: 1.0},
}
FALLBACK_DYNASTY_CURVES: dict[str, tuple[tuple[float, float], ...]] = {
    "F": ((19, 0.762), (21, 0.897), (23, 0.980), (25, 1.000), (27, 0.969), (29, 0.897),
          (31, 0.792), (33, 0.697), (35, 0.576), (37, 0.459)),
    "D": ((19, 0.848), (21, 0.946), (23, 1.000), (25, 0.992), (27, 0.979), (29, 0.943),
          (31, 0.901), (33, 0.831), (35, 0.746), (37, 0.625)),
}
SKATER_GROUPS = ("F", "D")


@lru_cache(maxsize=4)
def load_params(path: str | None = None) -> dict[str, Any]:
    """The parsed params file ({} when missing or unreadable). Cached per path."""
    p = Path(path) if path else PARAMS_FILE
    try:
        data = json.loads(p.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def _num(v: Any) -> float | None:
    try:
        f = float(v)
    except (TypeError, ValueError):
        return None
    return f if math.isfinite(f) and f >= 0 else None


def _section(*keys: str, params: Mapping[str, Any] | None = None) -> Any:
    d: Any = load_params() if params is None else params
    for k in keys:
        if not isinstance(d, Mapping):
            return None
        d = d.get(k)
    return d


def k_baseline(params: Mapping[str, Any] | None = None) -> dict[str, float]:
    """{F, D, G} shrinkage k for the 3-season history toward the positional mean."""
    raw = _section("preseason", "k_multi", params=params) or {}
    out = dict(FALLBACK_K_BASELINE)
    for g in out:
        v = _num(raw.get(g)) if isinstance(raw, Mapping) else None
        if v is not None:
            out[g] = v
    return out


def k_inseason(params: Mapping[str, Any] | None = None) -> dict[str, float]:
    """{skater, goalie} shrinkage k for season-to-date toward the preseason baseline."""
    raw = _section("inseason", params=params) or {}
    out = dict(FALLBACK_K_INSEASON)
    if isinstance(raw, Mapping):
        for key, src in (("skater", "k_skater"), ("goalie", "k_goalie")):
            v = _num(raw.get(src))
            if v is not None:
                out[key] = v
    return out


def recency_weights(params: Mapping[str, Any] | None = None) -> dict[str, float]:
    """Base recency weights (season / last30 / last15 / last7), summing to 1."""
    raw = _section("inseason", "recency_weights", params=params)
    if isinstance(raw, Mapping):
        vals = {k: _num(raw.get(k)) for k in FALLBACK_RECENCY}
        if all(v is not None for v in vals.values()) and abs(sum(vals.values()) - 1.0) < 1e-6:  # type: ignore[arg-type]
            return {k: float(v) for k, v in vals.items()}  # type: ignore[arg-type]
    return dict(FALLBACK_RECENCY)


def age_groups(params: Mapping[str, Any] | None = None) -> tuple[str, ...]:
    raw = _section("fm_fitted", "age_groups", params=params)
    if isinstance(raw, (list, tuple)) and all(isinstance(g, str) for g in raw):
        return tuple(raw)
    return FALLBACK_AGE_GROUPS


def age_yoy(params: Mapping[str, Any] | None = None) -> dict[str, dict[int, float]]:
    """{group: {age: year-over-year FPG factor}} for the aged groups (F, D)."""
    raw = _section("fm_fitted", "age_yoy", params=params)
    out: dict[str, dict[int, float]] = {}
    for g in age_groups(params):
        curve = raw.get(g) if isinstance(raw, Mapping) else None
        parsed: dict[int, float] = {}
        if isinstance(curve, Mapping):
            for a, r in curve.items():
                v = _num(r)
                try:
                    parsed[int(a)] = v  # type: ignore[assignment]
                except (TypeError, ValueError):
                    continue
            parsed = {a: v for a, v in parsed.items() if v is not None and v > 0}
        out[g] = parsed or dict(FALLBACK_AGE_YOY.get(g, {}))
    return {g: c for g, c in out.items() if c}


def age_factor(group: str, age_prev: float | None, yoy: Mapping[str, Mapping[int, float]] | None = None,
               groups: tuple[str, ...] | None = None) -> float:
    """Expected FPG multiplier from last season (played at ``age_prev`` on Oct 1) to this one.
    1.0 for an unknown age or a group without a curve; ages outside the curve use its ends."""
    groups = age_groups() if groups is None else groups
    if age_prev is None or group not in groups:
        return 1.0
    curve = (age_yoy() if yoy is None else yoy).get(group) or {}
    if not curve:
        return 1.0
    a = min(max(int(math.floor(age_prev)), min(curve)), max(curve))
    return float(curve.get(a, 1.0))


def dynasty_age_curves(params: Mapping[str, Any] | None = None) -> dict[str, tuple[tuple[float, float], ...]]:
    """{F, D}: ((age, level), ...) sorted by age, peak = 1.0 (goalies stay hand-set)."""
    raw = _section("dynasty_age_curves", params=params)
    out: dict[str, tuple[tuple[float, float], ...]] = {}
    for g in SKATER_GROUPS:
        pts = raw.get(g) if isinstance(raw, Mapping) else None
        parsed: list[tuple[float, float]] = []
        if isinstance(pts, (list, tuple)):
            for pt in pts:
                try:
                    a, lv = float(pt[0]), float(pt[1])
                except (TypeError, ValueError, IndexError):
                    continue
                if lv > 0:
                    parsed.append((a, lv))
        out[g] = tuple(sorted(parsed)) if len(parsed) >= 2 else FALLBACK_DYNASTY_CURVES[g]
    return out


def source(params: Mapping[str, Any] | None = None) -> str:
    """Short provenance label, e.g. 'fit 2026-09-28 (espn)' or 'built-in fallback'."""
    d = load_params() if params is None else params
    if not d:
        return "built-in fallback"
    return f"fit {str(d.get('generated', '?'))[:10]} ({d.get('scoring', '?')})"


def params_hash(params: Mapping[str, Any] | None = None) -> str:
    """sha1 of the loaded params (canonical JSON, sorted keys); identifies the parameter set a
    snapshot was produced with (the built-in fallback hashes as '{}')."""
    import hashlib

    d = load_params() if params is None else params
    return hashlib.sha1(json.dumps(d, sort_keys=True, separators=(",", ":"), default=str).encode("utf-8")).hexdigest()
