"""Candidate preseason projection models.

Every model maps a player's history (rows for seasons < N, oldest first) and age on Oct 1 of
season N to projected per-game rates for season N; ``project`` turns those into fantasy points
per game with the context's scoring. Rates (not FPG) are projected so one run can be scored
under any point preset.

* ``naive_last_season`` - last season's per-game rates.
* ``marcel``            - 5/4/3-weighted last three seasons (weighted by GP), regressed toward the
                          positional mean with ``MARCEL_K`` games, classic Marcel age adjustment.
* ``fm_current``        - exactly what the app does preseason without a league projection:
                          ``valuation.valuate.baseline_rates`` on a Player carrying seasons N-1..N-3
                          as ``prior`` / ``prior2`` / ``prior3`` lines (the packaged fitted k and
                          age factor from ``valuation/fitted_params.json``, i.e. fitted on all
                          seasons - in-sample for the backtest).
* ``fm_fitted``         - the same structure with k per position group and an age factor
                          fitted from data (``FittedParams``; see ``fit.py``).
* ``fm_multi``          - Marcel's 5/4/3 GP-weighted seasons, shrunk toward the positional mean
                          with a fitted k per group, times the fitted age factor; k and age factor
                          re-fitted before each target season (rolling origin). The rates come
                          from ``valuation.blend.multi_season_baseline``, the app's own function.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Mapping, Protocol

from ..models import StatLine
from ..scoring import PointsScoring
from ..valuation import params as app_params
from ..valuation.blend import MULTI_SEASON_WEIGHTS, PRIOR_SPLITS, multi_season_baseline, multi_season_sample, shrink_toward
from ..valuation.valuate import baseline_rates, positional_means
from .data import PlayerSeason, SeasonTable, prev_season
from .scoring import fpg

Rates = dict[str, float]

MARCEL_WEIGHTS = MULTI_SEASON_WEIGHTS  # seasons N-1, N-2, N-3 (5/4/3)
MARCEL_K = 30.0                        # regression, in games of the most recent season
MARCEL_PEAK_AGE = 27.0
MARCEL_YOUNG = 0.006                   # +0.6% per year below peak
MARCEL_OLD = 0.003                     # -0.3% per year above peak


@dataclass
class FittedParams:
    """Constants for ``fm_fitted``: shrinkage k per group and a year-over-year age factor
    (expected FPG ratio from the season at age a to the next one) per group."""
    k: dict[str, float] = field(default_factory=lambda: {"F": 20.0, "D": 20.0, "G": 12.0})
    k_multi: dict[str, float] = field(default_factory=lambda: {"F": 10.0, "D": 10.0, "G": 30.0})
    age_yoy: dict[str, dict[int, float]] = field(default_factory=dict)
    age_groups: tuple[str, ...] = ("F", "D")

    def age_factor(self, group: str, age_prev: float | None) -> float:
        """Multiplier from last season (age ``age_prev`` on Oct 1) to this one."""
        return app_params.age_factor(group, age_prev, self.age_yoy, self.age_groups)

    def to_dict(self) -> dict[str, Any]:
        return {"k": self.k, "k_multi": self.k_multi, "age_yoy": {g: {str(a): r for a, r in c.items()} for g, c in self.age_yoy.items()},
                "age_groups": list(self.age_groups)}

    @classmethod
    def from_dict(cls, d: Mapping[str, Any]) -> "FittedParams":
        return cls(k={g: float(v) for g, v in (d.get("k") or {}).items()},
                   k_multi={g: float(v) for g, v in (d.get("k_multi") or {}).items()} or FittedParams().k_multi,
                   age_yoy={g: {int(a): float(r) for a, r in c.items()} for g, c in (d.get("age_yoy") or {}).items()},
                   age_groups=tuple(d.get("age_groups") or ("F", "D")))


@dataclass
class ProjectionContext:
    season: int                                  # target season N
    scoring: PointsScoring
    means: dict[str, Rates]                      # positional means of season N-1 (F / D / G)
    fitted: FittedParams | None = None


def group_means(table: SeasonTable, season: int) -> dict[str, Rates]:
    """The app's ``positional_means`` over season ``season``'s rows (as 'prior' lines)."""
    players = [r.to_player("prior") for r in table.by_season.get(season, {}).values() if r.gp > 0]
    return positional_means(players)


def make_context(table: SeasonTable, season: int, scoring: PointsScoring,
                 fitted: FittedParams | None = None) -> ProjectionContext:
    return ProjectionContext(season=season, scoring=scoring, means=group_means(table, prev_season(season)),
                             fitted=fitted)


class Model(Protocol):
    name: str

    def rates(self, history: list[PlayerSeason], age: float | None, ctx: ProjectionContext) -> Rates | None: ...


def project(model: Model, history: list[PlayerSeason], age: float | None, ctx: ProjectionContext) -> float | None:
    """Projected fantasy points per game for season ``ctx.season`` (None: model abstains)."""
    r = model.rates(history, age, ctx)
    return None if r is None else fpg(r, ctx.scoring)


def _last(history: list[PlayerSeason], season: int | None = None) -> PlayerSeason | None:
    rows = [h for h in history if h.gp > 0 and (season is None or h.season == season)]
    return rows[-1] if rows else None


class NaiveLastSeason:
    name = "naive"

    def rates(self, history, age, ctx):
        last = _last(history)
        return last.per_game() if last else None


def prior_rows(history: list[PlayerSeason], target: int, n: int = len(MARCEL_WEIGHTS)) -> list[PlayerSeason | None]:
    """Rows of seasons N-1, N-2, ... (None where the player has no games that season)."""
    return [_last(history, target - 10001 * (i + 1)) for i in range(n)]


def prior_lines(rows: list[PlayerSeason | None]) -> list[StatLine | None]:
    """The rows as the app's ``prior`` / ``prior2`` / ``prior3`` StatLines."""
    return [r.statline(split) if r is not None else None for r, split in zip(rows, PRIOR_SPLITS)]


