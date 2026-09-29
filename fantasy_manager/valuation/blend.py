"""Rate estimation: a multi-season baseline shrunk toward the positional mean, season-to-date
shrunk toward that baseline, then a light blend of recent form.

Constants come from ``params`` (fitted by ``fm backtest fit`` on 10+ seasons of NHL history):

* ``K_BASELINE`` {F, D, G} - games of shrinkage of the 5/4/3 GP-weighted last three seasons
  toward the positional mean (``multi_season_baseline``). Goalie FPG is so noisy that three
  seasons are worth little more than a league-average prior (k = 120).
* ``K_INSEASON`` {skater, goalie} - games of shrinkage of season-to-date toward the baseline.
* ``K_PROJECTION`` {skater, goalie} - shrinkage of a league (ESPN / Fantrax) projection toward
  the positional mean. Not fitted (no historical projections exist); kept at the old values.
* ``RECENCY_WEIGHTS`` - season / L30 / L15 / L7. Recent form barely predicts the rest of the
  season (every non-zero L15 / L7 weight made held-out checkpoints worse), so it is 0.85 / 0.15.

Live code reads every constant through the ``params`` accessors at call time (so a harness
version promoted by ``fm harness refit`` takes effect on the next run / dashboard refresh); the
module-level names below are read-only import-time snapshots kept for the backtest and report.
Functions that depend on a tunable constant take an optional ``params`` mapping (a full merged
params dict) so the harness can replay the exact live computation with candidate values.
"""
from __future__ import annotations

from typing import Any, Mapping, Sequence

from ..models import StatLine
from . import params as _params

# Import-time snapshots (read-only aliases; live code uses the params accessors).
K_BASELINE: dict[str, float] = _params.k_baseline()
K_INSEASON: dict[str, float] = _params.k_inseason()
K_PROJECTION: dict[str, float] = _params.k_projection()
PROJECTION_GP = 82
MIN_FULL_PROJECTION_GP = 20

RECENCY_WEIGHTS = _params.recency_weights()
EXPECTED_GAMES = {"last30": 13.0, "last15": 6.5, "last7": 3.3}

# Multi-season baseline: seasons N-1, N-2, N-3 weighted 5/4/3 by GP (Marcel's weights).
MULTI_SEASON_WEIGHTS = (5.0, 4.0, 3.0)
PRIOR_SPLITS = ("prior", "prior2", "prior3")
# League projection blend: 0.6 projection + 0.4 multi-season history once the player has at
# least PROJECTION_BLEND_FULL_GP NHL games over the three prior seasons (linear ramp below).
PROJECTION_WEIGHT = _params.projection_weight()
PROJECTION_BLEND_FULL_GP = 40

Params = Mapping[str, Any] | None


def shrink_k(is_goalie: bool, params: Params = None) -> float:
    """In-season k: games of shrinkage of season-to-date toward the preseason baseline."""
    return _params.k_inseason("goalie" if is_goalie else "skater", params)


def baseline_k(group: str) -> float:
    """k of the multi-season history toward the positional mean for group F / D / G."""
    kb = _params.k_baseline()
    return kb.get(group, kb["F"])


def projection_k(is_goalie: bool, params: Params = None) -> float:
    return _params.k_projection("goalie" if is_goalie else "skater", params)


def shrink_toward(cur: dict[str, float], gp: float, prior: dict[str, float], k: float
                  ) -> dict[str, float]:
    """(gp*cur + k*prior)/(gp+k) per stat; a stat missing on one side borrows the other."""
    if gp <= 0 or not cur:
        return dict(prior)
    if not prior:
        return dict(cur)
    out: dict[str, float] = {}
    for key in set(cur) | set(prior):
        c = cur.get(key, prior.get(key, 0.0))
        p = prior.get(key, c)
        out[key] = (gp * c + k * p) / (gp + k)
    return out


def shrunk_rates(current: StatLine | None, prior_or_proj: StatLine | None,
                 is_goalie: bool, params: Params = None) -> dict[str, float]:
    """rate = (GP*rate_cur + k*rate_prior)/(GP+k); GP==0 -> prior rates; no prior -> current."""
    prior = prior_or_proj.per_game() if prior_or_proj else {}
    gp = current.gp if current else 0
    if gp <= 0:
        return dict(prior)
    return shrink_toward(current.per_game(), gp, prior, shrink_k(is_goalie, params))


