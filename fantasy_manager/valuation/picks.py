"""Draft picks as dynasty assets (Fantrax dynasty leagues).

Every pick a team holds (``LeagueContext.draft_picks``) gets a dynasty value in the same units as
``valuation.dynasty`` player values, so picks can be compared with players and traded.

1. **Expected slot** (1 = first pick of a round; the draft order is assumed to be reverse
   standings in every round, not a lottery or snake). Next draft (``years_ahead`` 0): teams are
   ranked by a blend of this season's starting-lineup strength (sum of the best
   ``starting slots`` season FPG on the roster) and win % so far, the record weighted by the share
   of the season played (at most RECORD_WEIGHT_MAX). Later drafts: ranked by dynasty strength
   (sum of the best ``starting slots`` dynasty values), pulled toward the middle slot by
   FUTURE_SLOT_SHRINK per year ahead (nobody knows who is bad in two years).
2. **Slot uncertainty**: the value averages over slots within ``spread`` of the expected one
   (triangular weights), spread = SPREAD_NOW + SPREAD_PER_YEAR x years ahead + SPREAD_EARLY x
   (share of the season still to play) for the next draft.
3. **Slot to value curve**, anchored on the league itself: overall pick k is worth
   DRAFT_CLASS_DISCOUNT x the dynasty value of the k-th best unproven player in the league today
   (rostered or free agent, ``valuation.rookie.is_unproven``), i.e. "pick 5 is worth about the
   5th-best prospect, less a discount because a new draftee is further from the NHL". Past the
   last anchor the curve stays at the last anchor's value.
4. **Future years** are discounted by ``dynasty.DISCOUNT`` (0.8) per year ahead.
5. **Net value**: the draftee will take a roster spot, worth ``roster_spot_value`` (the league's
   lowest replacement FPG over the dynasty horizon: what the trade engine charges per player
   added). ``net`` = value - that (same discount), never below 0. In leagues with short benches
   and good prospects on waivers most picks net little: they are cheap to give and, since
   managers price them on absolute value, useful as trade sweeteners.

Keeper leagues (``ctx.keepers_per_team`` set; see ``valuation.dynasty`` "Keeper leagues"): the
next draft is a redraft of everyone not kept, so steps 3 and 5 change (1, 2 and 4 do not).
The pool is every valued player, rostered or free agent, except the predicted keepers (rostered
players with ``keeper`` True; a free agent is always in the pool), ranked by next-season value
``nv = healthy FPG x trajectory_factor(group, age, 1, pedigree)`` (``dynasty._healthy_fpg``, the
DynastyValue's age and pedigree). Overall pick k is worth ``w_1 x nv`` of the k-th best pool
player (w_1 = the mode's year-1 weight, ``dynasty.DISCOUNT`` with a 1-year horizon); no
DRAFT_CLASS_DISCOUNT, since these are established players, not prospects years from the NHL.
``net`` = value - ``repl x w_1`` (same per-year discount), repl = the league's lowest replacement
FPG: the draftee takes next season's roster spot a waiver-level player would otherwise fill.
When ``dynasty_values`` carries no keeper flags (e.g. plain floats), ``apply_dynasty`` is rerun
for them.

Approximate by design: the curve and constants are documented here so they can be refit.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Mapping

from ..models import FantasyTeam, LeagueContext
from .dynasty import (DISCOUNT, _healthy_fpg, age_on, keeper_count, min_replacement_fpg, mode_weights,
                      position_group, trajectory_factor)

DRAFT_CLASS_DISCOUNT = 0.75     # a new draftee vs today's comparable-rank unproven player
FUTURE_SLOT_SHRINK = 0.5        # expected slot pulled halfway to the middle per year ahead
RECORD_WEIGHT_MAX = 0.8         # win % weight in next draft's order at season end
SEASON_DAYS = 186               # NHL regular season length, days
SPREAD_NOW = 1.0
SPREAD_PER_YEAR = 2.0
SPREAD_EARLY = 3.0


@dataclass
class PickAsset:
    """One draft pick and its value."""
    year: int
    round: int
    holder: str                  # team id that holds the pick now
    original: str                # team id whose draft slot it is
    original_name: str
    years_ahead: int
    expected_slot: float         # within the round, 1 = first
    overall: float               # expected overall pick number
    value: float                 # dynasty value units (the expected draftee, comparable to players)
    inferred: bool = False       # holder inferred (the provider did not report this team's picks)
    net: float = 0.0             # value over the roster spot the draftee will take (>= 0)

    @property
    def key(self) -> str:
        return f"pick:{self.year}:{self.round}:{self.original}"

    @property
    def label(self) -> str:
        return f"{self.year} R{self.round} ({self.original_name})"

    def short(self, holder_id: str | None = None) -> str:
        own = holder_id is not None and self.original == holder_id
        return f"{self.year} R{self.round}" + ("" if own else f" (from {self.original_name})")


def _dyn(x: Any) -> float | None:
    if isinstance(x, (int, float)):
        return float(x)
    v = getattr(x, "value", None)
    return float(v) if v is not None else None


def _starting_count(ctx: LeagueContext) -> int:
    n = sum(int(v) for k, v in ctx.roster_shape.items() if k not in ("BN", "IR", "IR+", "NA", "MIN"))
    return max(1, n)


def _elapsed(ctx: LeagueContext) -> float:
    if ctx.season_start is None:
        return 0.0
    return min(1.0, max(0.0, (ctx.as_of - ctx.season_start).days / SEASON_DAYS))


def _zscores(xs: Mapping[str, float]) -> dict[str, float]:
    if not xs:
        return {}
    m = sum(xs.values()) / len(xs)
    sd = (sum((v - m) ** 2 for v in xs.values()) / len(xs)) ** 0.5 or 1.0
    return {k: (v - m) / sd for k, v in xs.items()}


def _top_sum(team: FantasyTeam, score: Mapping[str, float], k: int) -> float:
    return sum(sorted((score.get(p.cid, 0.0) for p in team.players), reverse=True)[:k])


def _slots_by_strength(strength: Mapping[str, float]) -> dict[str, float]:
    """team id -> slot (1 = weakest team picks first)."""
    order = sorted(strength, key=lambda t: strength[t])
    return {tid: float(i + 1) for i, tid in enumerate(order)}


def expected_slots(ctx: LeagueContext, values: Mapping[str, Any], dyn: Mapping[str, float],
                   years_ahead: int) -> dict[str, float]:
    """team id -> expected slot in a draft ``years_ahead`` years after the next one."""
    k = _starting_count(ctx)
    n = len(ctx.teams)
    mid = (n + 1) / 2
    if years_ahead == 0:
        fpg = {cid: float(getattr(pv, "fpg_season", 0.0) or 0.0) for cid, pv in values.items()}
        lineup = _zscores({t.team_id: _top_sum(t, fpg, k) for t in ctx.teams})
        rec = {}
        for t in ctx.teams:
            if t.record and sum(t.record) > 0:
                w, l, ti = t.record
                rec[t.team_id] = (w + 0.5 * ti) / (w + l + ti)
        w_rec = RECORD_WEIGHT_MAX * _elapsed(ctx) if len(rec) == n else 0.0
        zr = _zscores(rec)
        strength = {tid: (1 - w_rec) * z + w_rec * zr.get(tid, 0.0) for tid, z in lineup.items()}
        return _slots_by_strength(strength)
    strength = {t.team_id: _top_sum(t, dyn, k) for t in ctx.teams}
    slots = _slots_by_strength(strength)
    shrink = FUTURE_SLOT_SHRINK ** years_ahead
    return {tid: mid + (s - mid) * shrink for tid, s in slots.items()}


def slot_curve(ctx: LeagueContext, dyn: Mapping[str, float]) -> list[float]:
    """Dynasty value by overall pick (index 0 = pick 1): DRAFT_CLASS_DISCOUNT x the league's
    unproven players' dynasty values, best first."""
    from .rookie import is_unproven

    seen: set[str] = set()
    anchors = []
    for p in ctx.all_players():
        if p.cid in seen or p.cid not in dyn:
            continue
        seen.add(p.cid)
        try:
            if is_unproven(p):
                anchors.append(dyn[p.cid])
        except Exception:  # noqa: BLE001 - a player the rookie test can't judge is not an anchor
            continue
    anchors.sort(reverse=True)
    return [DRAFT_CLASS_DISCOUNT * v for v in anchors]


