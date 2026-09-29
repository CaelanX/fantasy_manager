"""Trade proposals: lineup-solver gain for me, a fairness band on value, and plausibility
for the counterparty.

For each opponent we enumerate 1-for-1 and 2-for-1 (both directions) over each side's top-12
players by trade value V (season VORP, or dynasty value when supplied), then:

* fairness   equal counts: |sum V_in - sum V_out| <= 12% of the larger side.
             uneven counts: the fewer-player side must be within [0.85, 1.15] of the
             more-player side's sum minus the value of the roster spot(s) it frees. With
             VORP-based V a freed spot refilled from waivers is worth 0 by definition; with
             dynasty V it is the league's minimum replacement FPG times sum_{y<H} 0.8^y.
* ΔMe        optimal lineup value after - before (season FPG of starters), must be > 0.5.
             Uses recommend.lineup.optimal_lineup when importable, else the built-in
             value-greedy + augmenting-path solver (exact for this slot structure).
* ΔThem      same for the counterparty, must be >= -0.25 after a +1 bonus when an incoming
             player fills their weakest starting slot or a trade-block want.
* score      ΔMe + 0.5 * min(ΔThem_eff, 1)   (minus a roto balance term in roto leagues).

Dynasty leagues (V = dynasty value) also weigh the long-term value change ΔDyn = V_in - V_out
- V(my drops) + V(my FA fill), normalised to FPG-like units by the sum of the mode's year
weights plus its terminal weight (ΔDyn_n), and follow ``ctx.dynasty_mode``:

* contend    score = ΔMe + 0.5 * ΔDyn_n; ΔMe must be >= -WIN_NOW_TOLERANCE (0.3 FPG). A
             trade that costs more this season is not recommended; it goes to the
             ``future_only`` bucket with a WIN_NOW_COST reason.
* balanced   score = ΔMe + ΔDyn_n (ΔMe >= -1.0).
* rebuild    score = ΔDyn_n + 0.5 * ΔMe (no this-season floor).

and accept when score (before the ΔThem term) > MIN_GAIN_ME. Protected players (young
first-round picks aged <= 23, or <= 23 and widely rostered; see ``base.protection_reason``) are
dropped to make room only as a last resort and only
when the incoming value is >= 1.5x theirs; otherwise the trade is rejected.

Roster size is enforced: the side receiving more players first moves IR/LTIR-status players
into free IR slots, then drops its lowest-value droppable player(s) (lowest trade value V when
V is dynasty value, else lowest season FPG; named in the reasons); the side freeing a spot
picks up the best fitting FA.
"""
from __future__ import annotations

import itertools
import math
from dataclasses import dataclass, field
from functools import lru_cache
from typing import Any, Callable, Iterable, Mapping

from .strength import apply_ranks, apply_strength
from ..models import FantasyTeam, LeagueContext, Player, Reason, Recommendation, RosterSlot
from ..valuation.replacement import eligible_slots
from .base import PROTECT_OVERRIDE, has_data, player_age, protection_reason

try:  # written concurrently by another module owner; greedy fallback below
    from .lineup import optimal_lineup as _optimal_lineup  # type: ignore
except Exception:  # pragma: no cover - depends on sibling module availability
    _optimal_lineup = None

MIN_GAIN_ME = 0.5
FAIR_BAND = 0.12
UNEVEN_BAND = (0.85, 1.15)
MIN_DELTA_THEM = -0.25
NEED_BONUS = 1.0
THEM_WEIGHT = 0.5
TOP_N = 12
MAX_EVALS_PER_TEAM = 600
SCREEN_MARGIN = 0.1      # slack for the built-in solver's pre-screen before optimal_lineup runs
ROTO_PENALTY_WEIGHT = 2.0
WIN_NOW_TOLERANCE = 0.3
BALANCED_TOLERANCE = 1.0
DYN_WEIGHT = {"contend": 0.5, "balanced": 1.0, "rebuild": 1.0}
ME_WEIGHT = {"contend": 1.0, "balanced": 1.0, "rebuild": 0.5}
DYNASTY_DISCOUNT = 0.8
LINEUP_HORIZON = "season"
NON_STARTING = frozenset({"BN", "IR", "IR+", "NA"})
IR_SLOTS = frozenset({"IR", "IR+"})
IR_MOVE_STATUSES = ("ltir", "ir")     # most severe first
_SLOT_RANK = {"G": 0, "D": 0, "C": 0, "LW": 0, "RW": 0, "F": 1, "UTIL": 2}

