"""Dynasty / keeper value: a production-by-age trajectory over a weighted multi-year horizon,
a draft-pedigree boost for young players, and a market prior from league-wide ownership.

Model value (H = ``ctx.keeper_horizon_years``, weights ``w_y`` and terminal factor ``T`` from
the dynasty mode, see MODE_WEIGHTS)::

    value = w_0 * fpg_season
          + sum_{y=1}^{H-1} w_y * fpg_healthy * g(y) * ped
          + 0.8^H * fpg_healthy * g(H) * ped * T                  (terminal / asset term)

* ``g(y) = curve(age + y) / curve(age)`` is the growth (or decline) of production relative
  to today (``g(y) * ped`` is capped at 2.5), from a positional production curve normalised to 1.0 at the peak
  (PRODUCTION_CURVES, linear interpolation, flat outside the anchors; F and D fitted from NHL
  history, G hand-set). A 19-year-old forward producing 3 FPG today projects ~3.3 FPG at 20
  and ~3.5 at 21; a 31-year-old forward declines ~6% a year. The cap keeps 17-year-old top
  picks from absurd multiples.
* ``fpg_season`` is this season's availability- and start-share-adjusted value; later years use
  the healthy value ``fpg x start share``, so a player on IR today is not discounted forever.
* The terminal term stands in for the seasons beyond the horizon (0.8^H discount times a
  multiple of the year-H value), so a long-horizon prospect is not truncated at year H.
* ``ped`` (players <= 23 only): draft pedigree, overall pick 1-3 x1.30, 4-10 x1.20, 11-32
  x1.10, round 2 x1.03, later / undrafted x1.0; it fades linearly to 1.0 between ages 23
  and 25 and as career NHL GP approach 200 (proven players are valued on production).
  It multiplies the future years only, never this season.

Modes (``ctx.dynasty_mode``): contend weights this season most ([1.0, 0.65, 0.45], T=0.8),
balanced is a plain 0.8/yr discount ([1.0, 0.8, 0.64], T=1.5), rebuild favours later seasons
([0.7, 0.9, 0.9], T=2.0). Horizons longer than the weight list extend it geometrically.

Rookie confidence (``valuation.rookie``): for an unproven player valued by the rookie model
(``PlayerValue.rookie``), the upside term (growth + pedigree premium over a flat trajectory) is
scaled by ``0.6 + 0.4 * confidence`` (confidence 0..1 = how much evidence - projection, NHLe,
pedigree - stands behind his rates), so a thinly-evidenced prospect's projected growth counts
less (reason ROOKIE_CONFIDENCE).

Market prior: when at least MARKET_MIN_PLAYERS players carry a league-wide % rostered, every
such player is ranked by it and mapped to the model value at the same rank among all modelled
players (a rank-to-value transform); ``value = 0.65 * model + 0.35 * market``.
"""
from __future__ import annotations

from datetime import date
from typing import TYPE_CHECKING, Iterable, Mapping

from pydantic import BaseModel, Field

from ..models import LeagueContext, Player, Reason
from . import params as _params

if TYPE_CHECKING:
    from .valuate import PlayerValue

DISCOUNT = 0.8
MAX_GROWTH = 2.5
MIN_GP_FOR_RATE = 10

# (year weights, terminal factor) per dynasty mode
MODE_WEIGHTS: dict[str, tuple[tuple[float, ...], float]] = {
    "contend": ((1.0, 0.65, 0.45), 0.8),
    "balanced": ((1.0, 0.8, 0.64), 1.5),
    "rebuild": ((0.7, 0.9, 0.9), 2.0),
}
DEFAULT_MODE = "contend"

# Production relative to the positional peak (1.0), piecewise linear, flat outside the ends.
# F and D are fitted from 10+ seasons of NHL history (``fm backtest fit``, delta method, see
# valuation/params.py); forwards peak at 25, defensemen at 23. The goalie curve stays hand-set:
# the fitted one rests on very few goalies at either end.
_FITTED_CURVES = _params.dynasty_age_curves()
PRODUCTION_CURVES: dict[str, tuple[tuple[float, float], ...]] = {
    "F": _FITTED_CURVES["F"],
    "D": _FITTED_CURVES["D"],
    "G": ((18, 0.55), (20, 0.65), (22, 0.75), (24, 0.85), (27, 1.00), (32, 1.00), (34, 0.85),
          (36, 0.70)),
}
# Kept for callers of the old name: the curve anchors per position group.
AGE_CURVES = PRODUCTION_CURVES

