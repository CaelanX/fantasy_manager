"""How the league at large rates players (the market), independent of our model, and the
consensus check that compares the two.

``MarketPool`` gives every player a 0-100 market percentile within the league's rostered pool
(ESPN ADP, % rostered, Fantrax's projected FP/G; in dynasty leagues also our dynasty value as a
stand-in for dynasty consensus). Trades use it for acceptance (``trades.p_accept``).

Consensus check (``market_check_reason``, reason MARKET_CHECK): for each player a waiver pickup or
trade moves, his market percentile from the independent signals only (ADP, % rostered, Fantrax
projected FP/G; never our own dynasty value or FPG) next to his model percentile (our season FPG
within the same rostered pool, goalies and skaters ranked separately). A gap of DISAGREE_GAP
percentile points or more is flagged "model disagrees with market" with the model signals that
drive it (role news, rookie model, injury return, start share, ...). Display only: scores and
ranking are unchanged.
"""
from __future__ import annotations

import bisect
from typing import Any, Iterable, Mapping, Sequence

from ..models import Player, Reason, Recommendation

FPTS_STAT = "FPTS"   # Fantrax's own fantasy points in its StatLines (providers.fantrax.FPTS_KEY)
MARKET_WEIGHTS = {"adp": 2.0, "proj": 2.0, "owned": 1.0, "dyn": 2.0}   # % rostered saturates near 100
GROUPED_COMPONENTS = frozenset({"proj", "dyn", "fpg"})   # ranked within goalies / skaters
MARKET_MIN_REF = 10          # a component needs this many rostered players reporting it
MARKET_SOURCE_LABEL = {"adp": "ADP", "owned": "rostered %", "proj": "Fantrax projected FP/G",
                       "dyn": "dynasty value", "fpg": "our FPG"}