LineupFn = Callable[..., Any]


# -- lineup evaluation --------------------------------------------------------------

@lru_cache(maxsize=4096)
def _eligible(positions: tuple[str, ...], slot: str) -> bool:
    return bool(eligible_slots(positions, [slot]))


def _starting_slots(roster_shape: Mapping[str, int]) -> list[str]:
    slots = [s for s, n in roster_shape.items() if s not in NON_STARTING for _ in range(int(n))]
    return sorted(slots, key=lambda s: _SLOT_RANK.get(s, 0))


def _greedy_assignment(players: Iterable[Player], values: Mapping[str, Any],
                       roster_shape: Mapping[str, int], horizon: str = "season"
                       ) -> dict[str, list[str]]:
    """Starting lineup as {slot: [cid, ...]}; BN/IR never start.

    Greedy by player value with an augmenting-path feasibility check: a player joins the
    lineup if the chosen set can still be matched to the starting slots (reshuffling earlier
    picks between slots as needed). Lineups are independent sets of a transversal matroid,
    so this greedy is exact for non-negative values - same optimum as a full DP, far cheaper.
    Players with value <= 0 are left on the bench.
    """
    def val(p: Player) -> float:
        pv = values.get(p.cid)
        return pv.fpg_for(horizon) if pv is not None else 0.0

    slots = _starting_slots(roster_shape)
    holder: list[Player | None] = [None] * len(slots)

    def augment(p: Player, seen: set[int]) -> bool:
        pos = tuple(p.positions)
        for i, s in enumerate(slots):
            if i in seen or not _eligible(pos, s):
                continue
            seen.add(i)
            cur = holder[i]
            if cur is None or augment(cur, seen):
                holder[i] = p
                return True
        return False

    filled = 0
    for p in sorted(players, key=val, reverse=True):
        if filled >= len(slots) or val(p) <= 0:
            break
        if augment(p, set()):
            filled += 1
    out: dict[str, list[str]] = {}
    for s, p in zip(slots, holder):
        if p is not None:
            out.setdefault(s, []).append(p.cid)
    return out


def _greedy_lineup_value(players: Iterable[Player], values: Mapping[str, Any],
                         roster_shape: Mapping[str, int], horizon: str = "season") -> float:
    """Total per-game value of the best starting lineup (fallback for optimal_lineup):
    C/LW/RW/D/G take those positions, F takes C/LW/RW/F, UTIL any skater, G goalies only."""
    assign = _greedy_assignment(players, values, roster_shape, horizon)
    return sum(values[c].fpg_for(horizon) for cids in assign.values() for c in cids if c in values)


class _LineupEvaluator:
    """Caches lineup values by roster; falls back to greedy for good if the solver errors."""

    def __init__(self, values: Mapping[str, Any], roster_shape: Mapping[str, int],
                 lineup_fn: LineupFn | None):
        self.values = values
        self.roster_shape = dict(roster_shape)
        self.fn = lineup_fn
        self.cache: dict[frozenset[str], float] = {}
        self.greedy_cache: dict[frozenset[str], float] = {}

    @property
    def exact(self) -> bool:
        return self.fn is not None

    def greedy(self, players: list[Player]) -> float:
        key = frozenset(p.cid for p in players)
        hit = self.greedy_cache.get(key)
        if hit is None:
            hit = _greedy_lineup_value(players, self.values, self.roster_shape, LINEUP_HORIZON)
            self.greedy_cache[key] = hit
        return hit

    @property
    def source(self) -> str:
        return "optimal_lineup" if self.fn is not None else "greedy"

    def __call__(self, players: list[Player]) -> float:
        key = frozenset(p.cid for p in players)
        hit = self.cache.get(key)
        if hit is not None:
            return hit
        v = None
        if self.fn is not None:
            try:
                team = FantasyTeam(team_id="_hyp", name="_hyp", owner_is_me=False,
                                   slots=[RosterSlot(slot="BN", player=p, starting=False) for p in players])
                res = self.fn(team, self.values, self.roster_shape, LINEUP_HORIZON)
                v = float(res[1] if isinstance(res, tuple) else res)
                if not math.isfinite(v):
                    raise ValueError("non-finite lineup value")
            except Exception:
                self.fn = None          # switch permanently so before/after stay comparable
                self.cache.clear()
                v = None
        if v is None:
            v = _greedy_lineup_value(players, self.values, self.roster_shape, LINEUP_HORIZON)
        self.cache[key] = v
        return v


