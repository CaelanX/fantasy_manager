"""Trade proposals ranked by expected value: my gain x the chance the other manager says yes.

Realistic trades are small wins: small enough that the deal makes sense to the person receiving
it, big enough to be worth sending. So a proposal is scored from both sides (docs/trades.md):

* my edge    ΔMe = my optimal starting lineup's season FPG after - before (recommend.lineup's
             optimal_lineup when importable, else the built-in exact greedy solver), minus a roto
             balance term in roto leagues. Dynasty leagues blend in the long-term value change
             ΔDyn_n (below) by ``ctx.dynasty_mode``.
* p_accept   the counterparty's view, from MARKET perception rather than our model: every player
             gets a 0-100 market percentile within the league's rostered pool (ESPN ADP and %
             rostered, Fantrax % rostered and Fantrax's own projected FP/G; our season FPG only when
             a player has none of them; see ``MarketPool``). Deals add up in market worth =
             100 x (percentile/100)^3 (``market_worth``: rank percentiles are too flat at the top to
             add). Perceived fairness to them = package(what they get) - package(what they give)
             (a package counts its best player in full and each further player at DEPTH_WEIGHT),
             - ROSTER_SPOT_PENALTY per player they must drop to make room, + NEED_PTS when an
             incoming player fills their weakest starting slot, + BLOCK_PTS when the deal matches
             their trade block. A logistic maps it to a probability (even = 0.5, +10 worth points
             in their favour = 0.75, -10 = 0.25, capped at
             P_CEILING), which is multiplied by roster legality (0 when their roster would break a
             league maximum, ``base.roster_legal_after``) and, in dynasty leagues, by 0.85 when a
             contender gets no help this season or a rebuilder gets only older players. The
             mapping is a prior: ``calibrate_acceptance`` refits it from the harness ledger once
             enough proposals are logged.
* EV         my edge x p_accept; proposals are ranked by EV (``Recommendation.score``).

Hard filters: my edge > MIN_GAIN_ME (0.3), p_accept >= MIN_P_ACCEPT (0.25), both rosters legal
after the trade, and no forced drop of a protected prospect. The old fairness band on our own
model value (|V_in - V_out| <= 12%, uneven counts within [0.85, 1.15] of the freed-spot-adjusted
side) is still computed and shown (FAIR_PCT) but no longer filters.

Diversity: at most ``max_per_team`` (3) proposals per counterparty and MAX_PER_GIVEN (2) per
player I give away. The "sweet spot" list (``sweet_spot``) holds deals the market calls fair
(|perceived| <= 5 worth points) while our model says I gain >= 0.4: sweet_spot_score = my edge -
MARKET_PTS_TO_FPG x max(0, -perceived) (how much the market thinks they lose, in FPG).

Candidates: for each opponent, 1-for-1 and 2-for-1 (both directions) over each side's top-12
players by trade value V (season VORP, or dynasty value when supplied); deals whose market
packages alone cannot reach p_accept >= 0.25 even with every bonus are skipped before any lineup
solve.

Dynasty leagues (V = dynasty value): ΔDyn = V_in - V_out - V(my drops) + V(my FA fill) - repl x
(net players I add: a roster spot is worth a replacement-level asset, repl = the league's minimum
replacement FPG x the dynasty scale; before the rework the fairness band carried this term),
normalised to FPG-like units by the sum of the mode's year weights plus its terminal weight
(ΔDyn_n), with ``ctx.dynasty_mode``:

* contend    my edge = ΔMe + 0.5 * ΔDyn_n; ΔMe must be >= -WIN_NOW_TOLERANCE (0.3 FPG). A
             trade that costs more this season is not recommended; it goes to the
             ``future_only`` bucket with a WIN_NOW_COST reason.
* balanced   my edge = ΔMe + ΔDyn_n (ΔMe >= -1.0).
* rebuild    my edge = ΔDyn_n + 0.5 * ΔMe (no this-season floor).

Protected players (young first-round picks aged <= 23, or <= 23 and widely rostered; see
``base.protection_reason``) are dropped to make room only as a last resort and only when the
incoming value is >= 1.5x theirs; otherwise the trade is rejected.

Per-position roster maximums (``ctx.position_limits``, counted over the whole roster incl. IR)
are enforced for both sides: when the incoming players would put a side over a maximum, it must
drop its lowest-value player(s) at that position (POSITION_CAP reason; a trade is rejected when
it has no such player to drop), and an FA fill never breaks a maximum. Roster size is enforced:
the side receiving more players first moves IR/LTIR-status players into free IR slots, then
drops its lowest-value droppable player(s) (lowest V when V is dynasty value, else lowest season
FPG; named in the reasons); the side freeing a spot picks up the best fitting FA.

``Recommendation.predicted_gain`` stays ΔMe in lineup_fpg (what the harness grades). p_accept,
the market view and the sweet-spot score travel as reasons (P_ACCEPT, MARKET_VIEW, SWEET_SPOT
with values), and EV is the score.
"""
from __future__ import annotations

import bisect
import itertools
import json
import math
from dataclasses import dataclass, field
from functools import lru_cache
from typing import Any, Callable, Iterable, Mapping, Sequence

from .strength import apply_ranks, apply_strength
from ..models import FantasyTeam, LeagueContext, Player, Reason, Recommendation, RosterSlot
from ..valuation.replacement import eligible_slots
from .base import (PROTECT_OVERRIDE, _position_counts, cap_text, has_data, limit_positions, player_age,
                   protection_reason, roster_legal_after)

try:  # written concurrently by another module owner; greedy fallback below
    from .lineup import optimal_lineup as _optimal_lineup  # type: ignore
except Exception:  # pragma: no cover - depends on sibling module availability
    _optimal_lineup = None

# -- filters and ranking --------------------------------------------------------------------
MIN_GAIN_ME = 0.3            # my edge (lineup FPG, or the mode's blend in dynasty) must beat this
MIN_P_ACCEPT = 0.25          # plausibility: the counterparty must plausibly say yes
MAX_PER_GIVEN = 2            # diversity: proposals per player I give away
MAX_PER_RECEIVED = 2         # ... and per player I would receive
SWEET_SPOT_N = 5
SWEET_MAX_PERCEIVED = 5.0    # market calls it fair: |perceived| <= 5 worth points
SWEET_MIN_EDGE = 0.4         # ... while our model says I gain at least this (FPG)
MARKET_PTS_TO_FPG = 0.05     # 10 market worth points ~ 0.5 FPG (sweet-spot penalty only)

