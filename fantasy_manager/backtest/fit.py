"""Fit the valuation constants from history instead of hand-picked priors.

* ``fit_shrinkage_k``  - preseason k (prior season shrunk toward the positional mean) per
                         position group, grid search minimizing MAE of next-season FPG.
* ``fit_age_curve``    - delta method: for players with >= min_gp in consecutive seasons, the
                         median ratio FPG(next) / base by age (pooled over a +-1 year window),
                         chained into a trajectory normalized to peak = 1.0, with a
                         player-clustered bootstrap for 90% intervals. ``base`` is last season's
                         FPG shrunk toward the positional mean with the fitted k (the model's own
                         no-aging expectation). Against raw last-season FPG the ratios carry a
                         regression-to-the-mean drift (median 0.95 for goalies, 0.98 for
                         forwards, at every age) that the chaining turns into a spurious decline.
* ``fit_inseason``     - in-season k (season-to-date shrunk toward the preseason baseline) and
                         the recency weights (season / L30 / L15 / L7), coordinate-descent grid
                         search on checkpoint observations (see ``evaluate.inseason_observations``).

The delta method only sees players who kept a job (>= min_gp) in both seasons, so it is biased
toward survivors: the decline at 33+ is if anything understated.
"""
from __future__ import annotations

import math
import random
import statistics
from dataclasses import dataclass, field
from typing import Any, Iterable, Sequence

from ..scoring import PointsScoring
from ..valuation.blend import EXPECTED_GAMES, K_BASELINE, K_INSEASON, RECENCY_WEIGHTS
from .data import SeasonTable, prev_season
from .models import FittedParams, group_means, weighted_rates
from .scoring import fpg

GROUPS = ("F", "D", "G")
K_GRID = tuple(range(0, 101, 2)) + (110, 120, 140, 160, 200, 250, 300)
INSEASON_K_GRID = (0, 3, 5, 8, 10, 12, 15, 20, 25, 30, 40, 50, 60, 80, 100, 150)
MIN_GP = 20
MIN_FPG_FOR_RATIO = 0.25       # ratios off a tiny base explode
AGE_MIN, AGE_MAX = 18, 40
AGE_WINDOW = 1                 # pool ratios over age +- 1
AGE_MIN_POOLED = 25            # fewer pooled pairs than this -> flat (ratio 1.0)
YOY_CLAMP = (0.75, 1.30)
REPORT_AGES = (19, 21, 23, 25, 28, 31, 34)


# --------------------------------------------------------------------------- season pairs

@dataclass
class Pair:
    player_id: int
    season: int            # target season N (the pair is N-1 -> N)
    group: str
    age_prev: float | None
    gp_prev: int
    fpg_prev: float
    fpg_mean: float        # positional-mean FPG of season N-1 (the app's shrink target)
    gp_next: int
    fpg_next: float
    fpg_multi: float = 0.0  # 5/4/3 GP-weighted FPG over seasons N-1..N-3
    n_multi: float = 0.0    # its effective games (units of season N-1)


def season_pairs(table: SeasonTable, scoring: PointsScoring, seasons: Iterable[int],
                 min_gp_prev: int = MIN_GP, min_gp_next: int = MIN_GP) -> list[Pair]:
    """(N-1 -> N) pairs for every target season N in ``seasons``."""
    out: list[Pair] = []
    for n in seasons:
        prev = prev_season(n)
        means = {g: fpg(m, scoring) for g, m in group_means(table, prev).items()}
        cur = table.by_season.get(n, {})
        for pid, a in table.by_season.get(prev, {}).items():
            b = cur.get(pid)
            if b is None or a.gp < min_gp_prev or b.gp < min_gp_next or a.group != b.group:
                continue
            w = weighted_rates(table.history(pid, n), n)
            out.append(Pair(pid, n, a.group, a.age if a.age is not None else table.age(pid, prev),
                            a.gp, fpg(a.per_game(), scoring), means.get(a.group, 0.0),
                            b.gp, fpg(b.per_game(), scoring),
                            fpg(w[0], scoring) if w else 0.0, w[1] if w else 0.0))
    return out


# --------------------------------------------------------------------------- shrinkage k

def shrink_projection(p: Pair, k: float, age_factor: float = 1.0) -> float:
    return (p.gp_prev * p.fpg_prev + k * p.fpg_mean) / (p.gp_prev + k) * age_factor


