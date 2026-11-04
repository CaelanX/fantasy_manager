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

Keeper leagues (``ctx.keepers_per_team`` = K; None = a keep-everyone dynasty, valued exactly as
above): each team keeps K players into next season for free (no draft-pick cost, no limit on years
kept) and everyone else goes back into a redraft. Only a keeper's later seasons stay with his
team, so the value splits in two:

* ``season_part = w_0 * fpg_season``: this season, worth the same to whoever rosters him now.
* ``future``: the trajectory total minus the w_0 term (years 1..H-1 plus the terminal term), with
  the rookie-confidence scaling above. The market prior applies to ``future`` only: the
  rank-to-value transform runs over the future values (``market_value`` is then the market's
  future value, and ``future = 0.65 * model future + 0.35 * market_value``).
* ``spot_future = repl * (sum_{y>=1} w_y + 0.8^H * T)``, repl = the league's lowest replacement
  FPG (``valuate.replacement_for``, min over slots; 0 when unavailable). Keepers are free, so a
  keeper slot left unused is filled by a waiver-level player next season;
  ``keeper_surplus = max(0, future - spot_future)``.
* ``keeper``: on each fantasy team, the top K players by keeper_surplus (every slot, IR
  included) whose surplus is positive. A free agent (or any valued player on no team) is a keeper
  iff his surplus is positive and beats the K-th largest surplus on my team (any positive surplus
  when I have fewer than K): free agents are judged from my side because pickups are my moves.
  K = 0: nobody is kept.
