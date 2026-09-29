"""Pluggable scoring systems: turn per-game stat rates into a single fantasy value.

* ``PointsScoring``      - weighted sum of per-game stats (H2H points leagues).
* ``CategoriesScoring``  - sum of weighted z-scores against a reference pool (usually the
                           rostered population); ratio stats are volume-weighted.
* ``RotoScoring``        - same per-player values as categories plus team-level roto
                           standings / category-balance helpers used when evaluating trades.
"""
from __future__ import annotations

import math
from abc import ABC, abstractmethod
from typing import Iterable, Mapping, Sequence

from ..models import ScoringConfig

# Categories where a lower number is better. Their weight is always forced negative.
# PIM is NOT here: most leagues count PIM positively; configure {"PIM": -1} to flip it.
NEGATIVE_BY_DEFAULT = frozenset({"GAA", "GA", "L"})
RATIO_CATEGORIES = frozenset({"SVPCT", "GAA"})


class ScoringSystem(ABC):
    kind: str = ""

    @abstractmethod
    def value(self, per_game_stats: dict[str, float],
              pool: list[dict[str, float]] | None = None) -> float:
        """Fantasy value per game for a player's rates (pool used by relative systems)."""


class PointsScoring(ScoringSystem):
    kind = "points"
    # Stats that mark a stat dict as a goalie line (goalie_weights then apply).
    GOALIE_MARKERS = frozenset({"GS", "SA", "SV", "GA"})

    def __init__(self, weights: dict[str, float], goalie_weights: dict[str, float] | None = None):
        self.weights = dict(weights)
        self.goalie_weights = dict(goalie_weights or {})

    def weights_for(self, per_game_stats: Mapping[str, float]) -> dict[str, float]:
        """Weights for this line: goalie-group overrides apply to goalie-looking lines."""
        if self.goalie_weights and not self.GOALIE_MARKERS.isdisjoint(per_game_stats):
            return {**self.weights, **self.goalie_weights}
        return self.weights

    def value(self, per_game_stats: dict[str, float],
              pool: list[dict[str, float]] | None = None) -> float:
        w = self.weights_for(per_game_stats)
        return sum(v * per_game_stats[k] for k, v in w.items() if k in per_game_stats)

    def breakdown(self, per_game_stats: dict[str, float]) -> dict[str, float]:
        """Per-stat contribution to fantasy points per game."""
        w = self.weights_for(per_game_stats)
        return {k: v * per_game_stats[k] for k, v in w.items() if k in per_game_stats}


def _mean_std(xs: Sequence[float]) -> tuple[float, float]:
    if not xs:
        return 0.0, 0.0
    mu = sum(xs) / len(xs)
    var = sum((x - mu) ** 2 for x in xs) / len(xs)
    return mu, math.sqrt(var)


def _saves(pg: Mapping[str, float]) -> float | None:
    if "SV" in pg:
        return float(pg["SV"])
    if "SA" in pg and "GA" in pg:
        return float(pg["SA"]) - float(pg["GA"])
    return None


def _svpct(pg: Mapping[str, float]) -> float | None:
    if "SVPCT" in pg:
        v = float(pg["SVPCT"])
        return v / 100.0 if v > 1.0 else v   # tolerate 91.5-style percentages
    sa, sv = pg.get("SA"), _saves(pg)
    if sa and sv is not None:
        return sv / sa
    return None


def _starts(pg: Mapping[str, float]) -> float:
    """Volume proxy for GAA: starts per game played (GS/GP); 1.0 when GS is unknown.

    Lines carry GS for goalies and StatLine.per_game() divides it by GP, giving a start
    share. Without TOI a start is treated as one full game, so GAA * starts ~ GA per game.
    """
    gs = pg.get("GS")
    return float(gs) if gs is not None and gs > 0 else 1.0


def _gaa(pg: Mapping[str, float]) -> float | None:
    if "GAA" in pg:
        return float(pg["GAA"])
    if "GA" in pg:  # GA per game over start share approximates GAA when TOI is unknown
        return float(pg["GA"]) / _starts(pg)
    return None


def _is_goalie_row(pg: Mapping[str, float]) -> bool:
    return any(k in pg for k in ("SVPCT", "SV", "SA", "GAA", "GA", "GS"))