# -- market view and acceptance (a prior; calibrate_acceptance refits the logistic) ----------
MARKET_WEIGHTS = {"adp": 2.0, "proj": 2.0, "owned": 1.0, "dyn": 2.0}   # % rostered saturates near 100
GROUPED_COMPONENTS = frozenset({"proj", "dyn", "fpg"})   # ranked within goalies / skaters
MARKET_MIN_REF = 10          # a component needs this many rostered players reporting it
MARKET_CONVEXITY = 3.0       # market worth = 100 x (percentile / 100) ** 3 (see market_worth)
DEPTH_WEIGHT = 0.5           # a package's 2nd+ player counts at this share of his worth
ROSTER_SPOT_PENALTY = 3.0    # per player the counterparty must drop to make room
NEED_PTS = 5.0               # an incoming player fills their weakest starting slot
BLOCK_PTS = 5.0              # the deal matches their trade block (a wanted position / an offered player)
ACCEPT_X0 = 0.0              # perceived fairness at which p = 0.5
ACCEPT_K = math.log(3.0) / 10.0   # logistic slope: +10 points -> 0.75, -10 -> 0.25
P_CEILING = 0.9              # nobody accepts every offer (inactive managers, attachment)
CONTENDER_FUTURE_MULT = 0.85  # dynasty: a contender whose lineup gets no help this season
REBUILDER_VETERAN_MULT = 0.85  # dynasty: a rebuilder receiving only clearly older players
REBUILD_AGE_GAP = 3.0        # "clearly older": incoming mean age >= outgoing mean age + 3
CALIBRATION_MIN_N = 20       # logged proposals needed before calibrate_acceptance refits
MARKET_SOURCE_LABEL = {"adp": "ADP", "owned": "rostered %", "proj": "Fantrax projected FP/G",
                       "dyn": "dynasty value", "fpg": "our FPG"}

# -- enumeration, legacy display band, dynasty ------------------------------------------------
FAIR_BAND = 0.12             # model-value gap shown as FAIR_PCT (no longer a filter)
UNEVEN_BAND = (0.85, 1.15)
TOP_N = 12
MAX_EVALS_PER_TEAM = 900
REFINE_PER_TEAM = 12         # candidates per counterparty re-solved with the exact lineup solver
OVERPAY_CAP = 30.0           # skip candidates this far in their favour by market (p is at the ceiling)
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



# -- market view -----------------------------------------------------------------------------

FPTS_STAT = "FPTS"   # Fantrax's own fantasy points in its StatLines (providers.fantrax.FPTS_KEY)


def _market_raw(p: Player, values: Mapping[str, Any],
                dyn: Mapping[str, float] | None = None) -> dict[str, float]:
    """Raw market signals for `p`, oriented so that higher = more valuable: ESPN ADP (negated),
    % rostered, Fantrax's projected FP/G, dynasty value (dynasty leagues) and our season FPG
    (fallback only)."""
    out: dict[str, float] = {}
    if p.adp is not None and p.adp > 0:
        out["adp"] = -float(p.adp)
    if p.pct_owned is not None:
        out["owned"] = float(p.pct_owned)
    proj = p.lines.get("projected")
    if proj is not None and proj.gp > 0 and proj.stats.get(FPTS_STAT) is not None:
        out["proj"] = float(proj.stats[FPTS_STAT]) / proj.gp
    if dyn is not None and p.cid in dyn:
        out["dyn"] = float(dyn[p.cid])
    fpg = getattr(values.get(p.cid), "fpg_season", None)
    if isinstance(fpg, (int, float)) and not isinstance(fpg, bool):
        out["fpg"] = float(fpg)
    return out


def percentile(x: float, ref: Sequence[float]) -> float:
    """Mid-rank percentile (0-100) of `x` within the sorted reference `ref` (ties share the
    middle of their range; 50 when `ref` is empty)."""
    if not ref:
        return 50.0
    lo, hi = bisect.bisect_left(ref, x), bisect.bisect_right(ref, x)
    return 100.0 * (lo + 0.5 * (hi - lo)) / len(ref)


def _group(k: str, p: Player) -> str:
    """Reference group: per-game scoring components rank goalies and skaters separately
    (goalies out-score skaters per game in many points leagues); ADP and % rostered are
    already cross-position market prices."""
    return ("G" if p.is_goalie else "S") if k in GROUPED_COMPONENTS else ""


class MarketPool:
    """How the league at large rates each player (0-100), independent of our model.

    Reference pool: every rostered player in the league (the tradeable assets). Components, each
    a percentile within the rostered players that report it (projected FP/G, dynasty value and
    FPG: within the player's group, goalies or skaters) and used only when at least
    MARKET_MIN_REF of them do: ESPN ADP (lower = better), % rostered (ESPN / Fantrax), Fantrax's
    own projected FP/G, and in dynasty leagues our dynasty value as the stand-in for dynasty
    consensus (no provider publishes dynasty ADP; % rostered and season projections ignore age).
    market value = their MARKET_WEIGHTS-weighted mean. A player with none of them falls back to
    the percentile of our season FPG (needs only two rostered players with a value); with
    nothing at all, 0."""

    def __init__(self, ctx: LeagueContext, values: Mapping[str, Any] | None = None,
                 dynasty_values: Mapping[str, float] | None = None):
        self.values: Mapping[str, Any] = values or {}
        self.dyn = dict(dynasty_values) if (ctx.dynasty and dynasty_values) else None
        rostered = {q.cid: q for t in ctx.teams for q in t.players}
        acc: dict[tuple[str, str], list[float]] = {}
        for p in rostered.values():
            for k, v in _market_raw(p, self.values, self.dyn).items():
                acc.setdefault((k, _group(k, p)), []).append(v)
        self.ref: dict[tuple[str, str], list[float]] = {
            key: sorted(xs) for key, xs in acc.items()
            if len(xs) >= (MARKET_MIN_REF if key[0] in MARKET_WEIGHTS else 2)}
        self._cache: dict[str, tuple[float, tuple[str, ...]]] = {}

    def components(self, p: Player) -> dict[str, float]:
        """{component: percentile} for every component `p` reports and the pool supports."""
        out = {}
        for k, v in _market_raw(p, self.values, self.dyn).items():
            ref = self.ref.get((k, _group(k, p)))
            if ref is not None:
                out[k] = percentile(v, ref)
        return out

    def value_and_sources(self, p: Player) -> tuple[float, tuple[str, ...]]:
        hit = self._cache.get(p.cid)
        if hit is None:
            comps = self.components(p)
            market = {k: v for k, v in comps.items() if k in MARKET_WEIGHTS}
            if market:
                w = sum(MARKET_WEIGHTS[k] for k in market)
                hit = (sum(MARKET_WEIGHTS[k] * v for k, v in market.items()) / w, tuple(sorted(market)))
            elif "fpg" in comps:
                hit = (comps["fpg"], ("fpg",))
            else:
                hit = (0.0, ())
            self._cache[p.cid] = hit
        return hit

    def value(self, p: Player) -> float:
        return self.value_and_sources(p)[0]

    def sources(self, players: Iterable[Player]) -> list[str]:
        """Components behind these players' values, in a fixed display order."""
        used = {s for p in players for s in self.value_and_sources(p)[1]}
        return [k for k in ("adp", "owned", "proj", "dyn", "fpg") if k in used]