def roster_spot_value(ctx: LeagueContext, values: Mapping[str, Any]) -> float:
    """Dynasty value of one roster spot: the league's lowest replacement FPG held over the
    dynasty horizon (the mode's year weights plus its terminal weight), as the trade engine
    charges per player added (``recommend.trades.trade_values``)."""
    from .dynasty import mode_weights
    from .valuate import replacement_for

    try:
        levels = replacement_for(ctx, values, "season")  # type: ignore[arg-type]
    except Exception:  # noqa: BLE001 - no replacement level: a spot costs nothing
        return 0.0
    if not levels:
        return 0.0
    horizon = max(1, int(ctx.keeper_horizon_years or 1))
    w, terminal = mode_weights(getattr(ctx, "dynasty_mode", None), horizon)
    return min(levels.values()) * (sum(w) + DISCOUNT ** horizon * terminal)


# -- keeper leagues (module docstring "Keeper leagues") -------------------------------------

def next_season_weight(ctx: LeagueContext) -> float:
    """w_1, the mode's weight on next season (``DISCOUNT`` when the horizon is one year)."""
    years = max(1, int(ctx.keeper_horizon_years or 1))
    w, _ = mode_weights(getattr(ctx, "dynasty_mode", None), years)
    return w[1] if len(w) > 1 else DISCOUNT