def multi_projection(p: Pair, k: float, age_factor: float = 1.0) -> float:
    return (p.n_multi * p.fpg_multi + k * p.fpg_mean) / (p.n_multi + k) * age_factor


def mae(errors: Sequence[float]) -> float:
    return sum(abs(e) for e in errors) / len(errors) if errors else float("nan")


def fit_shrinkage_k(pairs: Sequence[Pair], grid: Sequence[float] = K_GRID,
                    age_factor=None, projection=shrink_projection) -> tuple[float, dict[float, float]]:
    """(best k, {k: MAE}) for projecting fpg_next from the shrunk prior season (or, with
    ``projection=multi_projection``, from the 5/4/3-weighted seasons)."""
    if not pairs:
        return float("nan"), {}
    factors = [age_factor(p) if age_factor else 1.0 for p in pairs]
    curve = {float(k): mae([projection(p, k, f) - p.fpg_next for p, f in zip(pairs, factors)])
             for k in grid}
    best = min(curve, key=lambda k: (round(curve[k], 9), k))
    return best, curve


# --------------------------------------------------------------------------- age curve

@dataclass
class AgeCurve:
    group: str
    yoy: dict[int, float]                     # expected FPG ratio age a -> a+1
    level: dict[int, float]                   # trajectory, peak = 1.0
    n: dict[int, int]                         # pairs whose prev-season age is exactly a
    lo: dict[int, float] = field(default_factory=dict)   # bootstrap 5th pct of level
    hi: dict[int, float] = field(default_factory=dict)   # bootstrap 95th pct of level

    def peak_age(self) -> int:
        return max(self.level, key=lambda a: self.level[a])

    def to_dict(self) -> dict[str, Any]:
        ages = sorted(self.level)
        return {"peak_age": self.peak_age(),
                "ages": {str(a): {"level": round(self.level[a], 4), "yoy": round(self.yoy.get(a, 1.0), 4),
                                  "n": self.n.get(a, 0),
                                  "lo": round(self.lo[a], 4) if a in self.lo else None,
                                  "hi": round(self.hi[a], 4) if a in self.hi else None} for a in ages}}


def _base(p: Pair, k: float | None) -> float:
    return p.fpg_prev if not k else shrink_projection(p, k)


def _yoy_levels(pairs: Sequence[Pair], ages: range, window: int, min_pooled: int, k: float | None = None
                ) -> tuple[dict[int, float], dict[int, float]]:
    by_age: dict[int, list[float]] = {}
    for p in pairs:
        base = _base(p, k)
        if p.age_prev is None or base < MIN_FPG_FOR_RATIO:
            continue
        by_age.setdefault(int(math.floor(p.age_prev)), []).append(p.fpg_next / base)
    yoy: dict[int, float] = {}
    for a in ages:
        pooled = [r for b in range(a - window, a + window + 1) for r in by_age.get(b, ())]
        r = statistics.median(pooled) if len(pooled) >= min_pooled else 1.0
        yoy[a] = min(max(r, YOY_CLAMP[0]), YOY_CLAMP[1])
    level = {ages[0]: 1.0}
    for a in ages[:-1]:
        level[a + 1] = level[a] * yoy[a]
    peak = max(level.values())
    return yoy, {a: v / peak for a, v in level.items()}


def fit_age_curve(pairs: Sequence[Pair], group: str, boot: int = 200, seed: int = 7,
                  ages: range = range(AGE_MIN, AGE_MAX + 1), window: int = AGE_WINDOW,
                  min_pooled: int = AGE_MIN_POOLED, k: float | None = None) -> AgeCurve:
    """Age curve of ``group``; ``k`` shrinks the base season toward the mean (None: raw FPG)."""
    gp = [p for p in pairs if p.group == group]
    yoy, level = _yoy_levels(gp, ages, window, min_pooled, k)
    n: dict[int, int] = {}
    for p in gp:
        if p.age_prev is not None and _base(p, k) >= MIN_FPG_FOR_RATIO:
            a = int(math.floor(p.age_prev))
            n[a] = n.get(a, 0) + 1
    curve = AgeCurve(group, yoy, level, {a: n.get(a, 0) for a in ages})
    if boot > 0 and gp:
        rng = random.Random(seed)
        by_player: dict[int, list[Pair]] = {}
        for p in gp:
            by_player.setdefault(p.player_id, []).append(p)
        ids = list(by_player)
        samples: dict[int, list[float]] = {a: [] for a in ages}
        for _ in range(boot):
            draw = [q for pid in rng.choices(ids, k=len(ids)) for q in by_player[pid]]
            _, lv = _yoy_levels(draw, ages, window, min_pooled, k)
            for a in ages:
                samples[a].append(lv[a])
        for a in ages:
            s = sorted(samples[a])
            curve.lo[a] = s[int(0.05 * (len(s) - 1))]
            curve.hi[a] = s[int(math.ceil(0.95 * (len(s) - 1)))]
    return curve