def market_value(player: Player, pool: MarketPool) -> float:
    """0-100 market percentile of `player` within the league's rostered pool."""
    return pool.value(player)


def market_worth(pct: float) -> float:
    """Market worth (0-100) of a player at market percentile `pct`: 100 x (pct/100)^MARKET_CONVEXITY.

    Rank percentiles are linear in rank while trade value is convex: in a 300-player rostered
    pool the #2 and the #42 player differ by only ~13 percentile points, so sums of percentiles
    let two good players "buy" a superstar. Deals are therefore added up in worth points (mid
    pool, 10 percentile points ~ 8-9 worth points; near the top ~25)."""
    x = min(max(pct, 0.0), 100.0) / 100.0
    return 100.0 * x ** MARKET_CONVEXITY


def package_value(worths: Iterable[float]) -> float:
    """Market worth of a package: the best player in full, each further one at DEPTH_WEIGHT
    (two mid-round players are not a star)."""
    xs = sorted(worths, reverse=True)
    return sum(x if i == 0 else DEPTH_WEIGHT * x for i, x in enumerate(xs))


# -- acceptance ------------------------------------------------------------------------------

@dataclass(frozen=True)
class AcceptParams:
    """Logistic acceptance curve p = 1 / (1 + exp(-k (perceived - x0))), capped at `ceiling`.
    The defaults are a prior (even = 0.5, +10 = 0.75, -10 = 0.25); ``calibrate_acceptance``
    refits x0 / k from logged proposals."""
    x0: float = ACCEPT_X0
    k: float = ACCEPT_K
    ceiling: float = P_CEILING
    n: int = 0                  # logged proposals behind the fit (0 or < CALIBRATION_MIN_N: prior)
    source: str = "prior"

    def p(self, perceived: float) -> float:
        z = max(-50.0, min(50.0, self.k * (perceived - self.x0)))
        return min(self.ceiling, 1.0 / (1.0 + math.exp(-z)))

    def perceived_for(self, p: float) -> float:
        """Perceived fairness at which the (uncapped) curve reaches `p`."""
        p = min(max(p, 1e-6), 1.0 - 1e-6)
        return self.x0 + math.log(p / (1.0 - p)) / max(self.k, 1e-9)


DEFAULT_ACCEPT = AcceptParams()


@dataclass
class Acceptance:
    """The counterparty's view of a deal (market worth points, + = in their favour)."""
    gets: float                   # package value they receive
    gives: float                  # package value they send
    perceived: float              # gets - gives - roster-spot penalties + need / trade-block bonuses
    p_raw: float                  # logistic of `perceived` (capped at the ceiling)
    p: float                      # after legality and standing multipliers
    notes: list[str] = field(default_factory=list)
    legal: bool = True
    legal_note: str | None = None
    sources: list[str] = field(default_factory=list)
    params: AcceptParams = DEFAULT_ACCEPT

    @property
    def base(self) -> float:
        return self.gets - self.gives


def team_standing(ctx: LeagueContext) -> dict[str, str]:
    """team_id -> "contender" (top half by win %), "rebuilder" (bottom third) or "middle";
    empty until at least two teams have played a game."""
    rows = []
    for t in ctx.teams:
        if not t.record:
            continue
        w, l, ti = t.record
        gp = w + l + ti
        if gp > 0:
            rows.append((t.team_id, (w + 0.5 * ti) / gp))
    if len(rows) < 2:
        return {}
    rows.sort(key=lambda r: -r[1])
    n = len(rows)
    return {tid: "contender" if i < n / 2 else ("rebuilder" if i >= n - n / 3 else "middle")
            for i, (tid, _) in enumerate(rows)}


def _mean_age(ps: Iterable[Player], ctx: LeagueContext, dynasty_values: Mapping[str, Any] | None) -> float | None:
    ages = [a for a in (player_age(p, ctx, dynasty_values) for p in ps) if a is not None]
    return sum(ages) / len(ages) if ages else None


def p_accept(deal: "TradeEval", their_team: FantasyTeam, ctx: LeagueContext, market: MarketPool | None = None,
             params: AcceptParams | None = None, standing: Mapping[str, str] | None = None,
             dynasty_values: Mapping[str, Any] | None = None) -> Acceptance:
    """Probability that `their_team` accepts `deal` (``deal.give`` goes to them, ``deal.get``
    comes from them), judged by market value rather than our model (module docstring)."""
    market = market or MarketPool(ctx, {})
    params = params or DEFAULT_ACCEPT
    gets = package_value(market_worth(market.value(p)) for p in deal.give)
    gives = package_value(market_worth(market.value(p)) for p in deal.get)
    perceived = gets - gives
    notes: list[str] = []
    if deal.their_drops:
        pen = ROSTER_SPOT_PENALTY * len(deal.their_drops)
        perceived -= pen
        notes.append(f"-{pen:g} they must drop {_names(deal.their_drops)}")
    if deal.need_text:
        perceived += NEED_PTS
        notes.append(f"+{NEED_PTS:g} fills their weakest slot")
    if deal.block_text:
        perceived += BLOCK_PTS
        notes.append(f"+{BLOCK_PTS:g} trade block")
    p_raw = params.p(perceived)
    p = p_raw
    add = list(deal.give) + ([deal.their_fill] if deal.their_fill is not None else [])
    legal, why = roster_legal_after(their_team, ctx, add=add, drop=list(deal.get) + list(deal.their_drops),
                                    to_ir=deal.their_ir)
    if not legal:
        p = 0.0
        notes.append(f"illegal for them ({why})")
    if ctx.dynasty and p > 0:
        st = (standing if standing is not None else team_standing(ctx)).get(their_team.team_id)
        if st == "contender" and deal.delta_them <= 0:
            p *= CONTENDER_FUTURE_MULT
            notes.append(f"x{CONTENDER_FUTURE_MULT:g}: contender, no help this season")
        elif st == "rebuilder":
            a_in, a_out = _mean_age(deal.give, ctx, dynasty_values), _mean_age(deal.get, ctx, dynasty_values)
            if a_in is not None and a_out is not None and a_in >= a_out + REBUILD_AGE_GAP:
                p *= REBUILDER_VETERAN_MULT
                notes.append(f"x{REBUILDER_VETERAN_MULT:g}: rebuilding, gets older players")
    return Acceptance(gets=gets, gives=gives, perceived=perceived, p_raw=p_raw, p=p, notes=notes, legal=legal,
                      legal_note=why, sources=market.sources(list(deal.give) + list(deal.get)), params=params)