def next_season_fpg(ctx: LeagueContext, pv: Any, dv: Any = None) -> float:
    """Next season's per-game value: healthy FPG x trajectory_factor(group, age, 1, pedigree),
    age and pedigree from the DynastyValue ``dv`` (birth date and 1.0 without one)."""
    p = pv.player
    healthy = _healthy_fpg(pv, float(getattr(pv, "fpg_season", 0.0) or 0.0))
    if dv is not None and hasattr(dv, "pedigree"):
        age, ped = getattr(dv, "age", None), float(getattr(dv, "pedigree", 1.0) or 1.0)
    else:
        age, ped = age_on(p.birth_date, ctx.as_of), 1.0
    return healthy * trajectory_factor(position_group(p), age, 1, ped)


def keeper_slot_curve(ctx: LeagueContext, values: Mapping[str, Any], dynasty_values: Mapping[str, Any]
                      ) -> list[float]:
    """Keeper leagues: value by overall pick (index 0 = pick 1) of next season's redraft, w_1 x
    next-season value of the pool (every valued player except rostered predicted keepers), best
    first."""
    rostered = {p.cid for t in ctx.teams for p in t.players}
    w1 = next_season_weight(ctx)
    pool = []
    for cid, pv in values.items():
        dv = dynasty_values.get(cid)
        if cid in rostered and dv is not None and getattr(dv, "keeper", False):
            continue
        pool.append(next_season_fpg(ctx, pv, dv))
    pool.sort(reverse=True)
    return [w1 * v for v in pool]


def keeper_spot_value(ctx: LeagueContext, values: Mapping[str, Any]) -> float:
    """Keeper leagues: next season's value of the roster spot a draftee takes, repl x w_1."""
    return min_replacement_fpg(ctx, values) * next_season_weight(ctx)


def _has_keeper_flags(dynasty_values: Mapping[str, Any] | None) -> bool:
    return bool(dynasty_values) and all(hasattr(dv, "keeper") for dv in dynasty_values.values())


def curve_at(curve: list[float], overall: int) -> float:
    if not curve:
        return 0.0
    return curve[min(max(overall, 1), len(curve)) - 1]


def pick_value(curve: list[float], n_teams: int, rnd: int, slot: float, years_ahead: int,
               elapsed: float = 1.0) -> float:
    """Expected dynasty value of round ``rnd`` at expected ``slot`` (module docstring 2-4)."""
    spread = SPREAD_NOW + SPREAD_PER_YEAR * years_ahead + (SPREAD_EARLY * (1.0 - elapsed) if years_ahead == 0 else 0.0)
    num = den = 0.0
    for t in range(1, n_teams + 1):
        w = max(0.0, spread + 1.0 - abs(t - slot))
        if w > 0:
            num += w * curve_at(curve, (rnd - 1) * n_teams + t)
            den += w
    v = num / den if den else curve_at(curve, (rnd - 1) * n_teams + round(slot))
    return v * DISCOUNT ** years_ahead