# -- trade values -------------------------------------------------------------------

def _dyn_value(x: Any) -> float:
    if isinstance(x, (int, float)):
        return float(x)
    return float(getattr(x, "value"))


def trade_values(ctx: LeagueContext, values: Mapping[str, Any],
                 dynasty_values: Mapping[str, Any] | None = None) -> tuple[dict[str, float], str, float]:
    """(V by cid, units label, value of one freed roster spot in V units)."""
    if dynasty_values is None and ctx.dynasty:
        try:
            from ..valuation.dynasty import apply_dynasty  # type: ignore
            dynasty_values = apply_dynasty(values, ctx)
        except Exception:
            dynasty_values = None
    if dynasty_values:
        v = {cid: _dyn_value(dv) for cid, dv in dynasty_values.items()}
        for cid, pv in values.items():
            v.setdefault(cid, 0.0)
        repl = 0.0
        try:
            from ..valuation.valuate import replacement_for
            levels = replacement_for(ctx, values, "season")  # type: ignore[arg-type]
            if levels:
                repl = min(levels.values()) * dynasty_scale(ctx)
        except Exception:
            repl = 0.0
        return v, "dynasty", repl
    return {cid: float(pv.vorp) for cid, pv in values.items()}, "VORP", 0.0


def dynasty_scale(ctx: LeagueContext) -> float:
    """Dynasty value of 1 FPG held flat over the horizon (sum of the mode's year weights plus
    the terminal weight): converts dynasty-value deltas to FPG-like units."""
    horizon = max(1, int(ctx.keeper_horizon_years or 1))
    try:
        from ..valuation.dynasty import mode_weights
        w, terminal = mode_weights(getattr(ctx, "dynasty_mode", None), horizon)
        return sum(w) + DYNASTY_DISCOUNT ** horizon * terminal
    except Exception:
        return sum(DYNASTY_DISCOUNT ** y for y in range(horizon))


def fairness(v_in: list[float], v_out: list[float], repl: float = 0.0) -> tuple[bool, float]:
    """(passes, deviation). Equal counts: relative gap vs the larger side (band 12%).
    Uneven: fewer-side sum / (more-side sum - freed spots * repl) within [0.85, 1.15]."""
    s_in, s_out = sum(v_in), sum(v_out)
    if len(v_in) == len(v_out):
        denom = max(s_in, s_out)
        if denom <= 0:
            return False, math.inf
        dev = abs(s_in - s_out) / denom
        return dev <= FAIR_BAND + 1e-9, dev
    few, many = (s_in, s_out) if len(v_in) < len(v_out) else (s_out, s_in)
    target = many - abs(len(v_in) - len(v_out)) * repl
    if target <= 0 or few <= 0:
        return False, math.inf
    ratio = few / target
    return UNEVEN_BAND[0] - 1e-9 <= ratio <= UNEVEN_BAND[1] + 1e-9, abs(ratio - 1.0)


# -- per-team state -----------------------------------------------------------------

@dataclass
class _Side:
    team: FantasyTeam
    active: list[Player]
    ir: list[Player]
    capacity: int
    free_ir: int = 0
    weakest: tuple[str, float] | None = None
    top: list[Player] = field(default_factory=list)