def _acceptance_observations(ledger: Any, league: str | None = None) -> list[tuple[float, int]]:
    """(perceived fairness, accepted) per logged trade proposal: rec_episodes of kind trade with
    status proposed (offered, not accepted) or followed (the trade went through); x = the
    MARKET_VIEW value of the episode's first logged rec. Episodes logged before MARKET_VIEW
    existed are skipped."""
    sql = ("SELECT league, rec_key, status FROM rec_episodes WHERE kind='trade' "
           "AND status IN ('proposed','followed')")
    args: list[Any] = []
    if league:
        sql += " AND league=?"
        args.append(league)
    out: list[tuple[float, int]] = []
    try:
        for ep in ledger.query(sql, args):
            rows = ledger.query("SELECT reasons_json FROM recs WHERE league=? AND rec_key=? ORDER BY as_of LIMIT 1",
                                (ep["league"], ep["rec_key"]))
            if not rows or not rows[0].get("reasons_json"):
                continue
            reasons = json.loads(rows[0]["reasons_json"])
            x = next((r.get("value") for r in reasons if isinstance(r, dict) and r.get("code") == "MARKET_VIEW"), None)
            if isinstance(x, (int, float)):
                out.append((float(x), 1 if ep["status"] == "followed" else 0))
    except Exception:
        return []
    return out


def calibrate_acceptance(ledger: Any, league: str | None = None, min_n: int = CALIBRATION_MIN_N,
                         prior: AcceptParams = DEFAULT_ACCEPT) -> AcceptParams:
    """Logistic parameters for ``p_accept`` from the harness ledger.

    TODO(harness): call this from the refit step and pass the result to recommend_trades
    (``accept_params``) once proposals are being logged; a proposal that is still pending when
    its window closes counts as a rejection. Until ``min_n`` (20) proposals exist the prior is
    returned unchanged (with ``n`` set); after that a ridge-regularised logistic regression of
    accepted on the logged MARKET_VIEW value, shrunk toward the prior, gives x0 and k."""
    obs = _acceptance_observations(ledger, league)
    if len(obs) < min_n:
        return AcceptParams(x0=prior.x0, k=prior.k, ceiling=prior.ceiling, n=len(obs), source=prior.source)
    a0, b0 = -prior.k * prior.x0, prior.k          # p = sigmoid(a + b x)
    lam_a, lam_b = 1.0, 100.0                       # prior strength (slopes live on a 0.1 scale)
    a, b = a0, b0
    for _ in range(100):
        ga, gb = -lam_a * (a - a0), -lam_b * (b - b0)
        haa, hab, hbb = -lam_a, 0.0, -lam_b
        for x, y in obs:
            z = max(-50.0, min(50.0, a + b * x))
            p = 1.0 / (1.0 + math.exp(-z))
            r, w = y - p, p * (1.0 - p)
            ga, gb = ga + r, gb + r * x
            haa, hab, hbb = haa - w, hab - w * x, hbb - w * x * x
        det = haa * hbb - hab * hab
        if abs(det) < 1e-12:
            break
        da, db = (hbb * ga - hab * gb) / det, (haa * gb - hab * ga) / det
        a, b = a - da, b - db
        if max(abs(da), abs(db)) < 1e-9:
            break
    k = max(b, 1e-3)
    return AcceptParams(x0=-a / k, k=k, ceiling=prior.ceiling, n=len(obs), source=f"fit on {len(obs)} proposals")


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
    fair: bool                            # model-value band (display only since the EV rework)
    fair_dev: float
    delta_me: float
    delta_them: float
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
    my_cap: str | None = None             # per-position maximum that forces one of my_drops
    their_cap: str | None = None
    block_text: str | None = None         # matches the counterparty's trade block
    my_illegal: str | None = None         # roster_legal_after failure on my side
    acceptance: Acceptance | None = None
    screened: bool = False                # stopped after my side: my gain cannot pass
    refined: bool = False                 # lineups re-solved exactly (``_TradeEngine.refine``)
    me_after: list[Player] = field(default_factory=list, repr=False)
    them_after: list[Player] = field(default_factory=list, repr=False)

    @property
    def need_bonus(self) -> float:
        """Market points the weakest-slot fit adds to the counterparty's view."""
        return NEED_PTS if self.need_text else 0.0

    @property
    def core(self) -> float:
        """ΔMe, or the mode's blend of ΔMe and ΔDyn in dynasty leagues."""
        if self.mode is None:
            return self.delta_me
        return ME_WEIGHT[self.mode] * self.delta_me + DYN_WEIGHT[self.mode] * self.delta_dyn_n

    @property
    def my_edge(self) -> float:
        return self.core - self.roto_adj

    @property
    def p(self) -> float:
        return self.acceptance.p if self.acceptance is not None else 0.0

    @property
    def perceived(self) -> float:
        return self.acceptance.perceived if self.acceptance is not None else 0.0

    @property
    def ev(self) -> float:
        return self.my_edge * self.p

    @property
    def score(self) -> float:
        return self.ev

    @property
    def sweet_spot_score(self) -> float:
        """My edge minus how much the market thinks they lose (in FPG-like units)."""
        return self.my_edge - MARKET_PTS_TO_FPG * max(0.0, -self.perceived)

    @property
    def sweet_spot(self) -> bool:
        return (self.accepted and abs(self.perceived) <= SWEET_MAX_PERCEIVED + 1e-9
                and self.my_edge >= SWEET_MIN_EDGE - 1e-9)

    @property
    def win_now_cost(self) -> bool:
        """Contend mode: the trade weakens this season's lineup beyond the tolerance."""
        return self.mode == "contend" and self.delta_me < -WIN_NOW_TOLERANCE

    @property
    def _plausible(self) -> bool:
        return (not self.screened and self.blocked is None and self.my_illegal is None
                and self.p >= MIN_P_ACCEPT - 1e-9)

    @property
    def near_plausible(self) -> bool:
        """Could pass once the exact solver has run (the greedy screen's slack)."""
        return (not self.screened and self.blocked is None and self.my_illegal is None
                and self.p >= MIN_P_ACCEPT - 0.05)

    @property
    def accepted_future(self) -> bool:
        """Would be accepted if this season's lineup did not matter (future-only bucket)."""
        return self._plausible and self.win_now_cost and self.delta_dyn_n > MIN_GAIN_ME

    @property
    def accepted(self) -> bool:
        if not self._plausible:
            return False
        if self.mode is not None:
            if self.win_now_cost:
                return False
            if self.mode == "balanced" and self.delta_me < -BALANCED_TOLERANCE:
                return False
        return self.my_edge > MIN_GAIN_ME