def holdings(ctx: LeagueContext) -> list[tuple[str, int, int, str, bool]]:
    """(holder, year, round, original team, inferred) for every known pick. Teams the provider
    did not report hold their own picks (rounds and years as in the reported teams) except those
    another team is known to hold; a reported team's own pick it no longer lists is not guessed."""
    known = ctx.draft_picks or {}
    if not known:
        return []
    out: list[tuple[str, int, int, str, bool]] = []
    taken: set[tuple[int, int, str]] = set()
    years: set[int] = set()
    rounds: set[int] = set()
    for holder, by_year in known.items():
        for year, picks in by_year.items():
            years.add(int(year))
            for pk in picks:
                try:
                    rnd = int(pk.get("round"))
                except (TypeError, ValueError):
                    continue
                rounds.add(rnd)
                orig = str(pk.get("original_owner") or holder)
                taken.add((int(year), rnd, orig))
                out.append((holder, int(year), rnd, orig, False))
    for t in ctx.teams:
        if t.team_id in known:
            continue
        for year in sorted(years):
            for rnd in sorted(rounds):
                if (year, rnd, t.team_id) not in taken:
                    out.append((t.team_id, year, rnd, t.team_id, True))
    return out


def value_picks(ctx: LeagueContext, values: Mapping[str, Any],
                dynasty_values: Mapping[str, Any] | None = None) -> list[PickAsset]:
    """Every known draft pick with its value, best first ([] outside dynasty leagues or without
    pick data). Keeper leagues value the redraft of everyone not kept (module docstring)."""
    if not ctx.dynasty or not ctx.draft_picks:
        return []
    from .dynasty import apply_dynasty
    if dynasty_values is None:
        dynasty_values = apply_dynasty(values, ctx)  # type: ignore[arg-type]
    dyn = {cid: v for cid, dv in (dynasty_values or {}).items() if (v := _dyn(dv)) is not None}
    rows = holdings(ctx)
    if not rows:
        return []
    names = {t.team_id: t.name for t in ctx.teams}
    n = max(1, len(ctx.teams))
    first = min(y for _, y, _, _, _ in rows)
    if keeper_count(ctx) is None:
        curve = slot_curve(ctx, dyn)
        spot = roster_spot_value(ctx, values)
    else:
        full = dynasty_values if _has_keeper_flags(dynasty_values) else apply_dynasty(values, ctx)  # type: ignore[arg-type]
        curve = keeper_slot_curve(ctx, values, full or {})
        spot = keeper_spot_value(ctx, values)
    elapsed = _elapsed(ctx)
    slots: dict[int, dict[str, float]] = {}
    out = []
    for holder, year, rnd, orig, inferred in rows:
        ahead = year - first
        if ahead not in slots:
            slots[ahead] = expected_slots(ctx, values, dyn, ahead)
        slot = slots[ahead].get(orig, (n + 1) / 2)
        value = pick_value(curve, n, rnd, slot, ahead, elapsed)
        out.append(PickAsset(year=year, round=rnd, holder=holder, original=orig,
                             original_name=names.get(orig, orig), years_ahead=ahead, expected_slot=slot,
                             overall=(rnd - 1) * n + slot, value=value, inferred=inferred,
                             net=max(0.0, value - spot * DISCOUNT ** ahead)))
    out.sort(key=lambda a: (-a.value, a.year, a.round))
    return out


def picks_by_holder(picks: list[PickAsset]) -> dict[str, list[PickAsset]]:
    out: dict[str, list[PickAsset]] = {}
    for a in picks:
        out.setdefault(a.holder, []).append(a)
    return out


__all__ = ["PickAsset", "value_picks", "picks_by_holder", "expected_slots", "slot_curve", "pick_value",
           "holdings", "keeper_slot_curve", "keeper_spot_value", "next_season_fpg", "next_season_weight",
           "DRAFT_CLASS_DISCOUNT", "FUTURE_SLOT_SHRINK"]