PEDIGREE_MAX_AGE = 23.0
PEDIGREE_FADE_AGE = 25.0
PEDIGREE_PROVEN_GP = 200

ROOKIE_UPSIDE_FLOOR = 0.6

MARKET_WEIGHT = 0.35
MARKET_MIN_PLAYERS = 50


class DynastyValue(BaseModel):
    player: Player
    value: float
    age: float | None
    age_mult: float                   # production-curve level today (1.0 = positional peak)
    upside: float                     # growth / pedigree premium over a flat trajectory (signed)
    horizon_years: int
    model_value: float | None = None  # before the market prior
    market_value: float | None = None
    pedigree: float = 1.0
    mode: str = DEFAULT_MODE
    reasons: list[Reason] = Field(default_factory=list)


def position_group(p: Player) -> str:
    if p.is_goalie:
        return "G"
    if "D" in p.positions and not set(p.positions) & {"C", "LW", "RW", "F"}:
        return "D"
    return "F"


def production_curve(position_group: str, age: float | None) -> float:
    """Production relative to the positional peak for F/D/G (unknown group -> F); 1.0 when
    age is unknown."""
    if age is None:
        return 1.0
    pts = PRODUCTION_CURVES.get(position_group, PRODUCTION_CURVES["F"])
    if age <= pts[0][0]:
        return pts[0][1]
    if age >= pts[-1][0]:
        return pts[-1][1]
    for (a0, m0), (a1, m1) in zip(pts, pts[1:]):
        if a0 <= age <= a1:
            return m0 + (m1 - m0) * (age - a0) / (a1 - a0)
    return 1.0  # unreachable


# Backwards-compatible name.
age_multiplier = production_curve


def growth_ratio(position_group: str, age: float | None, years: float) -> float:
    """curve(age + years) / curve(age); 1.0 when age is unknown."""
    if age is None:
        return 1.0
    return production_curve(position_group, age + years) / production_curve(position_group, age)


def trajectory_factor(position_group: str, age: float | None, years: float, pedigree: float = 1.0) -> float:
    """Future-season multiplier on today's healthy FPG: growth ratio x pedigree, capped at
    MAX_GROWTH (keeps a 17-year-old top pick from absurd multiples)."""
    return min(MAX_GROWTH, growth_ratio(position_group, age, years) * pedigree)


def mode_weights(mode: str | None, years: int) -> tuple[list[float], float]:
    """(year weights for years 0..years-1, terminal factor) for a dynasty mode. Longer horizons
    extend the list geometrically with its last ratio (0.8 for a flat tail)."""
    base, terminal = MODE_WEIGHTS.get(mode or DEFAULT_MODE, MODE_WEIGHTS[DEFAULT_MODE])
    w = list(base)
    ratio = w[-1] / w[-2] if len(w) >= 2 and w[-2] > 0 else DISCOUNT
    if ratio >= 1.0:
        ratio = DISCOUNT
    while len(w) < years:
        w.append(w[-1] * ratio)
    return w[:max(1, years)], terminal


def pedigree_base(p: Player) -> tuple[float, str]:
    """(multiplier, label) from draft position alone."""
    ov, rnd = p.draft_overall, p.draft_round
    if ov is not None and ov <= 3:
        return 1.30, f"#{ov} overall pick"
    if ov is not None and ov <= 10:
        return 1.20, f"#{ov} overall pick"
    if ov is not None and ov <= 32 or rnd == 1:
        return 1.10, f"1st-round pick (#{ov})" if ov is not None else "1st-round pick"
    if rnd == 2:
        return 1.03, f"2nd-round pick (#{ov})" if ov is not None else "2nd-round pick"
    return 1.0, "later-round pick" if ov is not None else "undrafted / unknown"