@dataclass
class TradeEval:
    team: FantasyTeam
    give: list[Player]
    get: list[Player]
    v_in: float
    v_out: float
    fair: bool
    fair_dev: float
    delta_me: float
    delta_them: float
    need_bonus: float
    need_text: str | None
    my_drops: list[Player]
    their_drops: list[Player]
    my_fill: Player | None
    their_fill: Player | None
    roto_adj: float = 0.0
    my_ir: list[Player] = field(default_factory=list)
    their_ir: list[Player] = field(default_factory=list)
    mode: str | None = None               # dynasty mode (None: plain season VORP trade values)
    delta_dyn: float = 0.0                # dynasty value change for me (V units)
    delta_dyn_n: float = 0.0              # ... normalised to FPG-like units
    blocked: str | None = None            # protected-player drop that the trade would force

    @property
    def delta_them_eff(self) -> float:
        return self.delta_them + self.need_bonus

    @property
    def core(self) -> float:
        """My side of the score: ΔMe, or the mode's blend of ΔMe and ΔDyn in dynasty leagues."""
        if self.mode is None:
            return self.delta_me
        return ME_WEIGHT[self.mode] * self.delta_me + DYN_WEIGHT[self.mode] * self.delta_dyn_n

    @property
    def win_now_cost(self) -> bool:
        """Contend mode: the trade weakens this season's lineup beyond the tolerance."""
        return self.mode == "contend" and self.delta_me < -WIN_NOW_TOLERANCE

    @property
    def accepted_future(self) -> bool:
        """Would be accepted if this season's lineup did not matter (future-only bucket)."""
        return self.fair and self.blocked is None and self.win_now_cost             and self.delta_dyn_n > MIN_GAIN_ME and self.delta_them_eff >= MIN_DELTA_THEM

    @property
    def accepted(self) -> bool:
        if not self.fair or self.delta_them_eff < MIN_DELTA_THEM or self.blocked is not None:
            return False
        if self.mode is None:
            return self.delta_me > MIN_GAIN_ME
        if self.win_now_cost:
            return False
        if self.mode == "balanced" and self.delta_me < -BALANCED_TOLERANCE:
            return False
        return self.core > MIN_GAIN_ME

    @property
    def score(self) -> float:
        return self.core + THEM_WEIGHT * min(self.delta_them_eff, 1.0) - self.roto_adj


