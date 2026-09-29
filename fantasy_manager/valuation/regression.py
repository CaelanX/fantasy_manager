"""Luck / regression signals from expected goals (MoneyPuck, filled by ``providers.xg_enrich``).

Inputs on ``Player`` (one MoneyPuck season: the current one, or the prior one while the player
has fewer than ``MIN_CURRENT_GP`` games this season - see ``xg_sample``):

* ``ixg_per_game``     individual expected goals per game (all situations)
* ``goals_minus_ixg``  goals - ixG, season total (all situations)
* ``onice_sh_pct``     team shooting % with the player on the ice, 5-on-5
* ``onice_xg_pct``     on-ice expected-goals share, 5-on-5 (context only, not used below)

Formulas (``luck_signals``), with GP = games of the MoneyPuck season used:

    goals            = ixG + (goals - ixG),   ixG = ixg_per_game * GP
    sh_pct_vs_xg     = goals / ixG                       (> 1: finishing above expected)
    goal_luck_fpg    = (goals - ixG) / GP * w_goal
    onice_delta      = onice_sh_pct - norm[group]       (norm: positional on-ice sh%, 5v5)
    assist_luck_fpg  = A/GP * clamp(1 - norm / onice_sh_pct, ASSIST_LUCK_CLAMP) * w_assist
    expected_regression_fpg = goal_luck_fpg + assist_luck_fpg
    confidence       = GP / (GP + CONFIDENCE_K)  (halved when the prior season is used)

``expected_regression_fpg`` is the part of the per-game fantasy scoring that is "luck": positive
means the player is running hot (expect his FPG to fall by up to that much), negative means cold.
The assist term assumes his assists scale with on-ice goals, so assists at a normal on-ice
shooting % would be ``A * norm / onice_sh_pct``. It slightly double counts a hot shooter's own
goals (they are part of the on-ice goals too); the clamp keeps tiny samples from exploding.

``w_goal`` / ``w_assist`` are the league's points for a goal / an assist (a ``PTS`` weight is added
to both); in category / roto leagues (no weights) they are 1.0, i.e. the signal is in goals /
assists per game.

Backtest (``backtest.xg_backtest``, MoneyPuck 2018-19..2025-26): about 59% of a season's goal
luck and 36% of its assist luck disappeared the next season (``next_season_drop_fpg``);
(G - ixG)/SOG correlates -0.53 with the next season's change in shooting %.

``regressed_rates`` / ``shrink_goals`` shrink the goal rate toward the ixG rate:

    G' = (GP * G + k * ixG/GP) / (GP + k),   k = K_GOALS (15 games)

so the weight of the observed goal rate, GP / (GP + k), rises with games played (0.5 at 15 GP,
0.85 at 82 GP). ``PTS`` moves by the same amount when present; nothing else changes. In the
backtest this helps raw single-season rates (last-season-only projections: MAE -1.5% at k=15,
-2.5% at k=60) but is neutral on top of the app's multi-season baseline, which already shrinks
toward the positional mean (MAE +0.15% / Spearman +0.001 at k=15) - use it on season-to-date
rates (in-season valuation, flags), not on the preseason history baseline.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Mapping

from ..models import Player

K_GOALS = 15.0               # games of shrinkage of goals/GP toward ixG/GP
MIN_CURRENT_GP = 5           # below this, xg_enrich uses the prior season's MoneyPuck line
CONFIDENCE_K = 20.0
PRIOR_CONFIDENCE = 0.5       # confidence multiplier when the signal comes from last season
ASSIST_LUCK_CLAMP = (-1.0, 0.6)
# Pooled 5-on-5 on-ice shooting % by position group, MoneyPuck 2025-26 (skaters with >= 200
# minutes). League shooting % has risen every season since 2018-19 (0.082 -> 0.095), so pass the
# season's own norms (``XgEnrichResult.onice_sh_norms`` / ``moneypuck.onice_sh_norms``) when known.
DEFAULT_ONICE_SH_PCT: dict[str, float] = {"F": 0.0956, "D": 0.0947}
# Share of each luck term that disappeared the following season in the backtest
# (``backtest.xg_backtest``, 2018-19..2025-26, >= 40 GP both seasons): slope of the change in
# G/GP on (G - ixG)/GP and of the change in A/GP on the assist-luck term.
GOAL_LUCK_REALIZED = 0.59
ASSIST_LUCK_REALIZED = 0.36

_GROUP = {"C": "F", "LW": "F", "RW": "F", "F": "F", "W": "F", "D": "D", "G": "G"}


@dataclass
class LuckSignals:
    gp: int                                 # games behind the MoneyPuck line used
    from_prior: bool                        # the line is last season's (current GP < 5)
    ixg_per_game: float
    goals_per_game: float
    goals_minus_ixg: float                  # season total
    sh_pct_vs_xg_ratio: float | None        # goals / ixG (None when ixG == 0)
    onice_sh_pct_delta: float | None        # on-ice sh% minus the positional norm
    goal_luck_fpg: float
    assist_luck_fpg: float
    expected_regression_fpg: float          # goal_luck_fpg + assist_luck_fpg (+ = running hot)
    confidence: float                       # 0..1

    @property
    def next_season_drop_fpg(self) -> float:
        """The part of the luck that historically went away the following season (backtest
        shares ``GOAL_LUCK_REALIZED`` / ``ASSIST_LUCK_REALIZED``); the rest behaved like talent."""
        return self.goal_luck_fpg * GOAL_LUCK_REALIZED + self.assist_luck_fpg * ASSIST_LUCK_REALIZED


def group_of(player: Player) -> str:
    if player.is_goalie:
        return "G"
    if "D" in player.positions and not set(player.positions) & {"C", "LW", "RW", "F"}:
        return "D"
    return "F"


def _norm(baseline: Mapping[str, float] | None, player: Player) -> float | None:
    base = dict(DEFAULT_ONICE_SH_PCT)
    base.update(baseline or {})
    g = group_of(player)
    if g in base:
        return base[g]
    for pos in player.positions:
        if pos in base:
            return base[pos]
        if _GROUP.get(pos) in base:
            return base[_GROUP[pos]]
    return None


def league_weights(scoring: Any = None) -> tuple[float, float]:
    """(points per goal, points per assist) of a points league (``PTS`` added to both);
    (1.0, 1.0) for category / roto scoring or None. Accepts a ScoringSystem with ``weights``,
    a ScoringConfig or a plain weights mapping."""
    w = None
    if isinstance(scoring, Mapping):
        w = scoring
    elif scoring is not None:
        if getattr(scoring, "kind", None) not in (None, "points"):
            return 1.0, 1.0
        w = getattr(scoring, "weights", None)
    if not w:
        return 1.0, 1.0
    pts = float(w.get("PTS", 0.0))
    return float(w.get("G", 0.0)) + pts, float(w.get("A", 0.0)) + pts


def xg_sample(player: Player) -> tuple[int, bool, str]:
    """(GP, from_prior, split) of the season the MoneyPuck fields describe - the same rule
    ``xg_enrich`` applies: this season once it has ``MIN_CURRENT_GP`` games, else last season."""
    cur = player.gp("season")
    if cur >= MIN_CURRENT_GP:
        return cur, False, "season"
    prior = player.gp("prior")
    if prior > 0:
        return prior, True, "prior"
    return cur, False, "season"


def luck_signals(player: Player, baseline_sh_pct_by_position: Mapping[str, float] | None = None,
                 scoring: Any = None, *, gp: int | None = None, from_prior: bool | None = None
                 ) -> LuckSignals | None:
    """Luck signals of a skater (None without MoneyPuck data or games). ``scoring`` supplies the
    goal / assist weights (``league_weights``); ``gp`` / ``from_prior`` override the sample
    inferred by ``xg_sample``. See the module docstring for the formulas."""
    if player.is_goalie or player.ixg_per_game is None or player.goals_minus_ixg is None:
        return None
    n, prior, split = xg_sample(player)
    if gp is not None:
        n = gp
    if from_prior is not None:
        prior = from_prior
        split = "prior" if prior else "season"
    if n <= 0:
        return None
    w_goal, w_assist = league_weights(scoring)
    ixg = player.ixg_per_game * n
    gmx = float(player.goals_minus_ixg)
    goals = ixg + gmx
    goal_luck = gmx / n * w_goal

    norm = _norm(baseline_sh_pct_by_position, player)
    delta = None
    assist_luck = 0.0
    if player.onice_sh_pct is not None and norm is not None:
        delta = player.onice_sh_pct - norm
        line = player.lines.get(split)
        if line is not None and line.gp > 0 and player.onice_sh_pct > 0:
            a_pg = float(line.stats.get("A", 0.0)) / line.gp
            lo, hi = ASSIST_LUCK_CLAMP
            frac = min(hi, max(lo, 1.0 - norm / player.onice_sh_pct))
            assist_luck = a_pg * frac * w_assist

    conf = n / (n + CONFIDENCE_K) * (PRIOR_CONFIDENCE if prior else 1.0)
    return LuckSignals(gp=n, from_prior=prior, ixg_per_game=player.ixg_per_game, goals_per_game=goals / n,
                       goals_minus_ixg=gmx, sh_pct_vs_xg_ratio=(goals / ixg) if ixg > 0 else None,
                       onice_sh_pct_delta=delta, goal_luck_fpg=goal_luck, assist_luck_fpg=assist_luck,
                       expected_regression_fpg=goal_luck + assist_luck, confidence=conf)


def shrink_goals(rates: Mapping[str, float], ixg_per_game: float | None, gp: float,
                 k: float = K_GOALS) -> dict[str, float]:
    """Rates with ``G`` shrunk toward ``ixg_per_game``: (gp*G + k*ixg)/(gp + k); ``PTS`` moves by
    the same amount. Unchanged without an ixG rate, a ``G`` rate or games; ``k=inf`` replaces
    goals with ixG."""
    out = dict(rates)
    if ixg_per_game is None or "G" not in out or gp <= 0:
        return out
    if k == float("inf"):
        new = float(ixg_per_game)
    else:
        new = (gp * out["G"] + k * ixg_per_game) / (gp + k)
    delta = new - out["G"]
    out["G"] = new
    if "PTS" in out:
        out["PTS"] = out["PTS"] + delta
    return out


def regressed_rates(rates: Mapping[str, float], signals: LuckSignals | None,
                    k: float = K_GOALS) -> dict[str, float]:
    """``rates`` with goals shrunk toward the ixG-implied rate of ``signals`` (weight of the
    observed goal rate GP/(GP+k)); a copy of ``rates`` when ``signals`` is None."""
    if signals is None:
        return dict(rates)
    return shrink_goals(rates, signals.ixg_per_game, signals.gp, k)