* ``value = season_part + (keeper_surplus if keeper else 0)``. ``model_value`` is the same with
  the pre-market future (the keeper flag as above). ``upside`` stays the growth / pedigree premium
  inside ``future`` (it reaches ``value`` only through a keeper's surplus).

Reasons KEEPER ("Keeper #2 of 3 on <team>: ...") and NOT_KEEPER ("... valued on this season
only") say which; ``keeper_total`` sums a roster's K largest surpluses, the long-term value it
carries. Keep-everyone leagues fill the same fields so callers can use them unconditionally:
``season_part = w_0 * fpg_season``, ``future = value - season_part`` (after the market blend),
``keeper_surplus = future`` and ``keeper = True``.
"""
from __future__ import annotations

from datetime import date
from typing import TYPE_CHECKING, Iterable, Mapping

from pydantic import BaseModel, Field

from ..models import FantasyTeam, LeagueContext, Player, Reason
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
    market_value: float | None = None  # keeper leagues: the market's view of `future` only
    pedigree: float = 1.0
    mode: str = DEFAULT_MODE
    # Keeper leagues (ctx.keepers_per_team set; see "Keeper leagues" in the module docstring).
    # In keep-everyone dynasties: season_part + future == value, keeper_surplus == future and
    # keeper is True, so callers can use these fields unconditionally.
    # Keeper leagues: value == season_part + (keeper_surplus if keeper else 0).
    season_part: float = 0.0          # this season's term (w_0 x fpg_season)
    future: float = 0.0               # seasons after this one (years >= 1 + terminal), market-blended
                                      # (keeper leagues: may be below the spot, never floored)
    keeper_surplus: float = 0.0       # max(0, future - a waiver-level roster spot's future value)
    keeper: bool = True               # among his holder's top keepers_per_team by keeper_surplus
                                      # (free agents: would enter MY top keepers)
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


def keeper_count(ctx: LeagueContext) -> int | None:
    """``ctx.keepers_per_team`` as an int >= 0, or None for a keep-everyone dynasty."""
    k = getattr(ctx, "keepers_per_team", None)
    if k is None:
        return None
    try:
        return max(0, int(k))
    except (TypeError, ValueError):
        return None


def min_replacement_fpg(ctx: LeagueContext, values: Mapping[str, "PlayerValue"]) -> float:
    """The league's lowest replacement FPG this season (min over slots of
    ``valuate.replacement_for``); 0.0 when it cannot be computed."""
    from .valuate import replacement_for  # local: valuate is the heavier module, avoid a cycle

    try:
        levels = replacement_for(ctx, values, "season")
    except Exception:  # noqa: BLE001 - no replacement level: a spot is worth nothing
        return 0.0
    return float(min(levels.values())) if levels else 0.0


def spot_future(ctx: LeagueContext, values: Mapping[str, "PlayerValue"], mode: str | None = None) -> float:
    """Future value of a waiver-level roster spot: ``min_replacement_fpg`` held flat over the
    seasons after this one (``sum_{y>=1} w_y + 0.8^H * T``). A keeper must beat it, since an
    unused keeper slot is filled by a waiver-level player next season."""
    years = max(1, int(ctx.keeper_horizon_years or 1))
    w, terminal = mode_weights(mode or getattr(ctx, "dynasty_mode", None), years)
    return min_replacement_fpg(ctx, values) * (sum(w[1:]) + DISCOUNT ** years * terminal)


def apply_dynasty(values: Mapping[str, "PlayerValue"], ctx: LeagueContext,
                  ages: Mapping[str, float] | None = None, mode: str | None = None) -> dict[str, DynastyValue]:
    """Dynasty value for every PlayerValue. `ages` ({cid: age}) fills in missing birth dates
    (e.g. FantraxProvider.ages); `mode` overrides ``ctx.dynasty_mode``. Keeper leagues
    (``ctx.keepers_per_team`` set) value non-keepers on this season only (module docstring)."""
    years = max(1, int(ctx.keeper_horizon_years or 1))
    mode = mode or getattr(ctx, "dynasty_mode", None) or DEFAULT_MODE
    weights, terminal = mode_weights(mode, years)
    k = keeper_count(ctx)
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
        season = weights[0] * fpg
        if k is None:
            htext = (f"{years}-year {mode} value {base:.2f} (weights {wtxt} + {term:.2f} beyond year {years}) "
                     f"from {fpg:.2f} FPG{later}")
        else:
            htext = (f"{years}-year {mode} trajectory {base:.2f} = this season {season:.2f} + future "
                     f"{base - season:.2f} (weights {wtxt} + {term:.2f} beyond year {years}) from {fpg:.2f} "
                     f"FPG{later}; the future counts only for a keeper")
        reasons.append(Reason(code="DYNASTY_HORIZON", text=htext, value=base, baseline=fpg))
        out[cid] = DynastyValue(player=p, value=base, age=age, age_mult=level, upside=base - flat,
                                horizon_years=years, model_value=base, pedigree=ped, mode=mode,
                                season_part=season, future=base - season, keeper_surplus=base - season,
                                keeper=True, reasons=reasons)
    if k is None:
        _apply_market(out, ctx)
        for dv in out.values():
            dv.future = dv.value - dv.season_part
            dv.keeper_surplus = dv.future
        return out
    _apply_keepers(out, values, ctx, k, mode)
    return out


def _apply_market(out: dict[str, DynastyValue], ctx: LeagueContext, field: str = "value") -> None:
    """Blend ``field`` ("value", or "future" in keeper leagues) with its market counterpart: the
    rank-to-value transform of league-wide % rostered over that field's model values."""
    owned = {p.cid: float(p.pct_owned) for p in ctx.all_players()
             if p.pct_owned is not None and p.cid in out}
    if len(owned) < MARKET_MIN_PLAYERS:
        return
    model = {cid: getattr(dv, field) for cid, dv in out.items()}
    market = market_values(model, owned)
    what = "market value" if field == "value" else "market future value"
    mdl = "model" if field == "value" else "model future"
    for cid, mv in market.items():
        dv = out[cid]
        cur = getattr(dv, field)
        blended = (1.0 - MARKET_WEIGHT) * cur + MARKET_WEIGHT * mv
        dv.reasons.append(Reason(
            code="MARKET_PRIOR",
            text=f"Rostered in {owned[cid]:.0f}% of leagues -> {what} {mv:.2f}; "
                 f"{1 - MARKET_WEIGHT:.0%} {mdl} {cur:.2f} + {MARKET_WEIGHT:.0%} market = {blended:.2f}",
            value=mv, baseline=owned[cid]))
        dv.market_value = mv
        setattr(dv, field, blended)


def _ordinal(n: int) -> str:
    suffix = "th" if 10 <= n % 100 <= 20 else {1: "st", 2: "nd", 3: "rd"}.get(n % 10, "th")
    return f"{n}{suffix}"


def _apply_keepers(out: dict[str, DynastyValue], values: Mapping[str, "PlayerValue"], ctx: LeagueContext,
                   k: int, mode: str) -> None:
    """Keeper-league pass (module docstring "Keeper leagues"): market prior on the future part,
    surplus over a waiver-level spot, keeper flags per team (free agents against my team), and
    value = season_part + the keeper's surplus."""
    model_future = {cid: dv.future for cid, dv in out.items()}
    _apply_market(out, ctx, field="future")
    spot = spot_future(ctx, values, mode)
    for dv in out.values():
        dv.keeper_surplus = max(0.0, dv.future - spot)

    def surplus_text(dv: DynastyValue) -> str:
        return f"long-term surplus {dv.keeper_surplus:+.2f} over a waiver-level spot " \
               f"(future {dv.future:.2f} - spot {spot:.2f})"

    def no_surplus(dv: DynastyValue) -> Reason:
        return Reason(code="NOT_KEEPER",
                      text=f"No keeper value: future {dv.future:.2f} does not beat a waiver-level spot "
                           f"({spot:.2f}); valued on this season only",
                      value=dv.keeper_surplus, baseline=0.0)

    top = f"top {k} keeper{'' if k == 1 else 's'}"
    holder: dict[str, FantasyTeam] = {}
    by_team: dict[str, list[str]] = {}
    for t in ctx.teams:
        for p in t.players:
            if p.cid in out and p.cid not in holder:
                holder[p.cid] = t
                by_team.setdefault(t.team_id, []).append(p.cid)
    for t in ctx.teams:
        ranked = sorted(by_team.get(t.team_id, []), key=lambda c: -out[c].keeper_surplus)
        kth = out[ranked[k - 1]].keeper_surplus if 0 < k <= len(ranked) else 0.0
        for i, cid in enumerate(ranked):
            dv = out[cid]
            dv.keeper = i < k and dv.keeper_surplus > 0.0
            if dv.keeper:
                dv.reasons.append(Reason(code="KEEPER", text=f"Keeper #{i + 1} of {k} on {t.name}: {surplus_text(dv)}",
                                         value=dv.keeper_surplus, baseline=spot))
            elif k == 0:
                dv.reasons.append(Reason(code="NOT_KEEPER", text="No keepers in this league: valued on this season only",
                                         value=dv.keeper_surplus, baseline=0.0))
            elif dv.keeper_surplus <= 0.0:
                dv.reasons.append(no_surplus(dv))
            else:
                dv.reasons.append(Reason(
                    code="NOT_KEEPER",
                    text=f"Not among {t.name}'s {top} ({_ordinal(k)} keeper surplus {kth:.2f}): "
                         f"valued on this season only",
                    value=dv.keeper_surplus, baseline=kth))

    try:
        me: FantasyTeam | None = ctx.my_team
    except LookupError:
        me = None
    my_name = me.name if me is not None else "my team"
    mine = sorted((out[c].keeper_surplus for c in by_team.get(me.team_id, [])) if me is not None else [],
                  reverse=True)
    mine_pos = [s for s in mine if s > 0.0][:k] if k > 0 else []
    bar = mine_pos[k - 1] if k > 0 and len(mine_pos) >= k else 0.0
    for cid, dv in out.items():
        if cid in holder:
            continue
        s = dv.keeper_surplus
        dv.keeper = k > 0 and s > 0.0 and s > bar
        if dv.keeper:
            rank = 1 + sum(1 for x in mine_pos if x >= s)
            dv.reasons.append(Reason(code="KEEPER",
                                     text=f"Free agent: would be keeper #{rank} of {k} on {my_name}: {surplus_text(dv)}",
                                     value=s, baseline=spot))
        elif k == 0:
            dv.reasons.append(Reason(code="NOT_KEEPER", text="No keepers in this league: valued on this season only",
                                     value=s, baseline=0.0))
        elif s <= 0.0:
            dv.reasons.append(no_surplus(dv))
        else:
            dv.reasons.append(Reason(
                code="NOT_KEEPER",
                text=f"Free agent: would not crack {my_name}'s {top} ({_ordinal(k)} keeper surplus {bar:.2f}): "
                     f"valued on this season only",
                value=s, baseline=bar))

    for cid, dv in out.items():
        model_surplus = max(0.0, model_future[cid] - spot)
        dv.model_value = dv.season_part + (model_surplus if dv.keeper else 0.0)
        dv.value = dv.season_part + (dv.keeper_surplus if dv.keeper else 0.0)


def keeper_total(players: Iterable[Player], dyn: Mapping[str, "DynastyValue"], k: int | None) -> float:
    """Long-term value a roster carries past this season: the sum of the ``k`` largest
    ``keeper_surplus`` among ``players`` (the keepers the team would choose). ``k`` None (a
    keep-everyone dynasty): every player's surplus counts. Players without a DynastyValue count 0.
    Used for exact marginal keeper value: a trade's or pickup's long-term change for a team is
    keeper_total(after) - keeper_total(before) (pass ``keeper_count(ctx)`` as ``k``). Surpluses
    are >= 0 in keeper leagues, so adding a player never lowers it; in keep-everyone leagues
    surplus == future (not floored) and the sum runs over every player."""
    xs = sorted((dyn[p.cid].keeper_surplus for p in players if p.cid in dyn), reverse=True)
    return sum(xs if k is None else xs[:max(0, int(k))])


def dynasty_rank(values: Mapping[str, DynastyValue] | Iterable[DynastyValue]) -> list[DynastyValue]:
    items = list(values.values()) if isinstance(values, Mapping) else list(values)
    return sorted(items, key=lambda d: d.value, reverse=True)