class CategoriesScoring(ScoringSystem):
    """Sum over categories of w_c * z_c, with z computed against a reference pool.

    ``directions`` maps category -> signed weight (default +1). Categories in
    NEGATIVE_BY_DEFAULT (GAA, GA, L) always get a negative weight, so {"GAA": 1} and
    {"GAA": -1} both mean "lower is better"; any other category can be flipped with a
    negative weight (e.g. {"PIM": -1}) or scaled (e.g. {"HIT": 0.5}).

    Counting stats: z = (x - mu) / sigma over the pool entries that carry the stat (so
    skater stats are standardized among skaters and goalie stats among goalies).

    Ratio stats are volume-weighted, so .915 on 30 shots/game beats .915 on 20:
      SVPCT: q = SV - svpct_pool * SA (per game);  z = q / sigma_pool(q)
      GAA:   q = (GAA - gaa_pool) * starts;        z = q / sigma_pool(q)
             starts = GS per GP is the TOI proxy (1.0 when GS is missing); gaa_pool is the
             starts-weighted pool mean. The negative weight makes a low GAA good.

    Usage: ``fit(pool)`` once, then ``value(stats)`` repeatedly. ``value(stats, pool)`` fits
    lazily when a (different) pool object is passed. If no pool has ever been supplied,
    ``value`` returns 0.0 - z-scores are undefined without a reference population.
    """
    kind = "categories"

    def __init__(self, categories: list[str], directions: dict[str, float] | None = None):
        self.categories = list(categories)
        self.directions = dict(directions or {})
        self._params: dict[str, tuple[float, float]] | None = None
        self._pool_key: int | None = None
        self._mean_sa: float | None = None

    # -- configuration ------------------------------------------------------------
    def weight(self, cat: str) -> float:
        w = float(self.directions.get(cat, 1.0))
        if cat in NEGATIVE_BY_DEFAULT:
            return -abs(w) if w else -1.0
        return w

    def better(self, cat: str) -> int:
        """+1 when a higher team total is better, -1 when lower is better."""
        return -1 if self.weight(cat) < 0 else 1

    @property
    def fitted(self) -> bool:
        return self._params is not None

    def params(self) -> dict[str, tuple[float, float]]:
        """Fitted (reference, sigma) per category (reference = mean, or pool ratio)."""
        return dict(self._params or {})

    # -- fitting --------------------------------------------------------------------
    def fit(self, pool: Iterable[Mapping[str, float]]) -> "CategoriesScoring":
        """Precompute per-category reference stats from per-game stat dicts."""
        pool = [p for p in pool if p]
        goalies = [p for p in pool if _is_goalie_row(p)]
        params: dict[str, tuple[float, float]] = {}
        for c in self.categories:
            if c == "SVPCT":
                rows = []
                for p in goalies:
                    sv, sa = _saves(p), p.get("SA")
                    if sv is not None and sa:
                        rows.append((sv, float(sa)))
                if rows:
                    ref = sum(sv for sv, _ in rows) / sum(sa for _, sa in rows)
                    _, sd = _mean_std([sv - ref * sa for sv, sa in rows])
                    self._mean_sa = sum(sa for _, sa in rows) / len(rows)
                else:  # only SV% known: unweighted fallback
                    ref, sd = _mean_std([v for v in (_svpct(p) for p in goalies) if v is not None])
                    self._mean_sa = None
                params[c] = (ref, sd)
            elif c == "GAA":
                rows = [(g, _starts(p)) for p in goalies if (g := _gaa(p)) is not None]
                if rows:
                    ref = sum(g * v for g, v in rows) / sum(v for _, v in rows)
                    _, sd = _mean_std([(g - ref) * v for g, v in rows])
                    params[c] = (ref, sd)
                else:
                    params[c] = (0.0, 0.0)
            else:
                params[c] = _mean_std([float(p[c]) for p in pool if c in p])
        self._params = params
        return self

    def _ensure_fit(self, pool: list[dict[str, float]] | None) -> bool:
        if pool and (self._params is None or self._pool_key != id(pool)):
            self.fit(pool)
            self._pool_key = id(pool)
        return self._params is not None

    # -- valuation ------------------------------------------------------------------
    def z_scores(self, per_game_stats: Mapping[str, float],
                 pool: list[dict[str, float]] | None = None) -> dict[str, float]:
        """Raw-orientation z per category (higher stat -> higher z); missing stats omitted."""
        if not self._ensure_fit(pool):
            return {}
        assert self._params is not None
        out: dict[str, float] = {}
        for c in self.categories:
            ref, sd = self._params.get(c, (0.0, 0.0))
            if sd <= 0:
                continue
            if c == "SVPCT":
                sv, sa = _saves(per_game_stats), per_game_stats.get("SA")
                if sv is not None and sa:
                    out[c] = (sv - ref * sa) / sd
                elif (pct := _svpct(per_game_stats)) is not None:
                    out[c] = (pct - ref) * (self._mean_sa or 1.0) / sd
            elif c == "GAA":
                g = _gaa(per_game_stats)
                if g is not None:
                    out[c] = (g - ref) * _starts(per_game_stats) / sd
            elif c in per_game_stats:
                out[c] = (float(per_game_stats[c]) - ref) / sd
        return out

    def breakdown(self, per_game_stats: Mapping[str, float],
                  pool: list[dict[str, float]] | None = None) -> dict[str, float]:
        """Per-category weighted z contribution (w_c * z_c)."""
        return {c: self.weight(c) * z for c, z in self.z_scores(per_game_stats, pool).items()}

    def value(self, per_game_stats: dict[str, float],
              pool: list[dict[str, float]] | None = None) -> float:
        return sum(self.breakdown(per_game_stats, pool).values())

    # -- team level -----------------------------------------------------------------
    def team_totals(self, per_game_rows: Iterable[Mapping[str, float]]) -> dict[str, float]:
        """Team per-game totals: counting stats summed; SV% = sum SV / sum SA;
        GAA = starts-weighted mean."""
        rows = [r for r in per_game_rows if r]
        out: dict[str, float] = {}
        for c in self.categories:
            if c == "SVPCT":
                sv = sa = 0.0
                for r in rows:
                    s, a = _saves(r), r.get("SA")
                    if s is not None and a:
                        sv, sa = sv + s, sa + float(a)
                if sa:
                    out[c] = sv / sa
            elif c == "GAA":
                num = den = 0.0
                for r in rows:
                    g = _gaa(r)
                    if g is not None:
                        v = _starts(r)
                        num, den = num + g * v, den + v
                if den:
                    out[c] = num / den
            else:
                out[c] = sum(float(r.get(c, 0.0)) for r in rows)
        return out