class _TradeEngine:
    def __init__(self, ctx: LeagueContext, values: Mapping[str, Any],
                 dynasty_values: Mapping[str, Any] | None = None,
                 wants: Mapping[str, set[str]] | None = None,
                 lineup_fn: LineupFn | None | bool = None,
                 offered: Mapping[str, set[str]] | None = None,
                 accept_params: AcceptParams | None = None):
        self.ctx = ctx
        self.values = values
        self.dynasty_values = dynasty_values
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
        self.offered = {k: set(v) for k, v in (offered or {}).items()}
        self.market = MarketPool(ctx, values, self.V if self.units == "dynasty" else None)
        self.params = accept_params or DEFAULT_ACCEPT
        self.standing = team_standing(ctx)
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
                       ) -> tuple[list[Player], list[Player], Player | None, list[Player], str | None, str | None]:
        """(roster, drops, FA fill, IR moves, blocked, cap note): players over a per-position
        maximum are dropped first (lowest value at that position; `cap note` names the limit);
        then, when over capacity, IR/LTIR-status players go to free IR slots, then the
        lowest-value players are dropped; when a spot opens, the best fitting FA that keeps the
        roster within its maximums is picked up. In dynasty mode protected players are dropped
        only as a last resort; `blocked` explains a forced protected drop that the incoming value
        (< 1.5x) does not justify, or a maximum that no drop can satisfy."""
        blocked: str | None = None
        cap_note: str | None = None
        drops: list[Player] = []
        to_ir: list[Player] = []
        fill: Player | None = None
        inc = {p.cid for p in incoming}
        out_ids = {p.cid for p in outgoing}
        ir_left = [p for p in side.ir if p.cid not in out_ids]
        limits = self.ctx.position_limits or {}

        def drop_order(cands: list[Player]) -> list[Player]:
            cands = sorted(cands, key=self._drop_key)
            if self.units == "dynasty":
                # no stats at all = unknown value (prospects) and protected prospects: drop them
                # only as a last resort
                cands.sort(key=lambda p: (p.cid in self.protected, not has_data(self.values.get(p.cid))))
            return cands

        if limits and incoming:
            before = _position_counts(side.active + side.ir, limits)
            after = _position_counts(players + ir_left, limits)
            notes = []
            for k in sorted(limits):
                need = after[k] - max(int(limits[k]), before[k])
                if need <= 0:
                    continue
                chosen = {p.cid for p in drops}
                pool = drop_order([p for p in players if p.cid not in inc and p.cid not in chosen
                                   and k in limit_positions(p, limits)])
                if len(pool) < need:
                    blocked = blocked or f"would break the {cap_text(self.ctx, k, after[k])}"
                drops.extend(pool[:need])
                notes.append(cap_text(self.ctx, k, after[k]))
            if notes:
                cap_note = "; ".join(notes)
                gone = {p.cid for p in drops}
                players = [p for p in players if p.cid not in gone]
        over = len(players) - side.capacity
        if over > 0:
            if side.free_ir > 0:
                movable = [p for p in players if p.status in IR_MOVE_STATUSES]
                movable.sort(key=lambda p: (IR_MOVE_STATUSES.index(p.status), -self._fpg(p)))
                to_ir = movable[:min(over, side.free_ir)]
                over -= len(to_ir)
            moved = {p.cid for p in to_ir}
            size_drops = drop_order([p for p in players if p.cid not in inc and p.cid not in moved])[:max(0, over)]
            drops.extend(size_drops)
            gone = moved | {p.cid for p in size_drops}
            players = [p for p in players if p.cid not in gone]
        elif len(players) < side.capacity and len(outgoing) > len(incoming):
            counts = _position_counts(players + ir_left, limits) if limits else {}
            for fa in self.fas:
                if limits and any(counts[k] + 1 > int(limits[k]) for k in limit_positions(fa, limits)):
                    continue
                if any(set(eligible_slots(fa.positions)) & set(eligible_slots(o.positions)) for o in outgoing):
                    fill = fa
                    players = players + [fa]
                    break
        v_in = sum(self.V.get(p.cid, 0.0) for p in incoming)
        for d in drops:
            why = self.protected.get(d.cid)
            if why and v_in < PROTECT_OVERRIDE * self.V.get(d.cid, 0.0):
                blocked = blocked or f"would force dropping {d.name} ({why})"
                break
        return players, drops, fill, to_ir, blocked, cap_note

    def _accept(self, ev: TradeEval) -> Acceptance:
        return p_accept(ev, ev.team, self.ctx, self.market, self.params, self.standing, self.dynasty_values)

    def _pkg(self, players: Iterable[Player]) -> float:
        return package_value(market_worth(self.market.value(p)) for p in players)

    def evaluate(self, team_id: str, give: list[Player], get: list[Player],
                 screen: bool = True, exact: bool = True) -> TradeEval:
        """Score one proposal. ``screen``: stop after my side when my (greedy) gain cannot pass
        (the result is marked ``screened`` and never accepted). ``exact``: re-solve plausible
        candidates with the exact lineup solver right away (``recommend_trades`` passes False
        and ``refine``s only the candidates that can reach its lists)."""
        me = self.sides[self.ctx.my_team.team_id]
        them = self.sides[team_id]
        give_ids, get_ids = {p.cid for p in give}, {p.cid for p in get}
        fair, dev = fairness([self.V.get(p.cid, 0.0) for p in get],
                             [self.V.get(p.cid, 0.0) for p in give], self.repl)

        me_after = [p for p in me.active if p.cid not in give_ids] + list(get)
        me_after, my_drops, my_fill, my_ir, my_block, my_cap = self._settle_roster(me, me_after, get, give)
        v_in = sum(self.V.get(p.cid, 0.0) for p in get)
        v_out = sum(self.V.get(p.cid, 0.0) for p in give)
        # each extra player I end up rostering takes a spot a replacement-level asset (repl) could fill
        net_added = len(get) - len(give) - len(my_drops) + (1 if my_fill is not None else 0)
        d_dyn = v_in - v_out - sum(self.V.get(p.cid, 0.0) for p in my_drops) \
            + (self.V.get(my_fill.cid, 0.0) if my_fill is not None else 0.0) - self.repl * net_added
        d_dyn_n = d_dyn / self.scale if self.mode is not None else 0.0
        # cheap greedy screen of my side first
        d_me = self.lineup.greedy(me_after) - self.lineup.greedy(me.active)
        if self.mode is None:
            mine_ok = d_me > MIN_GAIN_ME - SCREEN_MARGIN
        else:
            core = ME_WEIGHT[self.mode] * d_me + DYN_WEIGHT[self.mode] * d_dyn_n
            mine_ok = core > MIN_GAIN_ME - SCREEN_MARGIN or d_dyn_n > MIN_GAIN_ME - SCREEN_MARGIN
        if screen and (not mine_ok or my_block is not None):
            return TradeEval(team=them.team, give=list(give), get=list(get), v_in=v_in, v_out=v_out, fair=fair,
                             fair_dev=dev, delta_me=d_me, delta_them=0.0, need_text=None, my_drops=my_drops,
                             their_drops=[], my_fill=my_fill, their_fill=None, my_ir=my_ir, mode=self.mode,
                             delta_dyn=d_dyn, delta_dyn_n=d_dyn_n, blocked=my_block, my_cap=my_cap, screened=True)

        them_after = [p for p in them.active if p.cid not in get_ids] + list(give)
        them_after, their_drops, their_fill, their_ir, their_block, their_cap = self._settle_roster(
            them, them_after, give, get)
        blocked = my_block or (f"{them.team.name} {their_block}" if their_block else None)
        need_text = None
        if them.weakest is not None:
            slot, v = them.weakest
            fits = [p for p in give if _eligible(tuple(p.positions), slot)]
            if fits:
                vtxt = "empty" if v == -math.inf else f"starter VORP {v:+.2f}"
                need_text = f"{fits[0].name} fills {them.team.name}'s weakest slot {slot} ({vtxt})"
        block: list[str] = []
        offered = [p for p in get if p.cid in self.offered.get(team_id, set())]
        if offered:
            block.append(f"{_names(offered)} {'is' if len(offered) == 1 else 'are'} on "
                         f"{them.team.name}'s trade block")
        want = self.wants.get(team_id, set())
        fits = [p for p in give if set(p.positions) & want] if want else []
        if fits:
            block.append(f"{fits[0].name} matches {them.team.name}'s trade-block wants ({', '.join(sorted(want))})")
        ok, why = roster_legal_after(me.team, self.ctx, add=list(get) + ([my_fill] if my_fill else []),
                                     drop=list(give) + my_drops, to_ir=my_ir)
        d_them = self.lineup.greedy(them_after) - self.lineup.greedy(them.active)
        ev = TradeEval(team=them.team, give=list(give), get=list(get), v_in=v_in, v_out=v_out,
                       fair=fair, fair_dev=dev, delta_me=d_me, delta_them=d_them, need_text=need_text,
                       my_drops=my_drops, their_drops=their_drops, my_fill=my_fill, their_fill=their_fill,
                       my_ir=my_ir, their_ir=their_ir, mode=self.mode, delta_dyn=d_dyn, delta_dyn_n=d_dyn_n,
                       blocked=blocked, my_cap=my_cap, their_cap=their_cap,
                       block_text="; ".join(block) or None, my_illegal=None if ok else why)
        ev.me_after, ev.them_after = me_after, them_after
        ev.acceptance = self._accept(ev)
        if exact and (not screen or ev.near_plausible):
            self.refine(ev)
        if self.roto is not None:
            others = [t for tid, t in self.totals.items() if tid != me.team.team_id]
            before = self.roto.category_balance_penalty(self.totals[me.team.team_id], others)
            others_after = [self._totals(them_after) if tid == team_id else t
                            for tid, t in self.totals.items() if tid != me.team.team_id]
            after = self.roto.category_balance_penalty(self._totals(me_after), others_after)
            ev.roto_adj = ROTO_PENALTY_WEIGHT * (after - before)
        return ev

    def refine(self, ev: TradeEval) -> TradeEval:
        """Re-solve both lineups with the exact solver (a no-op with the greedy solver, which
        gives the same optimum for this slot structure) and refresh the acceptance, whose
        dynasty standing modifier reads ΔThem."""
        if ev.refined or ev.screened:
            return ev
        ev.refined = True
        if not self.lineup.exact:
            return ev
        me = self.sides[self.ctx.my_team.team_id]
        them = self.sides[ev.team.team_id]
        ev.delta_me = self.lineup(ev.me_after) - self.lineup(me.active)
        ev.delta_them = self.lineup(ev.them_after) - self.lineup(them.active)
        if self.ctx.dynasty:
            ev.acceptance = self._accept(ev)
        return ev

    def candidates(self, team_id: str) -> list[tuple[list[Player], list[Player], float]]:
        """(give, get, market base in their favour) for every 1-for-1 / 2-for-1 / 1-for-2 whose
        market packages could still reach p_accept >= MIN_P_ACCEPT with every bonus and that do
        not hand them more than OVERPAY_CAP worth points (p is long at its ceiling by then);
        1-for-1 first, then the closest to even, capped at MAX_EVALS_PER_TEAM."""
        mine = self.sides[self.ctx.my_team.team_id].top
        theirs = self.sides[team_id].top
        floor = self.params.perceived_for(MIN_P_ACCEPT) - NEED_PTS - BLOCK_PTS
        out: list[tuple[list[Player], list[Player], float]] = []
        shapes = [((a,), (b,)) for a in mine for b in theirs]
        shapes += [(pair, (b,)) for pair in itertools.combinations(mine, 2) for b in theirs]
        shapes += [((a,), pair) for a in mine for pair in itertools.combinations(theirs, 2)]
        for give, get in shapes:
            base = self._pkg(give) - self._pkg(get)
            if floor - 1e-9 <= base <= OVERPAY_CAP + 1e-9:
                out.append((list(give), list(get), base))
        out.sort(key=lambda c: (len(c[0]) + len(c[1]), abs(c[2])))
        return out[:MAX_EVALS_PER_TEAM]


