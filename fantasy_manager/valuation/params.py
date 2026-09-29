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

Harness layer (``fm harness refit``, docs/harness.md "Auto-correction"): ``load_params()`` is the
packaged file deep-merged with the active harness version, ``<fm_data_dir>/harness/params/
active.json`` (``{"version": "v0003"}``) -> ``v0003.json`` (``params`` holds only the overridden
keys). ``FM_PARAMS_OVERRIDE=0`` disables the layer; a missing or corrupt override is ignored.
The merged result is cached until ``reload()``; ``source()`` reports the provenance and
``params_hash()`` hashes the merged result. Accessors the harness may override (Tier A:
``k_inseason``, ``recency_weights``, ``projection_weight``, ``k_projection``; Tier B:
``availability``, ``start_share_prior`` / ``start_share_k``, ``offnight_bonus``) all fall back to
the hard-coded values below. Live code calls the accessors at call time; the module-level
constants in blend / adjust / schedule are import-time snapshots kept for the backtest.
"""
from __future__ import annotations

import json
import math
import os
import threading
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


# ---- not fitted from history (no historical league projections / schedules exist); the harness
# (``fm harness refit``) may override them through ``<fm_data_dir>/harness/params``
FALLBACK_PROJECTION_WEIGHT = 0.6
FALLBACK_K_PROJECTION: dict[str, float] = {"skater": 20.0, "goalie": 12.0}
# status -> (week multiplier, season multiplier)
FALLBACK_AVAILABILITY: dict[str, tuple[float, float]] = {
    "healthy": (1.0, 1.0),
    "dtd": (0.75, 0.75),
    "out": (0.0, 0.6),
    "ir": (0.0, 0.4),
    "ltir": (0.0, 0.1),
    "suspended": (0.0, 0.5),
    "unknown": (1.0, 1.0),
}
FALLBACK_START_SHARE_PRIOR = 0.5
FALLBACK_START_SHARE_K = 10.0
FALLBACK_OFFNIGHT_BONUS = 0.05

# ---- harness override layer
OVERRIDE_ENV = "FM_PARAMS_OVERRIDE"      # "0" / "false" / "off" / "no" disables the override layer
PARAMS_DIR_ENV = "FM_PARAMS_DIR"          # optional explicit versions dir (tests, tools)
ACTIVE_FILE = "active.json"

_lock = threading.RLock()
_merged: dict[str, Any] | None = None
_active: dict[str, Any] | None = None     # the active version record (None: packaged only)
_memo: dict[str, Any] = {}


@lru_cache(maxsize=4)
def _read_json(path: str) -> dict[str, Any]:
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def load_packaged(path: str | None = None) -> dict[str, Any]:
    """The packaged ``fitted_params.json`` ({} when missing or unreadable). Cached per path."""
    return _read_json(str(Path(path) if path else PARAMS_FILE))


def override_enabled() -> bool:
    return os.environ.get(OVERRIDE_ENV, "1").strip().lower() not in ("0", "false", "off", "no")


def params_dir(data_dir: str | Path | None = None) -> Path:
    """``<fm_data_dir>/harness/params`` (``FM_PARAMS_DIR`` wins when set)."""
    if data_dir is not None:
        return Path(data_dir) / "harness" / "params"
    env = os.environ.get(PARAMS_DIR_ENV)
    if env:
        return Path(env)
    try:
        from ..config import get_settings

        base = Path(get_settings().fm_data_dir)
    except Exception:  # noqa: BLE001 - params must load even without a valid config
        base = Path(os.environ.get("FM_DATA_DIR", "data"))
    return base / "harness" / "params"


def deep_merge(base: Mapping[str, Any], over: Mapping[str, Any]) -> dict[str, Any]:
    """``base`` with ``over`` merged in: nested dicts merge key by key, anything else replaces."""
    out: dict[str, Any] = dict(base)
    for k, v in over.items():
        if isinstance(v, Mapping) and isinstance(out.get(k), Mapping):
            out[k] = deep_merge(out[k], v)
        else:
            out[k] = v
    return out


def read_active(pdir: str | Path | None = None) -> dict[str, Any] | None:
    """The active version record (``active.json`` -> ``vNNNN.json``), or None when there is no
    pointer, it points nowhere (``{"version": null}``: rolled back to packaged) or either file
    is unreadable / malformed (a corrupt override is ignored, never fatal)."""
    d = Path(pdir) if pdir is not None else params_dir()
    try:
        ptr = json.loads((d / ACTIVE_FILE).read_text(encoding="utf-8"))
        version = ptr.get("version") if isinstance(ptr, dict) else None
        if not isinstance(version, str) or not version.startswith("v") or not version[1:].isdigit():
            return None
        rec = json.loads((d / f"{version}.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(rec, dict) or not isinstance(rec.get("params"), dict):
        return None
    rec.setdefault("version", version)
    return rec


def _ensure_loaded() -> dict[str, Any]:
    global _merged, _active
    with _lock:
        if _merged is None:
            packaged = load_packaged()
            active = read_active() if override_enabled() else None
            _active = active
            _merged = deep_merge(packaged, active["params"]) if active else dict(packaged)
        return _merged


def load_params(path: str | None = None) -> dict[str, Any]:
    """The effective params: packaged ``fitted_params.json`` deep-merged with the harness's
    active version (``<fm_data_dir>/harness/params/active.json``), cached until ``reload()``.

    With ``path`` only that file is read (no override layer); {} when missing / unreadable."""
    if path is not None:
        return load_packaged(path)
    return _ensure_loaded()


def reload() -> dict[str, Any]:
    """Drop every cache (packaged file, override, accessor memos) and re-read; the dashboard's
    /refresh calls it so a promoted or rolled-back version takes effect without a restart."""
    global _merged, _active
    with _lock:
        _read_json.cache_clear()
        _merged = None
        _active = None
        _memo.clear()
    return _ensure_loaded()


def active_version() -> dict[str, Any] | None:
    """The active harness version record behind ``load_params()`` (None: packaged only)."""
    _ensure_loaded()
    return _active


def _memoized(name: str, params: Mapping[str, Any] | None, fn):
    """Accessor results for the loaded params are computed once per load (cleared by reload)."""
    if params is not None:
        return fn(params)
    _ensure_loaded()
    with _lock:
        if name not in _memo:
            _memo[name] = fn(None)
        return _memo[name]


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


def _k_baseline_all(params: Mapping[str, Any] | None) -> dict[str, float]:
    raw = _section("preseason", "k_multi", params=params) or {}
    out = dict(FALLBACK_K_BASELINE)
    for g in out:
        v = _num(raw.get(g)) if isinstance(raw, Mapping) else None
        if v is not None:
            out[g] = v
    return out


def k_baseline(params: Mapping[str, Any] | None = None) -> dict[str, float]:
    """{F, D, G} shrinkage k for the 3-season history toward the positional mean."""
    return dict(_memoized("k_baseline", params, _k_baseline_all))


def _group_key(group: Any) -> str:
    """'goalie' for 'goalie' / 'G' / True (is_goalie), else 'skater'."""
    if isinstance(group, bool):
        return "goalie" if group else "skater"
    return "goalie" if str(group).lower() in ("goalie", "g") else "skater"


def _split_args(group: Any, params: Mapping[str, Any] | None) -> tuple[Any, Mapping[str, Any] | None]:
    # the pre-harness signature was ``k_inseason(params)``: a mapping as first argument is params
    if isinstance(group, Mapping):
        return None, group
    return group, params


def _k_inseason_all(params: Mapping[str, Any] | None) -> dict[str, float]:
    raw = _section("inseason", params=params) or {}
    out = dict(FALLBACK_K_INSEASON)
    if isinstance(raw, Mapping):
        for key, src in (("skater", "k_skater"), ("goalie", "k_goalie")):
            v = _num(raw.get(src))
            if v is not None:
                out[key] = v
    return out


def k_inseason(group: Any = None, params: Mapping[str, Any] | None = None) -> Any:
    """Shrinkage k (games) of season-to-date toward the preseason baseline: the float for
    ``group`` ('skater' / 'goalie', or 'F' / 'D' / 'G'), or {skater, goalie} without a group."""
    group, params = _split_args(group, params)
    allk = _memoized("k_inseason", params, _k_inseason_all)
    return dict(allk) if group is None else allk[_group_key(group)]


def _recency_all(params: Mapping[str, Any] | None) -> dict[str, float]:
    raw = _section("inseason", "recency_weights", params=params)
    if isinstance(raw, Mapping):
        vals = {k: _num(raw.get(k)) for k in FALLBACK_RECENCY}
        if all(v is not None for v in vals.values()) and abs(sum(vals.values()) - 1.0) < 1e-6:  # type: ignore[arg-type]
            return {k: float(v) for k, v in vals.items()}  # type: ignore[arg-type]
    return dict(FALLBACK_RECENCY)


def recency_weights(params: Mapping[str, Any] | None = None) -> dict[str, float]:
    """Base recency weights (season / last30 / last15 / last7), summing to 1 (all four must be
    present, non-negative and sum to 1, else the fallback is used as a whole)."""
    return dict(_memoized("recency_weights", params, _recency_all))


def _projection_weight(params: Mapping[str, Any] | None) -> float:
    v = _num(_section("projection", "weight", params=params))
    return v if v is not None and v <= 1.0 else FALLBACK_PROJECTION_WEIGHT


def projection_weight(params: Mapping[str, Any] | None = None) -> float:
    """Weight of the league projection vs the multi-season history (full-history players)."""
    return _memoized("projection_weight", params, _projection_weight)


def _k_projection_all(params: Mapping[str, Any] | None) -> dict[str, float]:
    raw = _section("projection", params=params)
    out = dict(FALLBACK_K_PROJECTION)
    if isinstance(raw, Mapping):
        for key, src in (("skater", "k_skater"), ("goalie", "k_goalie")):
            v = _num(raw.get(src))
            if v is not None:
                out[key] = v
    return out


def k_projection(group: Any = None, params: Mapping[str, Any] | None = None) -> Any:
    """Shrinkage k of a league projection toward the positional mean: float for ``group``,
    {skater, goalie} without one."""
    group, params = _split_args(group, params)
    allk = _memoized("k_projection", params, _k_projection_all)
    return dict(allk) if group is None else allk[_group_key(group)]


def _availability_all(params: Mapping[str, Any] | None) -> dict[str, tuple[float, float]]:
    raw = _section("availability", params=params)
    out = dict(FALLBACK_AVAILABILITY)
    if isinstance(raw, Mapping):
        for status, v in raw.items():
            week, season = out.get(str(status), (1.0, 1.0))
            if isinstance(v, Mapping):
                w, s_ = _num(v.get("week")), _num(v.get("season"))
            elif isinstance(v, (list, tuple)) and len(v) == 2:
                w, s_ = _num(v[0]), _num(v[1])
            else:
                continue
            out[str(status)] = (w if w is not None and w <= 1.0 else week,
                                s_ if s_ is not None and s_ <= 1.0 else season)
    return out


def availability_table(params: Mapping[str, Any] | None = None) -> dict[str, tuple[float, float]]:
    """{status: (week multiplier, season multiplier)}."""
    return dict(_memoized("availability", params, _availability_all))


def availability(status: str, horizon: str = "season", params: Mapping[str, Any] | None = None) -> float:
    """Availability multiplier of ``status`` for the 'week' or 'season' horizon (1.0 unknown)."""
    week, season = _memoized("availability", params, _availability_all).get(status, (1.0, 1.0))
    return week if horizon == "week" else season


def start_share_prior(params: Mapping[str, Any] | None = None) -> float:
    """Goalie start-share prior the observed share is shrunk toward."""
    def f(p):
        v = _num(_section("schedule", "start_share_prior", params=p))
        return v if v is not None and v <= 1.0 else FALLBACK_START_SHARE_PRIOR
    return _memoized("start_share_prior", params, f)


def start_share_k(params: Mapping[str, Any] | None = None) -> float:
    """Games of shrinkage of a goalie's start share toward the prior."""
    def f(p):
        v = _num(_section("schedule", "start_share_k", params=p))
        return v if v is not None else FALLBACK_START_SHARE_K
    return _memoized("start_share_k", params, f)