def weighted_rates(history: list[PlayerSeason], target: int, weights: tuple[float, ...] = MARCEL_WEIGHTS
                   ) -> tuple[Rates, float, str] | None:
    """(GP-weighted per-game rates over seasons N-1, N-2, ..., effective games in units of the
    most recent season, position group) or None without any of those seasons. Same arithmetic
    as the app (``blend.multi_season_sample``)."""
    rows = prior_rows(history, target, len(weights))
    group = next((r.group for r in rows if r is not None), None)
    sample = multi_season_sample(prior_lines(rows), weights)
    if sample is None or group is None:
        return None
    return sample[0], sample[1], group


def marcel_age_factor(age: float | None) -> float:
    if age is None:
        return 1.0
    if age < MARCEL_PEAK_AGE:
        return 1.0 + MARCEL_YOUNG * (MARCEL_PEAK_AGE - age)
    return 1.0 - MARCEL_OLD * (age - MARCEL_PEAK_AGE)


class Marcel:
    name = "marcel"

    def __init__(self, weights: tuple[float, ...] = MARCEL_WEIGHTS, k: float = MARCEL_K, age: bool = True):
        self.weights = weights
        self.k = k
        self.age = age

    def rates(self, history, age, ctx):
        w = weighted_rates(history, ctx.season, self.weights)
        if w is None:
            return None
        rate, n, group = w
        mean = ctx.means.get(group) or {}
        out = {k: (n * v + self.k * mean.get(k, v)) / (n + self.k) for k, v in rate.items()}
        if self.age and group != "G":
            f = marcel_age_factor(age)
            out = {k: v * f for k, v in out.items()}
        return out


class FmCurrent:
    """The app's preseason baseline without a league projection: ``valuate.baseline_rates`` on
    seasons N-1..N-3 (``prior`` / ``prior2`` / ``prior3``) with the packaged fitted params."""
    name = "fm_current"

    def rates(self, history, age, ctx):
        rows = prior_rows(history, ctx.season)
        latest = next((r for r in rows if r is not None), None)
        if latest is None:
            return None
        player = latest.to_player("prior")
        player.lines = {ln.split: ln for ln in prior_lines(rows) if ln is not None}
        rates, _, _, _ = baseline_rates(player, ctx.means, age)
        return rates or None


class FmFitted:
    name = "fm_fitted"

    def __init__(self, params: FittedParams | None = None):
        self.params = params

    def rates(self, history, age, ctx):
        params = self.params or ctx.fitted or FittedParams()
        last = _last(history, prev_season(ctx.season))
        if last is None:
            return None
        mean = ctx.means.get(last.group) or {}
        k = params.k.get(last.group, 20.0)
        rates = shrink_toward(last.per_game(), last.gp, mean, k) if mean else last.per_game()
        f = params.age_factor(last.group, None if age is None else age - 1.0)
        return {key: v * f for key, v in rates.items()}


class FmMulti:
    """Candidate: 5/4/3-weighted seasons, fitted k per group, fitted age factor."""
    name = "fm_multi"

    def __init__(self, params: FittedParams | None = None):
        self.params = params

    def rates(self, history, age, ctx):
        params = self.params or ctx.fitted or FittedParams()
        rows = prior_rows(history, ctx.season)
        group = next((r.group for r in rows if r is not None), None)
        if group is None:
            return None
        out = multi_season_baseline(prior_lines(rows), group, ctx.means, params.k_multi.get(group, 10.0))
        if not out:
            return None
        f = params.age_factor(group, None if age is None else age - 1.0)
        return {key: v * f for key, v in out.items()}


MODELS: dict[str, Any] = {"naive": NaiveLastSeason, "marcel": Marcel, "fm_current": FmCurrent,
                          "fm_fitted": FmFitted, "fm_multi": FmMulti}
DEFAULT_MODELS = ("naive", "marcel", "fm_current", "fm_fitted", "fm_multi")


def get_models(names: list[str] | tuple[str, ...] | None = None) -> list[Model]:
    names = list(names or DEFAULT_MODELS)
    bad = [n for n in names if n not in MODELS]
    if bad:
        raise ValueError(f"unknown model(s) {', '.join(bad)}; choose from {', '.join(MODELS)}")
    return [MODELS[n]() for n in names]
