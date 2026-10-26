"""Trade proposals ranked by expected value: my gain x the chance the other manager says yes.

Realistic trades are small wins: small enough that the deal makes sense to the person receiving
it, big enough to be worth sending. So a proposal is scored from both sides (docs/trades.md):

* my edge    ΔMe = my optimal starting lineup's season FPG after - before (recommend.lineup's
             optimal_lineup when importable, else the built-in exact greedy solver; both value a
             player at ``PlayerValue.fpg_season``, goalies already start-share adjusted), minus a
             roto balance term in roto leagues. "Before" is the optimal lineup of my roster, not my
             current lineup; when the trade frees a spot that I fill with a free agent, it is the
             best of my roster and my roster with that same pickup (``_baselines``): an FA who
             would start is an improvement I can make today, not a trade gain. ΔThem likewise.
             Dynasty leagues blend in the long-term value change ΔDyn_n (below) by
             ``ctx.dynasty_mode``.
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
* EV         my edge x p_accept.
* score      EV - OVERPAY_COST_FPG x max(0, overpay - OVERPAY_FREE_PTS), where overpay = market
             worth they get - market worth they give (``Acceptance.gets - gives``: the market value I
             hand over, without the need / block / pressure bonuses that only make them likelier to
             say yes). Proposals are ranked by score (``Recommendation.score``). EV alone favours deals
             the market calls lopsided in THEIR favour (that is when acceptance is highest), and
             market value given away is real: it is what I could get in another trade. So a deal the
             market calls fair or in my favour outranks an overpay with a similar edge, and a clear
             overpay (overpay > CLEAR_FAVOR_PTS) is listed only when my edge >= OVERPAY_MIN_EDGE,
             with a deal-level MARKET_CHECK warning that names the model's drivers. A deal whose
             score is <= 0 after the market cost is not listed (``TradeEval.listable``).

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
drops its lowest-value droppable player(s) outside its optimal starting lineup (lowest V when V is
dynasty value, else lowest season FPG; a starter only when nobody else can go, the one whose loss
costs the lineup least; named in the reasons); the side freeing a spot picks up the FA who adds
the most to its starting lineup (an open starting need), else the best FA at an outgoing
player's position (bench depth) (``_TradeEngine._pick_fill``).

``Recommendation.predicted_gain`` stays ΔMe in lineup_fpg (what the harness grades). p_accept,
the market view and the sweet-spot score travel as reasons (P_ACCEPT, MARKET_VIEW, SWEET_SPOT
with values), and EV is the score.

Gain units: ΔMe is also given as GAIN_WEEK (ΔMe x my starters' average NHL games per week) and
GAIN_SEASON (ΔMe x their average games left in the regular season), both from ``ctx.schedule``
(``base.game_rate``; defaults 3.4 games/week and 82 x the share of the season left).

Exploits (``exploit_opportunities``): opponents under roster pressure (at or over a position
maximum with an injured player at that position, more injured players than IR slots, fewer than
two healthy goalies with games this week, a starting slot with VORP < -1) are offered the best
deal for me that relieves it; the pressure adds PRESSURE_PTS (8) to their perceived fairness.
A counterparty's 0-moves-left pressure is not detected: providers expose only my own
transaction counter (``ctx.moves_used_this_period``).
"""
from __future__ import annotations

import itertools
import json
import math
from dataclasses import dataclass, field, replace
from datetime import timedelta
from functools import lru_cache
from typing import Any, Callable, Iterable, Mapping, Sequence

from .strength import apply_ranks, apply_strength
from .market import (CLEAR_FAVOR_PTS, FPTS_STAT, GROUPED_COMPONENTS, MARKET_MIN_REF,  # noqa: F401 (re-exported)
                     MARKET_SOURCE_LABEL, MARKET_WEIGHTS, MarketPool, _group, _market_raw, deal_drivers,
                     market_check_reasons, market_value, percentile, trade_market_check)
from ..models import FantasyTeam, LeagueContext, Player, Reason, Recommendation, RosterSlot
from ..valuation.replacement import eligible_slots
from .base import (DEFAULT_GAMES_PER_WEEK, IR_SLOT_NAMES, POSITION_PLURAL, PROTECT_OVERRIDE, SEASON_GAMES,
                   GameRate, _position_counts, cap_text,
                   gain_season, gain_week, game_rate, has_data, limit_positions, moves_left, moves_note,
                   player_age, protection_reason, roster_legal_after)

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
MARKET_PTS_TO_FPG = 0.05     # 10 market worth points ~ 0.5 FPG (sweet-spot penalty)
OVERPAY_FREE_PTS = SWEET_MAX_PERCEIVED   # market worth I may hand over before it costs ranking score
OVERPAY_COST_FPG = MARKET_PTS_TO_FPG     # ... each further worth point costs this much score (FPG)
OVERPAY_MIN_EDGE = 1.0       # a clear overpay (> CLEAR_FAVOR_PTS worth pts to them) is listed only with this edge

# -- market view and acceptance (a prior; calibrate_acceptance refits the logistic) ----------
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
PRESSURE_PTS = 8.0           # exploit: the deal relieves the counterparty's roster pressure

# -- draft picks (dynasty leagues with pick data; valuation.picks) ----------------------------
PICK_REBUILDER_MULT = 1.2    # a rebuilding team prizes picks: their market worth to it
PICK_CONTENDER_MULT = 0.8    # ... a contender less
PICK_ASK_MIN = CLEAR_FAVOR_PTS   # ask for one of their picks back when the deal is this far in their favour
PICK_TRIES_PER_TEAM = 6      # deals per counterparty tried with a pick added in

# -- enumeration, legacy display band, dynasty ------------------------------------------------
FAIR_BAND = 0.12             # model-value gap shown as FAIR_PCT (no longer a filter)
UNEVEN_BAND = (0.85, 1.15)
TOP_N = 12
MAX_EVALS_PER_TEAM = 900
REFINE_PER_TEAM = 12         # candidates per counterparty re-solved with the exact lineup solver
OVERPAY_CAP = 30.0           # skip candidates this far in their favour by market (p is at the ceiling)
FILL_CANDIDATES = 3          # FAs tried per starting slot type as the pickup for a freed roster spot
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

# -- exploits (roster pressure on the counterparty) -------------------------------------------
EXPLOIT_N = 8                # exploit_opportunities' default limit
EXPLOIT_PER_TEAM = 2         # deals per pressured team
INJURED_STATUSES = ("out", "ir", "ltir")   # IR-slot eligible on ESPN / Fantrax
PRESSURE_WEAK_VORP = -1.0    # a starting slot this far below replacement is "desperate"
MIN_HEALTHY_GOALIES = 2      # fewer healthy goalies with games this week = goalie shortage

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



# -- market view (MarketPool and helpers live in .market) ---------------------------------

def market_worth(pct: float) -> float:
    """Market worth (0-100) of a player at market percentile `pct`: 100 x (pct/100)^MARKET_CONVEXITY.

    Rank percentiles are linear in rank while trade value is convex: in a 300-player rostered
    pool the #2 and the #42 player differ by only ~13 percentile points, so sums of percentiles
    let two good players "buy" a superstar. Deals are therefore added up in worth points (mid
    pool, 10 percentile points ~ 8-9 worth points; near the top ~25)."""
    x = min(max(pct, 0.0), 100.0) / 100.0
    return 100.0 * x ** MARKET_CONVEXITY