def _names(ps: Iterable[Player]) -> str:
    return " + ".join(p.name for p in ps)


def market_label(perceived: float) -> str:
    """How a deal looks to the counterparty by market value."""
    if abs(perceived) <= 3.0:
        return "Looks even to them"
    if perceived > 10.0:
        return "Looks clearly in their favor"
    if perceived > 0:
        return "Looks slightly in their favor"
    if perceived >= -10.0:
        return "Looks slightly in your favor"
    return "Looks clearly in your favor"


def _pct_text(p: float) -> str:
    """Acceptance rounded to 5%: '~70%'."""
    return f"~{5 * round(p * 20):.0f}%"


def _roster_text(ev: TradeEval) -> str:
    parts = []
    if ev.my_ir:
        parts.append(f"you move {_names(ev.my_ir)} to IR")
    if ev.my_drops:
        parts.append(f"you drop {_names(ev.my_drops)}")
    if ev.my_fill:
        parts.append(f"you pick up {ev.my_fill.name} (FA)")
    if ev.their_ir:
        parts.append(f"{ev.team.name} moves {_names(ev.their_ir)} to IR")
    if ev.their_drops:
        parts.append(f"{ev.team.name} drops {_names(ev.their_drops)}")
    if ev.their_fill:
        parts.append(f"{ev.team.name} can pick up {ev.their_fill.name} (FA)")
    if not parts:
        return f"Roster: straight {len(ev.give)}-for-{len(ev.get)}, no other moves needed"
    return "Roster: " + "; ".join(parts)