class RotoScoring(CategoriesScoring):
    """Roto: per-player values identical to categories; adds team-level balance helpers."""
    kind = "roto"

    def roto_points(self, my_totals: Mapping[str, float],
                    league_totals: Iterable[Mapping[str, float]]) -> dict[str, float]:
        """Per-category standings points vs the OTHER teams: 1 + teams beaten + 0.5 * ties."""
        others = list(league_totals)
        pts: dict[str, float] = {}
        for c in self.categories:
            if c not in my_totals:
                continue
            mine, sgn = my_totals[c], self.better(c)
            p = 1.0
            for t in others:
                if c not in t:
                    continue
                d = sgn * (mine - t[c])
                p += 1.0 if d > 1e-12 else (0.5 if abs(d) <= 1e-12 else 0.0)
            pts[c] = p
        return pts

    def category_balance_penalty(self, my_totals: Mapping[str, float],
                                 league_totals: Iterable[Mapping[str, float]]) -> float:
        """How far my weak categories sit below mid-pack (0 = none below the median).

        mean over categories of max(0, mid - pts_c) / n_teams with mid = (n_teams + 1) / 2 and
        pts_c from ``roto_points``; ``league_totals`` are the OTHER teams' totals. Punting one
        of k categories entirely costs about 0.5 / k.
        """
        others = list(league_totals)
        n = len(others) + 1
        pts = self.roto_points(my_totals, others)
        if not pts:
            return 0.0
        mid = (n + 1) / 2
        return sum(max(0.0, mid - p) / n for p in pts.values()) / len(pts)


def pool_from_players(players: Iterable, splits: Sequence[str] = ("season", "projected", "prior"),
                      min_gp: int = 1) -> list[dict[str, float]]:
    """Per-game rows for a reference pool: each player's first split with >= min_gp games."""
    rows: list[dict[str, float]] = []
    for p in players:
        for s in splits:
            ln = p.lines.get(s)
            if ln is not None and ln.gp >= min_gp:
                rows.append(ln.per_game())
                break
    return rows


def fit_to_context(scoring: ScoringSystem, ctx) -> ScoringSystem:
    """Fit a relative scoring system on the league's rostered players (no-op for points)."""
    if isinstance(scoring, CategoriesScoring):
        scoring.fit(pool_from_players(p for t in ctx.teams for p in t.players))
    return scoring


def from_config(cfg: ScoringConfig) -> ScoringSystem:
    if cfg.kind == "points":
        return PointsScoring(cfg.weights, getattr(cfg, "goalie_weights", None))
    if cfg.kind == "categories":
        return CategoriesScoring(cfg.categories, cfg.weights)
    return RotoScoring(cfg.categories, cfg.weights)