def pick_worth(market: MarketPool, pick: Any, standing: str | None = None) -> float:
    """Market worth of a draft pick to a counterparty with ``standing``: no provider publishes a
    consensus pick value, so it is our pick value's percentile among the rostered players'
    dynasty values (model-only, ``market.pick_percentile``), x PICK_REBUILDER_MULT for a
    rebuilding team and PICK_CONTENDER_MULT for a contender."""
    from .market import pick_percentile

    mult = {"rebuilder": PICK_REBUILDER_MULT, "contender": PICK_CONTENDER_MULT}.get(standing or "", 1.0)
    return market_worth(pick_percentile(market, float(pick.value))) * mult


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
    give_picks, get_picks = getattr(deal, "give_picks", []), getattr(deal, "get_picks", [])
    st_them = (standing if standing is not None else team_standing(ctx)).get(their_team.team_id) \
        if (give_picks or get_picks) else None
    gets = package_value([market_worth(market.value(p)) for p in deal.give]
                         + [pick_worth(market, a, st_them) for a in give_picks])
    gives = package_value([market_worth(market.value(p)) for p in deal.get]
                          + [pick_worth(market, a, st_them) for a in get_picks])
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
    if getattr(deal, "pressure_text", None):
        perceived += PRESSURE_PTS
        notes.append(f"+{PRESSURE_PTS:g} relieves their roster pressure")
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
    pressure_text: str | None = None      # exploit: the counterparty roster pressure this relieves
    rate: GameRate | None = None          # my starters' games per week / left (gain units)
    refined: bool = False                 # lineups re-solved exactly (``_TradeEngine.refine``)
    me_after: list[list[Player]] = field(default_factory=list, repr=False)  # "after" rosters (_with_free_fill)
    them_after: list[list[Player]] = field(default_factory=list, repr=False)
    me_base: list[list[Player]] = field(default_factory=list, repr=False)    # "before" rosters (_baselines)
    them_base: list[list[Player]] = field(default_factory=list, repr=False)
    free_fill: Player | None = None       # FA my "before" uses to fill an empty starting slot (_free_fill)
    give_picks: list[Any] = field(default_factory=list)   # my draft picks sent (valuation.picks.PickAsset)
    get_picks: list[Any] = field(default_factory=list)    # their draft picks received

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
    def market_overpay(self) -> float:
        """Market worth points I hand over (they get - they give; bonuses excluded)."""
        acc = self.acceptance
        return acc.gets - acc.gives if acc is not None else 0.0

    @property
    def overpay_cost(self) -> float:
        return OVERPAY_COST_FPG * max(0.0, self.market_overpay - OVERPAY_FREE_PTS)

    @property
    def clear_overpay(self) -> bool:
        return self.market_overpay > CLEAR_FAVOR_PTS

    @property
    def listable(self) -> bool:
        """Worth showing after the market cost (score > 0); a clear overpay also needs a large edge."""
        return self.score > 0 and (not self.clear_overpay or self.my_edge >= OVERPAY_MIN_EDGE - 1e-9)

    @property
    def score(self) -> float:
        return self.ev - self.overpay_cost

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
        self.rate = game_rate(ctx)
        self.picks: dict[str, list[Any]] = {}     # holder team id -> PickAsset list (dynasty + pick data only)
        if self.units == "dynasty" and ctx.draft_picks:
            try:
                from ..valuation.picks import picks_by_holder, value_picks
                self.picks = picks_by_holder(value_picks(ctx, values, self.V))
            except Exception as e:  # noqa: BLE001 - picks are an optional add-in
                ctx.warnings.append(f"Draft pick values unavailable: {type(e).__name__}: {e}")
        fn = _optimal_lineup if lineup_fn is None else (lineup_fn or None)
        self.lineup = _LineupEvaluator(values, ctx.roster_shape, fn)  # type: ignore[arg-type]
        self._starter_cache: dict[frozenset[str], frozenset[str]] = {}
        self._base_cache: dict[tuple[str, str], list[list[Player]]] = {}
        self._free_cache: dict[frozenset[str], tuple[Player, list[Player]] | None] = {}
        cap = sum(int(n) for s, n in ctx.roster_shape.items() if s not in IR_SLOTS)
        self.sides: dict[str, _Side] = {}
        for t in ctx.teams:
            active = [s.player for s in t.slots if s.player is not None and s.slot not in IR_SLOTS]
            ir = [s.player for s in t.slots if s.player is not None and s.slot in IR_SLOTS]
            ir_cap = sum(int(n) for s, n in ctx.roster_shape.items() if s in IR_SLOTS)
            side = _Side(team=t, active=active, ir=ir, capacity=max(cap, len(active)),
                         free_ir=max(0, ir_cap - len(ir)))
            side.weakest = self._weakest_slot(active + ir)
            side.top = sorted((p for p in active + ir if self.V.get(p.cid, 0.0) > 0),
                              key=lambda p: self.V[p.cid], reverse=True)[:TOP_N]
            self.sides[t.team_id] = side
        self.fas = sorted((p for p in ctx.free_agents if p.cid in values
                           and p.status in ("healthy", "unknown")),
                          key=self._fpg, reverse=True)
        self.fas_by_slot = {t: [fa for fa in self.fas if _eligible(tuple(fa.positions), t)]
                            for t in dict.fromkeys(_starting_slots(ctx.roster_shape))}
        self.n_slots = len(_starting_slots(ctx.roster_shape))
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

    def _drop_order(self, cands: list[Player]) -> list[Player]:
        """Candidates in drop order (``_drop_key``); in dynasty mode players with no stats at all
        (unknown value: prospects) and protected prospects go last (a last resort)."""
        cands = sorted(cands, key=self._drop_key)
        if self.units == "dynasty":
            cands.sort(key=self._drop_tier)
        return cands

    def _drop_tier(self, p: Player) -> tuple[bool, ...]:
        if self.units != "dynasty":
            return ()
        return (p.cid in self.protected, not has_data(self.values.get(p.cid)))

    def _starters(self, players: list[Player]) -> frozenset[str]:
        """cids in the optimal season starting lineup of ``players`` (cached by roster)."""
        key = frozenset(p.cid for p in players)
        hit = self._starter_cache.get(key)
        if hit is None:
            assign = _greedy_assignment(players, self.values, self.ctx.roster_shape, LINEUP_HORIZON)
            hit = self._starter_cache[key] = frozenset(c for cids in assign.values() for c in cids)
            # the same assignment's value is the greedy lineup value (_greedy_lineup_value)
            self.lineup.greedy_cache.setdefault(key, sum(self.values[c].fpg_for(LINEUP_HORIZON)
                                                         for c in hit if c in self.values))
        return hit

    def _choose_drops(self, players: list[Player], cands: list[Player], n: int,
                      ir: Sequence[Player] = ()) -> list[Player]:
        """The ``n`` players a side cuts from ``players`` (only ``cands`` may go), one at a time:
        within the first drop tier (``_drop_tier``), the first in drop order outside the optimal
        starting lineup (cutting him costs the lineup nothing), else the starter whose loss costs
        the lineup least. A manager does not cut the backup goalie he now has to start (leaving
        a G slot empty) to keep a bench skater. ``ir``: players in IR slots, never cut but part
        of the lineup (their season value already counts the games they miss)."""
        drops: list[Player] = []
        cur = list(players)
        pool = self._drop_order(list(cands))
        tiers = {p.cid: self._drop_tier(p) for p in pool}
        ir = list(ir)
        for _ in range(max(0, n)):
            if not pool:
                break
            tier = tiers[pool[0].cid]
            group = [p for p in pool if tiers[p.cid] == tier]
            starters = self._starters(cur + ir)
            bench = [p for p in group if p.cid not in starters]
            if bench:
                pick = bench[0]
            else:
                base = self.lineup.greedy(cur + ir)
                pick = min(group, key=lambda p: base - self.lineup.greedy([q for q in cur + ir if q.cid != p.cid]))
            drops.append(pick)
            pool = [p for p in pool if p.cid != pick.cid]
            cur = [p for p in cur if p.cid != pick.cid]
        return drops

    def _pick_fill(self, players: list[Player], outgoing: list[Player], ir: list[Player]) -> Player | None:
        """The free agent a side picks up with the roster spot a trade frees (never one that breaks
        a position maximum): the one who adds the most to the optimal starting lineup of
        ``players`` (an open starting need: a slot the roster can no longer fill, or a starter he
        beats), among the FILL_CANDIDATES best FAs by season FPG per starting slot type; when none
        would start, the best FA at an outgoing player's position (bench depth: adds nothing to
        the lineup). His lineup value is not a trade gain by itself: ``_baselines`` gives the
        before-trade roster the same pickup."""
        limits = self.ctx.position_limits or {}
        counts = _position_counts(players + ir, limits) if limits else {}

        present = {p.cid for p in players + ir}    # e.g. a FA this trade already picked up

        def legal(fa: Player) -> bool:
            return fa.cid not in present and not (
                limits and any(counts[k] + 1 > int(limits[k]) for k in limit_positions(fa, limits)))

        cands: dict[str, Player] = {}
        for fas in self.fas_by_slot.values():
            n = 0
            for fa in fas:
                if n >= FILL_CANDIDATES:
                    break
                if legal(fa):
                    cands.setdefault(fa.cid, fa)
                    n += 1
        lineup = players + ir          # IR-slot players start once back (season value counts the games missed)
        starters = self._starters(lineup)
        base = self.lineup.greedy(lineup)
        # adding one player to a full lineup swaps him for one starter: gain <= his FPG - the
        # weakest starter's (the lineup is a matroid basis), else <= his FPG
        floor = min((self._fpg(p) for p in lineup if p.cid in starters), default=0.0) \
            if len(starters) >= self.n_slots else 0.0
        best: Player | None = None
        best_gain = 1e-9
        for fa in sorted(cands.values(), key=self._fpg, reverse=True):
            if self._fpg(fa) - floor <= best_gain + 1e-12:
                break
            gain = self.lineup.greedy(lineup + [fa]) - base
            if gain > best_gain + 1e-12:
                best, best_gain = fa, gain
        if best is not None:
            return best
        types = [t for t in self.fas_by_slot if t != "UTIL"] or list(self.fas_by_slot)
        need = {t for o in outgoing for t in types if _eligible(tuple(o.positions), t)}
        for fa in self.fas:
            if legal(fa) and any(_eligible(tuple(fa.positions), t) for t in need):
                return fa
        return None

    def _baselines(self, side: _Side, fill: Player | None) -> list[list[Player]]:
        """Rosters whose best optimal lineup is the side's "before": its current roster and, when
        the trade has it pick up ``fill``, its current roster with that same pickup made without
        the trade (cutting whom ``_choose_drops`` would cut). A free agent who would start is an
        improvement the side can make today, so the trade is credited only with what it adds on
        top of it (unmade lineup improvements are never credited to a trade). Each roster
        includes the side's IR-slot players (lineup pools, like ``evaluate``'s "after")."""
        rosters = self._with_free_fill(side.active, side.ir, side.capacity)
        if fill is None or any(p.cid == fill.cid for p in side.active):
            return rosters
        key = (side.team.team_id, fill.cid)
        hit = self._base_cache.get(key)
        if hit is None:
            hit = self._base_cache[key] = rosters + self._baselines_uncached(side, fill)[1:]
        return hit

    def _free_fill(self, active: list[Player], ir: list[Player], capacity: int
                   ) -> tuple[Player, list[Player]] | None:
        """(FA, active roster with him) when the lineup of ``active`` + ``ir`` leaves a starting
        slot empty and a free agent can fill it today, cutting a bench player (``_choose_drops``)
        when the roster is full; None otherwise. Cached by roster."""
        key = frozenset(p.cid for p in active + ir)
        if key in self._free_cache:
            return self._free_cache[key]
        hit = None
        if len(self._starters(active + ir)) < self.n_slots:
            fa = self._pick_fill(active, [], ir)
            if fa is not None:
                if len(active) < capacity:
                    alt = active + [fa]
                else:
                    cut = self._choose_drops(active, active, 1, ir=ir)
                    alt = [p for p in active if p.cid != cut[0].cid] + [fa] if cut else None
                limits = self.ctx.position_limits or {}
                if alt is not None and limits:
                    before = _position_counts(active + ir, limits)
                    after = _position_counts(alt + ir, limits)
                    if any(after[k] > max(int(limits[k]), before[k]) for k in limits):
                        alt = None
                if alt is not None:
                    hit = (fa, alt)
        self._free_cache[key] = hit
        return hit

    def _with_free_fill(self, active: list[Player], ir: list[Player], capacity: int,
                        side: _Side | None = None) -> list[list[Player]]:
        """Lineup pools for a roster: as is and, when a starting slot is empty, with the free-agent
        fill a manager can make today (``_free_fill``). Used for "before" and, when ``side``'s
        roster already had that hole before the trade, for "after" too, so a trade is credited only
        with what it adds over filling the slot from waivers (a hole the trade itself opens is
        ``_settle_roster``'s FA fill, as before)."""
        rosters = [active + ir]
        if side is not None and self._free_fill(side.active, side.ir, side.capacity) is None:
            return rosters
        free = self._free_fill(active, ir, capacity)
        if free is not None:
            rosters.append(free[1] + ir)
        return rosters

    def _baselines_uncached(self, side: _Side, fill: Player) -> list[list[Player]]:
        rosters = [side.active + side.ir]
        if len(side.active) < side.capacity:
            alt = side.active + [fill]
        else:
            cut = self._choose_drops(side.active, side.active, 1, ir=side.ir)
            if not cut:
                return rosters
            alt = [p for p in side.active if p.cid != cut[0].cid] + [fill]
        limits = self.ctx.position_limits or {}
        if limits:
            before = _position_counts(side.active + side.ir, limits)
            after = _position_counts(alt + side.ir, limits)
            if any(after[k] > max(int(limits[k]), before[k]) for k in limits):
                return rosters
        return [side.active + side.ir, alt + side.ir]

    def _delta(self, afters: list[list[Player]], bases: list[list[Player]], exact: bool) -> float:
        """optimal(after) - optimal(before) on the season horizon; each = the best of its rosters."""
        f = self.lineup if exact else self.lineup.greedy
        return max(f(r) for r in afters) - max(f(r) for r in bases)

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
        drop_order = self._drop_order

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
            kept = [p for p in players if p.cid not in moved]
            size_drops = self._choose_drops(kept, [p for p in kept if p.cid not in inc], max(0, over),
                                            ir=ir_left + to_ir)
            drops.extend(size_drops)
            gone = moved | {p.cid for p in size_drops}
            players = [p for p in players if p.cid not in gone]
        elif len(players) < side.capacity and len(outgoing) > len(incoming):
            fill = self._pick_fill(players, outgoing, ir_left)
            if fill is not None:
                players = players + [fill]
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
                 screen: bool = True, exact: bool = True, pressure: str | None = None) -> TradeEval:
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
        # lineup pools add the IR-slot players (season value already counts the games they miss),
        # so trading away an injured or day-to-day player in IR is not free
        me_ir_pool = [p for p in me.ir if p.cid not in give_ids] + my_ir
        me_pool = self._with_free_fill(me_after, me_ir_pool, me.capacity, me)
        # cheap greedy screen of my side first
        me_base = self._baselines(me, my_fill)
        d_me = self._delta(me_pool, me_base, exact=False)
        if self.mode is None:
            mine_ok = d_me > MIN_GAIN_ME - SCREEN_MARGIN
        else:
            core = ME_WEIGHT[self.mode] * d_me + DYN_WEIGHT[self.mode] * d_dyn_n
            mine_ok = core > MIN_GAIN_ME - SCREEN_MARGIN or d_dyn_n > MIN_GAIN_ME - SCREEN_MARGIN
        if screen and (not mine_ok or my_block is not None):
            return TradeEval(team=them.team, give=list(give), get=list(get), v_in=v_in, v_out=v_out, fair=fair,
                             fair_dev=dev, delta_me=d_me, delta_them=0.0, need_text=None, my_drops=my_drops,
                             their_drops=[], my_fill=my_fill, their_fill=None, my_ir=my_ir, mode=self.mode,
                             delta_dyn=d_dyn, delta_dyn_n=d_dyn_n, blocked=my_block, my_cap=my_cap, screened=True,
                             pressure_text=pressure, rate=self.rate)

        them_after = [p for p in them.active if p.cid not in get_ids] + list(give)
        them_after, their_drops, their_fill, their_ir, their_block, their_cap = self._settle_roster(
            them, them_after, give, get)
        blocked = my_block or (f"{them.team.name} {their_block}" if their_block else None)
        need_text = None
        if them.weakest is not None:
            slot, v = them.weakest
            # an injured / suspended player can't fill their hole now
            fits = [p for p in give if _eligible(tuple(p.positions), slot)
                    and p.status not in (*INJURED_STATUSES, "suspended")]
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
        them_pool = self._with_free_fill(them_after, [p for p in them.ir if p.cid not in get_ids] + their_ir,
                                         them.capacity, them)
        them_base = self._baselines(them, their_fill)
        d_them = self._delta(them_pool, them_base, exact=False)
        ev = TradeEval(team=them.team, give=list(give), get=list(get), v_in=v_in, v_out=v_out,
                       fair=fair, fair_dev=dev, delta_me=d_me, delta_them=d_them, need_text=need_text,
                       my_drops=my_drops, their_drops=their_drops, my_fill=my_fill, their_fill=their_fill,
                       my_ir=my_ir, their_ir=their_ir, mode=self.mode, delta_dyn=d_dyn, delta_dyn_n=d_dyn_n,
                       blocked=blocked, my_cap=my_cap, their_cap=their_cap,
                       block_text="; ".join(block) or None, my_illegal=None if ok else why,
                       pressure_text=pressure, rate=self.rate)
        ev.me_after, ev.them_after = me_pool, them_pool
        ev.me_base, ev.them_base = me_base, them_base
        free = self._free_fill(me.active, me.ir, me.capacity)
        ev.free_fill = free[0] if free is not None else None
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
        ev.delta_me = self._delta(ev.me_after, ev.me_base, exact=True)
        ev.delta_them = self._delta(ev.them_after, ev.them_base, exact=True)
        if self.ctx.dynasty:
            ev.acceptance = self._accept(ev)
        return ev

    def with_pick(self, ev: TradeEval) -> TradeEval | None:
        """``ev`` with one draft pick added to balance it: when the market calls it far in their
        favour (perceived > PICK_ASK_MIN) the pick of theirs that gives the best EV while they
        still plausibly accept; when they would refuse it (p_accept < MIN_P_ACCEPT), my cheapest
        pick that makes it acceptable. A pick counts in my dynasty change at its ``net`` value
        (over the roster spot the draftee takes) and in their view at its full value
        (``pick_worth``). The variant must be accepted and beat ``ev``'s EV (or rescue a deal
        ``ev`` could not make). None when no pick helps."""
        if (self.mode is None or not self.picks or ev.screened or ev.acceptance is None
                or ev.give_picks or ev.get_picks):
            return None
        mine = self.ctx.my_team.team_id
        if ev.perceived > PICK_ASK_MIN:
            side, options = "get", self.picks.get(ev.team.team_id, [])
        elif ev.p < MIN_P_ACCEPT:
            # cheapest to me (net of the roster spot) first; among equals, the one they value most
            side, options = "give", sorted(self.picks.get(mine, []), key=lambda a: (a.net, -a.value))
        else:
            return None
        best: TradeEval | None = None
        for a in options:
            var = replace(ev, give_picks=[a] if side == "give" else [], get_picks=[a] if side == "get" else [])
            var.delta_dyn = ev.delta_dyn + (a.net if side == "get" else -a.net)
            var.delta_dyn_n = var.delta_dyn / self.scale
            var.acceptance = self._accept(var)
            if not var.accepted or (ev.accepted and var.ev <= ev.ev):
                continue
            if side == "give":
                return var                  # the cheapest pick that makes the deal
            if best is None or var.ev > best.ev:
                best = var
        return best

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
    if perceived > CLEAR_FAVOR_PTS:
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