class _TradeEngine:
    def __init__(self, ctx: LeagueContext, values: Mapping[str, Any],
                 dynasty_values: Mapping[str, Any] | None = None,
                 wants: Mapping[str, set[str]] | None = None,
                 lineup_fn: LineupFn | None | bool = None):
        self.ctx = ctx
        self.values = values
        self.V, self.units, self.repl = trade_values(ctx, values, dynasty_values)
        self.mode = (getattr(ctx, "dynasty_mode", None) or "contend") \
            if ctx.dynasty and self.units == "dynasty" else None
        self.scale = max(1e-9, dynasty_scale(ctx))
        self.protected: dict[str, str] = {}
        if self.units == "dynasty":
            for t in ctx.teams:
                for p in t.players:
                    why = protection_reason(p, player_age(p, ctx, dynasty_values))
                    if why:
                        self.protected[p.cid] = why
        self.wants = {k: set(v) for k, v in (wants or {}).items()}
        fn = _optimal_lineup if lineup_fn is None else (lineup_fn or None)
        self.lineup = _LineupEvaluator(values, ctx.roster_shape, fn)  # type: ignore[arg-type]
        cap = sum(int(n) for s, n in ctx.roster_shape.items() if s not in IR_SLOTS)
        self.sides: dict[str, _Side] = {}
        for t in ctx.teams:
            active = [s.player for s in t.slots if s.player is not None and s.slot not in IR_SLOTS]
            ir = [s.player for s in t.slots if s.player is not None and s.slot in IR_SLOTS]
            ir_cap = sum(int(n) for s, n in ctx.roster_shape.items() if s in IR_SLOTS)
            side = _Side(team=t, active=active, ir=ir, capacity=max(cap, len(active)),
                         free_ir=max(0, ir_cap - len(ir)))
            side.weakest = self._weakest_slot(active)
            side.top = sorted((p for p in active + ir if self.V.get(p.cid, 0.0) > 0),
                              key=lambda p: self.V[p.cid], reverse=True)[:TOP_N]
            self.sides[t.team_id] = side
        self.fas = sorted((p for p in ctx.free_agents if p.cid in values
                           and p.status in ("healthy", "unknown")),
                          key=self._fpg, reverse=True)
        self.roto = None
        if ctx.scoring.kind == "roto" and ctx.scoring.categories:
            from ..scoring import RotoScoring
            self.roto = RotoScoring(ctx.scoring.categories, ctx.scoring.weights)
            self.totals = {tid: self._totals(s.active) for tid, s in self.sides.items()}

    def _fpg(self, p: Player) -> float:
        pv = self.values.get(p.cid)
        return pv.fpg_for(LINEUP_HORIZON) if pv is not None else 0.0

    def _totals(self, players: list[Player]) -> dict[str, float]:
        assert self.roto is not None
        return self.roto.team_totals(getattr(self.values.get(p.cid), "rates", {}) or {} for p in players)

    def _weakest_slot(self, active: list[Player]) -> tuple[str, float] | None:
        assign = _greedy_assignment(active, self.values, self.ctx.roster_shape, LINEUP_HORIZON)
        worst: tuple[str, float] | None = None
        for slot in dict.fromkeys(_starting_slots(self.ctx.roster_shape)):
            if slot == "UTIL":
                continue
            need = int(self.ctx.roster_shape.get(slot, 0))
            cids = assign.get(slot, [])
            v = -math.inf if len(cids) < need else min(
                (self.values[c].vorp for c in cids if c in self.values), default=-math.inf)
            if worst is None or v < worst[1]:
                worst = (slot, v)
        return worst

    def _drop_key(self, p: Player) -> tuple[float, float]:
        """Lowest dynasty value first in dynasty mode (then season FPG), else season FPG."""
        if self.units == "dynasty":
            return (self.V.get(p.cid, 0.0), self._fpg(p))
        return (self._fpg(p), self.V.get(p.cid, 0.0))

    def _settle_roster(self, side: _Side, players: list[Player], incoming: list[Player],
                       outgoing: list[Player]
                       ) -> tuple[list[Player], list[Player], Player | None, list[Player], str | None]:
        """(roster, drops, FA fill, IR moves, blocked): when over capacity, IR/LTIR-status
        players go to free IR slots first, then the lowest-value players are dropped; when a
        spot opens, the best fitting FA is picked up. In dynasty mode protected players are
        dropped only as a last resort; `blocked` explains a forced protected drop that the
        incoming value (< 1.5x) does not justify."""
        blocked: str | None = None
        drops: list[Player] = []
        to_ir: list[Player] = []
        fill: Player | None = None
        over = len(players) - side.capacity
        if over > 0:
            inc = {p.cid for p in incoming}
            if side.free_ir > 0:
                movable = [p for p in players if p.status in IR_MOVE_STATUSES]
                movable.sort(key=lambda p: (IR_MOVE_STATUSES.index(p.status), -self._fpg(p)))
                to_ir = movable[:min(over, side.free_ir)]
                over -= len(to_ir)
            moved = {p.cid for p in to_ir}
            cands = sorted((p for p in players if p.cid not in inc and p.cid not in moved),
                           key=self._drop_key)
            if self.units == "dynasty":
                # no stats at all = unknown value (prospects) and protected prospects: drop them
                # only as a last resort
                cands.sort(key=lambda p: (p.cid in self.protected, not has_data(self.values.get(p.cid))))
            drops = cands[:max(0, over)]
            v_in = sum(self.V.get(p.cid, 0.0) for p in incoming)
            for d in drops:
                why = self.protected.get(d.cid)
                if why and v_in < PROTECT_OVERRIDE * self.V.get(d.cid, 0.0):
                    blocked = f"would force dropping {d.name} ({why})"
                    break
            gone = moved | {p.cid for p in drops}
            players = [p for p in players if p.cid not in gone]
        elif len(players) < side.capacity and len(outgoing) > len(incoming):
            for fa in self.fas:
                if any(set(eligible_slots(fa.positions)) & set(eligible_slots(o.positions)) for o in outgoing):
                    fill = fa
                    players = players + [fa]
                    break
        return players, drops, fill, to_ir, blocked

    def evaluate(self, team_id: str, give: list[Player], get: list[Player],
                 screen: bool = True) -> TradeEval:
        me = self.sides[self.ctx.my_team.team_id]
        them = self.sides[team_id]
        give_ids, get_ids = {p.cid for p in give}, {p.cid for p in get}
        fair, dev = fairness([self.V.get(p.cid, 0.0) for p in get],
                             [self.V.get(p.cid, 0.0) for p in give], self.repl)

        me_after = [p for p in me.active if p.cid not in give_ids] + list(get)
        me_after, my_drops, my_fill, my_ir, my_block = self._settle_roster(me, me_after, get, give)
        them_after = [p for p in them.active if p.cid not in get_ids] + list(give)
        them_after, their_drops, their_fill, their_ir, their_block = self._settle_roster(
            them, them_after, give, get)
        blocked = my_block or (f"{them.team.name} {their_block}" if their_block else None)
        v_in = sum(self.V.get(p.cid, 0.0) for p in get)
        v_out = sum(self.V.get(p.cid, 0.0) for p in give)
        d_dyn = v_in - v_out - sum(self.V.get(p.cid, 0.0) for p in my_drops) \
            + (self.V.get(my_fill.cid, 0.0) if my_fill is not None else 0.0)
        d_dyn_n = d_dyn / self.scale if self.mode is not None else 0.0

        bonus, need_text = 0.0, None
        if them.weakest is not None:
            slot, v = them.weakest
            fits = [p for p in give if _eligible(tuple(p.positions), slot)]
            if fits:
                bonus = NEED_BONUS
                vtxt = "empty" if v == -math.inf else f"starter VORP {v:+.2f}"
                need_text = f"{fits[0].name} fills {them.team.name}'s weakest slot {slot} ({vtxt})"
        want = self.wants.get(team_id, set())
        if not bonus and want:
            fits = [p for p in give if set(p.positions) & want]
            if fits:
                bonus = NEED_BONUS
                need_text = (f"{fits[0].name} matches {them.team.name}'s trade-block wants "
                             f"({', '.join(sorted(want))})")

        # cheap greedy screen first; the exact solver only runs for plausible candidates
        d_me = self.lineup.greedy(me_after) - self.lineup.greedy(me.active)
        d_them = self.lineup.greedy(them_after) - self.lineup.greedy(them.active)
        if self.mode is None:
            mine_ok = d_me > MIN_GAIN_ME - SCREEN_MARGIN
        else:
            core = ME_WEIGHT[self.mode] * d_me + DYN_WEIGHT[self.mode] * d_dyn_n
            mine_ok = core > MIN_GAIN_ME - SCREEN_MARGIN or d_dyn_n > MIN_GAIN_ME - SCREEN_MARGIN
        if not screen or not self.lineup.exact or (
                mine_ok and blocked is None
                and d_them + bonus >= MIN_DELTA_THEM - SCREEN_MARGIN):
            d_me = self.lineup(me_after) - self.lineup(me.active)
            d_them = self.lineup(them_after) - self.lineup(them.active)

        roto_adj = 0.0
        if self.roto is not None:
            others = [t for tid, t in self.totals.items() if tid != me.team.team_id]
            before = self.roto.category_balance_penalty(self.totals[me.team.team_id], others)
            others_after = [self._totals(them_after) if tid == team_id else t
                            for tid, t in self.totals.items() if tid != me.team.team_id]
            after = self.roto.category_balance_penalty(self._totals(me_after), others_after)
            roto_adj = ROTO_PENALTY_WEIGHT * (after - before)

        return TradeEval(team=them.team, give=list(give), get=list(get), v_in=v_in, v_out=v_out,
                         fair=fair, fair_dev=dev, delta_me=d_me, delta_them=d_them,
                         need_bonus=bonus, need_text=need_text, my_drops=my_drops,
                         their_drops=their_drops, my_fill=my_fill, their_fill=their_fill,
                         roto_adj=roto_adj, my_ir=my_ir, their_ir=their_ir, mode=self.mode,
                         delta_dyn=d_dyn, delta_dyn_n=d_dyn_n, blocked=blocked)

    def candidates(self, team_id: str) -> list[tuple[list[Player], list[Player], float]]:
        mine = self.sides[self.ctx.my_team.team_id].top
        theirs = self.sides[team_id].top
        out: list[tuple[list[Player], list[Player], float]] = []
        shapes = [((a,), (b,)) for a in mine for b in theirs]
        shapes += [(pair, (b,)) for pair in itertools.combinations(mine, 2) for b in theirs]
        shapes += [((a,), pair) for a in mine for pair in itertools.combinations(theirs, 2)]
        for give, get in shapes:
            ok, dev = fairness([self.V[p.cid] for p in get], [self.V[p.cid] for p in give], self.repl)
            if ok:
                out.append((list(give), list(get), dev))
        # 1-for-1 first, then the most balanced; cap the expensive lineup evaluations
        out.sort(key=lambda c: (len(c[0]) + len(c[1]), c[2]))
        return out[:MAX_EVALS_PER_TEAM]