def dynasty_anchors(curve: AgeCurve, ages: Sequence[int] = (19, 21, 23, 25, 27, 29, 31, 33, 35, 37)
                    ) -> list[list[float]]:
    """[[age, level], ...] in the shape of ``valuation.dynasty.AGE_CURVES`` (peak = 1.0)."""
    return [[a, round(curve.level[a], 3)] for a in ages if a in curve.level]


# --------------------------------------------------------------------------- preseason bundle

def fit_preseason(table: SeasonTable, scoring: PointsScoring, target_seasons: Iterable[int],
                  boot: int = 0, age_groups: tuple[str, ...] = ("F", "D")
                  ) -> tuple[FittedParams, dict[str, Any]]:
    """k and age curve per group from (N-1 -> N) pairs: k without aging, the age curve against
    the k-shrunk base, k again on age-adjusted projections, and the final curve (+ bootstrap)."""
    pairs = season_pairs(table, scoring, target_seasons)
    by_group = {g: [p for p in pairs if p.group == g] for g in GROUPS}
    k0 = {g: fit_shrinkage_k(by_group[g])[0] for g in GROUPS}
    k0 = {g: (float(K_BASELINE[g]) if math.isnan(v) else v) for g, v in k0.items()}
    curves = {g: fit_age_curve(pairs, g, boot=0, k=k0[g]) for g in GROUPS}
    params = FittedParams(k={}, age_yoy={g: dict(c.yoy) for g, c in curves.items()}, age_groups=age_groups)
    k_curves: dict[str, dict[float, float]] = {}
    for g in GROUPS:
        best, curve = fit_shrinkage_k(by_group[g], age_factor=lambda p, g=g: params.age_factor(g, p.age_prev))
        params.k[g] = best if not math.isnan(best) else k0[g]
        k_curves[g] = curve
    curves = {g: fit_age_curve(pairs, g, boot=boot, k=params.k[g]) for g in GROUPS}
    params.age_yoy = {g: dict(c.yoy) for g, c in curves.items()}
    k_multi_curves: dict[str, dict[float, float]] = {}
    for g in GROUPS:
        best, curve = fit_shrinkage_k([p for p in by_group[g] if p.n_multi > 0], projection=multi_projection,
                                      age_factor=lambda p, g=g: params.age_factor(g, p.age_prev))
        params.k_multi[g] = best if not math.isnan(best) else params.k_multi.get(g, 10.0)
        k_multi_curves[g] = curve
    # skaters pooled, for the app's single skater constant
    sk = [p for p in pairs if p.group in ("F", "D")]
    best_sk, curve_sk = fit_shrinkage_k(sk, age_factor=lambda p: params.age_factor(p.group, p.age_prev))
    info = {"pairs": len(pairs), "curves": curves, "k_curves": k_curves, "k_multi_curves": k_multi_curves,
            "k_skater_pooled": best_sk, "k_curve_skater_pooled": curve_sk}
    return params, info


# --------------------------------------------------------------------------- in-season

@dataclass
class InseasonObs:
    season: int
    day: str
    player_id: int
    group: str
    gp_td: int
    fpg_td: float
    fpg_base: float
    has_base: bool
    splits: dict[str, tuple[int, float]]     # last30/last15/last7 -> (gp, fpg)
    gp_rest: int
    fpg_rest: float
    fpg_app: float = 0.0                      # the app's own pipeline (blend.py functions)
    # live (harness replay) rows only; historical checkpoints have no league projection
    fpg_proj: float | None = None             # league projection FPG (unshrunk)
    fpg_hist: float | None = None             # multi-season history baseline FPG
    status: str | None = None                 # availability status that day
    gp_hist3: int | None = None               # NHL GP over the three prior seasons
    start_share: float | None = None          # goalie start share that day


def recency_weights_for(base: dict[str, float], gps: dict[str, int],
                        expected: dict[str, float] = EXPECTED_GAMES) -> dict[str, float]:
    """``blend.recency_weights`` with arbitrary base weights (shortfall goes to 'season')."""
    w = {"season": base["season"]}
    for split, exp in expected.items():
        frac = min(1.0, max(0, gps.get(split, 0)) / exp)
        w[split] = base.get(split, 0.0) * frac
        w["season"] += base.get(split, 0.0) * (1.0 - frac)
    return w