def _to_rec(ev: TradeEval, V: Mapping[str, float], units: str, source: str) -> Recommendation:
    def vl(ps: list[Player]) -> str:
        return ", ".join(f"{p.name} ({V.get(p.cid, 0.0):.2f})" for p in ps)

    acc = ev.acceptance
    band = FAIR_BAND if len(ev.give) == len(ev.get) else UNEVEN_BAND[1] - 1.0
    edge_txt = f"You gain {ev.delta_me:+.2f} pts/game this season"
    if ev.mode is not None:
        edge_txt += f" (dynasty {ev.delta_dyn_n:+.2f}; {ev.mode} edge {ev.core:+.2f})"
    if ev.roto_adj:
        edge_txt += f"; roto balance {-ev.roto_adj:+.2f}"
    reasons = [Reason(code="MY_EDGE", text=edge_txt, value=ev.my_edge, baseline=MIN_GAIN_ME)]
    if acc is not None:
        src = "/".join(MARKET_SOURCE_LABEL[s] for s in acc.sources) or "no market data"
        reasons.append(Reason(code="MARKET_VIEW",
                              text=f"{market_label(acc.perceived)} by market value ({src}); acceptance "
                                   f"{_pct_text(acc.p)}",
                              value=acc.perceived, baseline=acc.base))
    if ev.need_text:
        reasons.append(Reason(code="THEIR_NEED", text=ev.need_text, value=NEED_PTS))
    if ev.block_text:
        reasons.append(Reason(code="TRADE_BLOCK", text=ev.block_text, value=BLOCK_PTS))
    reasons.append(Reason(code="ROSTER_CONSEQUENCE", text=_roster_text(ev),
                          value=float(len(ev.my_drops) + len(ev.their_drops))))
    if ev.sweet_spot:
        reasons.append(Reason(code="SWEET_SPOT",
                              text=f"Sweet spot: the market calls it fair ({ev.perceived:+.1f} pts) while our model "
                                   f"has you gaining {ev.my_edge:+.2f}",
                              value=ev.sweet_spot_score, baseline=ev.perceived))
    if acc is not None:
        how = "; ".join(acc.notes)
        reasons.append(Reason(code="P_ACCEPT",
                              text=f"Acceptance {acc.p:.0%} ({acc.params.source} logistic: they get "
                                   f"{acc.gets:.0f}, give {acc.gives:.0f} market pts"
                                   + (f"; {how}" if how else "") + f" -> {acc.perceived:+.1f} pts). "
                                   f"EV = {ev.my_edge:+.2f} x {acc.p:.2f} = {ev.ev:+.2f}",
                              value=acc.p, baseline=acc.p_raw))
    reasons += [
        Reason(code="DELTA_ME", text=f"My lineup {ev.delta_me:+.2f} FPG ({source})",
               value=ev.delta_me, baseline=MIN_GAIN_ME),
        Reason(code="DELTA_THEM", text=f"{ev.team.name} lineup {ev.delta_them:+.2f} FPG (our model)",
               value=ev.delta_them),
        Reason(code="VALUE_IN", text=f"Receive {vl(ev.get)}: {ev.v_in:.2f} {units}", value=ev.v_in),
        Reason(code="VALUE_OUT", text=f"Send {vl(ev.give)}: {ev.v_out:.2f} {units}", value=ev.v_out),
        Reason(code="FAIR_PCT", text=f"Model-value gap {ev.fair_dev:.0%} (shown only; old band {band:.0%})",
               value=ev.fair_dev, baseline=band),
    ]
    if ev.mode is not None:
        reasons.append(Reason(code="DYNASTY_DELTA",
                              text=f"Dynasty value {ev.delta_dyn:+.2f} for me ({ev.delta_dyn_n:+.2f} FPG-equivalent); "
                                   f"{ev.mode} edge = {ME_WEIGHT[ev.mode]:g} x lineup + "
                                   f"{DYN_WEIGHT[ev.mode]:g} x dynasty",
                              value=ev.delta_dyn_n, baseline=ev.delta_dyn))
    if ev.win_now_cost:
        reasons.append(Reason(code="WIN_NOW_COST",
                              text=f"Costs {-ev.delta_me:.2f} FPG of this season's lineup (contend mode allows "
                                   f"{WIN_NOW_TOLERANCE:g}): future-only",
                              value=ev.delta_me, baseline=-WIN_NOW_TOLERANCE))
    if ev.my_ir:
        reasons.append(Reason(code="IR_MOVE", text=f"You would move {_names(ev.my_ir)} to IR (no drop)"))
    if ev.their_ir:
        reasons.append(Reason(code="IR_MOVE",
                              text=f"{ev.team.name} would move {_names(ev.their_ir)} to IR (no drop)"))
    if ev.my_cap:
        reasons.append(Reason(code="POSITION_CAP",
                              text=f"League roster maximum ({ev.my_cap}): the drop must be a "
                                   f"same-position player"))
    if ev.their_cap:
        reasons.append(Reason(code="POSITION_CAP",
                              text=f"{ev.team.name} is at a league roster maximum ({ev.their_cap}): "
                                   f"they must drop a same-position player"))
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
                   lineup_fn: LineupFn | None | bool = None,
                   offered: Mapping[str, set[str]] | None = None,
                   accept_params: AcceptParams | None = None) -> TradeEval:
    """Evaluate an arbitrary n-for-m proposal (cids). team_id defaults to the owner of `get`."""
    eng = _TradeEngine(ctx, values, dynasty_values, wants, lineup_fn, offered, accept_params)
    by_cid = {p.cid: p for t in ctx.teams for p in t.players}
    if team_id is None:
        owner = [t.team_id for t in ctx.teams if not t.owner_is_me
                 and {p.cid for p in t.players} >= set(get)]
        if not owner:
            raise LookupError("could not find a single team owning all requested players")
        team_id = owner[0]
    return eng.evaluate(team_id, [by_cid[c] for c in give], [by_cid[c] for c in get], screen=False)