def pedigree_multiplier(p: Player, age: float | None) -> tuple[float, str | None]:
    """Draft-pedigree boost to the growth trajectory, fading to 1.0 by age 25 and by 200 career
    NHL games. (1.0, None) when it does not apply."""
    base, label = pedigree_base(p)
    if age is None or base <= 1.0 or age >= PEDIGREE_FADE_AGE:
        return 1.0, None
    f_age = 1.0 if age <= PEDIGREE_MAX_AGE else (PEDIGREE_FADE_AGE - age) / (PEDIGREE_FADE_AGE - PEDIGREE_MAX_AGE)
    gp = p.career_gp or 0
    f_gp = max(0.0, 1.0 - gp / PEDIGREE_PROVEN_GP)
    mult = 1.0 + (base - 1.0) * f_age * f_gp
    if mult <= 1.0 + 1e-9:
        return 1.0, None
    year = f" {p.draft_year}" if p.draft_year else ""
    return mult, f"{label}{year}, {gp} NHL GP"


def age_on(birth_date: date | None, as_of: date) -> float | None:
    if birth_date is None:
        return None
    return (as_of - birth_date).days / 365.25


def production_rate(p: Player) -> float | None:
    """PTS/GP for skaters, SV% for goalies from the first of season / prior / projected with at
    least MIN_GP_FOR_RATE games; None when every line is a small sample."""
    lines = [p.lines.get(s) for s in ("season", "prior", "projected")]
    line = next((ln for ln in lines if ln is not None and ln.gp >= MIN_GP_FOR_RATE), None)
    if line is None:
        return None
    if p.is_goalie:
        sv = line.stats.get("SVPCT")
        if sv is None and line.stats.get("SA"):
            sv = line.stats.get("SV", line.stats["SA"] - line.stats.get("GA", 0.0)) / line.stats["SA"]
        return float(sv) if sv is not None else None
    pts = line.stats.get("PTS")
    if pts is None and ("G" in line.stats or "A" in line.stats):
        pts = line.stats.get("G", 0.0) + line.stats.get("A", 0.0)
    return pts / line.gp if pts is not None else None


def horizon_value(fpg: float, group: str, age: float | None, years: int,
                  later_fpg: float | None = None, mode: str | None = "balanced",
                  pedigree: float = 1.0) -> float:
    """Trajectory value: w_0 * fpg + sum_{y>=1} w_y * later * g(y) * ped + terminal term
    (see the module docstring). `later_fpg` is the healthy value for future seasons."""
    return _trajectory(fpg, group, age, max(1, years), fpg if later_fpg is None else later_fpg,
                       mode, pedigree)[0]


def _trajectory(fpg: float, group: str, age: float | None, years: int, later: float,
                mode: str | None, ped: float) -> tuple[float, float, float]:
    """(total, flat total without growth / pedigree, terminal term)."""
    w, terminal_factor = mode_weights(mode, years)
    total = w[0] * fpg
    flat = w[0] * fpg
    for y in range(1, years):
        total += w[y] * later * trajectory_factor(group, age, y, ped)
        flat += w[y] * later
    term = DISCOUNT ** years * later * trajectory_factor(group, age, years, ped) * terminal_factor
    total += term
    flat += DISCOUNT ** years * later * terminal_factor
    return total, flat, term


def _healthy_fpg(pv: "PlayerValue", fpg_season: float) -> float:
    """Healthy per-game value for future seasons: fpg x goalie start share (no availability
    discount). Falls back to fpg_season when the value object carries no healthy fpg."""
    fpg = getattr(pv, "fpg", None)
    if fpg is None:
        return fpg_season
    share = getattr(pv, "start_share", None)
    return float(fpg) * (float(share) if share is not None else 1.0)


def market_values(model: Mapping[str, float], owned: Mapping[str, float]) -> dict[str, float]:
    """Rank-to-value transform: the player with the i-th highest % owned gets the i-th highest
    model value (among every modelled player). Ties in % owned share their mean rank value."""
    ranked_values = sorted(model.values(), reverse=True)
    if not ranked_values:
        return {}
    cids = sorted((c for c in owned if c in model), key=lambda c: -owned[c])
    out: dict[str, float] = {}
    i = 0
    while i < len(cids):
        j = i
        while j + 1 < len(cids) and owned[cids[j + 1]] == owned[cids[i]]:
            j += 1
        vals = [ranked_values[min(k, len(ranked_values) - 1)] for k in range(i, j + 1)]
        mean = sum(vals) / len(vals)
        for k in range(i, j + 1):
            out[cids[k]] = mean
        i = j + 1
    return out