def _names(ps: Iterable[Player]) -> str:
    return " + ".join(p.name for p in ps)


def _to_rec(ev: TradeEval, V: Mapping[str, float], units: str, source: str) -> Recommendation:
    def vl(ps: list[Player]) -> str:
        return ", ".join(f"{p.name} ({V.get(p.cid, 0.0):.2f})" for p in ps)

    band = FAIR_BAND if len(ev.give) == len(ev.get) else UNEVEN_BAND[1] - 1.0
    reasons = [
        Reason(code="VALUE_IN", text=f"Receive {vl(ev.get)}: {ev.v_in:.2f} {units}", value=ev.v_in),
        Reason(code="VALUE_OUT", text=f"Send {vl(ev.give)}: {ev.v_out:.2f} {units}", value=ev.v_out),
        Reason(code="FAIR_PCT", text=f"Value gap {ev.fair_dev:.0%} (band {band:.0%})",
               value=ev.fair_dev, baseline=band),
        Reason(code="DELTA_ME", text=f"My lineup {ev.delta_me:+.2f} FPG ({source})",
               value=ev.delta_me, baseline=MIN_GAIN_ME),
        Reason(code="DELTA_THEM", text=f"{ev.team.name} lineup {ev.delta_them:+.2f} FPG",
               value=ev.delta_them, baseline=MIN_DELTA_THEM),
    ]
    if ev.mode is not None:
        reasons.append(Reason(code="DYNASTY_DELTA",
                              text=f"Dynasty value {ev.delta_dyn:+.2f} for me ({ev.delta_dyn_n:+.2f} FPG-equivalent); "
                                   f"{ev.mode} score = {ME_WEIGHT[ev.mode]:g} x lineup + "
                                   f"{DYN_WEIGHT[ev.mode]:g} x dynasty",
                              value=ev.delta_dyn_n, baseline=ev.delta_dyn))
    if ev.win_now_cost:
        reasons.append(Reason(code="WIN_NOW_COST",
                              text=f"Costs {-ev.delta_me:.2f} FPG of this season's lineup (contend mode allows "
                                   f"{WIN_NOW_TOLERANCE:g}): future-only",
                              value=ev.delta_me, baseline=-WIN_NOW_TOLERANCE))
    if ev.need_text:
        reasons.append(Reason(code="THEIR_NEED", text=ev.need_text, value=ev.need_bonus))
    if ev.my_ir:
        reasons.append(Reason(code="IR_MOVE", text=f"You would move {_names(ev.my_ir)} to IR (no drop)"))
    if ev.their_ir:
        reasons.append(Reason(code="IR_MOVE",
                              text=f"{ev.team.name} would move {_names(ev.their_ir)} to IR (no drop)"))
    if ev.my_drops:
        reasons.append(Reason(code="ROSTER_DROP", text=f"You would need to drop {_names(ev.my_drops)}"))
    if ev.their_drops:
        reasons.append(Reason(code="ROSTER_DROP",
                              text=f"{ev.team.name} would need to drop {_names(ev.their_drops)}"))
    if ev.my_fill:
        reasons.append(Reason(code="FA_FILL", text=f"Open roster spot: pick up {ev.my_fill.name}"))
    if ev.roto_adj:
        reasons.append(Reason(code="ROTO_BALANCE", text=f"Roto category balance {-ev.roto_adj:+.2f}",
                              value=-ev.roto_adj))
    return Recommendation(kind="trade", score=ev.score,
                          title=f"Trade {_names(ev.give)} to {ev.team.name} for {_names(ev.get)}",
                          add=list(ev.get), drop=list(ev.give), counterparty=ev.team.name,
                          reasons=reasons, predicted_gain=float(ev.delta_me), gain_units="lineup_fpg",
                          horizon_days=None)