def offnight_bonus(params: Mapping[str, Any] | None = None) -> float:
    """Week-projection bonus per off-night game (``1 + bonus * offnight_games``)."""
    def f(p):
        v = _num(_section("schedule", "offnight_bonus", params=p))
        return v if v is not None and v <= 1.0 else FALLBACK_OFFNIGHT_BONUS
    return _memoized("offnight_bonus", params, f)


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
    groups = _memoized("age_groups", None, age_groups) if groups is None else groups
    if age_prev is None or group not in groups:
        return 1.0
    curve = (_memoized("age_yoy", None, age_yoy) if yoy is None else yoy).get(group) or {}
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
    """Short provenance label: 'fit 2026-09-28 (espn)', plus ' + harness v0003 (2026-11-17)' when
    a harness version is active, or 'built-in fallback' without a packaged file."""
    if params is not None:
        d = params
        active = None
    else:
        d = load_params()
        active = active_version()
    packaged = load_packaged() if params is None else d
    base = (f"fit {str(packaged.get('generated', '?'))[:10]} ({packaged.get('scoring', '?')})"
            if packaged else "built-in fallback")
    if active:
        created = str(active.get("applied_at") or active.get("created") or "?")[:10]
        base += f" + harness {active.get('version', '?')} ({created})"
    return base


def params_hash(params: Mapping[str, Any] | None = None) -> str:
    """sha1 of the effective params (packaged merged with the active harness version; canonical
    JSON, sorted keys); identifies the parameter set a snapshot was produced with (the built-in
    fallback hashes as '{}')."""
    import hashlib

    d = load_params() if params is None else params
    return hashlib.sha1(json.dumps(d, sort_keys=True, separators=(",", ":"), default=str).encode("utf-8")).hexdigest()