CONSENSUS_COMPONENTS = ("adp", "owned", "proj")   # independent of our model
DISAGREE_GAP = 25.0          # |model - market| percentile points flagged as a disagreement
CLEAR_FAVOR_PTS = 10.0       # a trade this many market worth points in their favour "looks clearly in their favor"
# PlayerValue reason codes that move our value away from consensus, as reader-facing drivers
DRIVER_LABELS = {
    "ROLE_DFO": "line / PP role", "ROLE_NEWS": "role news", "ROLE_BOOST": "line / PP role",
    "ROLE_DEPTH": "line / PP role", "ROLE_SCRATCH": "scratched", "START_NEWS": "goalie news",
    "ROOKIE_BLEND": "rookie model", "NHLE": "rookie model", "PEDIGREE_PRIOR": "rookie model",
    "GP_EXPECTATION": "expected games", "ROOKIE_GAMES": "expected games",
    "RETURN_ESTIMATE": "injury return estimate", "START_SHARE": "goalie start share",
    "START_ACTUAL": "goalie start share",
    "PRESEASON": "preseason", "TEAMMATE_OUT": "teammate out",
}


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

    def __init__(self, ctx: Any, values: Mapping[str, Any] | None = None,
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

    def consensus(self, p: Player) -> tuple[float, tuple[str, ...]] | None:
        """(market percentile, sources) from the independent signals only, or None without any."""
        comps = {k: v for k, v in self.components(p).items() if k in CONSENSUS_COMPONENTS}
        if not comps:
            return None
        w = sum(MARKET_WEIGHTS[k] for k in comps)
        return sum(MARKET_WEIGHTS[k] * v for k, v in comps.items()) / w, tuple(k for k in CONSENSUS_COMPONENTS
                                                                              if k in comps)

    def model_percentile(self, p: Player) -> float | None:
        """Percentile of our season FPG among the rostered players of his group."""
        fpg = getattr(self.values.get(p.cid), "fpg_season", None)
        ref = self.ref.get(("fpg", _group("fpg", p)))
        if fpg is None or ref is None:
            return None
        return percentile(float(fpg), ref)


def _ordinal(x: float) -> str:
    n = int(round(x))
    suffix = "th" if 10 <= n % 100 <= 20 else {1: "st", 2: "nd", 3: "rd"}.get(n % 10, "th")
    return f"{n}{suffix}"


def market_value(player: Player, pool: MarketPool) -> float:
    """0-100 market percentile of `player` within the league's rostered pool."""
    return pool.value(player)


def drivers(p: Player, pv: Any) -> list[str]:
    """Model signals behind `p`'s value that consensus may not price in (DRIVER_LABELS order of
    appearance), plus an unhealthy status."""
    out = [DRIVER_LABELS[r.code] for r in (getattr(pv, "reasons", None) or []) if r.code in DRIVER_LABELS]
    if p.status not in ("healthy", "unknown"):
        out.append(f"status {p.status}")
    return list(dict.fromkeys(out))[:3]


def market_check_reason(p: Player, pool: MarketPool, role: str) -> Reason | None:
    """MARKET_CHECK for one player (``role``: "add", "drop", "receive", "send"): market vs model
    percentile; value = gap (model - market), baseline = market percentile. None when the
    league publishes no independent signal for him or he has no model value."""
    cons, model = pool.consensus(p), pool.model_percentile(p)
    if cons is None or model is None:
        return None
    market, src = cons
    gap = model - market
    text = (f"Market check ({role}) {p.name}: market {_ordinal(market)} pct "
            f"({'/'.join(MARKET_SOURCE_LABEL[s] for s in src)}) vs model {_ordinal(model)} ({gap:+.0f})")
    if abs(gap) >= DISAGREE_GAP:
        why = drivers(p, pool.values.get(p.cid))
        text += (": model disagrees with market, rates him "
                 + ("higher" if gap > 0 else "lower")
                 + (f" ({', '.join(why)})" if why else " (projection only, no specific signal)"))
    else:
        text += ": market agrees"
    return Reason(code="MARKET_CHECK", text=text, value=round(gap, 1), baseline=round(market, 1))


def market_check_reasons(pool: MarketPool, players: Iterable[tuple[Player, str]]) -> list[Reason]:
    """MARKET_CHECK reasons for (player, role) pairs, skipping players without signals."""
    return [r for p, role in players if (r := market_check_reason(p, pool, role)) is not None]


def pick_percentile(pool: MarketPool, value: float) -> float:
    """Percentile of a draft pick's dynasty value among the rostered skaters' dynasty values:
    a model-only market stand-in (no provider publishes consensus pick values); 50 without a
    dynasty reference."""
    ref = pool.ref.get(("dyn", "S"))
    return percentile(value, ref) if ref else 50.0


def pick_check_reason(pick: Any, role: str, holder_id: str | None = None) -> Reason:
    """MARKET_CHECK for a draft pick a trade moves: always model-only (value None, so it never
    counts as an agreement or a disagreement in ``market_disagrees``)."""
    return Reason(code="MARKET_CHECK",
                  text=f"Market check ({role}) {pick.short(holder_id)} pick: model-only value {pick.value:.1f} "
                       f"({pick.net:.1f} net of its roster spot; expected slot {pick.expected_slot:.0f}); "
                       f"no consensus source for draft picks",
                  value=None, baseline=None)


def market_disagrees(rec: Recommendation) -> bool | None:
    """True when any MARKET_CHECK on ``rec`` is a disagreement (|gap| >= DISAGREE_GAP) or the
    deal-level overpay check (``trade_market_check``: value None, baseline = worth points to them)
    is present; False when checks exist and all agree; None without any market check."""
    checks = [r for r in rec.reasons if r.code == "MARKET_CHECK"]
    if not checks:
        return None
    return any((r.value is None and r.baseline is not None and r.baseline > CLEAR_FAVOR_PTS)
               or (r.value is not None and abs(r.value) >= DISAGREE_GAP) for r in checks)


def deal_drivers(pool: MarketPool, get: Iterable[Player], give: Iterable[Player]) -> list[str]:
    """Why the model likes a trade the market does not: the drivers of players I receive whom the
    model rates above the market and of players I send whom it rates below."""
    out: list[str] = []
    for ps, sign in ((get, 1.0), (give, -1.0)):
        for p in ps:
            cons, model = pool.consensus(p), pool.model_percentile(p)
            if cons is not None and model is not None and sign * (model - cons[0]) >= DISAGREE_GAP:
                out += [f"{p.name}: {d}" for d in drivers(p, pool.values.get(p.cid))] or \
                    [f"{p.name}: projection only"]
    return list(dict.fromkeys(out))[:3]


def trade_market_check(overpay: float, my_edge: float, why: list[str] | None = None) -> Reason | None:
    """Deal-level MARKET_CHECK when the market calls a trade clearly in the counterparty's favour
    (overpay > CLEAR_FAVOR_PTS market worth points handed over) while our model has me gaining."""
    if overpay <= CLEAR_FAVOR_PTS or my_edge <= 0:
        return None
    return Reason(code="MARKET_CHECK",
                  text=f"Model disagrees with market: you overpay by market value ({overpay:+.0f} worth pts to "
                       f"them); worth it only if you trust the model's edge of {my_edge:+.2f} (drivers: "
                       f"{'; '.join(why) if why else 'projection only, no specific signal'})",
                  value=None, baseline=overpay)