def inseason_projection(o: InseasonObs, k: float, weights: dict[str, float]) -> float:
    shrunk = (o.gp_td * o.fpg_td + k * o.fpg_base) / (o.gp_td + k) if o.has_base else o.fpg_td
    if o.gp_td <= 0:
        shrunk = o.fpg_base
    w = recency_weights_for(weights, {s: gp for s, (gp, _) in o.splits.items()})
    out = w["season"] * shrunk
    for s, (gp, v) in o.splits.items():
        if gp > 0 and w.get(s, 0.0) > 0:
            out += w[s] * v
    return out


def inseason_mae(obs: Sequence[InseasonObs], k: float, weights: dict[str, float]) -> float:
    return mae([inseason_projection(o, k, weights) - o.fpg_rest for o in obs])


def recency_grid(step: float = 0.05) -> list[dict[str, float]]:
    out = []
    r = lambda hi: [round(i * step, 4) for i in range(int(round(hi / step)) + 1)]
    for w30 in r(0.5):
        for w15 in r(0.4):
            for w7 in r(0.2):
                if w30 + w15 + w7 <= 0.8 + 1e-9:
                    out.append({"season": round(1 - w30 - w15 - w7, 4), "last30": w30, "last15": w15, "last7": w7})
    return out


_SPLITS = ("last30", "last15", "last7")


def _components(obs: Sequence[InseasonObs], k: float) -> list[tuple[float, float, float, float, float]]:
    """Per observation (A_season, A_30, A_15, A_7, actual) so that for base weights w the
    projection is sum(w_i * A_i): each split's weight shortfall goes back to the season rate."""
    out = []
    for o in obs:
        shrunk = inseason_projection(o, k, {"season": 1.0, "last30": 0.0, "last15": 0.0, "last7": 0.0})
        comps = [shrunk]
        for s in _SPLITS:
            gp, v = o.splits.get(s, (0, 0.0))
            frac = min(1.0, max(0, gp) / EXPECTED_GAMES[s]) if gp > 0 else 0.0
            comps.append((1.0 - frac) * shrunk + frac * v)
        out.append((comps[0], comps[1], comps[2], comps[3], o.fpg_rest))
    return out


def _mae_components(comps, w: dict[str, float]) -> float:
    ws, w30, w15, w7 = w["season"], w["last30"], w["last15"], w["last7"]
    return sum(abs(ws * a + w30 * b + w15 * c + w7 * d - y) for a, b, c, d, y in comps) / len(comps)


def fit_inseason(obs: Sequence[InseasonObs], rounds: int = 2) -> dict[str, Any]:
    """Coordinate descent: k_skater with the app's weights, then weights, then k again
    (starting from the app's current constants, ``blend.K_INSEASON`` / ``RECENCY_WEIGHTS``);
    k_goalie with the fitted weights (goalie samples are too small for their own weights)."""
    sk = [o for o in obs if o.group != "G"]
    gl = [o for o in obs if o.group == "G"]
    weights = dict(RECENCY_WEIGHTS)
    k = float(K_INSEASON["skater"])
    grid = recency_grid()

    def best_k(sample, w):
        return min(INSEASON_K_GRID, key=lambda kk: (_mae_components(_components(sample, kk), w), kk))

    if sk:
        for _ in range(rounds):
            k = best_k(sk, weights)
            comps = _components(sk, k)
            weights = min(grid, key=lambda w: _mae_components(comps, w))
        k = best_k(sk, weights)
    kg = best_k(gl, weights) if gl else float(K_INSEASON["goalie"])
    return {"k_skater": float(k), "k_goalie": float(kg), "recency_weights": weights,
            "mae_app": inseason_mae(sk, K_INSEASON["skater"], RECENCY_WEIGHTS) if sk else None,
            "mae_fitted": inseason_mae(sk, k, weights) if sk else None,
            "mae_shrink_only_fitted_k": inseason_mae(sk, k, {"season": 1.0, "last30": 0.0, "last15": 0.0,
                                                             "last7": 0.0}) if sk else None,
            "k_curve_skater": {kk: inseason_mae(sk, kk, weights) for kk in INSEASON_K_GRID} if sk else {},
            "n_skater_obs": len(sk), "n_goalie_obs": len(gl)}