def diverse(evals: Iterable[TradeEval], n: int, per_team: int, per_given: int = MAX_PER_GIVEN,
            per_received: int = MAX_PER_RECEIVED) -> list[TradeEval]:
    """The first `n` of `evals` (already in preference order) with at most `per_team` per
    counterparty, `per_given` per player I give away and `per_received` per player I get."""
    out: list[TradeEval] = []
    by_team: dict[str, int] = {}
    by_given: dict[str, int] = {}
    by_got: dict[str, int] = {}
    for e in evals:
        if len(out) >= n:
            break
        tid = e.team.team_id
        if (by_team.get(tid, 0) >= per_team or any(by_given.get(p.cid, 0) >= per_given for p in e.give)
                or any(by_got.get(p.cid, 0) >= per_received for p in e.get)):
            continue
        out.append(e)
        by_team[tid] = by_team.get(tid, 0) + 1
        for p in e.give:
            by_given[p.cid] = by_given.get(p.cid, 0) + 1
        for p in e.get:
            by_got[p.cid] = by_got.get(p.cid, 0) + 1
    return out


def recommend_trades(ctx: LeagueContext, values: Mapping[str, Any],
                     dynasty_values: Mapping[str, Any] | None = None, max_per_team: int = 3,
                     limit: int = 10, wants: Mapping[str, set[str]] | None = None,
                     lineup_fn: LineupFn | None | bool = None,
                     future_only: list[Recommendation] | None = None,
                     sweet_spot: list[Recommendation] | None = None,
                     offered: Mapping[str, set[str]] | None = None,
                     accept_params: AcceptParams | None = None,
                     max_per_given: int = MAX_PER_GIVEN) -> list[Recommendation]:
    """Trade proposals ranked by EV = my edge x p_accept (module docstring), at most
    `max_per_team` per counterparty and `max_per_given` per player given away (and per player
    received).

    `wants` / `offered`: trade-block positions wanted / player cids offered, by team_id (Fantrax).
    `lineup_fn`: None = optimal_lineup if importable, False = force the greedy solver.
    `future_only` (a list, contend mode) collects trades that gain dynasty value but cost this
    season's lineup more than WIN_NOW_TOLERANCE (reason WIN_NOW_COST), best first.
    `sweet_spot` (a list) collects the top SWEET_SPOT_N sweet-spot deals by sweet_spot_score
    and the returned list is the plain EV ranking. Without it, up to min(SWEET_SPOT_N, limit // 2)
    slots of the returned list are reserved for sweet-spot deals (same caps, still ordered by
    EV), so callers that only see this list (advise, the web Moves page) can show them.
    `accept_params`: the acceptance curve (``calibrate_acceptance``); default = the prior."""
    eng = _TradeEngine(ctx, values, dynasty_values, wants, lineup_fn, offered, accept_params)
    me = ctx.my_team
    accepted: list[TradeEval] = []
    future: list[TradeEval] = []
    for t in ctx.teams:
        if t.team_id == me.team_id:
            continue
        seen: set[tuple[frozenset[str], frozenset[str]]] = set()
        team_evals: list[TradeEval] = []
        near: list[TradeEval] = []
        for give, get, _ in eng.candidates(t.team_id):
            key = (frozenset(p.cid for p in give), frozenset(p.cid for p in get))
            if key in seen:
                continue
            seen.add(key)
            ev = eng.evaluate(t.team_id, give, get, exact=False)
            if ev.near_plausible:
                near.append(ev)
        # the exact solver only runs where it can change what is shown: the best candidates by
        # (greedy) EV, by sweet-spot score and, in contend mode, by future value
        refine = sorted(near, key=lambda e: e.score, reverse=True)[:REFINE_PER_TEAM]
        refine += sorted((e for e in near if abs(e.perceived) <= SWEET_MAX_PERCEIVED + 2.0),
                         key=lambda e: e.sweet_spot_score, reverse=True)[:REFINE_PER_TEAM]
        if future_only is not None:
            refine += sorted((e for e in near if e.win_now_cost), key=lambda e: e.delta_dyn_n * e.p,
                             reverse=True)[:REFINE_PER_TEAM]
        for ev in refine:
            eng.refine(ev)
        for ev in near:
            if eng.lineup.exact and not ev.refined:
                continue
            if ev.accepted:
                team_evals.append(ev)
            elif future_only is not None and ev.accepted_future:
                future.append(ev)
        # a 2-for-1 that merely pads an accepted 1-for-1 (same core, no better EV) is redundant
        singles = {(frozenset(p.cid for p in e.give), frozenset(p.cid for p in e.get)): e.score
                   for e in team_evals if len(e.give) == 1 and len(e.get) == 1}
        for e in team_evals:
            if len(e.give) + len(e.get) > 2:
                cores = [(frozenset({g.cid}), frozenset({r.cid})) for g in e.give for r in e.get]
                if any(singles.get(c, -math.inf) >= e.score for c in cores):
                    continue
            accepted.append(e)
    accepted.sort(key=lambda e: e.score, reverse=True)
    sweet = diverse(sorted((e for e in accepted if e.sweet_spot), key=lambda e: e.sweet_spot_score, reverse=True),
                    SWEET_SPOT_N, max_per_team, max_per_given, max_per_given)
    if sweet_spot is not None:
        chosen = diverse(accepted, limit, max_per_team, max_per_given, max_per_given)
    else:
        # no separate list: reserve up to half the slots for sweet-spot deals, then fill by EV
        reserved = sweet[:min(SWEET_SPOT_N, limit // 2)]
        ids = {id(e) for e in reserved}
        chosen = diverse(reserved + [e for e in accepted if id(e) not in ids], limit, max_per_team, max_per_given,
                         max_per_given)
        chosen.sort(key=lambda e: e.score, reverse=True)
    source = eng.lineup.source
    if future_only is not None:
        future.sort(key=lambda e: e.delta_dyn_n * e.p, reverse=True)
        for e in future[:limit]:
            r = _to_rec(e, eng.V, eng.units, source)
            r.score = e.delta_dyn_n * e.p
            r.predicted_gain, r.gain_units = float(e.delta_dyn_n), "dynasty"
            future_only.append(r)
        apply_ranks(apply_strength(future_only))
    if sweet_spot is not None:
        sweet_spot.extend(apply_ranks(apply_strength([_to_rec(e, eng.V, eng.units, source) for e in sweet])))
    return apply_ranks(apply_strength([_to_rec(e, eng.V, eng.units, source) for e in chosen]))