def evaluate_trade(ctx: LeagueContext, values: Mapping[str, Any], give: list[str], get: list[str],
                   team_id: str | None = None, dynasty_values: Mapping[str, Any] | None = None,
                   wants: Mapping[str, set[str]] | None = None,
                   lineup_fn: LineupFn | None | bool = None) -> TradeEval:
    """Evaluate an arbitrary n-for-m proposal (cids). team_id defaults to the owner of `get`."""
    eng = _TradeEngine(ctx, values, dynasty_values, wants, lineup_fn)
    by_cid = {p.cid: p for t in ctx.teams for p in t.players}
    if team_id is None:
        owner = [t.team_id for t in ctx.teams if not t.owner_is_me
                 and {p.cid for p in t.players} >= set(get)]
        if not owner:
            raise LookupError("could not find a single team owning all requested players")
        team_id = owner[0]
    return eng.evaluate(team_id, [by_cid[c] for c in give], [by_cid[c] for c in get], screen=False)


def recommend_trades(ctx: LeagueContext, values: Mapping[str, Any],
                     dynasty_values: Mapping[str, Any] | None = None, max_per_team: int = 3,
                     limit: int = 10, wants: Mapping[str, set[str]] | None = None,
                     lineup_fn: LineupFn | None | bool = None,
                     future_only: list[Recommendation] | None = None) -> list[Recommendation]:
    """Ranked trade proposals. `wants` = trade-block positions by team_id (Fantrax).
    `lineup_fn`: None = optimal_lineup if importable, False = force the greedy solver.
    `future_only` (a list, contend mode) collects trades that gain dynasty value but cost
    this season's lineup more than WIN_NOW_TOLERANCE (reason WIN_NOW_COST), best first."""
    eng = _TradeEngine(ctx, values, dynasty_values, wants, lineup_fn)
    me = ctx.my_team
    accepted: list[TradeEval] = []
    future: list[TradeEval] = []
    for t in ctx.teams:
        if t.team_id == me.team_id:
            continue
        seen: set[tuple[frozenset[str], frozenset[str]]] = set()
        team_evals: list[TradeEval] = []
        for give, get, _ in eng.candidates(t.team_id):
            key = (frozenset(p.cid for p in give), frozenset(p.cid for p in get))
            if key in seen:
                continue
            seen.add(key)
            ev = eng.evaluate(t.team_id, give, get)
            if ev.accepted:
                team_evals.append(ev)
            elif future_only is not None and ev.accepted_future:
                future.append(ev)
        # a 2-for-1 that merely pads an accepted 1-for-1 (same core, no better) is redundant
        singles = {(frozenset(p.cid for p in e.give), frozenset(p.cid for p in e.get)): e.score
                   for e in team_evals if len(e.give) == 1 and len(e.get) == 1}
        kept = []
        for e in team_evals:
            if len(e.give) + len(e.get) > 2:
                cores = [(frozenset({g.cid}), frozenset({r.cid})) for g in e.give for r in e.get]
                if any(singles.get(c, -math.inf) >= e.score for c in cores):
                    continue
            kept.append(e)
        kept.sort(key=lambda e: e.score, reverse=True)
        accepted.extend(kept[:max_per_team])
    accepted.sort(key=lambda e: e.score, reverse=True)
    if future_only is not None:
        future.sort(key=lambda e: e.delta_dyn_n + THEM_WEIGHT * min(e.delta_them_eff, 1.0), reverse=True)
        for e in future[:limit]:
            r = _to_rec(e, eng.V, eng.units, eng.lineup.source)
            r.score = e.delta_dyn_n
            r.predicted_gain, r.gain_units = float(e.delta_dyn_n), "dynasty"
            future_only.append(r)
        apply_ranks(apply_strength(future_only))
    return apply_ranks(apply_strength([_to_rec(e, eng.V, eng.units, eng.lineup.source) for e in accepted[:limit]]))