def baseline_sample(line: StatLine | None) -> float:
    """Effective games behind a baseline line for shrinking it toward the positional mean.

    ESPN projections are already regressed, so a projection counts as a full season
    (PROJECTION_GP) unless it projects very few games (< MIN_FULL_PROJECTION_GP), in which
    case its own GP is used. Any other line counts its own GP."""
    if line is None or line.gp <= 0:
        return 0.0
    if line.split == "projected" and line.gp >= MIN_FULL_PROJECTION_GP:
        return float(PROJECTION_GP)
    return float(line.gp)


def shrink_baseline(line: StatLine | None, mean: dict[str, float], is_goalie: bool,
                    k: float | None = None, params: Params = None) -> tuple[dict[str, float], float]:
    """(baseline per-game rates shrunk toward the positional-group mean, effective GP).

    rate = (GP_base*rate_base + k*mean)/(GP_base + k), k = ``projection_k`` unless given.
    Stats the mean does not carry keep the baseline rate; an empty mean leaves the baseline
    untouched."""
    if line is None or line.gp <= 0:
        return {}, 0.0
    n = baseline_sample(line)
    rates = line.per_game()
    if not mean:
        return rates, n
    k = projection_k(is_goalie, params) if k is None else k
    return {key: (n * v + k * mean.get(key, v)) / (n + k) for key, v in rates.items()}, n


def multi_season_sample(lines: Sequence[StatLine | None], weights: Sequence[float] = MULTI_SEASON_WEIGHTS
                        ) -> tuple[dict[str, float], float, int] | None:
    """(GP-weighted per-game rates, effective games, total GP) over ``lines`` = seasons N-1,
    N-2, N-3 (most recent first; None / 0-GP entries are skipped), or None without games.

    Each season's rates weigh ``w_i * GP_i`` (5/4/3); a stat only some seasons carry is averaged
    over those seasons. Effective games are ``sum(w_i * GP_i) / w_1``, i.e. in units of games of
    the most recent season, which is what ``K_BASELINE`` is measured in."""
    num: dict[str, float] = {}
    den: dict[str, float] = {}
    wgp = 0.0
    gp_total = 0
    for w, line in zip(weights, lines):
        if line is None or line.gp <= 0:
            continue
        wg = w * line.gp
        wgp += wg
        gp_total += line.gp
        for key, v in line.per_game().items():
            num[key] = num.get(key, 0.0) + wg * v
            den[key] = den.get(key, 0.0) + wg
    if wgp <= 0:
        return None
    return {k: num[k] / den[k] for k in num}, wgp / weights[0], gp_total


def multi_season_baseline(lines: Sequence[StatLine | None], position_group: str,
                          means: dict[str, dict[str, float]], k: float | None = None) -> dict[str, float]:
    """The backtest's ``fm_multi`` rates (before aging): seasons N-1..N-3 weighted 5/4/3 by GP,
    shrunk toward the position group's mean with ``k`` (default ``K_BASELINE[group]``) games:
    ``(n_eff * rate + k * mean) / (n_eff + k)``. {} without any games; no mean -> unshrunk.

    ``backtest.models.FmMulti`` calls this function, so the app and the backtest cannot drift."""
    sample = multi_season_sample(lines)
    if sample is None:
        return {}
    rates, n, _ = sample
    mean = means.get(position_group) or {}
    if not mean:
        return rates
    return shrink_toward(rates, n, mean, baseline_k(position_group) if k is None else k)


def projection_blend_weight(history_gp: int, params: Params = None) -> float:
    """Weight of the league projection when blended with the multi-season history:
    ``params.projection_weight()`` (0.6) with >= PROJECTION_BLEND_FULL_GP (40) prior NHL GP, 1.0
    without NHL history (rookies), linear in between."""
    if history_gp <= 0:
        return 1.0
    frac = min(1.0, history_gp / PROJECTION_BLEND_FULL_GP)
    return 1.0 - (1.0 - _params.projection_weight(params)) * frac


def blend_rates(a: dict[str, float], b: dict[str, float], wa: float) -> dict[str, float]:
    """wa*a + (1-wa)*b per stat; a stat only one side carries keeps that side's rate."""
    out: dict[str, float] = {}
    for key in set(a) | set(b):
        va = a.get(key, b.get(key, 0.0))
        vb = b.get(key, va)
        out[key] = wa * va + (1.0 - wa) * vb
    return out