def apply_dynasty(values: Mapping[str, "PlayerValue"], ctx: LeagueContext,
                  ages: Mapping[str, float] | None = None, mode: str | None = None) -> dict[str, DynastyValue]:
    """Dynasty value for every PlayerValue. `ages` ({cid: age}) fills in missing birth dates
    (e.g. FantraxProvider.ages); `mode` overrides ``ctx.dynasty_mode``."""
    years = max(1, int(ctx.keeper_horizon_years or 1))
    mode = mode or getattr(ctx, "dynasty_mode", None) or DEFAULT_MODE
    weights, terminal = mode_weights(mode, years)
    out: dict[str, DynastyValue] = {}
    for cid, pv in values.items():
        p = pv.player
        group = position_group(p)
        fpg = float(pv.fpg_season)
        age = age_on(p.birth_date, ctx.as_of)
        age_src = "birth date"
        if age is None and ages and cid in ages:
            age, age_src = float(ages[cid]), "league-reported age"
        reasons: list[Reason] = []
        if age is None:
            level = 1.0
            reasons.append(Reason(code="AGE_UNKNOWN", text="Age unknown: flat production trajectory", value=1.0))
        else:
            level = production_curve(group, age)
            g_end = growth_ratio(group, age, years)
            reasons.append(Reason(
                code="AGE_CURVE",
                text=f"Age {age:.1f} ({group}, {age_src}): at {level:.0%} of peak production, "
                     f"x{g_end:.2f} of today's rate by year {years}",
                value=level, baseline=1.0))
        ped, ped_text = pedigree_multiplier(p, age)
        if ped_text:
            reasons.append(Reason(code="PEDIGREE", text=f"Pedigree: {ped_text}: future years x{ped:.2f}",
                                  value=ped, baseline=1.0))
        healthy = _healthy_fpg(pv, fpg)
        base, flat, term = _trajectory(fpg, group, age, years, healthy, mode, ped)
        rk_conf = getattr(getattr(pv, "rookie", None), "confidence", None)
        if rk_conf is not None and abs(base - flat) > 1e-9:
            f = ROOKIE_UPSIDE_FLOOR + (1.0 - ROOKIE_UPSIDE_FLOOR) * max(0.0, min(1.0, float(rk_conf)))
            scaled = flat + (base - flat) * f
            reasons.append(Reason(code="ROOKIE_CONFIDENCE",
                                  text=f"Rookie model confidence {float(rk_conf):.0%}: upside {base - flat:+.2f} "
                                       f"x{f:.2f} -> {scaled - flat:+.2f}", value=f, baseline=1.0))
            base = scaled
        later = f", {healthy:.2f} healthy in later years" if years > 1 and abs(healthy - fpg) > 1e-9 else ""
        wtxt = "/".join(f"{x:g}" for x in weights)
        reasons.append(Reason(
            code="DYNASTY_HORIZON",
            text=f"{years}-year {mode} value {base:.2f} (weights {wtxt} + {term:.2f} beyond year {years}) "
                 f"from {fpg:.2f} FPG{later}",
            value=base, baseline=fpg))
        out[cid] = DynastyValue(player=p, value=base, age=age, age_mult=level, upside=base - flat,
                                horizon_years=years, model_value=base, pedigree=ped, mode=mode,
                                reasons=reasons)
    _apply_market(out, ctx)
    return out


def _apply_market(out: dict[str, DynastyValue], ctx: LeagueContext) -> None:
    owned = {p.cid: float(p.pct_owned) for p in ctx.all_players()
             if p.pct_owned is not None and p.cid in out}
    if len(owned) < MARKET_MIN_PLAYERS:
        return
    model = {cid: dv.value for cid, dv in out.items()}
    market = market_values(model, owned)
    for cid, mv in market.items():
        dv = out[cid]
        blended = (1.0 - MARKET_WEIGHT) * dv.value + MARKET_WEIGHT * mv
        dv.reasons.append(Reason(
            code="MARKET_PRIOR",
            text=f"Rostered in {owned[cid]:.0f}% of leagues -> market value {mv:.2f}; "
                 f"{1 - MARKET_WEIGHT:.0%} model {dv.value:.2f} + {MARKET_WEIGHT:.0%} market = {blended:.2f}",
            value=mv, baseline=owned[cid]))
        dv.market_value = mv
        dv.value = blended


def dynasty_rank(values: Mapping[str, DynastyValue] | Iterable[DynastyValue]) -> list[DynastyValue]:
    items = list(values.values()) if isinstance(values, Mapping) else list(values)
    return sorted(items, key=lambda d: d.value, reverse=True)