DEFAULT_RATE = GameRate(DEFAULT_GAMES_PER_WEEK, float(SEASON_GAMES), "default")   # a TradeEval without a rate


def _to_rec(ev: TradeEval, V: Mapping[str, float], units: str, source: str,
            market: MarketPool | None = None) -> Recommendation:
    def vl(ps: list[Player]) -> str:
        return ", ".join(f"{p.name} ({V.get(p.cid, 0.0):.2f})" for p in ps)

    acc = ev.acceptance
    band = FAIR_BAND if len(ev.give) == len(ev.get) else UNEVEN_BAND[1] - 1.0
    rate = ev.rate or DEFAULT_RATE
    wk, ssn = gain_week(ev.delta_me, rate), gain_season(ev.delta_me, rate)
    edge_txt = f"You gain {ev.delta_me:+.2f} pts/game this season ({wk:+.1f} pts/week, {ssn:+.0f} rest of season)"
    if ev.mode is not None:
        edge_txt += f" (dynasty {ev.delta_dyn_n:+.2f}; {ev.mode} edge {ev.core:+.2f})"
    if ev.roto_adj:
        edge_txt += f"; roto balance {-ev.roto_adj:+.2f}"
    reasons = [Reason(code="MY_EDGE", text=edge_txt, value=ev.my_edge, baseline=MIN_GAIN_ME)]
    if acc is not None:
        src = "/".join(MARKET_SOURCE_LABEL[s] for s in acc.sources) or "no market data"
        # label = market value moved (they get - give); value stays the acceptance input (harness calibration)
        reasons.append(Reason(code="MARKET_VIEW",
                              text=f"{market_label(ev.market_overpay)} by market value ({src}, "
                                   f"{ev.market_overpay:+.0f} worth pts to them); acceptance {_pct_text(acc.p)}",
                              value=acc.perceived, baseline=acc.base))
        if ev.overpay_cost > 0:
            reasons.append(Reason(code="MARKET_COST",
                                  text=f"You hand over {ev.market_overpay:.0f} market worth pts ({OVERPAY_FREE_PTS:g} "
                                       f"free): ranked {ev.overpay_cost:.2f} lower, since that value could buy "
                                       f"something in another trade",
                                  value=-ev.overpay_cost, baseline=ev.market_overpay))
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
                                   f"EV = {ev.my_edge:+.2f} x {acc.p:.2f} = {ev.ev:+.2f}"
                                   + (f"; score {ev.score:+.2f} after the market cost" if ev.overpay_cost > 0 else ""),
                              value=acc.p, baseline=acc.p_raw))
    src_txt = "your starters' NHL schedule" if rate.source == "schedule" else (
        "season over" if rate.source == "season over" else "no schedule loaded: league-average default")
    reasons += [
        Reason(code="GAIN_WEEK", text=f"{wk:+.1f} pts/week ({ev.delta_me:+.2f}/game x {rate.per_week:.1f} "
                                      f"games/week, {src_txt})", value=wk, baseline=rate.per_week),
        Reason(code="GAIN_SEASON", text=f"{ssn:+.0f} pts rest of season ({ev.delta_me:+.2f}/game x "
                                        f"{rate.remaining:.0f} games left)", value=ssn, baseline=rate.remaining),
    ]
    reasons += [
        Reason(code="DELTA_ME", text=f"My lineup {ev.delta_me:+.2f} FPG ({source}"
                                     + (f"; before = my roster with the same pickup of {ev.my_fill.name}"
                                        if ev.my_fill is not None and len(ev.me_base) > 1 else "")
                                     + (f"; before and after both fill your empty slot from waivers "
                                        f"({ev.free_fill.name}) when that is better"
                                        if ev.free_fill is not None else "") + ")",
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
    for a in ev.get_picks:
        reasons.append(Reason(code="PICK_IN",
                              text=f"Ask for {ev.team.name}'s {a.short(ev.team.team_id)} pick (dynasty value "
                                   f"{a.value:.1f}, {a.net:.1f} net of its roster spot; expected slot "
                                   f"{a.expected_slot:.0f}): the deal is far in their favour by market value, so a "
                                   f"pick back evens it",
                              value=a.net, baseline=a.value))
    for a in ev.give_picks:
        reasons.append(Reason(code="PICK_OUT",
                              text=f"Add your {a.short(a.holder)} pick (dynasty value {a.value:.1f}, costs you "
                                   f"{a.net:.1f} net of its roster spot; expected slot {a.expected_slot:.0f}): "
                                   f"without it they would likely refuse",
                              value=-a.net, baseline=a.value))
    if market is not None:   # consensus check: display only
        if acc is not None and (deal := trade_market_check(ev.market_overpay, ev.my_edge,
                                                           deal_drivers(market, ev.get, ev.give))) is not None:
            reasons.append(deal)
        reasons += market_check_reasons(market, [(p, "receive") for p in ev.get] + [(p, "send") for p in ev.give])
        from .market import pick_check_reason
        reasons += [pick_check_reason(a, "receive", ev.team.team_id) for a in ev.get_picks]
        reasons += [pick_check_reason(a, "send", a.holder) for a in ev.give_picks]
    give_txt = " + ".join([p.name for p in ev.give] + [f"your {a.short(a.holder)} pick" for a in ev.give_picks])
    get_txt = " + ".join([p.name for p in ev.get] + [f"their {a.short(ev.team.team_id)} pick" for a in ev.get_picks])
    return Recommendation(kind="trade", score=ev.score,
                          title=f"Trade {give_txt} to {ev.team.name} for {get_txt}",
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
    """Trade proposals ranked by score = EV less the market overpay cost (module docstring), at most
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
        refused: list[TradeEval] = []      # good for me but they would say no: a pick may rescue them
        for give, get, _ in eng.candidates(t.team_id):
            key = (frozenset(p.cid for p in give), frozenset(p.cid for p in get))
            if key in seen:
                continue
            seen.add(key)
            ev = eng.evaluate(t.team_id, give, get, exact=False)
            if ev.near_plausible:
                near.append(ev)
            elif eng.picks and not ev.screened and ev.blocked is None and ev.my_illegal is None:
                refused.append(ev)
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
        if eng.picks:   # draft picks as add-ins: ask for one back from overpays, sweeten refusals
            asks = sorted((e for e in team_evals if e.perceived > PICK_ASK_MIN), key=lambda e: e.score,
                          reverse=True)[:PICK_TRIES_PER_TEAM]
            sweeten = sorted(refused, key=lambda e: e.my_edge, reverse=True)[:PICK_TRIES_PER_TEAM]
            for e in asks + sweeten:
                eng.refine(e)
                var = eng.with_pick(e)
                if var is None:
                    continue
                at = next((i for i, x in enumerate(team_evals) if x is e), None)
                if at is not None:
                    team_evals[at] = var
                else:
                    team_evals.append(var)
        # a 2-for-1 that merely pads an accepted 1-for-1 (same core, no better EV) is redundant
        singles = {(frozenset(p.cid for p in e.give), frozenset(p.cid for p in e.get)): e.score
                   for e in team_evals if len(e.give) == 1 and len(e.get) == 1}
        for e in team_evals:
            if not e.listable:   # a clear market overpay needs a large edge (module docstring)
                continue
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
            r = _to_rec(e, eng.V, eng.units, source, eng.market)
            r.score = e.delta_dyn_n * e.p
            r.predicted_gain, r.gain_units = float(e.delta_dyn_n), "dynasty"
            future_only.append(r)
        apply_ranks(apply_strength(future_only))
    if sweet_spot is not None:
        sweet_spot.extend(apply_ranks(apply_strength([_to_rec(e, eng.V, eng.units, source, eng.market) for e in sweet])))
    return apply_ranks(apply_strength([_to_rec(e, eng.V, eng.units, source, eng.market) for e in chosen]))


# -- exploits: counterparties under roster pressure --------------------------------------------

@dataclass
class Pressure:
    """Roster pressure on a counterparty that a trade can relieve (``exploit_opportunities``).

    kind: "cap" (at a position maximum with an injured player there, or over it), "ir" (more
    injured players than IR slots), "goalies" (fewer than MIN_HEALTHY_GOALIES healthy goalies
    with games this week), "weak" (a starting slot with starter VORP < PRESSURE_WEAK_VORP).
    ``pool``: their players a relieving deal can take (cap / ir); ``position`` / ``slot``: what
    a relieving deal moves; ``floor``: their weakest starter's VORP (weak); ``healthy``: their
    healthy goalies with games this week (goalies)."""
    kind: str
    text: str
    pool: list[Player] = field(default_factory=list)
    position: str | None = None
    slot: str | None = None
    floor: float = 0.0
    healthy: frozenset[str] = frozenset()


def _injured(p: Player) -> bool:
    return p.status in INJURED_STATUSES


def _games_this_week(ctx: LeagueContext, values: Mapping[str, Any], p: Player) -> int | None:
    """NHL games in the 7 days from ``ctx.as_of`` (valuation's games_next7, else the schedule)."""
    g = getattr(values.get(p.cid), "games_next7", None)
    if isinstance(g, int):
        return g
    dates = (ctx.schedule or {}).get(p.team or "")
    if dates is None:
        return None
    end = ctx.as_of + timedelta(days=6)
    return sum(1 for d in dates if ctx.as_of <= d <= end)


def _league_has_games_this_week(ctx: LeagueContext) -> bool:
    end = ctx.as_of + timedelta(days=6)
    return any(ctx.as_of <= d <= end for ds in (ctx.schedule or {}).values() for d in ds)


def _healthy_goalie(ctx: LeagueContext, values: Mapping[str, Any], p: Player) -> bool:
    return (p.is_goalie and not _injured(p) and p.status != "suspended"
            and (_games_this_week(ctx, values, p) or 0) > 0)


def team_pressures(ctx: LeagueContext, values: Mapping[str, Any], team: FantasyTeam,
                   weakest: tuple[str, float] | None = None,
                   median_weak: Mapping[str, float] | None = None) -> list[Pressure]:
    """Roster pressures on `team` (``Pressure``); `weakest` = its weakest starting slot and that
    starter's VORP, `median_weak` = the league median of the same per slot (display only)."""
    out: list[Pressure] = []
    players = team.players
    limits = ctx.position_limits or {}
    if limits:
        counts = _position_counts(players, limits)
        for k in sorted(limits):
            lim = int(limits[k])
            at_k = [p for p in players if k in limit_positions(p, limits)]
            hurt = [p for p in at_k if _injured(p)]
            plural = POSITION_PLURAL.get(k, k)
            if counts[k] > lim:
                out.append(Pressure("cap", f"{counts[k]} {plural}, over the {k} limit {lim}", pool=at_k, position=k))
            elif counts[k] == lim and hurt:
                out.append(Pressure("cap", f"{counts[k]} {plural} at the {k} limit {lim} with {_names(hurt)} "
                                           f"injured (no room to add a healthy one)", pool=at_k, position=k))
    ir_slots = sum(int(ctx.roster_shape.get(s, 0)) for s in IR_SLOT_NAMES)
    hurt = [p for p in players if _injured(p)]
    if hurt and len(hurt) > ir_slots:
        plural = "slot" if ir_slots == 1 else "slots"
        out.append(Pressure("ir", f"{len(hurt)} injured players ({_names(hurt)}) for {ir_slots} IR {plural}",
                            pool=hurt))
    if ctx.schedule and _league_has_games_this_week(ctx):
        healthy = [p for p in players if _healthy_goalie(ctx, values, p)]
        if len(healthy) < MIN_HEALTHY_GOALIES:
            n = len(healthy)
            word = "goalie" if n == 1 else "goalies"
            out.append(Pressure("goalies", f"only {n} healthy {word} with games this week",
                                healthy=frozenset(p.cid for p in healthy)))
    if weakest is not None and weakest[1] < PRESSURE_WEAK_VORP:
        slot, v = weakest
        if v == -math.inf:
            text = f"an empty {slot} slot"
        else:
            med = (median_weak or {}).get(slot)
            tail = f", league median {med:+.2f}" if med is not None and math.isfinite(med) else ""
            text = f"a weak {slot} slot (starter VORP {v:+.2f}{tail})"
        out.append(Pressure("weak", text, slot=slot, floor=v))
    return out


def relieves(pr: Pressure, give: Sequence[Player], get: Sequence[Player], ctx: LeagueContext,
             values: Mapping[str, Any]) -> bool:
    """Does a deal (`give` goes to them, `get` comes from them) relieve `pr`?"""
    if pr.kind == "cap":
        limits = ctx.position_limits or {}
        n_out = sum(1 for p in get if pr.position in limit_positions(p, limits))
        n_in = sum(1 for p in give if pr.position in limit_positions(p, limits))
        return n_out > n_in
    if pr.kind == "ir":
        return sum(1 for p in get if _injured(p)) > sum(1 for p in give if _injured(p))
    if pr.kind == "goalies":
        return (sum(1 for p in give if _healthy_goalie(ctx, values, p))
                > sum(1 for p in get if p.cid in pr.healthy))
    if pr.kind == "weak":
        def vorp(p: Player) -> float:
            return float(getattr(values.get(p.cid), "vorp", 0.0) or 0.0)
        return any(_eligible(tuple(p.positions), pr.slot) and vorp(p) > pr.floor for p in give)
    return False


def _median(xs: Iterable[float]) -> float | None:
    ys = sorted(x for x in xs if math.isfinite(x))
    if not ys:
        return None
    m = len(ys) // 2
    return ys[m] if len(ys) % 2 else 0.5 * (ys[m - 1] + ys[m])


def exploit_opportunities(ctx: LeagueContext, values: Mapping[str, Any],
                          dynasty_values: Mapping[str, Any] | None = None, limit: int = EXPLOIT_N,
                          wants: Mapping[str, set[str]] | None = None,
                          offered: Mapping[str, set[str]] | None = None,
                          lineup_fn: LineupFn | None | bool = None,
                          accept_params: AcceptParams | None = None,
                          per_team: int = EXPLOIT_PER_TEAM) -> list[Recommendation]:
    """Trades that exploit a counterparty's roster pressure (``team_pressures``): for each
    pressured opponent, the best deals for me (by EV) that relieve one of its pressures, scored
    like ``recommend_trades`` with PRESSURE_PTS added to their perceived fairness.

    Candidates: 1-for-1 over my whole active roster (a weaker piece may be enough) x their
    top-12 plus the pressured players (``Pressure.pool``), 2-for-1 over my top 12, 1-for-2 taking
    at least one pressured player; a candidate must relieve a pressure (``relieves``). Same
    filters as ``recommend_trades`` (my edge > MIN_GAIN_ME, p_accept >= MIN_P_ACCEPT, both
    rosters legal, no protected-prospect drop, the dynasty mode's this-season floor); a deal that
    needs my free-agent pickup is skipped when I have no moves left (``base.moves_left``).
    At most one deal per pressure kind and `per_team` per team, MAX_PER_GIVEN per player I give,
    `limit` overall, ranked by EV. Each rec carries an EXPLOIT reason (third, after MY_EDGE and
    MARKET_VIEW): "Exploit: Team X has 3 goalies at the G limit 3 with Y injured (...)",
    value PRESSURE_PTS.

    Not detected: a counterparty with 0 moves left this week; providers only expose my own
    transaction counter (``ctx.moves_used_this_period``)."""
    eng = _TradeEngine(ctx, values, dynasty_values, wants, lineup_fn, offered, accept_params)
    me = eng.sides[ctx.my_team.team_id]
    no_moves = moves_left(ctx) == 0
    by_slot: dict[str, list[float]] = {}
    for side in eng.sides.values():
        if side.weakest is not None:
            by_slot.setdefault(side.weakest[0], []).append(side.weakest[1])
    median_weak = {k: m for k, v in by_slot.items() if (m := _median(v)) is not None}
    floor = eng.params.perceived_for(MIN_P_ACCEPT) - NEED_PTS - BLOCK_PTS - PRESSURE_PTS
    mine_all = sorted(me.active, key=lambda p: eng.V.get(p.cid, 0.0), reverse=True)
    picked: list[tuple[TradeEval, list[Pressure]]] = []
    for t in ctx.teams:
        if t.team_id == me.team.team_id:
            continue
        side = eng.sides[t.team_id]
        prs = team_pressures(ctx, values, t, side.weakest, median_weak)
        if not prs:
            continue
        pool = {p.cid: p for pr in prs for p in pr.pool}
        theirs = list({**{p.cid: p for p in side.top}, **pool}.values())
        shapes: list[tuple[tuple[Player, ...], tuple[Player, ...]]] = [((a,), (b,)) for a in mine_all for b in theirs]
        shapes += [(pair, (b,)) for pair in itertools.combinations(me.top, 2) for b in theirs]
        shapes += [((a,), pair) for a in me.top for pair in itertools.combinations(theirs, 2)
                   if not pool or any(p.cid in pool for p in pair)]
        cands = []
        for give, get in shapes:
            rel = [pr for pr in prs if relieves(pr, give, get, ctx, values)]
            if not rel:
                continue
            base = eng._pkg(give) - eng._pkg(get)
            if floor - 1e-9 <= base <= OVERPAY_CAP + 1e-9:
                cands.append((give, get, rel, base))
        cands.sort(key=lambda c: (len(c[0]) + len(c[1]), abs(c[3])))
        near: list[tuple[TradeEval, list[Pressure]]] = []
        for give, get, rel, _ in cands[:MAX_EVALS_PER_TEAM]:
            ev = eng.evaluate(t.team_id, list(give), list(get), exact=False,
                              pressure="; ".join(pr.text for pr in rel))
            if ev.near_plausible:
                near.append((ev, rel))
        near.sort(key=lambda x: x[0].score, reverse=True)
        for ev, _ in near[:REFINE_PER_TEAM]:
            eng.refine(ev)
        ok = [(ev, rel) for ev, rel in near
              if (ev.refined or not eng.lineup.exact) and ev.accepted
              and not (no_moves and ev.my_fill is not None)]
        ok.sort(key=lambda x: x[0].score, reverse=True)
        seen_kinds: set[str] = set()
        for ev, rel in ok:
            kinds = {pr.kind for pr in rel}
            if kinds <= seen_kinds:
                continue
            seen_kinds |= kinds
            picked.append((ev, rel))
    picked.sort(key=lambda x: x[0].score, reverse=True)
    rel_of = {id(ev): rel for ev, rel in picked}
    chosen = diverse([ev for ev, _ in picked], limit, per_team)
    budget = moves_note(ctx)
    out: list[Recommendation] = []
    for ev in chosen:
        rel = rel_of[id(ev)]
        r = _to_rec(ev, eng.V, eng.units, eng.lineup.source, eng.market)
        r.reasons.insert(2, Reason(code="EXPLOIT",
                                   text=f"Exploit: {ev.team.name} has {'; '.join(pr.text for pr in rel)}",
                                   value=PRESSURE_PTS, baseline=float(len(rel))))
        if ev.my_fill is not None and budget:
            r.reasons.append(Reason(code="MOVE_BUDGET", text=f"The free-agent pickup uses a move ({budget})",
                                    value=float(moves_left(ctx) or 0)))
        out.append(r)
    return apply_ranks(apply_strength(out))


def exploit_text(r: Recommendation) -> str | None:
    """The EXPLOIT reason's text ("Exploit: Team X has ...") of an exploit rec, else None."""
    return next((x.text for x in r.reasons if x.code == "EXPLOIT"), None)


# -- trade finder by need (``fm trades --need``) ------------------------------------------------

NEED_SLOTS = 2               # my weakest starting slot types targeted
NEED_GET_N = 6               # their players per team tried at my need slots
NEED_PER_TEAM = 2
COMPLEMENT_MULT = 1.15       # ranking only: a partner heading the other way (contender <-> rebuilder)
STAR_MULT = 1.10             # ranking only: 2-for-1 where I get the best player in the deal
LONG_TERM_BOTTOM = 1 / 3     # a position group whose future value ranks in the league's bottom third
_STANDING_DIR = {"contender": "contend", "rebuilder": "rebuild"}
_GROUP_SLOTS = {"G": ("G",), "D": ("D",), "F": ("F", "C", "LW", "RW", "UTIL")}


@dataclass
class Need:
    """A starting slot type I should trade for: ``gap`` = my starters' mean VORP there minus the
    league median (-inf: an empty slot); ``long_term`` = from future value (dynasty)."""
    slot: str
    gap: float
    text: str
    long_term: bool = False


def _slot_vorps(players: list[Player], values: Mapping[str, Any], shape: Mapping[str, int]) -> dict[str, float]:
    """slot type -> mean VORP of the starters there (-inf when a slot of that type is empty)."""
    assign = _greedy_assignment(players, values, shape, LINEUP_HORIZON)
    out: dict[str, float] = {}
    for slot in dict.fromkeys(_starting_slots(shape)):
        if slot == "UTIL":
            continue
        cids = assign.get(slot, [])
        if len(cids) < int(shape.get(slot, 0)):
            out[slot] = -math.inf
        else:
            vs = [float(values[c].vorp) for c in cids if c in values]
            out[slot] = sum(vs) / len(vs) if vs else -math.inf
    return out


def find_needs(ctx: LeagueContext, values: Mapping[str, Any], outlook: Any = None,
               dynasty_values: Mapping[str, Any] | None = None) -> list[Need]:
    """My weakest starting slot types this season (vs the league median, at most NEED_SLOTS) and,
    in dynasty leagues heading to rebuild / balanced, the position group whose future value ranks
    in the league's bottom third."""
    per_team = {t.team_id: _slot_vorps(t.players, values, ctx.roster_shape) for t in ctx.teams}
    mine = per_team.get(ctx.my_team.team_id, {})
    needs: list[Need] = []
    for slot, v in mine.items():
        med = _median(x.get(slot, -math.inf) for x in per_team.values())
        med = med if med is not None else 0.0
        gap = v - med
        if v == -math.inf:
            text = f"an empty {slot} slot"
        else:
            text = f"{slot}: your starters' VORP {v:+.2f} vs the league median {med:+.2f}"
        needs.append(Need(slot, gap, text))
    needs.sort(key=lambda n: n.gap)
    out = [n for n in needs if n.gap < 0][:NEED_SLOTS] or needs[:1]
    direction = getattr(outlook, "recommended", None) or (getattr(ctx, "dynasty_mode", None) if ctx.dynasty else None)
    if ctx.dynasty and dynasty_values and direction in ("rebuild", "balanced"):
        try:
            from ..analysis.outlook import future_values
            from ..valuation.dynasty import position_group
        except Exception:  # pragma: no cover - optional
            return out
        totals: dict[str, dict[str, float]] = {}
        for t in ctx.teams:
            for p in t.players:
                f = future_values(p, ctx, values, dynasty_values)
                g = position_group(p)
                totals.setdefault(g, {}).setdefault(t.team_id, 0.0)
                totals[g][t.team_id] += sum(f) / len(f)
        for g, by_team in totals.items():
            ranked = sorted(by_team, key=lambda tid: -by_team[tid])
            me_id = ctx.my_team.team_id
            if me_id not in ranked or ranked.index(me_id) < len(ranked) * (1 - LONG_TERM_BOTTOM):
                continue
            slot = next((s for s in _GROUP_SLOTS[g] if s in ctx.roster_shape), None)
            if slot is None or any(n.slot == slot for n in out):
                continue
            out.append(Need(slot, -1.0, f"{g} long term: your future value ranks #{ranked.index(me_id) + 1} of "
                                        f"{len(ranked)}", long_term=True))
    return out


def _direction(team_id: str, outlook: Any, standing: Mapping[str, str]) -> str | None:
    d = outlook.direction(team_id) if outlook is not None else None
    return d or _STANDING_DIR.get(standing.get(team_id, ""))


def _complementary(a: str | None, b: str | None) -> bool:
    win, later = {"contend", "contender"}, {"rebuild", "pretender"}
    return bool(a and b and ((a in win and b in later) or (a in later and b in win)))


def pitch_text(ev: TradeEval, need: Need | None, mine_dir: str | None, their_dir: str | None,
               ctx: LeagueContext, values: Mapping[str, Any], dynasty_values: Mapping[str, Any] | None) -> str:
    """A short message to send the other manager: the offer, why it helps them (their direction,
    their weakest slot) and, briefly, what I am after. It never quotes our model's edge."""
    give = " + ".join([p.name for p in ev.give] + [f"my {a.short(a.holder)} pick" for a in ev.give_picks])
    get = " + ".join([p.name for p in ev.get] + [f"your {a.short(ev.team.team_id)} pick" for a in ev.get_picks])
    def fpg(p: Player) -> float:
        return float(getattr(values.get(p.cid), "fpg_season", 0.0) or 0.0)

    lead = max(ev.give, key=fpg, default=None)
    parts = [f"Hi {ev.team.name}, I'd send {give} for {get}."]
    if their_dir in ("contend", "contender") and lead is not None and ev.delta_them > 0:
        parts.append(f"You're in the race this season: {lead.name} ({fpg(lead):.1f} pts/game) makes your lineup "
                     f"better right now.")
    elif their_dir in ("rebuild", "pretender") and ev.give:
        young = min(ev.give, key=lambda p: player_age(p, ctx, dynasty_values) or 99.0)
        age = player_age(young, ctx, dynasty_values)
        parts.append(f"You're building for later: {young.name}"
                     + (f" is {age:.0f} and" if age is not None else "") + " fits your timeline.")
    elif len(ev.give) > len(ev.get) and ev.give:
        parts.append(f"Two everyday players for one: {' and '.join(p.name for p in ev.give)} give you depth across "
                     f"your lineup.")
    elif len(ev.give) < len(ev.get) and lead is not None:
        parts.append(f"You get the best player in the deal ({lead.name}) and a free roster spot.")
    elif lead is not None:
        parts.append(f"{lead.name} has been good for {fpg(lead):.1f} pts/game.")
    if ev.need_text:
        slot = ev.need_text.split("weakest slot ", 1)[-1].split(" ", 1)[0] if "weakest slot " in ev.need_text else None
        fits = next((p for p in ev.give if slot and _eligible(tuple(p.positions), slot)), lead)
        if fits is not None and slot:
            parts.append(f"{fits.name} slots right into your weakest spot ({slot}).")
    if ev.give_picks:
        parts.append("The pick is there to make it worth your while.")
    if need is not None:
        what = "an empty spot" if need.gap == -math.inf else "a hole"
        when = " for the next few seasons" if need.long_term else ""
        if mine_dir in ("contend", "contender"):
            parts.append(f"I'm going for it this season and have {what} at {need.slot}.")
        elif mine_dir == "rebuild":
            parts.append(f"I'm building for the next few seasons and want to shore up {need.slot}.")
        else:
            parts.append(f"I'm looking to shore up {need.slot}{when}.")
    return " ".join(parts)


def recommend_need_trades(ctx: LeagueContext, values: Mapping[str, Any],
                          dynasty_values: Mapping[str, Any] | None = None, outlook: Any = None,
                          limit: int = 5, per_team: int = NEED_PER_TEAM,
                          wants: Mapping[str, set[str]] | None = None,
                          offered: Mapping[str, set[str]] | None = None,
                          lineup_fn: LineupFn | None | bool = None,
                          accept_params: AcceptParams | None = None,
                          needs: list[Need] | None = None) -> list[Recommendation]:
    """Trades aimed at my needs (``find_needs``): from every other team, the players eligible at a
    need slot who beat my current starters there, against my top players: 1-for-1, 2-for-1 (I give
    two) and 1-for-2, plus a draft pick to even overpays / rescue refusals, all scored by the
    regular evaluator (same filters as ``recommend_trades``). Ranking = score (EV less the market
    overpay cost) x COMPLEMENT_MULT when the partner heads the other way (``outlook`` directions,
    else standings) x STAR_MULT for a star consolidation (2-for-1, I get the best player). Each rec
    carries NEED_MATCH, PITCH (a message to send) and, when they apply, COMPLEMENT and
    STAR_CONSOLIDATION reasons; ``score`` stays the plain trade score."""
    needs = needs if needs is not None else find_needs(ctx, values, outlook, dynasty_values)
    if not needs:
        return []
    eng = _TradeEngine(ctx, values, dynasty_values, wants, lineup_fn, offered, accept_params)
    me = eng.sides[ctx.my_team.team_id]
    mine_slot = _slot_vorps(me.active + me.ir, values, ctx.roster_shape)
    mine_dir = getattr(outlook, "recommended", None) or (ctx.dynasty_mode if ctx.dynasty else None)
    floor = eng.params.perceived_for(MIN_P_ACCEPT) - NEED_PTS - BLOCK_PTS

    def vorp(p: Player) -> float:
        return float(getattr(values.get(p.cid), "vorp", 0.0) or 0.0)

    def fills(p: Player) -> Need | None:
        for n in needs:
            if _eligible(tuple(p.positions), n.slot) and p.status not in (*INJURED_STATUSES, "suspended") \
                    and (n.long_term or vorp(p) > mine_slot.get(n.slot, -math.inf)):
                return n
        return None

    picked: list[tuple[float, TradeEval, Need]] = []
    for t in ctx.teams:
        if t.team_id == me.team.team_id:
            continue
        side = eng.sides[t.team_id]
        gets = sorted((p for p in side.active + side.ir if eng.V.get(p.cid, 0.0) > 0 and fills(p) is not None),
                      key=lambda p: eng.V[p.cid], reverse=True)[:NEED_GET_N]
        if not gets:
            continue
        theirs = list({p.cid: p for p in [*gets, *side.top]}.values())
        shapes: list[tuple[tuple[Player, ...], tuple[Player, ...]]] = [((a,), (b,)) for a in me.top for b in gets]
        shapes += [(pair, (b,)) for pair in itertools.combinations(me.top, 2) for b in gets]
        shapes += [((a,), pair) for a in me.top for pair in itertools.combinations(theirs, 2)
                   if any(p.cid in {g.cid for g in gets} for p in pair)]
        cands = []
        for give, get in shapes:
            base = eng._pkg(give) - eng._pkg(get)
            if floor - 1e-9 <= base <= OVERPAY_CAP + 1e-9:
                cands.append((give, get, base))
        cands.sort(key=lambda c: (len(c[0]) + len(c[1]), abs(c[2])))
        near: list[TradeEval] = []
        refused: list[TradeEval] = []
        for give, get, _ in cands[:MAX_EVALS_PER_TEAM]:
            ev = eng.evaluate(t.team_id, list(give), list(get), exact=False)
            if ev.near_plausible:
                near.append(ev)
            elif eng.picks and not ev.screened and ev.blocked is None and ev.my_illegal is None:
                refused.append(ev)
        near.sort(key=lambda e: e.score, reverse=True)
        for ev in near[:REFINE_PER_TEAM]:
            eng.refine(ev)
        ok = [e for e in near if (e.refined or not eng.lineup.exact) and e.accepted]
        if eng.picks:
            tries = sorted((e for e in ok if e.perceived > PICK_ASK_MIN), key=lambda e: e.score, reverse=True)
            tries += sorted(refused, key=lambda e: e.my_edge, reverse=True)
            for e in tries[:PICK_TRIES_PER_TEAM]:
                eng.refine(e)
                var = eng.with_pick(e)
                if var is not None:
                    ok = [var if x is e else x for x in ok] if any(x is e for x in ok) else ok + [var]
        their_dir = _direction(t.team_id, outlook, eng.standing)
        comp = _complementary(mine_dir, their_dir)
        team_rows = []
        for e in ok:
            if not e.listable:
                continue
            need = next((n for p in e.get if (n := fills(p)) is not None), None)
            if need is None:
                continue
            star = len(e.give) >= 2 and len(e.get) == 1 \
                and eng.V.get(e.get[0].cid, 0.0) > max(eng.V.get(p.cid, 0.0) for p in e.give)
            rank = e.score * (COMPLEMENT_MULT if comp else 1.0) * (STAR_MULT if star else 1.0)
            team_rows.append((rank, e, need))
        team_rows.sort(key=lambda x: -x[0])
        picked += team_rows[:per_team]
    picked.sort(key=lambda x: -x[0])
    by_ev = {id(e): (rank, need) for rank, e, need in picked}
    chosen = diverse([e for _, e, _ in picked], limit, per_team)
    out: list[Recommendation] = []
    for e in chosen:
        _, need = by_ev[id(e)]
        their_dir = _direction(e.team.team_id, outlook, eng.standing)
        r = _to_rec(e, eng.V, eng.units, eng.lineup.source, eng.market)
        incoming = next((p for p in e.get if _eligible(tuple(p.positions), need.slot)), e.get[0])
        extra = [Reason(code="NEED_MATCH", text=f"Fills your need at {need.slot}: {incoming.name} ({need.text})",
                        value=None if need.gap == -math.inf else need.gap)]
        if _complementary(mine_dir, their_dir):
            extra.append(Reason(code="COMPLEMENT",
                                text=f"{e.team.name} is heading the other way ({their_dir} vs your {mine_dir}): "
                                     f"you value these players differently, which makes a deal easier"))
        if len(e.give) >= 2 and len(e.get) == 1 \
                and eng.V.get(e.get[0].cid, 0.0) > max(eng.V.get(p.cid, 0.0) for p in e.give):
            extra.append(Reason(code="STAR_CONSOLIDATION",
                                text=f"Star consolidation: you get the best player in the deal ({e.get[0].name}) "
                                     f"and free a roster spot; trade calculators tend to undervalue the star side "
                                     f"of a 2-for-1"))
        extra.append(Reason(code="PITCH", text=pitch_text(e, need, mine_dir, their_dir, ctx, values, dynasty_values)))
        r.reasons[2:2] = extra
        out.append(r)
    return apply_ranks(apply_strength(out))


def pitch_of(r: Recommendation) -> str | None:
    """The PITCH reason's text of a need trade, else None."""
    return next((x.text for x in r.reasons if x.code == "PITCH"), None)