def recency_weights(gp_l30: int, gp_l15: int, gp_l7: int, params: Params = None) -> dict[str, float]:
    """Base weights (``params.recency_weights()``) scaled by min(1, GP/expected); the shortfall
    goes back to 'season'."""
    base = _params.recency_weights(params)
    gps = {"last30": gp_l30, "last15": gp_l15, "last7": gp_l7}
    w = {"season": base["season"]}
    for split, exp in EXPECTED_GAMES.items():
        frac = min(1.0, max(0, gps[split]) / exp)
        w[split] = base[split] * frac
        w["season"] += base[split] * (1.0 - frac)
    return w


def blend_recency(season_rates: dict[str, float], l30: StatLine | None, l15: StatLine | None,
                  l7: StatLine | None, params: Params = None) -> dict[str, float]:
    """Blend season rates with recent splits using `recency_weights`."""
    splits = {"last30": l30, "last15": l15, "last7": l7}
    w = recency_weights(*(s.gp if s else 0 for s in (l30, l15, l7)), params=params)
    rates = {k: w["season"] * v for k, v in season_rates.items()}
    for name, line in splits.items():
        if not line or w[name] <= 0:
            continue
        pg = line.per_game()
        for k in set(rates) | set(pg):
            # stats absent from a split fall back to the season rate
            rates[k] = rates.get(k, 0.0) + w[name] * pg.get(k, season_rates.get(k, 0.0))
    # keys present only in splits need the season share filled from the split itself
    for k in rates:
        if k not in season_rates:
            share = sum(w[n] for n, ln in splits.items() if ln and w[n] > 0 and k in ln.per_game())
            if share > 0:
                rates[k] = rates[k] / share
    return rates


# --------------------------------------------------------------------------- preseason (weak signal)
#
# Unproven skaters only (providers.preseason_enrich.is_unproven): their preseason per-game rates
# are blended into the baseline, baseline' = (1 - w) * baseline + w * preseason, on the
# points-driving stats only. w = min(0.25, preseason GP / 12): 1 GP -> 0.08, 2 -> 0.17, 3+ -> 0.25.
# The cap is low on purpose: preseason rosters are half AHL / junior players, veterans play a
# few games at reduced effort, coaches audition kids in offensive roles and on the power play,
# and 3-7 games are a tiny sample, so even a dominant preseason is weak evidence of regular-
# season scoring. It stays a nudge on top of the projection / pedigree baseline, never a
# replacement. Regular-season games supersede it: w fades linearly to 0 over the first 20 GP.
PRESEASON_STATS = ("G", "A", "PTS", "SOG", "PPP", "PPG", "PPA")
PRESEASON_MAX_WEIGHT = 0.25
PRESEASON_FULL_GP = 12
PRESEASON_SHRINK_K = 9  # games; 3 GP -> 25% of max, 6 GP -> 40%, 9 GP -> 50%
PRESEASON_FADE_GP = 20


def preseason_weight(gp_pre: int, season_gp: int = 0) -> float:
    """Weight of preseason rates in an unproven player's baseline.

    ``PRESEASON_MAX_WEIGHT * gp_pre / (gp_pre + PRESEASON_SHRINK_K)``, faded by
    ``max(0, 1 - season_gp / PRESEASON_FADE_GP)``. The shrinkage denominator keeps a
    3-game preseason at ~6% and a full 6-game preseason at ~10%: exhibition games are
    weak evidence (mixed rosters, weak opponents), so they nudge a rookie's value
    rather than rewrite it. 0 without preseason games.
    """
    if gp_pre <= 0:
        return 0.0
    w = PRESEASON_MAX_WEIGHT * gp_pre / (gp_pre + PRESEASON_SHRINK_K)
    return w * max(0.0, 1.0 - max(0, season_gp) / PRESEASON_FADE_GP)


def blend_preseason(base: dict[str, float], pre: Mapping[str, float], w: float,
                    stats: Sequence[str] = PRESEASON_STATS) -> dict[str, float]:
    """``(1 - w) * base + w * pre`` for ``stats`` both sides carry; every other stat (and a stat
    only one side has) keeps the baseline rate, so the blend never invents a stat."""
    out = dict(base)
    if w <= 0:
        return out
    for key in stats:
        if key in base and key in pre:
            out[key] = (1.0 - w) * base[key] + w * float(pre[key])
    return out
